//! Contains message of the `Actor`'s lifecycle.

use crate::actor_runtime::Actor;
use crate::handlers::{InstantAction, InstantActionHandler, Operation};
use crate::ids::{Id, IdOf};
use crate::linkage::Address;
use crate::lite_runtime::{LiteTask, TaskAddress, TaskError};
use anyhow::Error;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

struct Stage {
    terminating: bool,
    ids: HashSet<Id>,
}

impl Default for Stage {
    fn default() -> Self {
        Self {
            terminating: false,
            ids: HashSet::new(),
        }
    }
}

impl Stage {
    fn is_finished(&self) -> bool {
        self.terminating && self.ids.is_empty()
    }
}

struct Record<A: Actor> {
    group: A::GroupBy,
    notifier: Box<dyn LifecycleNotifier<Interrupt<A>>>,
}

// TODO: Rename to Terminator again
pub(crate) struct LifetimeTracker<A: Actor> {
    terminating: bool,
    prioritized: Vec<A::GroupBy>,
    stages: HashMap<A::GroupBy, Stage>,
    records: HashMap<Id, Record<A>>,
}

// TODO: Change T to A
impl<A: Actor> LifetimeTracker<A> {
    // TODO: Make the constructor private
    pub fn new() -> Self {
        Self {
            terminating: false,
            // TODO: with_capacity 0 ?
            prioritized: Vec::new(),
            stages: HashMap::new(),
            records: HashMap::new(),
        }
    }

    pub fn is_terminating(&self) -> bool {
        self.terminating
    }

    // TODO: Rename to `insert_actor`
    pub fn insert<T>(&mut self, address: Address<T>, group: A::GroupBy)
    where
        T: Actor + InstantActionHandler<Interrupt<A>>,
    {
        let stage = self.stages.entry(group.clone()).or_default();
        let id: Id = address.id().into();
        stage.ids.insert(id.clone());
        let mut notifier = LifecycleNotifier::once(address, Operation::Forward);
        if stage.terminating {
            log::warn!(
                "Actor added into the terminating state (interrupt it immediately): {}",
                id
            );
            let interrupt_event = Interrupt::new();
            if let Err(err) = notifier.notify(interrupt_event) {
                log::error!("Can't interrupt actor {:?} immediately: {}", id, err);
            }
        }
        let record = Record { group, notifier };
        self.records.insert(id, record);
    }

    pub fn insert_task<T>(&mut self, stopper: TaskAddress<T>, group: A::GroupBy)
    where
        T: LiteTask,
    {
        let stage = self.stages.entry(group.clone()).or_default();
        let id: Id = stopper.id().into();
        stage.ids.insert(id.clone());
        let mut notifier = LifecycleNotifier::stop(stopper);
        if stage.terminating {
            log::warn!(
                "Task added into the terminating state (interrupt it immediately): {}",
                id
            );
            // But this event will never received, because LiteTasks can't do that.
            // Instead it will set stop signal to watcher.
            let interrupt_event = Interrupt::new();
            if let Err(err) = notifier.notify(interrupt_event) {
                log::error!("Can't interrupt task {:?} immediately: {}", id, err);
            }
        }
        let record = Record { group, notifier };
        self.records.insert(id, record);
    }

    pub fn remove(&mut self, id: &Id) {
        if let Some(record) = self.records.remove(id) {
            if let Some(stage) = self.stages.get_mut(&record.group) {
                stage.ids.remove(id);
            }
        }
        if self.terminating {
            self.try_terminate_next();
        }
    }

    // TODO: Change `Vec` to `OrderedSet`
    pub fn prioritize_termination_by(&mut self, groups: Vec<A::GroupBy>) {
        self.prioritized = groups;
    }

    pub fn is_finished(&self) -> bool {
        self.stages.values().all(Stage::is_finished)
    }

    fn stage_sequence(&self) -> Vec<A::GroupBy> {
        let stages_to_term: HashSet<_> = self.stages.keys().cloned().collect();
        let prior_set: HashSet<_> = self.prioritized.iter().cloned().collect();
        let remained = stages_to_term.difference(&prior_set).cloned();
        let mut sequence = self.prioritized.clone();
        sequence.extend(remained);
        sequence
    }

