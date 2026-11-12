//! [`ClientRuntime`]: the one object a host drives.
//!
//! The host owns input, rendering and the clock of its loop; the runtime owns
//! everything that happens off the reducer's turn. The host calls:
//!
//! * [`ClientRuntime::start`] once, to claim the seat, replay its history and
//!   obtain the core and its first effects;
//! * [`ClientRuntime::submit`] with any effects it wants executed;
//! * [`ClientRuntime::after_mutation`] after every change it made to the core;
//! * [`ClientRuntime::drain`] whenever the wake callback fires, to fold what
//!   asynchronous work produced, in order, into the core.
//!
//! Closing is the host's loop: it calls [`ClientRuntime::begin_close`], keeps
//! pumping `drain` for a bounded time while [`ClientRuntime::may_close`] is
//! false, then calls [`ClientRuntime::relinquish`] (or [`ClientRuntime::detach`]
//! when the seat must not be given back).
//!
//! Ordering rules `drain` guarantees:
//!
//! * messages fold in the order they were pushed;
//! * the results of one effect batch fold in emission order, and when a fold
//!   replaces the UI session the rest of that batch is discarded, only the
//!   successor's bootstrap effects survive, and the streams turn over;
//! * a message tagged for a stream generation that is no longer current is
//!   dropped before it is seen (a late append acknowledgement of a fenced
//!   producer, a tail frame of a replaced route, a hint of a replaced session);
//! * the effects of every fold are collected, not only the last.

use std::cell::Cell;
use std::rc::Rc;

use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{
    BrowserSession, BrowserViewport, RyeOsCore, RyeOsEffect, RyeOsEffectResult, RyeOsEvent,
    SeatEvent, UiBindingAttachmentCoordinate, UiPresentationProducerObservation,
    UiSeatControlRequest,
};

use crate::admission::{
    AdmissionTarget, check_admitted_session, exact_membership, retained_seat_binding,
};
use crate::api::{MAX_SEAT_BATCH_EVENTS, chain_events_stream_path, session_events_path};
use crate::effects::{
    AdapterHandler, EffectEnv, execute_batch, failure_result, unsupported_adapter,
};
use crate::documents::DocumentOffers;
use crate::error::{RuntimeError, SeatOpenStage};
use crate::history::{HistoryCompletion, LiveEvent, LiveHistory, SeatCoordinate};
use crate::inbox::{Idle, Inbox, InboxPayload, Stream, StreamTag, TaskResult, WakeFn};
use crate::layout::LayoutPersistence;
use crate::platform::{Clock, Platform, Spawner, Store, TaskHandle};
use crate::recovery::{
    ReconnectChoice, ensure_settled, ensure_settled_without_core, observe_for_reconnect,
    observe_without_core, reconcile_pending,
};
use crate::replay::{
    CoreConfig, ReplayCommit, new_core, replay_acknowledged_events, require_producer_ready,
};
use crate::retention;
use crate::seat::{
    AppendFailure, AppendResult, ClaimIntent, Frames, PendingTracker, PlainSeatEvent, ReplayOutput,
    ReproofOutput, RetryPolicy, SeatShared, TOUCH_INTERVAL_MS, TOUCH_RETRY_AFTER_MS, TouchOutcome,
    Unavailable, claim_and_replay, may_close_seat, may_start_seat_touch, plan_append_len,
    read_session, reconcile_retained_append, relinquish_claimed_seat, relinquish_current,
    reproof_seat, send_append, session_unchanged, touch_seat,
};
use crate::streams::{TaskOf, focused_tail_payload, spawn_hints, spawn_tail};

mod fold;
mod io;
#[cfg(test)]
mod tests;

pub use crate::seat::SeatStart as StartMode;

/// Why the seat can no longer be driven. After a stop the host presents the
/// reason and, where it offers one, recovery; it does not send more
/// mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// An invariant broke or a local rule stopped delivery; nothing is
    /// recoverable in place.
    Fatal(String),
    /// The seat was fenced by the runtime itself.
    Fenced(String),
    /// Writes are no longer possible and the operator must choose.
    Unavailable(Unavailable),
}

