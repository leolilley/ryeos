//! The ordered inbox between asynchronous work and the single reducer.
//!
//! Effects, hints and stream state arrive from futures the host spawns on the
//! owning thread; the reducer consumes them one at a time, in arrival order.
//! The inbox gives the guarantees the browser's commit queue gives today:
//!
//! * messages are delivered in the order they were pushed;
//! * an effect id is started at most once, ever: ids are monotonic across
//!   session replacement, so the dedupe set is never cleared;
//! * every message carries the [`StreamTag`] of the stream and producer that
//!   made it; when a stream's current tag changes, queued and late messages of
//!   the older tag are dropped, never folded into a successor;
//! * an effect that fails is turned into a failed result, so the reducer
//!   always hears back, and each effect is delivered at most once;
//! * [`Inbox::pending_effects`] counts started effects whose delivery has not
//!   happened yet, and keeps counting down after [`Inbox::detach`];
//! * [`Inbox::idle`] resolves when nothing is queued and nothing is in
//!   flight, or the inbox is detached.
//!
//! Everything runs on the owning thread: the inbox is shared as an `Rc` and
//! wakes the host through a callback. It makes no `Send` claim.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{
    RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind, RyeOsTransportChannel,
    RyeOsTransportFreshness,
};
use serde_json::Value;

use crate::error::RuntimeError;

/// Effect identity assigned by the core.
pub type EffectId = u64;

/// Identity of the producer and stream generation a message was made under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct StreamTag {
    /// Producer incarnation, if the stream is bound to one.
    pub producer: Option<String>,
    /// Bumped each time the stream is reconnected or replaced.
    pub generation: u64,
}

/// The independently ordered streams feeding the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Effects,
    Hints,
    FocusedTail,
    Seat,
    /// Messages with no producer (wake-ups).
    Control,
}

/// Something that happened off the reducer's turn.
#[derive(Debug, Clone, PartialEq)]
pub enum InboxPayload {
    /// Results of one batch of effects, in emission order.
    EffectResults(Vec<RyeOsEffectResult>),
    /// A session hint: `kind` plus its payload.
    Hint(Value),
    /// The hint kinds seen in one coalescing window, first seen first.
    HintFlush(Vec<String>),
    /// A decoded frame of the chain tail opened for `chain_root_id`, before
    /// the focus predicate.
    ThreadTail { chain_root_id: String, frame: Value },
    /// A typed UI intent or untyped message from the session stream.
    DaemonEvent(Value),
    /// The session stream came back after a gap; every hint of the gap is lost.
    Resubscribed,
    /// The result of a seat task (append, touch, live history, reproof).
    Task(TaskResult),
    TransportState {
        channel: RyeOsTransportChannel,
        freshness: RyeOsTransportFreshness,
        observed_at_ms: u64,
    },
    /// An append acknowledgement the seat state machine verified.
    SeatAck(Value),
    /// The seat producer became (or stopped being) ready.
    ProducerReady(Option<UiPresentationProducer>),
    /// Run the reducer without new data (render request, timer).
    Wake,
}

impl InboxPayload {
    /// The stream whose current tag judges this payload.
    pub fn stream(&self) -> Stream {
        match self {
            Self::EffectResults(_) => Stream::Effects,
            Self::Hint(_) | Self::HintFlush(_) | Self::DaemonEvent(_) | Self::Resubscribed => {
                Stream::Hints
            }
            Self::ThreadTail { .. } => Stream::FocusedTail,
            Self::TransportState { channel, .. } => match channel {
                RyeOsTransportChannel::Hints => Stream::Hints,
                RyeOsTransportChannel::FocusedTail => Stream::FocusedTail,
                RyeOsTransportChannel::Session => Stream::Seat,
            },
            Self::SeatAck(_) | Self::ProducerReady(_) | Self::Task(_) => Stream::Seat,
            Self::Wake => Stream::Control,
        }
    }
}

