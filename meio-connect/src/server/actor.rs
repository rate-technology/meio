use super::link;
use crate::talker::{Talker, TalkerCompatible, TermReason, WsIncoming};
use anyhow::Error;
use async_trait::async_trait;
use futures::channel::mpsc;
use headers::HeaderMapExt;
use hyper::server::conn::AddrStream;
use hyper::service::Service;
use hyper::upgrade::Upgraded;
use hyper::{Body, Request, Response, Server, StatusCode};
use meio::prelude::{
    Action, ActionHandler, Actor, Address, Context, IdOf, Interaction, InteractionHandler,
    InterruptedBy, LiteTask, Scheduled, StartedBy, StopReceiver, TaskEliminated, TaskError,
};
use meio_protocol::Protocol;
use serde::de::DeserializeOwned;
use slab::Slab;
use std::future::Future;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{self, Poll};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_tungstenite::WebSocketStream;
use tungstenite::protocol::Role;

pub trait DirectPath: Sized + Send + Sync + 'static {
    type Parameter;
    fn paths() -> &'static [&'static str];
}

impl<T> FromRequest for T
where
    T: DirectPath,
    Self: DeserializeOwned,
{
    type Output = Self;

    fn from_request(request: &Request<Body>) -> Option<Self::Output> {
        let uri = request.uri();
        let path = uri.path();
        if Self::paths().iter().any(|p| p == &path) {
            let query = uri.query().unwrap_or("");
            serde_qs::from_str(query).ok()
        } else {
            None
        }
    }
}

impl<T> WsFromRequest for T
where
    T: DirectPath,
    T::Parameter: Protocol,
    Self: DeserializeOwned,
{
    type Output = Self;
    type Protocol = T::Parameter;

    fn from_request(request: &Request<Body>) -> Option<Self::Output> {
        let uri = request.uri();
        let path = uri.path();
        if Self::paths().iter().any(|p| p == &path) {
            let query = uri.query().unwrap_or("");
            serde_qs::from_str(query).ok()
        } else {
            None
        }
    }
}

pub trait FromRequest: Sized + Send + Sync + 'static {
    type Output: Send;

    fn from_request(request: &Request<Body>) -> Option<Self::Output>;
}

pub struct Req<T: FromRequest> {
    pub request: T::Output,
}

impl<T: FromRequest> Interaction for Req<T> {
    type Output = Response<Body>;
}

pub type RouteResult =
    Result<Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send>>, Request<Body>>;

pub trait Route: Send + Sync + 'static {
    fn try_route(&self, addr: &SocketAddr, request: Request<Body>) -> RouteResult;
}

pub struct WebRoute<E, A>
where
    A: Actor,
{
    extracted: PhantomData<E>,
    address: Address<A>,
}

impl<E, A> WebRoute<E, A>
where
    A: Actor,
{
    pub fn new(address: Address<A>) -> Self {
        Self {
            extracted: PhantomData,
            address,
        }
    }
}

impl<E, A> Route for WebRoute<E, A>
where
    E: FromRequest,
    A: Actor + InteractionHandler<Req<E>>,
{
    fn try_route(&self, _addr: &SocketAddr, request: Request<Body>) -> RouteResult {
        if let Some(value) = E::from_request(&request) {
            let mut address = self.address.clone();
            let msg = Req { request: value };
            let fut = async move { address.interact(msg).await };
            Ok(Box::pin(fut))
        } else {
            Err(request)
        }
    }
}

pub trait WsFromRequest: Sized + Send + Sync + 'static {
    type Output: Send;
    type Protocol: Protocol;

    fn from_request(request: &Request<Body>) -> Option<Self::Output>;
}

pub struct WsReq<T: WsFromRequest> {
    pub request: T::Output,
    pub stream: WsHandler<T::Protocol>,
}

impl<T: WsFromRequest> Action for WsReq<T> {}

pub struct WsRoute<E, A>
where
    A: Actor,
{
    extracted: PhantomData<E>,
    address: Address<A>,
}

impl<E, A> WsRoute<E, A>
where
    A: Actor,
{
    pub fn new(address: Address<A>) -> Self {
        Self {
            extracted: PhantomData,
            address,
        }
    }
}

