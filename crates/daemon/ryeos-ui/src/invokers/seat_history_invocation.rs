//! Browser-session-authenticated, fixed-anchor provisional seat history.
//! This source is deliberately distinct from the ephemeral session hint bus
//! and the projection-backed generic thread event stream.

use std::sync::Arc;
use std::time::Duration;

use ryeos_api::route_error::{RouteConfigError, RouteDispatchError};
use ryeos_api::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteEventStream, RouteInvocationContext,
    RouteInvocationContract, RouteInvocationOutput, RouteInvocationResult,
};
use ryeos_api::routes::response_modes::event_stream_mode::{
    EventStreamStrategy, StreamSourceCompiler,
};
use ryeos_app::route_raw::{RawRequestBody, RawRouteSpec};
use ryeos_app::state::AppState;
use ryeos_app::state_store::PersistedEventRecord;
use ryeos_app::stream_envelope::RouteStreamEnvelope;
use serde::Deserialize;
use serde_json::json;

use crate::handlers::ui_seat::paired_read::{FixedTransferCut, read_live_root_cut};
use crate::handlers::ui_seat::{
    FixedAnchorReplay, FixedAnchorReplayStep, LiveSeatReplayAdvance, ReplayFrameBounds,
    authorize_history_stream_batch, replay_frame_bounds, try_send_authorized_history_frame,
    try_send_authorized_transfer_history_frame, try_send_reconciled_transfer_complete,
};
use crate::state::get_ui_state;
use crate::transfer_store::discovery::discover_seat_transfer_identities;

const SOURCE: &str = "ui_seat_history";
const PATH: &str = "/ui/api/session/seat/history";
const PROTOCOL: u32 = 1;

/// Split one verified interval into frames no larger than the signed replay
/// bounds. The web and terminal cold-replay decoders each admit a bounded
/// number of records per provisional frame, and a verified append interval can
/// hold more events than that, so locator-page size alone cannot bound a
/// frame.
fn replay_event_chunks<T: serde::Serialize>(
    events: &[T],
    bounds: ReplayFrameBounds,
) -> Result<Vec<&[T]>, String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0usize;
    for (index, event) in events.iter().enumerate() {
        let event_bytes = serde_json::to_vec(event)
            .map_err(|error| format!("serialize verified replay record: {error}"))?
            .len();
        // A comma is the maximum per-record array overhead.
        let next_bytes = bytes
            .checked_add(event_bytes)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| "replay frame byte count overflowed".to_string())?;
        if index > start && (index - start == bounds.events || next_bytes > bounds.bytes) {
            chunks.push(&events[start..index]);
            start = index;
            bytes = 0;
        }
        bytes = bytes
            .checked_add(event_bytes)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| "replay frame byte count overflowed".to_string())?;
        if bytes > bounds.bytes {
            return Err("one verified replay record exceeds the frame byte bound".to_string());
        }
    }
    if start < events.len() {
        chunks.push(&events[start..]);
    }
    Ok(chunks)
}

/// Disconnect cancels future page scheduling. A blocking page already in
/// flight may finish, but cannot turn its partial result into stream authority.
async fn await_while_connected<T>(
    sender: &tokio::sync::mpsc::Sender<RouteStreamEnvelope>,
    read: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = sender.closed() => None,
        result = read => Some(result),
    }
}

pub struct SeatHistorySourceFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryRequest {
    presentation_id: String,
    chain_root_id: String,
}

struct CompiledSeatHistoryInvocation;

static CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Stream,
    principal: PrincipalPolicy::Required,
};

impl StreamSourceCompiler for SeatHistorySourceFactory {
    fn compile(&self, raw: &RawRouteSpec) -> Result<EventStreamStrategy, RouteConfigError> {
        if raw.auth != "browser_session" {
            return Err(RouteConfigError::SourceAuthRequirement {
                id: raw.id.clone(),
                src: SOURCE.into(),
                required: "browser_session".into(),
                got: raw.auth.clone(),
            });
        }
        if raw.path != PATH
            || raw.methods.len() != 1
            || !raw.methods.contains("POST")
            || raw.request.body != RawRequestBody::Json
            || raw.limits.timeout_ms != 0
            || !raw.response.source_config.is_null()
        {
            return Err(RouteConfigError::InvalidSourceConfig {
                id: raw.id.clone(),
                src: SOURCE.into(),
                reason: "seat history requires its exact POST JSON browser-session route, zero dispatcher timeout and no source_config".into(),
            });
        }
        Ok(EventStreamStrategy::BodyJson {
            invoker: Arc::new(CompiledSeatHistoryInvocation),
        })
    }
}