/// A type-erased value handed from a task to the reducer exactly once. It is
/// cloneable and comparable by identity so the message stays an ordinary
/// value; only the first `take` of the right type yields the contents.
#[derive(Clone)]
pub struct TaskResult(Rc<RefCell<Option<Box<dyn std::any::Any>>>>);

impl TaskResult {
    /// Wrap `value` for delivery.
    pub fn new<T: std::any::Any>(value: T) -> Self {
        Self(Rc::new(RefCell::new(Some(Box::new(value)))))
    }

    /// Take the contents if they are a `T`; otherwise leave them in place.
    pub fn take<T: std::any::Any>(&self) -> Option<T> {
        let mut slot = self.0.borrow_mut();
        let boxed = slot.take()?;
        match boxed.downcast::<T>() {
            Ok(value) => Some(*value),
            Err(other) => {
                *slot = Some(other);
                None
            }
        }
    }
}

impl std::fmt::Debug for TaskResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TaskResult")
    }
}

impl PartialEq for TaskResult {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// A payload stamped with the tag it was produced under.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxMessage {
    pub tag: StreamTag,
    pub payload: InboxPayload,
}

/// Host callback invoked after the inbox changed, outside any borrow. It must
/// not block and must tolerate spurious calls.
pub type WakeFn = Rc<dyn Fn()>;

struct State {
    detached: bool,
    current: HashMap<Stream, StreamTag>,
    queue: Vec<InboxMessage>,
    /// Every effect id ever registered. Never cleared.
    started: HashSet<EffectId>,
    /// Registered effects not yet delivered.
    in_flight: HashSet<EffectId>,
    idle_wakers: Vec<Waker>,
}

impl State {
    fn is_current(&self, stream: Stream, tag: &StreamTag) -> bool {
        match self.current.get(&stream) {
            Some(current) => current == tag,
            None => *tag == StreamTag::default(),
        }
    }

    fn is_idle(&self) -> bool {
        self.detached || (self.queue.is_empty() && self.in_flight.is_empty())
    }
}

pub struct Inbox {
    state: RefCell<State>,
    wake: WakeFn,
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.borrow();
        formatter
            .debug_struct("Inbox")
            .field("detached", &state.detached)
            .field("queued", &state.queue.len())
            .field("in_flight", &state.in_flight.len())
            .finish()
    }
}

impl Inbox {
    /// The host's wake callback, for state the host reads outside the inbox.
    pub(crate) fn wake_fn(&self) -> WakeFn {
        self.wake.clone()
    }

    pub fn new(wake: WakeFn) -> Self {
        Self {
            state: RefCell::new(State {
                detached: false,
                current: HashMap::new(),
                queue: Vec::new(),
                started: HashSet::new(),
                in_flight: HashSet::new(),
                idle_wakers: Vec::new(),
            }),
            wake,
        }
    }

    /// The tag `stream` currently accepts.
    pub fn current_tag(&self, stream: Stream) -> StreamTag {
        self.state
            .borrow()
            .current
            .get(&stream)
            .cloned()
            .unwrap_or_default()
    }

    /// Make `tag` the only tag `stream` accepts. Queued messages of the stream
    /// under any other tag are discarded, and later arrivals stamped with one
    /// are refused.
    pub fn set_current(&self, stream: Stream, tag: StreamTag) {
        let wakers = {
            let mut state = self.state.borrow_mut();
            state
                .queue
                .retain(|message| message.payload.stream() != stream || message.tag == tag);
            state.current.insert(stream, tag);
            if state.is_idle() {
                std::mem::take(&mut state.idle_wakers)
            } else {
                Vec::new()
            }
        };
        wakers.into_iter().for_each(Waker::wake);
    }