impl<E, A> Route for WsRoute<E, A>
where
    E: WsFromRequest,
    A: Actor + ActionHandler<WsReq<E>>,
{
    fn try_route(&self, addr: &SocketAddr, mut request: Request<Body>) -> RouteResult {
        if let Some(value) = E::from_request(&request) {
            let mut res = Response::new(Body::empty());
            let mut address = self.address.clone();
            if request.headers().typed_get::<headers::Upgrade>().is_none() {
                *res.status_mut() = StatusCode::BAD_REQUEST;
                // TODO: Return error
            }
            let ws_key = request.headers().typed_get::<headers::SecWebsocketKey>();
            let addr = *addr;
            tokio::task::spawn(async move {
                match hyper::upgrade::on(&mut request).await {
                    Ok(upgraded) => {
                        let websocket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                            upgraded,
                            Role::Server,
                            None,
                        )
                        .await;
                        let stream = WsHandler::new(addr, websocket);
                        let msg = WsReq {
                            request: value,
                            stream,
                        };
                        address.act(msg).await?;
                        Ok(())
                    }
                    Err(err) => {
                        log::error!("upgrade error: {}", err);
                        Err(Error::from(err))
                    }
                }
            });
            *res.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
            res.headers_mut()
                .typed_insert(headers::Connection::upgrade());
            res.headers_mut()
                .typed_insert(headers::Upgrade::websocket());
            if let Some(value) = ws_key {
                res.headers_mut()
                    .typed_insert(headers::SecWebsocketAccept::from(value));
            }
            let fut = futures::future::ready(Ok(res));
            Ok(Box::pin(fut))
        } else {
            Err(request)
        }
    }
}

#[derive(Clone, Default)]
struct RoutingTable {
    routes: Arc<RwLock<Slab<Box<dyn Route>>>>,
}

pub struct HttpServer {
    addr: SocketAddr,
    routing_table: RoutingTable,
    insistent: bool,
}

impl HttpServer {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            routing_table: RoutingTable::default(),
            insistent: true,
        }
    }

    fn start_http_listener(&self, ctx: &mut Context<Self>) {
        let server_task = HyperRoutine {
            addr: self.addr,
            routing_table: self.routing_table.clone(),
        };
        ctx.spawn_task(server_task, ());
    }
}

impl Actor for HttpServer {
    type GroupBy = ();
}

#[async_trait]
impl<T: Actor> StartedBy<T> for HttpServer {
    async fn handle(&mut self, ctx: &mut Context<Self>) -> Result<(), Error> {
        self.start_http_listener(ctx);
        Ok(())
    }
}

#[async_trait]
impl<T: Actor> InterruptedBy<T> for HttpServer {
    async fn handle(&mut self, ctx: &mut Context<Self>) -> Result<(), Error> {
        ctx.shutdown();
        Ok(())
    }
}

#[async_trait]
impl TaskEliminated<HyperRoutine> for HttpServer {
    async fn handle(
        &mut self,
        _id: IdOf<HyperRoutine>,
        result: Result<(), TaskError>,
        ctx: &mut Context<Self>,
    ) -> Result<(), Error> {
        if !ctx.is_terminating() {
            if let Err(err) = result {
                log::error!("Server failed: {}", err);
                if self.insistent {
                    let when = Instant::now() + Duration::from_secs(3);
                    log::debug!("Schedule restarting of: {} at {:?}", self.addr, when);
                    ctx.address().schedule(RestartListener, when)?;
                }
            }
        }
        Ok(())
    }
}

struct RestartListener;

#[async_trait]
impl Scheduled<RestartListener> for HttpServer {
    async fn handle(
        &mut self,
        _timestamp: Instant,
        _action: RestartListener,
        ctx: &mut Context<Self>,
    ) -> Result<(), Error> {
        log::info!("Attempt to restart server: {}", self.addr);
        self.start_http_listener(ctx);
        Ok(())
    }
}

#[async_trait]
impl ActionHandler<link::AddRoute> for HttpServer {
    async fn handle(&mut self, msg: link::AddRoute, _ctx: &mut Context<Self>) -> Result<(), Error> {
        let mut routes = self.routing_table.routes.write().await;
        routes.insert(msg.route);
        Ok(())
    }
}

struct HyperRoutine {
    addr: SocketAddr,
    routing_table: RoutingTable,
}

#[async_trait]
impl LiteTask for HyperRoutine {
    type Output = ();

    async fn routine(self, stop: StopReceiver) -> Result<Self::Output, Error> {
        let routing_table = self.routing_table.clone();
        let make_svc = MakeSvc { routing_table };
        let server = Server::try_bind(&self.addr)?.serve(make_svc);
        server.with_graceful_shutdown(stop.into_future()).await?;
        Ok(())
    }
}