/// What the stream owner holds for one connection.
struct HistoryStream {
    state: Arc<AppState>,
    session_id: String,
    presentation_id: String,
    detail: Arc<ryeos_app::state_store::ThreadDetail>,
    sender: tokio::sync::mpsc::Sender<RouteStreamEnvelope>,
    bounds: ReplayFrameBounds,
}

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledSeatHistoryInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let request: HistoryRequest = serde_json::from_value(ctx.input).map_err(|error| {
            RouteDispatchError::BadRequest(format!("invalid seat history request: {error}"))
        })?;
        let principal = ctx.principal.ok_or(RouteDispatchError::Unauthorized)?;
        let session_id = principal
            .id
            .strip_prefix("session:")
            .filter(|id| !id.is_empty())
            .ok_or(RouteDispatchError::Unauthorized)?
            .to_owned();
        let state = Arc::new(ctx.state);
        let detail = Arc::new(
            authorize_history_stream_batch(
                &state,
                &session_id,
                &request.presentation_id,
                &request.chain_root_id,
                None,
            )
            .map_err(|_| RouteDispatchError::Forbidden("seat history is not authorized".into()))?,
        );
        let bounds = replay_frame_bounds(&state)
            .map_err(|error| RouteDispatchError::Internal(error.to_string()))?;
        // Subscribe before capturing the anchor. Persistence-first seat
        // publication then cannot disappear between the signed cold proof and
        // the live invalidation lane; lag is a resnapshot condition, never
        // permission to guess a missing interval.
        let live = state.event_streams.subscribe(&detail.thread_id);
        // Capture the signed endpoint before response headers. A failed proof
        // after headers can only end the stream without `complete`.
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)
            .map_err(|error| RouteDispatchError::Internal(error.to_string()))?
            .ok_or_else(|| {
                RouteDispatchError::Internal("seat history has no signed anchor".into())
            })?;
        let (sender, receiver) = tokio::sync::mpsc::channel::<RouteStreamEnvelope>(2);
        let stream = HistoryStream {
            state,
            session_id,
            presentation_id: request.presentation_id,
            detail,
            sender,
            bounds,
        };
        tokio::spawn(async move {
            stream.run(anchor, live).await;
        });
        let events = Box::pin(async_stream::stream! {
            let mut receiver = receiver;
            while let Some(frame) = receiver.recv().await {
                yield Ok::<_, std::convert::Infallible>(frame);
            }
        });
        Ok(RouteInvocationResult::Stream(RouteEventStream {
            events,
            keep_alive_secs: 15,
        }))
    }
}

impl HistoryStream {
    async fn send(
        &self,
        frame: &RouteStreamEnvelope,
        expected_tip: Option<(&str, u64)>,
        paired_cut: Option<&FixedTransferCut>,
    ) -> bool {
        loop {
            if self.sender.is_closed() {
                return false;
            }
            let result = match paired_cut {
                Some(cut) => try_send_authorized_transfer_history_frame(
                    &self.state,
                    &self.session_id,
                    cut,
                    &self.sender,
                    frame,
                ),
                None => try_send_authorized_history_frame(
                    &self.state,
                    &self.session_id,
                    &self.presentation_id,
                    &self.detail,
                    &self.sender,
                    frame,
                    expected_tip,
                ),
            };
            match result {
                Ok(true) => return true,
                Ok(false) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(error) => {
                    tracing::warn!(%error, "fixed history disclosure stopped without completion");
                    return false;
                }
            }
        }
    }