    /// Stop delivering. Queued messages are dropped and every later push is
    /// refused; admitted execution is not cancelled, and its eventual results
    /// are counted off [`Inbox::pending_effects`] without being queued.
    /// [`Inbox::idle`] resolves immediately.
    pub fn detach(&self) {
        let wakers = {
            let mut state = self.state.borrow_mut();
            state.detached = true;
            state.queue.clear();
            std::mem::take(&mut state.idle_wakers)
        };
        wakers.into_iter().for_each(Waker::wake);
    }

    pub fn is_detached(&self) -> bool {
        self.state.borrow().detached
    }

    /// Queue a payload. Returns false (and drops it) if its tag is not current
    /// for its stream or the inbox is detached.
    pub fn push(&self, tag: StreamTag, payload: InboxPayload) -> bool {
        {
            let mut state = self.state.borrow_mut();
            if state.detached || !state.is_current(payload.stream(), &tag) {
                return false;
            }
            state.queue.push(InboxMessage { tag, payload });
        }
        (self.wake)();
        true
    }

    /// Register an effect before starting it. Returns true exactly once per id
    /// for the life of the inbox; the caller starts the effect only on true.
    /// The effect counts as pending until [`Inbox::finish_effects`].
    pub fn register_effect(&self, id: EffectId) -> bool {
        let mut state = self.state.borrow_mut();
        if !state.started.insert(id) {
            return false;
        }
        state.in_flight.insert(id);
        true
    }

    /// Started effects whose results have not been delivered. Decrements on
    /// delivery even when the inbox is detached or the tag is stale.
    pub fn pending_effects(&self) -> usize {
        self.state.borrow().in_flight.len()
    }

    /// Deliver the results of started effects. Each effect id is accepted
    /// once; a repeat is ignored. The accepted results leave flight and, if the
    /// inbox is attached and `tag` is current, are queued as one message in
    /// their given order. Returns whether a message was queued.
    pub fn finish_effects(&self, tag: StreamTag, results: Vec<RyeOsEffectResult>) -> bool {
        let queued = {
            let mut state = self.state.borrow_mut();
            let accepted: Vec<RyeOsEffectResult> = results
                .into_iter()
                .filter(|result| state.in_flight.remove(&result.id))
                .collect();
            if accepted.is_empty() || state.detached || !state.is_current(Stream::Effects, &tag) {
                false
            } else {
                state.queue.push(InboxMessage {
                    tag,
                    payload: InboxPayload::EffectResults(accepted),
                });
                true
            }
        };
        if queued {
            (self.wake)();
        }
        queued
    }

    /// True when `tag` is the tag `stream` currently accepts.
    pub fn accepts(&self, stream: Stream, tag: &StreamTag) -> bool {
        self.state.borrow().is_current(stream, tag)
    }

    /// Take every queued message whose tag is still current, oldest first.
    pub fn drain(&self) -> Vec<InboxMessage> {
        let (messages, wakers) = {
            let mut state = self.state.borrow_mut();
            let queue = std::mem::take(&mut state.queue);
            let messages: Vec<InboxMessage> = queue
                .into_iter()
                .filter(|message| state.is_current(message.payload.stream(), &message.tag))
                .collect();
            let wakers = if state.is_idle() {
                std::mem::take(&mut state.idle_wakers)
            } else {
                Vec::new()
            };
            (messages, wakers)
        };
        wakers.into_iter().for_each(Waker::wake);
        messages
    }

    /// Resolves when nothing is queued and no effect is pending, or the inbox
    /// is detached. A message pushed after it resolves is not awaited.
    pub fn idle(&self) -> Idle<'_> {
        Idle { inbox: self }
    }
}

/// Future returned by [`Inbox::idle`].
#[derive(Debug)]
pub struct Idle<'a> {
    inbox: &'a Inbox,
}

impl Future for Idle<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let mut state = self.inbox.state.borrow_mut();
        if state.is_idle() {
            return Poll::Ready(());
        }
        if !state
            .idle_wakers
            .iter()
            .any(|waker| waker.will_wake(context.waker()))
        {
            state.idle_wakers.push(context.waker().clone());
        }
        Poll::Pending
    }
}