/// The seat as the host should present it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeatStatus {
    /// A producer is current and delivery is unblocked.
    pub ready: bool,
    /// Delivery was fenced; nothing more is sent.
    pub blocked: bool,
    /// The authenticated cut must be replaced; local input is suspended until
    /// the same-producer reproof is promoted.
    pub reproof_pending: bool,
}

/// What one [`ClientRuntime::drain`] produced.
#[derive(Debug, Default)]
pub struct Drained {
    /// Effects the folds emitted, in order. Submit them with
    /// [`ClientRuntime::submit`].
    pub effects: Vec<RyeOsEffect>,
    /// Whether the core changed and the host should render.
    pub changed: bool,
    pub seat: SeatStatus,
    /// The session was replaced and the streams were turned over.
    pub turned_over: bool,
    /// The seat can no longer be driven.
    pub stop: Option<StopReason>,
}

impl Drained {
    fn stop_with(&mut self, reason: StopReason) {
        if self.stop.is_none() {
            self.stop = Some(reason);
        }
    }
}

struct ActiveSeat {
    producer: UiPresentationProducer,
    claim_request: UiSeatControlRequest,
    claim_next_engine_seq: u64,
}

struct EffectBatch<P: Platform> {
    task: TaskOf<P>,
    effects: Vec<RyeOsEffect>,
    tag: StreamTag,
}

/// What seat tasks report. Boxed through [`TaskResult`].
enum SeatTask<P: Platform> {
    Append {
        producer: UiPresentationProducer,
        events: Vec<SeatEvent>,
        upto: usize,
        result: AppendResult,
    },
    Touch {
        producer: UiPresentationProducer,
        outcome: TouchOutcome,
    },
    LiveEnded {
        producer: UiPresentationProducer,
        event: LiveEvent,
    },
    Reproof {
        generation: u64,
        producer: UiPresentationProducer,
        result: Result<ReproofOutput<P>, RuntimeError>,
    },
}

/// The start failure for a claimed seat whose replay the core did not accept.
fn left_open(coordinate: &SeatCoordinate, message: String) -> RuntimeError {
    RuntimeError::SeatLeftOpen {
        thread_id: coordinate.thread_id.clone(),
        stage: SeatOpenStage::ReplayAcceptance,
        message,
    }
}

fn session_id_of(core: &RyeOsCore) -> Option<String> {
    core.data
        .session
        .as_ref()
        .map(|session| session.session_id.clone())
}

fn hints_path(core: &RyeOsCore) -> Option<String> {
    let session = core.data.session.as_ref()?;
    session
        .events_url
        .clone()
        .or_else(|| session_events_path(&session.session_id).ok())
}

/// The client runtime for one host and one seat.
pub struct ClientRuntime<P: Platform + 'static> {
    shared: Rc<SeatShared<P>>,
    inbox: Rc<Inbox>,
    retry: RetryPolicy,
    adapter: AdapterHandler,
    documents: Option<Rc<DocumentOffers>>,
    config: CoreConfig,
    viewport: Rc<Cell<BrowserViewport>>,
    intent: ClaimIntent,
    seat: Option<ActiveSeat>,
    hints_task: Option<TaskOf<P>>,
    tail_task: Option<TaskOf<P>>,
    live_task: Option<TaskOf<P>>,
    reproof_task: Option<TaskOf<P>>,
    io_tasks: Vec<TaskOf<P>>,
    batches: Vec<EffectBatch<P>>,
    generation: u64,
    effects_tag: StreamTag,
    seat_tag: StreamTag,
    hints_tag: StreamTag,
    tail_tag: StreamTag,
    tail_chain: Option<String>,
    tracker: PendingTracker,
    append_inflight: bool,
    touch_inflight: bool,
    reproof_pending: bool,
    reproof_generation: Option<u64>,
    last_touch_ms: u64,
    closing: bool,
    pending_stop: Option<StopReason>,
    layout: LayoutPersistence,
}