    async fn send_reconciled_complete(
        &self,
        cut: &FixedTransferCut,
        replay: &ryeos_client_base::ui::composition_replay::UiCompositionReplay,
        frame: &RouteStreamEnvelope,
        tip: (&str, u64),
    ) -> bool {
        loop {
            if self.sender.is_closed() {
                return false;
            }
            match try_send_reconciled_transfer_complete(
                &self.state,
                &self.session_id,
                &self.presentation_id,
                &self.detail.chain_root_id,
                &self.detail.thread_id,
                tip,
                cut,
                replay,
                &self.sender,
                frame,
            ) {
                Ok(true) => return true,
                Ok(false) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(error) => {
                    tracing::warn!(%error, "paired cleanup/completion stopped without publication");
                    return false;
                }
            }
        }
    }

    /// Cold proof, then live suffix.
    async fn run(
        self,
        anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
        live: tokio::sync::broadcast::Receiver<PersistedEventRecord>,
    ) {
        let begin = RouteStreamEnvelope::new(
            "ui_seat.replay.begin.v1",
            json!({
                "schema": PROTOCOL,
                "presentation_id": self.presentation_id,
                "chain_root_id": self.detail.chain_root_id,
                "thread_id": self.detail.thread_id,
            }),
        );
        if !self.send(&begin, None, None).await {
            return;
        }
        let Some((membership, paired_cut)) = self.read_paired_cut(&anchor).await else {
            return;
        };
        let mut proof = match FixedAnchorReplay::begin_at_anchor(
            &self.state,
            &self.presentation_id,
            anchor,
            paired_cut.clone(),
        ) {
            Ok(proof) => proof,
            Err(error) => {
                tracing::warn!(%error, "seat history proof cannot begin at its anchor");
                return;
            }
        };
        if let Some(cut) = &paired_cut
            && !self.send_proof_frames(cut).await
        {
            return;
        }
        let mut emitted = 0_u64;
        let (tip_hash, thread_seq) = loop {
            if self.sender.is_closed() {
                return;
            }
            let read_state = self.state.clone();
            let result = tokio::task::spawn_blocking(move || {
                let step = proof.next_step(&read_state);
                (proof, step)
            })
            .await;
            let (returned, step) = match result {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(%error, "fixed replay page worker failed");
                    return;
                }
            };
            proof = returned;
            match step {
                Ok(FixedAnchorReplayStep::Batch { events }) => {
                    if events.is_empty() {
                        tokio::task::yield_now().await;
                        continue;
                    }
                    // The entire interval is verified before this step, but
                    // every provisional chunk still needs its own
                    // browser-session reauthorization and backpressure.
                    let chunks = match replay_event_chunks(&events, self.bounds) {
                        Ok(chunks) => chunks,
                        Err(error) => {
                            tracing::warn!(%error, "verified seat replay cannot fit a bounded frame");
                            return;
                        }
                    };
                    for chunk in chunks {
                        let frame = RouteStreamEnvelope::new(
                            "ui_seat.replay.events.v1",
                            json!({"schema": PROTOCOL, "events": chunk}),
                        );
                        if !self.send(&frame, None, paired_cut.as_deref()).await {
                            return;
                        }
                        emitted = match emitted.checked_add(chunk.len() as u64) {
                            Some(count) => count,
                            None => return,
                        };
                    }
                }
                Ok(FixedAnchorReplayStep::Complete {
                    tip_hash,
                    thread_seq,
                }) => break (tip_hash, thread_seq),
                Ok(FixedAnchorReplayStep::ReproofRequired) => {
                    tracing::warn!("cold seat history asked for a reproof");
                    return;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "authenticated seat history stream stopped without completion");
                    return;
                }
            }
        };
        // The subscribed hub is a wake, not signed history. A queued wake
        // alone must not abort an otherwise current proof. The exact signed
        // tip is checked under the transition guard when this completion is
        // sent; if it advanced, the client discards this provisional cut and
        // starts a fresh proof of the same claimed seat.
        let complete = RouteStreamEnvelope::new(
            "ui_seat.replay.complete.v1",
            json!({
                "schema": PROTOCOL,
                "tip_hash": tip_hash,
                "thread_seq": thread_seq,
                "event_count": emitted,
            }),
        );
        let sent = match &paired_cut {
            Some(cut) => {
                let replay = match proof.completed_paired_projection(&membership.initialization) {
                    Ok(replay) => replay,
                    Err(error) => {
                        tracing::warn!(%error, "paired completion has no full root projection");
                        return;
                    }
                };
                self.send_reconciled_complete(
                    cut,
                    replay,
                    &complete,
                    (tip_hash.as_str(), thread_seq),
                )
                .await
            }
            None => {
                self.send(&complete, Some((tip_hash.as_str(), thread_seq)), None)
                    .await
            }
        };
        if !sent {
            return;
        }
        self.follow(proof, paired_cut, live).await;
    }

    /// The completed paired cut of the seat, if its history names transfers,
    /// with the membership it was read for.
    async fn read_paired_cut(
        &self,
        anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    ) -> Option<(
        crate::browser_session::PresentationMembership,
        Option<Arc<FixedTransferCut>>,
    )> {
        let ui = get_ui_state(&self.state)?;
        let session = ui.browser_sessions.get_session(&self.session_id)?;
        let membership = session
            .presentations
            .get(&self.presentation_id)
            .filter(|membership| {
                membership.thread_id.as_deref() == Some(self.detail.thread_id.as_str())
                    && membership.chain_root_id.as_deref()
                        == Some(self.detail.chain_root_id.as_str())
            })?
            .clone();
        let discovery_state = self.state.clone();
        let discovery_anchor = anchor.clone();
        let named = tokio::task::spawn_blocking(move || {
            discover_seat_transfer_identities(
                &discovery_state.state_store,
                discovery_anchor.chain_root_id(),
                &discovery_anchor,
            )
        })
        .await;
        let named = match named {
            Ok(Ok(named)) => named,
            Ok(Err(error)) => {
                tracing::warn!(%error, "seat history transfer discovery failed");
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, "seat history transfer discovery worker failed");
                return None;
            }
        };
        if named.is_empty() {
            return Some((membership, None));
        }
        let cut = await_while_connected(
            &self.sender,
            read_live_root_cut(self.state.clone(), &session, &membership, anchor.clone()),
        )
        .await?;
        match cut {
            Ok(cut) => Some((membership, Some(cut))),
            Err(error) => {
                tracing::warn!(%error, "seat history paired cut failed");
                None
            }
        }
    }

    /// Proofs precede every canonical row. They are bounded per operation and
    /// never advance the event count.
    async fn send_proof_frames(&self, cut: &Arc<FixedTransferCut>) -> bool {
        let max_bytes = self.bounds.bytes as u64;
        for operation_id in
            cut.seat_operation_ids(&self.presentation_id, &self.detail.chain_root_id)
        {
            let read_cut = cut.clone();
            let read_state = self.state.clone();
            let frame = tokio::task::spawn_blocking(move || {
                read_cut.proof_frame(&read_state.state_store, &operation_id, max_bytes)
            })
            .await;
            let Ok(Ok(frame)) = frame else {
                return false;
            };
            let frame = RouteStreamEnvelope::new(
                "ui_seat.replay.transfer_proof.v1",
                json!({"schema": PROTOCOL, "proof": frame}),
            );
            if !self.send(&frame, None, Some(cut.as_ref())).await {
                return false;
            }
        }
        for operation_id in
            cut.seat_cancellation_ids(&self.presentation_id, &self.detail.chain_root_id)
        {
            let read_cut = cut.clone();
            let read_state = self.state.clone();
            let frame = tokio::task::spawn_blocking(move || {
                read_cut.cancellation_frame(&read_state.state_store, &operation_id, max_bytes)
            })
            .await;
            let Ok(Ok(frame)) = frame else {
                return false;
            };
            let frame = RouteStreamEnvelope::new(
                "ui_seat.replay.move_freeze_cancellation_proof.v1",
                json!({"schema": PROTOCOL, "proof": frame}),
            );
            if !self.send(&frame, None, Some(cut.as_ref())).await {
                return false;
            }
        }
        true
    }

    /// After completion, each wake re-reads the signed tip and releases only
    /// the receipts that followed. Anything else ends the stream with a
    /// reproof or invalidation frame.
    async fn follow(
        self,
        proof: FixedAnchorReplay,
        paired_cut: Option<Arc<FixedTransferCut>>,
        mut live: tokio::sync::broadcast::Receiver<PersistedEventRecord>,
    ) {
        let mut proof = proof;
        loop {
            if self.sender.is_closed() {
                return;
            }
            // A disconnect ends the follow at once rather than at the next
            // wake, so the stream releases the state it holds.
            tokio::select! {
                biased;
                _ = self.sender.closed() => return,
                woke = tokio::time::timeout(Duration::from_secs(15), live.recv()) => {
                    if let Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) = woke {
                        return;
                    }
                }
            }
            // Event, lag and timer expiry are all signed-read wakes.
            if !self.still_authorized() {
                return;
            }
            let read_state = self.state.clone();
            let detail = self.detail.clone();
            let session_id = self.session_id.clone();
            let presentation_id = self.presentation_id.clone();
            let sender = self.sender.clone();
            let cut = paired_cut.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                match dependencies_moved(&read_state, &session_id, &detail, cut.as_deref()) {
                    Ok(true) => return (proof, Ok(LiveSeatReplayAdvance::ReproofRequired)),
                    Err(error) => return (proof, Err(error)),
                    Ok(false) => {}
                }
                let result = proof.advance_receipts(
                    &read_state,
                    || {
                        !sender.is_closed()
                            && authorize_history_stream_batch(
                                &read_state,
                                &session_id,
                                &presentation_id,
                                &detail.chain_root_id,
                                Some(&detail.thread_id),
                            )
                            .as_ref()
                            .is_ok_and(|current| same_association(current, &detail))
                    },
                    |record| {
                        let receipt: ryeos_ui_presentation_contract::seat_control::SeatAppendReceipt =
                            match serde_json::from_value(record.payload) {
                                Ok(receipt) => receipt,
                                Err(_) => return false,
                            };
                        let frame = RouteStreamEnvelope::new(
                            "ui_seat.replay.append_verified.v1",
                            json!({
                                "schema": PROTOCOL,
                                "operation_id": receipt.operation_id,
                                "producer_incarnation": receipt.producer_incarnation,
                                "first_engine_seq": receipt.first_engine_seq,
                                "last_engine_seq": receipt.last_engine_seq,
                                "event_count": receipt.event_count,
                                "payload_digest": receipt.payload_digest,
                                "event_hash": record.event_hash,
                                "thread_seq": record.thread_seq,
                            }),
                        );
                        send_blocking(
                            &read_state,
                            &session_id,
                            &presentation_id,
                            &detail,
                            &sender,
                            &frame,
                        )
                    },
                );
                (proof, result)
            })
            .await;
            let (next, advance) = match outcome {
                Ok(value) => value,
                Err(_) => return,
            };
            proof = next;
            match advance {
                Ok(LiveSeatReplayAdvance::Current) => {}
                Ok(LiveSeatReplayAdvance::Stopped) => return,
                Ok(LiveSeatReplayAdvance::ReproofRequired) => {
                    let reproof = RouteStreamEnvelope::new(
                        "ui_seat.replay.reproof_required.v1",
                        json!({"schema": PROTOCOL}),
                    );
                    let _ = self.send(&reproof, None, None).await;
                    return;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "authenticated live seat suffix failed");
                    let invalidated = RouteStreamEnvelope::new(
                        "ui_seat.replay.invalidated.v1",
                        json!({"schema": PROTOCOL}),
                    );
                    let _ = self.send(&invalidated, None, None).await;
                    return;
                }
            }
        }
    }

    fn still_authorized(&self) -> bool {
        authorize_history_stream_batch(
            &self.state,
            &self.session_id,
            &self.presentation_id,
            &self.detail.chain_root_id,
            Some(&self.detail.thread_id),
        )
        .as_ref()
        .is_ok_and(|current| same_association(current, &self.detail))
    }
}