/// The result kind an effect's outcome is reported under, success or not.
pub fn result_kind_for(effect: &RyeOsEffect) -> RyeOsEffectResultKind {
    match &effect.kind {
        RyeOsEffectKind::ObserveTransferDestination { .. } => {
            RyeOsEffectResultKind::TransferDestinationObserved
        }
        RyeOsEffectKind::FetchSource { .. } => RyeOsEffectResultKind::SourceData,
        RyeOsEffectKind::InvokeBinding { .. }
        | RyeOsEffectKind::InvokePresentationOperation { .. }
        | RyeOsEffectKind::PrepareTransfer { .. }
        | RyeOsEffectKind::ContactTransfer { .. } => RyeOsEffectResultKind::BindingInvoked,
        RyeOsEffectKind::OpenNewPresentationDocument { .. }
        | RyeOsEffectKind::ShowPreparedPresentationDocument { .. }
        | RyeOsEffectKind::SetLocationHash { .. }
        | RyeOsEffectKind::CopyToClipboard { .. }
        | RyeOsEffectKind::OpenUrl { .. }
        | RyeOsEffectKind::ReplaceSession { .. }
        | RyeOsEffectKind::ReleaseBindingAttachment { .. } => RyeOsEffectResultKind::BrowserOnly,
    }
}

