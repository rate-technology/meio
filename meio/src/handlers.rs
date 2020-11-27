//! This module contains the `Envelope` that allow
//! to call methods of actors related to a sepcific
//! imcoming message.

use crate::{Actor, Context, Id};
use anyhow::{anyhow, Error};
use async_trait::async_trait;
use futures::channel::oneshot;

pub(crate) struct Envelope<A: Actor> {
    handler: Box<dyn Handler<A>>,
}

impl<A: Actor> Envelope<A> {
    pub(crate) async fn handle(
        &mut self,
        actor: &mut A,
        ctx: &mut Context<A>,
    ) -> Result<(), Error> {
        self.handler.handle(actor, ctx).await
    }

    /*
    /// Creates an `Envelope` for `Interaction`.
    pub(crate) fn interaction<I>(input: I) -> (Self, oneshot::Receiver<Result<I::Output, Error>>)
    where
        A: InteractionHandler<I>,
        I: Interaction,
    {
        let (tx, rx) = oneshot::channel();
        let handler = InteractionHandlerImpl {
            input: Some(input),
            tx: Some(tx),
        };
        let this = Self {
            handler: Box::new(handler),
        };
        (this, rx)
    }
    */

    // TODO: Is it posiible to use `handle` method directly and drop this one?
    /// Creates an `Envelope` for `Action`.
    pub(crate) fn action<I>(input: I) -> Self
    where
        A: ActionHandler<I>,
        I: Action,
    {
        let handler = ActionHandlerImpl { input: Some(input) };
        Self {
            handler: Box::new(handler),
        }
    }
}

// TODO: Consider renaming to attached action
pub(crate) enum Operation {
    // TODO: Awake, Interrupt, also can be added here!
    Done { id: Id },
    Forward,
}

pub(crate) struct HpEnvelope<A: Actor> {
    pub operation: Operation,
    pub envelope: Envelope<A>,
}

/// Internal `Handler` type that used by `Actor`'s routine to execute
/// `ActionHandler` or `InteractionHandler`.
#[async_trait]
trait Handler<A: Actor>: Send {
    /// Main method that expects a mutable reference to `Actor` that
    /// will be used by implementations to handle messages.
    async fn handle(&mut self, actor: &mut A, _ctx: &mut Context<A>) -> Result<(), Error>;
}

/*
/// `Interaction` type can be sent to an `Actor` that implements
/// `InteractionHandler` for that message type.
/// It has to return a response of `Output` type.
pub trait Interaction: Send + 'static {
    /// The type of a response.
    type Output: Send + 'static;

    /// Indicates that this message have to be sent with high-priority.
    fn is_high_priority(&self) -> bool {
        false
    }
}

/// Type of `Handler` to process interaction in request-response style.
#[async_trait]
pub trait InteractionHandler<I: Interaction>: Actor {
    /// Asyncronous method that receives incoming message and return a response.
    async fn handle(&mut self, input: I, _ctx: &mut Context<Self>) -> Result<I::Output, Error>;
}

struct InteractionHandlerImpl<I, O> {
    input: Option<I>,
    tx: Option<oneshot::Sender<Result<O, Error>>>,
}

#[async_trait]
impl<A, I, O> Handler<A> for InteractionHandlerImpl<I, O>
where
    A: Actor + InteractionHandler<I>,
    I: Interaction<Output = O>,
    O: Send + 'static,
{
    async fn handle(&mut self, actor: &mut A, ctx: &mut Context<A>) -> Result<(), Error> {
        let input = self
            .input
            .take()
            .expect("interaction handler called twice (no msg)");
        let response = actor.handle(input, ctx).await;
        let tx = self
            .tx
            .take()
            .expect("interaction handler called twice (no tx)");
        tx.send(response)
            .map_err(|_| anyhow!("can't send a response of interaction"))?;
        Ok(())
    }
}
*/

/// `Action` type can be sent to an `Actor` that implements
/// `ActionHandler` for that message type.
pub trait Action: Send + 'static {
    /// Indicates that this message have to be sent with high-priority.
    fn is_high_priority(&self) -> bool {
        false
    }
}

/// Type of `Handler` to process incoming messages in one-shot style.
#[async_trait]
pub trait ActionHandler<I: Action>: Actor {
    /// Asyncronous method that receives incoming message.
    async fn handle(&mut self, input: I, _ctx: &mut Context<Self>) -> Result<(), Error>;
}

struct ActionHandlerImpl<I> {
    input: Option<I>,
}

#[async_trait]
impl<A, I> Handler<A> for ActionHandlerImpl<I>
where
    A: Actor + ActionHandler<I>,
    I: Action,
{
    async fn handle(&mut self, actor: &mut A, ctx: &mut Context<A>) -> Result<(), Error> {
        let input = self.input.take().expect("action handler called twice");
        actor.handle(input, ctx).await
    }
}

pub struct Interaction<IN, OUT> {
    pub(crate) request: IN,
    pub(crate) responder: oneshot::Sender<Result<OUT, Error>>,
}

impl<IN, OUT> Action for Interaction<IN, OUT>
where
    IN: Send + 'static,
    OUT: Send + 'static,
{}

pub struct Joiner {
    pub(crate) responder: oneshot::Sender<Result<(), Error>>,
}

impl Action for Joiner {}