    fn try_terminate_next(&mut self) {
        self.terminating = true;
        for stage_name in self.stage_sequence() {
            if let Some(stage) = self.stages.get_mut(&stage_name) {
                if !stage.terminating {
                    stage.terminating = true;
                    for id in stage.ids.iter() {
                        if let Some(record) = self.records.get_mut(id) {
                            let interrupt_event = Interrupt::new();
                            if let Err(err) = record.notifier.notify(interrupt_event) {
                                log::error!(
                                    "Can't notify the supervisor about actor with {:?} termination: {}",
                                    id,
                                    err
                                );
                            }
                        }
                    }
                }
                if !stage.is_finished() {
                    break;
                }
            }
        }
    }

    pub fn start_termination(&mut self) {
        self.try_terminate_next();
    }
}

pub(crate) trait LifecycleNotifier<P>: Send {
    fn notify(&mut self, parameter: P) -> Result<(), Error>;
}

impl<T, P> LifecycleNotifier<P> for T
where
    T: FnMut(P) -> Result<(), Error>,
    T: Send,
{
    fn notify(&mut self, parameter: P) -> Result<(), Error> {
        (self)(parameter)
    }
}

impl<P> dyn LifecycleNotifier<P> {
    pub fn ignore() -> Box<Self> {
        Box::new(|_| Ok(()))
    }

    // TODO: Make it `async` and take priorities into account
    pub fn once<A>(mut address: Address<A>, operation: Operation) -> Box<Self>
    where
        A: Actor + InstantActionHandler<P>,
        P: InstantAction,
    {
        let notifier = move |msg| {
            // TODO: Take the priority into account (don't put all in hp)
            address.send_hp_direct(operation.clone(), msg)
        };
        Box::new(notifier)
    }

    pub fn stop<T: LiteTask>(stopper: TaskAddress<T>) -> Box<Self> {
        let notifier = move |_| stopper.stop();
        Box::new(notifier)
    }
}

/// This message sent by a `Supervisor` to a spawned child actor.
#[derive(Debug)]
pub(crate) struct Awake<T: Actor> {
    _origin: PhantomData<T>,
}

impl<T: Actor> Awake<T> {
    pub(crate) fn new() -> Self {
        Self {
            _origin: PhantomData,
        }
    }
}

// High priority to indicate it will be called first.
impl<T: Actor> InstantAction for Awake<T> {}

/// The event to ask an `Actor` to interrupt its activity.
#[derive(Debug)]
pub(crate) struct Interrupt<T: Actor> {
    _origin: PhantomData<T>,
}

impl<T: Actor> Interrupt<T> {
    pub(crate) fn new() -> Self {
        Self {
            _origin: PhantomData,
        }
    }
}

// `Interrupt` event has high-priority,
// because all actors have to react to it as fast
// as possible even in case when all queues are full.
impl<T: Actor> InstantAction for Interrupt<T> {}

/// Notifies when `Actor`'s activity is completed.
#[derive(Debug)]
pub(crate) struct Done<T: Actor> {
    pub id: IdOf<T>,
}

impl<T: Actor> Done<T> {
    pub(crate) fn new(id: IdOf<T>) -> Self {
        Self { id }
    }
}

// This type of messages can be send in hp queue only, because if the
// normal channel will be full that this message can block the thread
// that have to notify the actor. It can be high-priority only.
impl<T: Actor> InstantAction for Done<T> {}

#[derive(Debug)]
pub(crate) struct TaskDone<T: LiteTask> {
    pub id: IdOf<T>,
    pub result: Result<T::Output, TaskError>,
}

impl<T: LiteTask> TaskDone<T> {
    pub(crate) fn new(id: IdOf<T>, result: Result<T::Output, TaskError>) -> Self {
        Self { id, result }
    }
}

// It's high priority, because it's impossible to use a channel with limited
// size for this type of messages.
impl<T: LiteTask> InstantAction for TaskDone<T> {}