/// The result reported for an effect whose future failed: not ok, no data,
/// code `platform_effect_failed`, never retryable, outcome `unknown` only for
/// an unknown delivery and `refused` otherwise.
pub fn failed_effect_result(effect: &RyeOsEffect, error: &RuntimeError) -> RyeOsEffectResult {
    RyeOsEffectResult {
        id: effect.id,
        ok: false,
        kind: result_kind_for(effect),
        data: None,
        error: Some(error.to_ui_error()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use ryeos_client_base::ui::RyeOsEffectOutcome;

    use super::*;

    fn inbox() -> (Inbox, Rc<Cell<usize>>) {
        let wakes = Rc::new(Cell::new(0));
        let counter = wakes.clone();
        let inbox = Inbox::new(Rc::new(move || counter.set(counter.get() + 1)));
        (inbox, wakes)
    }

    fn tag(producer: &str, generation: u64) -> StreamTag {
        StreamTag {
            producer: Some(producer.to_owned()),
            generation,
        }
    }

    fn hint(kind: &str) -> InboxPayload {
        InboxPayload::Hint(serde_json::json!({ "kind": kind }))
    }

    fn effect(id: u64, kind: RyeOsEffectKind) -> RyeOsEffect {
        RyeOsEffect { id, kind }
    }

    fn failure(id: u64) -> RyeOsEffectResult {
        failed_effect_result(
            &effect(id, RyeOsEffectKind::OpenUrl { url: "u".into() }),
            &RuntimeError::Network {
                path: "/p".into(),
                message: "down".into(),
            },
        )
    }

    fn poll_idle(inbox: &Inbox) -> Poll<()> {
        let mut idle = inbox.idle();
        Pin::new(&mut idle).poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn messages_drain_in_push_order_and_wake_the_host() {
        let (inbox, wakes) = inbox();
        for kind in ["a", "b", "c"] {
            assert!(inbox.push(StreamTag::default(), hint(kind)));
        }
        assert_eq!(wakes.get(), 3);
        let payloads: Vec<_> = inbox.drain().into_iter().map(|m| m.payload).collect();
        assert_eq!(payloads, [hint("a"), hint("b"), hint("c")]);
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn changing_a_streams_tag_drops_only_that_streams_stale_messages() {
        let (inbox, _) = inbox();
        let old = tag("p1", 1);
        inbox.set_current(Stream::Hints, old.clone());
        assert!(inbox.push(old.clone(), hint("old")));
        assert!(inbox.push(StreamTag::default(), InboxPayload::Wake));
        let new = tag("p1", 2);
        inbox.set_current(Stream::Hints, new.clone());
        assert!(!inbox.push(old, hint("late")));
        assert!(inbox.push(new.clone(), hint("fresh")));
        let drained = inbox.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].payload, InboxPayload::Wake);
        assert_eq!(drained[1].tag, new);
        assert_eq!(drained[1].payload, hint("fresh"));
    }

    #[test]
    fn a_message_made_stale_after_queueing_is_dropped_on_drain() {
        let (inbox, _) = inbox();
        let first = tag("p1", 1);
        inbox.set_current(Stream::Seat, first.clone());
        assert!(inbox.push(first, InboxPayload::SeatAck(Value::Null)));
        inbox.set_current(Stream::Seat, tag("p2", 1));
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn effect_ids_start_once_for_the_life_of_the_inbox() {
        let (inbox, _) = inbox();
        assert!(inbox.register_effect(7));
        assert!(!inbox.register_effect(7));
        inbox.set_current(Stream::Effects, tag("p2", 1));
        assert!(!inbox.register_effect(7));
        assert!(inbox.register_effect(8));
    }

    #[test]
    fn delivery_is_once_only_and_counts_down() {
        let (inbox, _) = inbox();
        assert!(inbox.register_effect(1));
        assert!(inbox.register_effect(2));
        assert_eq!(inbox.pending_effects(), 2);
        assert!(inbox.finish_effects(StreamTag::default(), vec![failure(1)]));
        assert_eq!(inbox.pending_effects(), 1);
        assert!(!inbox.finish_effects(StreamTag::default(), vec![failure(1)]));
        assert_eq!(inbox.pending_effects(), 1);
    }

    #[test]
    fn pending_effects_count_down_after_detach_without_queueing() {
        let (inbox, _) = inbox();
        assert!(inbox.register_effect(1));
        inbox.detach();
        assert!(inbox.is_detached());
        assert_eq!(poll_idle(&inbox), Poll::Ready(()));
        assert_eq!(inbox.pending_effects(), 1);
        assert!(!inbox.finish_effects(StreamTag::default(), vec![failure(1)]));
        assert_eq!(inbox.pending_effects(), 0);
        assert!(!inbox.push(StreamTag::default(), hint("x")));
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn idle_waits_for_in_flight_effects_and_the_drain() {
        let (inbox, _) = inbox();
        assert_eq!(poll_idle(&inbox), Poll::Ready(()));
        assert!(inbox.register_effect(1));
        assert_eq!(poll_idle(&inbox), Poll::Pending);
        assert!(inbox.finish_effects(StreamTag::default(), vec![failure(1)]));
        assert_eq!(poll_idle(&inbox), Poll::Pending);
        assert_eq!(inbox.drain().len(), 1);
        assert_eq!(poll_idle(&inbox), Poll::Ready(()));
    }

    #[test]
    fn failure_synthesis_reports_kind_code_and_outcome() {
        let hash = effect(3, RyeOsEffectKind::SetLocationHash { hash: "#x".into() });
        let refused = failed_effect_result(&hash, &RuntimeError::BindingRequest("no".into()));
        assert_eq!(refused.id, 3);
        assert!(!refused.ok);
        assert_eq!(refused.kind, RyeOsEffectResultKind::BrowserOnly);
        assert!(refused.data.is_none());
        let error = refused.error.unwrap();
        assert_eq!(error.code, "platform_effect_failed");
        assert_eq!(error.message, "UI binding request: no");
        assert!(!error.retryable);
        assert_eq!(error.outcome, RyeOsEffectOutcome::Refused);

        let unknown = failed_effect_result(
            &hash,
            &RuntimeError::UnreadableResponse { path: "/p".into() },
        );
        let error = unknown.error.unwrap();
        assert_eq!(error.message, "/p: response outcome is unknown");
        assert_eq!(error.outcome, RyeOsEffectOutcome::Unknown);
    }
}