fn same_association(
    current: &ryeos_app::state_store::ThreadDetail,
    expected: &ryeos_app::state_store::ThreadDetail,
) -> bool {
    current.item_ref == expected.item_ref
        && current.executor_ref == expected.executor_ref
        && current.requested_by == expected.requested_by
}

/// Whether another seat the original paired cut depended on has advanced: a
/// live wake then cannot vouch for the old cut.
fn dependencies_moved(
    state: &AppState,
    session_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
    cut: Option<&FixedTransferCut>,
) -> Result<bool, ryeos_app::handler_error::HandlerError> {
    use ryeos_app::handler_error::HandlerError;
    let Some(cut) = cut else {
        return Ok(false);
    };
    let ui = get_ui_state(state).expect("UiState not set");
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired".into()))?;
    cut.publication_cut(state, &session)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    for anchor in cut.current_anchors() {
        if anchor.chain_root_id() == detail.chain_root_id {
            continue;
        }
        let current = state
            .state_store
            .capture_thread_event_read_anchor(anchor.chain_root_id(), anchor.thread_id())
            .map_err(|error| HandlerError::Internal(error.to_string()))?;
        if current.as_ref() != Some(&anchor) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn send_blocking(
    state: &AppState,
    session_id: &str,
    presentation_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
    sender: &tokio::sync::mpsc::Sender<RouteStreamEnvelope>,
    frame: &RouteStreamEnvelope,
) -> bool {
    loop {
        if sender.is_closed() {
            return false;
        }
        match try_send_authorized_history_frame(
            state,
            session_id,
            presentation_id,
            detail,
            sender,
            frame,
            None,
        ) {
            Ok(true) => return true,
            Ok(false) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                tracing::warn!(error = %error, "seat history disclosure stopped before completion");
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closed_receiver_prevents_paired_read_polling() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let polled = std::sync::atomic::AtomicBool::new(false);
        let result = await_while_connected(&sender, async {
            polled.store(true, std::sync::atomic::Ordering::SeqCst);
            7
        })
        .await;
        assert!(result.is_none());
        assert!(!polled.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn receiver_drop_cancels_in_progress_paired_future() {
        struct DropSignal(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let (started, start) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let read_dropped = dropped.clone();
        let read = async move {
            let _signal = DropSignal(read_dropped);
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        };
        let (result, ()) = tokio::join!(await_while_connected(&sender, read), async move {
            start.await.unwrap();
            drop(receiver);
        },);
        assert!(result.is_none());
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn connected_paired_future_returns_its_exact_result() {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        assert_eq!(await_while_connected(&sender, async { 7 }).await, Some(7));
    }

    #[test]
    fn verified_full_append_interval_is_split_before_provisional_disclosure() {
        let bounds = ReplayFrameBounds {
            events: 64,
            bytes: 4 * 1024 * 1024,
        };
        let interval = (0..=128_u64).collect::<Vec<_>>();
        let chunks = replay_event_chunks(&interval, bounds).unwrap();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
            [64, 64, 1]
        );
        assert_eq!(
            chunks.into_iter().flatten().copied().collect::<Vec<_>>(),
            interval
        );
    }

    #[test]
    fn verified_replay_frame_is_also_bounded_by_serialized_bytes() {
        let bounds = ReplayFrameBounds {
            events: 64,
            bytes: 4 * 1024 * 1024,
        };
        let records = vec!["x".repeat(1536 * 1024); 3];
        let chunks = replay_event_chunks(&records, bounds).unwrap();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
            [2, 1]
        );
    }

    #[test]
    fn frame_bounds_come_from_the_signed_policy_not_a_literal() {
        let (_directory, _owner, state) = crate::test_state::build_test_state();
        let policy = state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()
            .unwrap();
        let bounds = replay_frame_bounds(&state).unwrap();
        assert_eq!(
            bounds.events,
            usize::try_from(policy.seat.max_replay_frame_events).unwrap()
        );
        assert_eq!(
            bounds.bytes,
            usize::try_from(policy.seat.max_replay_frame_bytes).unwrap()
        );
    }
}