impl<P: Platform + 'static> ClientRuntime<P> {
    /// A runtime over `platform`. `wake` is called, without blocking, whenever
    /// a message is waiting for [`ClientRuntime::drain`]; `retry` is how long
    /// an unresolved append is retried before it is reported unknown.
    pub fn new(platform: Rc<P>, wake: WakeFn, retry: RetryPolicy) -> Self {
        let intent = ClaimIntent::new(&*platform);
        Self {
            shared: SeatShared::new(platform),
            inbox: Rc::new(Inbox::new(wake)),
            retry,
            adapter: unsupported_adapter(),
            documents: None,
            config: CoreConfig::default(),
            viewport: Rc::new(Cell::new(BrowserViewport::default())),
            intent,
            seat: None,
            hints_task: None,
            tail_task: None,
            live_task: None,
            reproof_task: None,
            io_tasks: Vec::new(),
            batches: Vec::new(),
            generation: 0,
            effects_tag: StreamTag::default(),
            seat_tag: StreamTag::default(),
            hints_tag: StreamTag::default(),
            tail_tag: StreamTag::default(),
            tail_chain: None,
            tracker: PendingTracker::new(),
            append_inflight: false,
            touch_inflight: false,
            reproof_pending: false,
            reproof_generation: None,
            last_touch_ms: 0,
            closing: false,
            pending_stop: None,
            layout: LayoutPersistence::default(),
        }
    }

    /// Install the host's implementation of the effects the runtime cannot
    /// perform itself (hash navigation, clipboard, opening URLs and documents,
    /// navigating to a successor session).
    pub fn with_adapter(mut self, adapter: AdapterHandler) -> Self {
        self.adapter = adapter;
        self
    }

    /// Offer independent presentation documents as links (a host that can
    /// show one). The host reads [`ClientRuntime::documents`] to draw them.
    pub fn with_documents(mut self) -> Self {
        self.documents = Some(DocumentOffers::new(self.inbox.wake_fn()));
        self
    }

    /// The offered documents, when this host offers them.
    pub fn documents(&self) -> Option<&Rc<DocumentOffers>> {
        self.documents.as_ref()
    }

    /// Repeat an unconfirmed document preparation with its original request.
    pub fn retry_document(&mut self, id: u64) {
        let Some(retry) = self
            .documents
            .as_ref()
            .and_then(|documents| documents.retry(&self.shared.platform, id))
        else {
            return;
        };
        let task = self.shared.platform.spawner().spawn(retry);
        self.io_tasks.retain(|task| !task.is_finished());
        self.io_tasks.push(task);
    }

    fn platform(&self) -> &P {
        &self.shared.platform
    }

    fn now_ms(&self) -> u64 {
        self.platform().clock().now_ms()
    }

    fn stamp(&mut self) -> StreamTag {
        self.generation += 1;
        StreamTag {
            producer: self
                .seat
                .as_ref()
                .map(|seat| seat.producer.producer_incarnation.clone()),
            generation: self.generation,
        }
    }

    /// Give every stream a fresh current tag for the current producer, so
    /// anything produced under the previous one is dropped.
    fn retag_streams(&mut self) {
        self.effects_tag = self.stamp();
        self.seat_tag = self.stamp();
        self.hints_tag = self.stamp();
        self.tail_tag = self.stamp();
        self.inbox
            .set_current(Stream::Effects, self.effects_tag.clone());
        self.inbox.set_current(Stream::Seat, self.seat_tag.clone());
        self.inbox
            .set_current(Stream::Hints, self.hints_tag.clone());
        self.inbox
            .set_current(Stream::FocusedTail, self.tail_tag.clone());
    }

    /// The writer the platform currently mutates under.
    pub fn producer(&self) -> Option<UiPresentationProducer> {
        self.shared.current_producer()
    }

    /// The seat as the host should present it.
    pub fn seat_status(&self) -> SeatStatus {
        let blocked = self.shared.delivery.borrow().blocked;
        SeatStatus {
            ready: self.shared.current_producer().is_some() && !blocked,
            blocked,
            reproof_pending: self.reproof_pending,
        }
    }

    // -----------------------------------------------------------------
    // Start
    // -----------------------------------------------------------------

    /// Claim the seat and build the core.
    ///
    /// A retained start first settles any append the previous process left
    /// pending (a record whose receipt cannot be confirmed blocks the claim).
    /// Then the seat is claimed and its history proven; a retained
    /// presentation is replayed in a private core, a fresh one is built after
    /// its singleton proof. The re-read session must still name this seat, the
    /// producer must be positively ready in the reducer, and only then are the
    /// streams started. The returned effects are the core's first: submit them
    /// after the first frame.
    pub async fn start(
        &mut self,
        session: BrowserSession,
        viewport: BrowserViewport,
        mode: StartMode,
        config: CoreConfig,
    ) -> Result<(RyeOsCore, Vec<RyeOsEffect>), RuntimeError> {
        self.config = config;
        self.viewport.set(viewport);
        if let StartMode::Retained {
            presentation_id, ..
        } = &mode
        {
            self.settle_previous_life(&session, presentation_id).await?;
        }
        let shared = self.shared.clone();
        let claimed =
            claim_and_replay(&shared, &mut self.intent, &session, viewport, &mode, config).await?;
        let resumed = matches!(claimed.output, ReplayOutput::Cold { .. });
        let producer = claimed.producer.clone();
        let next_engine_seq = claimed.next_engine_seq;

        let built = self
            .admit(
                &session,
                &claimed.coordinate,
                &claimed.root,
                claimed.output,
                &claimed.completion,
                viewport,
            )
            .await;
        let (mut core, replay_effects) = match built {
            Ok(built) => built,
            Err(error) => {
                // Which claimed seats are given back: a fresh seat is
                // relinquished when admission or the singleton initialization
                // replay failed (nothing of the user's is on it). A retained
                // seat is never relinquished after a replay the core refused,
                // and neither is any seat whose replay acceptance or producer
                // readiness was refused (`SeatLeftOpen`): in those cases whether
                // the claimed owner is safe to give back is uncertain, so its
                // lease lifecycle settles it and the host is told which seat
                // stays open.
                if resumed || matches!(error, RuntimeError::SeatLeftOpen { .. }) {
                    self.shared.fence(&producer);
                } else {
                    let _ = relinquish_claimed_seat(
                        self.platform(),
                        &producer,
                        &claimed.coordinate,
                        &claimed.root,
                    )
                    .await;
                    *self.shared.producer.borrow_mut() = None;
                }
                return Err(error);
            }
        };
        let initial_acknowledged = core.acknowledged_seat_prefix_len();
        let producer_effects = core.dispatch(RyeOsEvent::PresentationProducerReady {
            producer: producer.clone(),
            next_engine_seq,
        });
        if let Err(message) = require_producer_ready(&core, &producer, next_engine_seq) {
            self.shared.fence(&producer);
            return Err(RuntimeError::SeatLeftOpen {
                thread_id: producer.thread_id.clone(),
                stage: SeatOpenStage::ProducerReadiness,
                message,
            });
        }
        let mut start_effects = if resumed {
            let mut merged = replay_effects;
            merged.extend(producer_effects);
            core.initial_effects_after_cold_replay(merged)
        } else {
            let mut merged = producer_effects;
            merged.extend(replay_effects);
            merged.extend(core.initial_effects());
            merged
        };

        self.seat = Some(ActiveSeat {
            producer: producer.clone(),
            claim_request: claimed.claim_request,
            claim_next_engine_seq: next_engine_seq,
        });
        self.tracker.reset(initial_acknowledged);
        self.append_inflight = false;
        self.touch_inflight = false;
        self.reproof_pending = false;
        self.reproof_generation = None;
        self.closing = false;
        self.last_touch_ms = self.now_ms();
        self.retag_streams();
        self.spawn_live(claimed.history, producer);
        self.restart_hints(&core);
        self.sync_tail(&core);
        // Restore the cached arrangement of the scope the core already
        // projects. A refused restore leaves the admitted structure as it is.
        if let Ok(restored) = self.restore_layout_preferences(&mut core) {
            start_effects.extend(restored);
        }
        Ok((core, start_effects))
    }

    /// Settle the retained append of an earlier life of this presentation,
    /// before any new claim is made.
    async fn settle_previous_life(
        &self,
        session: &BrowserSession,
        presentation_id: &str,
    ) -> Result<(), RuntimeError> {
        let binding = retained_seat_binding(session, presentation_id)?;
        let membership = exact_membership(session, presentation_id)?;
        let (Some(thread_id), Some(chain_root_id)) =
            (&membership.thread_id, &membership.chain_root_id)
        else {
            return Ok(());
        };
        let coordinate = SeatCoordinate {
            presentation_id: presentation_id.to_owned(),
            thread_id: thread_id.clone(),
            chain_root_id: chain_root_id.clone(),
        };
        let key = self
            .shared
            .retention_key_for(&session.session_id, presentation_id);
        // Bounded whatever the host's steady-state policy is: this runs before
        // the host's event loop, where nothing could interrupt an unbounded
        // retry. When the budget is spent the retained record stays in the
        // store and the start fails clearly, so the user can try again later.
        let reconciled = reconcile_retained_append(
            self.platform(),
            &session.session_id,
            &binding.coordinate,
            &coordinate,
            &key,
            RetryPolicy::WEB,
        )
        .await
        .map_err(|failure| {
            RuntimeError::Fatal(format!(
                "the previous run's retained seat append could not be settled before the claim; it is kept for the next start: {}",
                failure.message()
            ))
        })?;
        if let Some(reconciled) = reconciled {
            let record = reconciled.record;
            retention::clear_matching(
                self.platform().store(),
                &key,
                &record.operation_id,
                &record.producer_incarnation,
                Some(record.encoded_request.as_str()),
            )?;
        }
        Ok(())
    }

    /// Post-claim admission and core construction: the re-read session must
    /// still name exactly this seat, and a private replay must have been
    /// built from this very session.
    async fn admit(
        &self,
        session: &BrowserSession,
        coordinate: &SeatCoordinate,
        root: &UiBindingAttachmentCoordinate,
        output: ReplayOutput,
        completion: &HistoryCompletion,
        viewport: BrowserViewport,
    ) -> Result<(RyeOsCore, Vec<RyeOsEffect>), RuntimeError> {
        let (_, admitted) = read_session(self.platform()).await?;
        check_admitted_session(
            &admitted,
            &AdmissionTarget {
                session_id: &session.session_id,
                seat: coordinate,
                root,
            },
        )?;
        let fatal = RuntimeError::Fatal;
        match output {
            ReplayOutput::Cold { candidate } => {
                if !session_unchanged(&candidate, &admitted) {
                    return Err(fatal(
                        "authenticated UI session changed during retained seat replay".to_owned(),
                    ));
                }
                let committed = candidate
                    .commit(completion, None)
                    .map_err(|message| left_open(coordinate, message))?;
                match committed {
                    ReplayCommit::Cold {
                        core,
                        replay_effects,
                    } => Ok((*core, replay_effects)),
                    ReplayCommit::Promoted { .. } => {
                        Err(fatal("cold replay promoted over a live core".to_owned()))
                    }
                }
            }
            ReplayOutput::Fresh { initialization } => {
                let mut core = new_core(
                    admitted,
                    &coordinate.presentation_id,
                    viewport,
                    self.now_ms(),
                    self.config,
                )
                .map_err(fatal)?;
                let replayed = [initialization.event.clone()];
                core.validate_replayed_initialization(&replayed)
                    .map_err(fatal)?;
                let effects = core
                    .replay_authenticated_seat_events(vec![*initialization])
                    .map_err(fatal)?;
                core.require_replayed_initialization()
                    .map_err(|message| left_open(coordinate, message))?;
                Ok((core, effects))
            }
        }
    }

    // -----------------------------------------------------------------
    // Effects
    // -----------------------------------------------------------------

    /// Execute effects. An effect id is started at most once for the life of
    /// the runtime (ids are monotonic across session replacement), registered
    /// before anything runs so the core never waits on an effect nobody
    /// started. The effects of one call run concurrently and report as one
    /// message, in emission order. An effect re-checks, when it runs, that
    /// its batch is still the current one and reads the producer then.
    pub fn submit(&mut self, effects: Vec<RyeOsEffect>) {
        self.reap_batches();
        if effects.is_empty() || self.inbox.is_detached() {
            return;
        }
        let accepted: Vec<RyeOsEffect> = effects
            .into_iter()
            .filter(|effect| self.inbox.register_effect(effect.id))
            .collect();
        if accepted.is_empty() {
            return;
        }
        let env = EffectEnv {
            shared: self.shared.clone(),
            adapter: self.adapter.clone(),
            documents: self.documents.clone(),
            viewport: self.viewport.clone(),
            config: self.config,
            inbox: self.inbox.clone(),
            tag: self.effects_tag.clone(),
        };
        let inbox = self.inbox.clone();
        let tag = self.effects_tag.clone();
        let batch = accepted.clone();
        let task = self.shared.platform.spawner().spawn({
            let tag = tag.clone();
            async move {
                let results = execute_batch(&env, &batch).await;
                inbox.finish_effects(tag, results);
            }
        });
        self.batches.push(EffectBatch {
            task,
            effects: accepted,
            tag,
        });
    }

    /// A batch whose task ended must have delivered. If it did not (the task
    /// panicked or was dropped), its effects get failed results so the core
    /// is never left waiting and `may_close` can become true.
    fn reap_batches(&mut self) {
        let mut finished = Vec::new();
        self.batches.retain_mut(|batch| {
            if batch.task.is_finished() {
                finished.push((batch.tag.clone(), std::mem::take(&mut batch.effects)));
                false
            } else {
                true
            }
        });
        for (tag, effects) in finished {
            let error = RuntimeError::Fatal("effect task ended without a result".to_owned());
            let results = effects
                .iter()
                .map(|effect| failure_result(effect, &error))
                .collect();
            // Ids already delivered are ignored.
            self.inbox.finish_effects(tag, results);
        }
    }

    // -----------------------------------------------------------------
    // Closing
    // -----------------------------------------------------------------

    /// Stop everything that is not part of settling: hints, tail and reproof
    /// start no more. Appends, the live reader and in-flight effects continue
    /// so the host can pump [`ClientRuntime::drain`] until
    /// [`ClientRuntime::may_close`].
    pub fn begin_close(&mut self) {
        self.closing = true;
        self.hints_task = None;
        self.tail_task = None;
        self.tail_chain = None;
        // Hint and tail messages already queued belong to streams that no
        // longer run; fresh tags make the drain drop them.
        self.hints_tag = self.stamp();
        self.tail_tag = self.stamp();
        self.inbox
            .set_current(Stream::Hints, self.hints_tag.clone());
        self.inbox
            .set_current(Stream::FocusedTail, self.tail_tag.clone());
        self.layout.flush(self.shared.platform.store());
        self.reproof_task = None;
        self.reproof_generation = None;
    }

    /// The final local fence of a clean close: every local seat event is in
    /// the acknowledged prefix, no append or touch is in flight and no effect
    /// is pending. The host bounds how long it waits for this.
    ///
    /// It first reaps effect tasks that ended without a result, so a panicked
    /// task yields failure results (folded by the next `drain`, which the
    /// inbox's wake announces) and cannot hold the close until the deadline.
    pub fn may_close(&mut self, core: &RyeOsCore) -> bool {
        self.reap_batches();
        may_close_seat(
            core.seat.events().len(),
            core.acknowledged_seat_prefix_len(),
            self.append_inflight,
            self.touch_inflight,
            !core.pending_effects.is_empty(),
        )
    }

    /// Give the seat back: capture nothing new, observe the predecessor,
    /// freeze, close with retries. This is `relinquish`, not `close`: a
    /// relinquished seat can be resumed, a closed one is terminal. Call only
    /// after [`ClientRuntime::may_close`].
    pub async fn relinquish(&mut self) -> Result<(), RuntimeError> {
        if self.append_inflight || self.touch_inflight {
            return Err(RuntimeError::Fatal(
                "seat mutations must settle before producer relinquishment".to_owned(),
            ));
        }
        self.begin_close();
        self.live_task = None;
        let result = relinquish_current(&self.shared).await;
        if result.is_ok() {
            self.seat = None;
        }
        result
    }

    /// Stop delivering. Streams end; effects already admitted keep running
    /// and their results are counted off but not delivered. The seat is not
    /// given back (a disappearing document cannot finalize a seat; the
    /// daemon's lease expiry covers it).
    pub fn detach(&mut self) {
        self.inbox.detach();
        self.begin_close();
        self.live_task = None;
    }

    /// Resolves when nothing is queued and no effect is pending, or after
    /// [`ClientRuntime::detach`].
    pub fn idle(&self) -> Idle<'_> {
        self.inbox.idle()
    }

    // -----------------------------------------------------------------
    // Recovery
    // -----------------------------------------------------------------

    /// Settle the retained append of this seat against the daemon (see
    /// [`reconcile_pending`]). Refused while an append is in flight.
    pub async fn reconcile_pending_append(
        &mut self,
        core: &mut RyeOsCore,
    ) -> Result<Vec<RyeOsEffect>, RuntimeError> {
        if self.append_inflight {
            return Err(RuntimeError::Fatal(
                "seat append is still in flight".to_owned(),
            ));
        }
        reconcile_pending(&self.shared, core, self.retry).await
    }

    /// Reconcile, then read the writer and the choices it supports.
    pub async fn observe_reconnect(
        &mut self,
        core: &mut RyeOsCore,
    ) -> Result<
        (
            UiPresentationProducerObservation,
            Vec<ReconnectChoice>,
            Vec<RyeOsEffect>,
        ),
        RuntimeError,
    > {
        if self.append_inflight {
            return Err(RuntimeError::Fatal(
                "seat append is still in flight".to_owned(),
            ));
        }
        observe_for_reconnect(&self.shared, core, self.retry).await
    }

    /// [`ClientRuntime::observe_reconnect`] after [`ClientRuntime::start`]
    /// failed and no core was installed.
    pub async fn observe_reconnect_without_core(
        &mut self,
    ) -> Result<(UiPresentationProducerObservation, Vec<ReconnectChoice>), RuntimeError> {
        observe_without_core(&self.shared, self.retry).await
    }

    /// [`ClientRuntime::reconnect_mode`] after a failed start, with no core.
    pub fn reconnect_mode_without_core(
        &self,
        choice: &ReconnectChoice,
    ) -> Result<StartMode, RuntimeError> {
        ensure_settled_without_core(&self.shared)?;
        if matches!(choice, ReconnectChoice::Independent) {
            return Ok(StartMode::Fresh);
        }
        Ok(choice.start_mode(&self.prepared_presentation()?))
    }

    fn prepared_presentation(&self) -> Result<String, RuntimeError> {
        self.shared
            .delivery
            .borrow()
            .coordinate
            .as_ref()
            .map(|coordinate| coordinate.presentation_id.clone())
            .ok_or_else(|| {
                RuntimeError::Fatal("reconnect requires an exact prepared presentation".to_owned())
            })
    }

    /// Whether a presentation was prepared, so the writer can be inspected.
    pub fn has_prepared_presentation(&self) -> bool {
        self.shared.delivery.borrow().coordinate.is_some()
    }

    /// The start that carries out `choice` for this seat's presentation, once
    /// nothing local is unsettled. The host runs it on a new runtime; this one
    /// is then closed or detached.
    pub fn reconnect_mode(
        &self,
        choice: &ReconnectChoice,
        core: &RyeOsCore,
    ) -> Result<StartMode, RuntimeError> {
        ensure_settled(&self.shared, core)?;
        let presentation_id = self
            .shared
            .delivery
            .borrow()
            .coordinate
            .as_ref()
            .map(|coordinate| coordinate.presentation_id.clone())
            .ok_or_else(|| {
                RuntimeError::Fatal("reconnect requires an exact prepared presentation".to_owned())
            })?;
        Ok(choice.start_mode(&presentation_id))
    }
}