struct Svc {
    addr: SocketAddr,
    routing_table: RoutingTable,
}

pub type SvcFut<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl Service<Request<Body>> for Svc {
    type Response = Response<Body>;
    type Error = hyper::Error;
    type Future = SvcFut<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _: &mut task::Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let uri = req.uri().to_owned();
        log::trace!("Incoming request path: {}", uri.path());
        let routing_table = self.routing_table.clone();
        let addr = self.addr;
        let fut = async move {
            let mut route = None;
            {
                let routes = routing_table.routes.read().await;
                let mut opt_req = Some(req);
                for (_idx, r) in routes.iter() {
                    if let Some(req) = opt_req.take() {
                        let res = r.try_route(&addr, req);
                        match res {
                            Ok(r) => {
                                route = Some(r);
                                break;
                            }
                            Err(req) => {
                                opt_req = Some(req);
                            }
                        }
                    }
                }
            }
            let mut response;
            if let Some(route) = route {
                let resp = route.await;
                match resp {
                    Ok(resp) => {
                        response = resp;
                    }
                    Err(err) => {
                        log::error!("Server error for {}: {}", uri, err);
                        response = Response::new(Body::empty());
                        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    }
                }
            } else {
                log::warn!("No route for {}", uri);
                response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::NOT_FOUND;
            }
            Ok(response)
        };
        Box::pin(fut)
    }
}

struct MakeSvc {
    routing_table: RoutingTable,
}

impl<'a> Service<&'a AddrStream> for MakeSvc {
    type Response = Svc;
    type Error = hyper::Error;
    type Future = SvcFut<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _: &mut task::Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, addr_stream: &'a AddrStream) -> Self::Future {
        let routing_table = self.routing_table.clone();
        let addr = addr_stream.remote_addr();
        let fut = async move {
            Ok(Svc {
                addr,
                routing_table,
            })
        };
        Box::pin(fut)
    }
}

pub type WebSocket = WebSocketStream<Upgraded>;

struct WsInfo<P: Protocol> {
    addr: SocketAddr,
    connection: WebSocket,
    rx: mpsc::UnboundedReceiver<P::ToClient>,
}

/// This struct wraps a `WebSocket` connection
/// and produces processors for every incoming connection.
pub struct WsHandler<P: Protocol> {
    addr: SocketAddr,
    info: Option<WsInfo<P>>,
    tx: mpsc::UnboundedSender<P::ToClient>,
}

impl<P: Protocol> WsHandler<P> {
    fn new(addr: SocketAddr, websocket: WebSocket) -> Self {
        let (tx, rx) = mpsc::unbounded();
        let info = WsInfo {
            addr,
            connection: websocket,
            rx,
        };
        Self {
            addr,
            info: Some(info),
            tx,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn worker<A>(&mut self, address: Address<A>) -> WsProcessor<P, A>
    where
        A: Actor + ActionHandler<WsIncoming<P::ToServer>>,
    {
        let info = self.info.take().expect("already started");
        WsProcessor { info, address }
    }

    pub fn send(&mut self, msg: P::ToClient) {
        if let Err(err) = self.tx.unbounded_send(msg) {
            log::error!("Can't send outgoing WS message: {}", err);
        }
    }
}

pub struct WsProcessor<P: Protocol, A: Actor> {
    info: WsInfo<P>,
    address: Address<A>,
}

impl<P, A> TalkerCompatible for WsProcessor<P, A>
where
    P: Protocol,
    A: Actor + ActionHandler<WsIncoming<P::ToServer>>,
{
    type WebSocket = WebSocket;
    type Message = tungstenite::Message;
    type Error = tungstenite::Error;
    type Actor = A;
    type Codec = P::Codec;
    type Incoming = P::ToServer;
    type Outgoing = P::ToClient;
}

#[async_trait]
impl<P, A> LiteTask for WsProcessor<P, A>
where
    P: Protocol,
    A: Actor + ActionHandler<WsIncoming<P::ToServer>>,
{
    type Output = TermReason;

    fn name(&self) -> String {
        format!("WsProcessor({})", self.info.addr)
    }

    async fn routine(self, stop: StopReceiver) -> Result<Self::Output, Error> {
        let mut talker =
            Talker::<Self>::new(self.address, self.info.connection, self.info.rx, stop);
        talker.routine().await
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Default, Deserialize)]
    struct Index {}

    #[test]
    fn qs_parsing() {
        let _index: Index = serde_qs::from_str("").unwrap();
    }
}
