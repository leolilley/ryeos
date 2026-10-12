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
use ryeos_app::stream_envelope::RouteStreamEnvelope;
use serde::Deserialize;
use serde_json::json;

use crate::handlers::ui_seat::{
    DiscoveredRootTransferCut, FixedAnchorReplay, FixedAnchorReplayStep, LiveSeatReplayAdvance,
    authorize_history_stream_batch, read_replay_transfer_cut_at_anchor,
    try_send_authorized_history_frame, try_send_authorized_transfer_history_frame,
    try_send_reconciled_transfer_complete,
};

const SOURCE: &str = "ui_seat_history";
const PATH: &str = "/ui/api/session/seat/history";
const PROTOCOL: u32 = 1;
// The web and terminal cold-replay decoders each admit at most 64 records
// per provisional frame. A verified append interval can contain 128 events
// plus its receipt, so locator-page size alone cannot bound frame size.
const MAX_REPLAY_FRAME_EVENTS: usize = 64;
// Leave room for the JSON envelope and SSE framing below the browser's
// 16 MiB and the terminal's 32 MiB frame buffers. One retained thread event
// is itself capped at 512 KiB by the state owner.
const MAX_REPLAY_FRAME_RECORD_BYTES: usize = 4 * 1024 * 1024;

fn replay_event_chunks<T: serde::Serialize>(events: &[T]) -> Result<Vec<&[T]>, String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0usize;
    for (index, event) in events.iter().enumerate() {
        let event_bytes = serde_json::to_vec(event)
            .map_err(|error| format!("serialize verified replay record: {error}"))?
            .len();
        // A comma is the maximum per-record array overhead. The schema and
        // SSE envelope are much smaller than the remaining 12 MiB margin.
        let next_bytes = bytes
            .checked_add(event_bytes)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| "replay frame byte count overflowed".to_string())?;
        if index > start
            && (index - start == MAX_REPLAY_FRAME_EVENTS
                || next_bytes > MAX_REPLAY_FRAME_RECORD_BYTES)
        {
            chunks.push(&events[start..index]);
            start = index;
            bytes = 0;
        }
        bytes = bytes
            .checked_add(event_bytes)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| "replay frame byte count overflowed".to_string())?;
        if bytes > MAX_REPLAY_FRAME_RECORD_BYTES {
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
        let state = ctx.state;
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
        // Subscribe before capturing the anchor. Persistence-first seat
        // publication then cannot disappear between the signed cold proof and
        // the live invalidation lane; lag is a resnapshot condition, never
        // permission to guess a missing interval.
        let mut live = state.event_streams.subscribe(&detail.thread_id);
        // Capture the signed endpoint before response headers. A failed proof
        // after headers can only end the stream without `complete`.
        let proof = FixedAnchorReplay::begin(
            &state,
            &request.presentation_id,
            &detail.chain_root_id,
            &detail.thread_id,
        )
        .map_err(|error| RouteDispatchError::Internal(error.to_string()))?;
        let (sender, receiver) = tokio::sync::mpsc::channel::<RouteStreamEnvelope>(2);
        let live_sender = sender.clone();
        tokio::spawn(async move {
            let mut proof = proof;
            let mut paired_cut = None;
            let begin = RouteStreamEnvelope::new(
                "ui_seat.replay.begin.v1",
                json!({
                    "schema":PROTOCOL,
                    "presentation_id":request.presentation_id,
                    "chain_root_id":detail.chain_root_id,
                    "thread_id":detail.thread_id,
                }),
            );
            if !send_replay_authorized(
                &state,
                &session_id,
                &request.presentation_id,
                &detail,
                &sender,
                &begin,
                None,
                None,
            )
            .await
            {
                return;
            }
            let mut emitted = 0_u64;
            loop {
                if sender.is_closed() {
                    return;
                }
                let read_state = state.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let step = proof.next_step(&read_state);
                    (proof, step)
                })
                .await;
                let (returned_proof, step) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        tracing::warn!(%error, "fixed replay page worker failed");
                        return;
                    }
                };
                proof = returned_proof;
                let step = match step {
                    Ok(step) => step,
                    Err(error) => {
                        tracing::warn!(error = %error, "authenticated seat history stream stopped without completion");
                        return;
                    }
                };
                let (frame, expected_tip) = match step {
                    FixedAnchorReplayStep::RequiresPairedTransfer { anchor } => {
                        let Some(Ok(DiscoveredRootTransferCut::Transfers { cut, .. })) =
                            await_while_connected(
                                &sender,
                                read_replay_transfer_cut_at_anchor(
                                    Arc::new(state.clone()),
                                    &session_id,
                                    &request.presentation_id,
                                    &detail.chain_root_id,
                                    &detail.thread_id,
                                    anchor,
                                ),
                            )
                            .await
                        else {
                            return;
                        };
                        let cut = Arc::new(cut);
                        if let Err(error) = proof.attach_paired_cut(cut.clone()) {
                            tracing::warn!(%error, "paired cut cannot bind replay anchor");
                            return;
                        }
                        // Proofs precede all resumed canonical rows. They are
                        // bounded per operation and never advance event_count.
                        for operation_id in cut.seat_operation_ids(
                            &request.presentation_id,
                            &detail.chain_root_id,
                            &detail.thread_id,
                        ) {
                            let read_cut = cut.clone();
                            let read_state = state.clone();
                            let operation_id = operation_id.to_owned();
                            let frame = tokio::task::spawn_blocking(move || {
                                read_cut.proof_frame(
                                    &read_state.state_store,
                                    &operation_id,
                                    MAX_REPLAY_FRAME_RECORD_BYTES as u64,
                                )
                            })
                            .await;
                            let Ok(Ok(frame)) = frame else {
                                return;
                            };
                            let frame = RouteStreamEnvelope::new(
                                "ui_seat.replay.transfer_proof.v1",
                                json!({"schema":PROTOCOL,"proof":frame}),
                            );
                            if !send_replay_authorized(
                                &state,
                                &session_id,
                                &request.presentation_id,
                                &detail,
                                &sender,
                                &frame,
                                None,
                                Some(&cut),
                            )
                            .await
                            {
                                return;
                            }
                        }
                        for operation_id in cut.seat_cancellation_ids(
                            &request.presentation_id,
                            &detail.chain_root_id,
                            &detail.thread_id,
                        ) {
                            let read_cut = cut.clone();
                            let read_state = state.clone();
                            let operation_id = operation_id.to_owned();
                            let frame = tokio::task::spawn_blocking(move || {
                                read_cut.cancellation_frame(
                                    &read_state.state_store,
                                    &operation_id,
                                    MAX_REPLAY_FRAME_RECORD_BYTES as u64,
                                )
                            })
                            .await;
                            let Ok(Ok(frame)) = frame else {
                                return;
                            };
                            let frame = RouteStreamEnvelope::new(
                                "ui_seat.replay.move_freeze_cancellation_proof.v1",
                                json!({"schema":PROTOCOL,"proof":frame}),
                            );
                            if !send_replay_authorized(
                                &state,
                                &session_id,
                                &request.presentation_id,
                                &detail,
                                &sender,
                                &frame,
                                None,
                                Some(&cut),
                            )
                            .await
                            {
                                return;
                            }
                        }
                        paired_cut = Some(cut);
                        continue;
                    }
                    FixedAnchorReplayStep::Batch { events } => {
                        if events.is_empty() {
                            tokio::task::yield_now().await;
                            continue;
                        }
                        // The entire interval is verified before this step,
                        // but every provisional chunk still needs its own
                        // browser-session reauthorization and backpressure.
                        let chunks = match replay_event_chunks(&events) {
                            Ok(chunks) => chunks,
                            Err(error) => {
                                tracing::warn!(%error, "verified seat replay cannot fit a bounded frame");
                                return;
                            }
                        };
                        for chunk in chunks {
                            let frame = RouteStreamEnvelope::new(
                                "ui_seat.replay.events.v1",
                                json!({"schema":PROTOCOL,"events":chunk}),
                            );
                            if !send_replay_authorized(
                                &state,
                                &session_id,
                                &request.presentation_id,
                                &detail,
                                &sender,
                                &frame,
                                None,
                                paired_cut.as_deref(),
                            )
                            .await
                            {
                                return;
                            }
                            emitted = match emitted.checked_add(chunk.len() as u64) {
                                Some(count) => count,
                                None => return,
                            };
                        }
                        continue;
                    }
                    FixedAnchorReplayStep::Complete {
                        tip_hash,
                        thread_seq,
                    } => {
                        // The subscribed hub is a wake, not signed history.
                        // A queued wake alone must not abort an otherwise
                        // current proof. The exact signed tip is checked under
                        // the transition guard when this completion is sent;
                        // if it advanced, the client discards this provisional
                        // cut and starts a fresh proof of the same claimed seat.
                        (
                            RouteStreamEnvelope::new(
                                "ui_seat.replay.complete.v1",
                                json!({
                                    "schema":PROTOCOL,
                                    "tip_hash":tip_hash.clone(),
                                    "thread_seq":thread_seq,
                                    "event_count":emitted,
                                }),
                            ),
                            Some((tip_hash, thread_seq)),
                        )
                    }
                };
                let complete = frame.event_type == "ui_seat.replay.complete.v1";
                let sent = if complete && paired_cut.is_some() {
                    let cut = paired_cut.as_ref().expect("checked paired cut");
                    let replay = match proof.completed_paired_projection() {
                        Ok(replay) => replay,
                        Err(error) => {
                            tracing::warn!(%error, "paired completion has no full root projection");
                            return;
                        }
                    };
                    send_reconciled_complete_authorized(
                        &state,
                        &session_id,
                        &request.presentation_id,
                        &detail,
                        cut,
                        replay,
                        &sender,
                        &frame,
                        expected_tip.as_ref().expect("complete has tip"),
                    )
                    .await
                } else {
                    send_replay_authorized(
                        &state,
                        &session_id,
                        &request.presentation_id,
                        &detail,
                        &sender,
                        &frame,
                        expected_tip
                            .as_ref()
                            .map(|(hash, sequence)| (hash.as_str(), *sequence)),
                        paired_cut.as_deref(),
                    )
                    .await
                };
                if !sent {
                    return;
                }
                if complete {
                    let state = state.clone();
                    let detail = detail.clone();
                    let session_id = session_id.clone();
                    let presentation_id = request.presentation_id.clone();
                    tokio::spawn(async move {
                        let mut proof = proof;
                        loop {
                            if live_sender.is_closed() {
                                return;
                            }
                            match tokio::time::timeout(Duration::from_secs(15), live.recv()).await {
                                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return,
                                _ => {} // event, lag, and timer expiry are all signed-read wakes
                            }
                            let current = authorize_history_stream_batch(
                                &state,
                                &session_id,
                                &presentation_id,
                                &detail.chain_root_id,
                                Some(&detail.thread_id),
                            );
                            if !current.as_ref().is_ok_and(|current| {
                                current.item_ref == detail.item_ref
                                    && current.executor_ref == detail.executor_ref
                                    && current.requested_by == detail.requested_by
                            }) {
                                return;
                            }
                            let read_state = state.clone();
                            let read_detail = detail.clone();
                            let read_session_id = session_id.clone();
                            let read_presentation_id = presentation_id.clone();
                            let read_sender = live_sender.clone();
                            let read_cut = paired_cut.clone();
                            let outcome = tokio::task::spawn_blocking(move || {
                                let dependencies = (|| -> Result<bool, ryeos_app::handler_error::HandlerError> {
                                    let Some(cut) = read_cut.as_ref() else { return Ok(false); };
                                    let ui = crate::state::get_ui_state(&read_state).expect("UiState not set");
                                    let session = ui.browser_sessions.get_session(&read_session_id)
                                        .ok_or_else(|| ryeos_app::handler_error::HandlerError::Forbidden("session expired".into()))?;
                                    let anchors = cut.live_dependency_anchors(&read_state, &session)
                                        .map_err(|error| ryeos_app::handler_error::HandlerError::Forbidden(error.to_string()))?;
                                    for anchor in anchors {
                                        if anchor.chain_root_id() == read_detail.chain_root_id { continue; }
                                        let current = read_state.state_store.capture_thread_event_read_anchor(anchor.chain_root_id(), anchor.thread_id())
                                            .map_err(|error| ryeos_app::handler_error::HandlerError::Internal(error.to_string()))?;
                                        if current.as_ref() != Some(&anchor) { return Ok(true); }
                                    }
                                    Ok(false)
                                })();
                                match dependencies {
                                    Ok(true) => return (proof, Ok(LiveSeatReplayAdvance::ReproofRequired)),
                                    Err(error) => return (proof, Err(error)),
                                    Ok(false) => {},
                                }
                                let result = proof.advance_receipts(&read_state, || {
                                    if read_sender.is_closed() {
                                        return false;
                                    }
                                    let current = authorize_history_stream_batch(
                                        &read_state,
                                        &read_session_id,
                                        &read_presentation_id,
                                        &read_detail.chain_root_id,
                                        Some(&read_detail.thread_id),
                                    );
                                    current.as_ref().is_ok_and(|current| {
                                        current.item_ref == read_detail.item_ref
                                            && current.executor_ref == read_detail.executor_ref
                                            && current.requested_by == read_detail.requested_by
                                    })
                                }, |record| {
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
                                    send_authorized(
                                        &read_state, &read_session_id, &read_presentation_id,
                                        &read_detail, &read_sender, &frame,
                                        None,
                                    )
                                });
                                (proof, result)
                            })
                            .await;
                            let (next_proof, receipts) = match outcome {
                                Ok(value) => value,
                                Err(_) => return,
                            };
                            proof = next_proof;
                            match receipts {
                                Ok(LiveSeatReplayAdvance::Current) => {}
                                Ok(LiveSeatReplayAdvance::Stopped) => return,
                                Ok(LiveSeatReplayAdvance::ReproofRequired) => {
                                    let reproof = RouteStreamEnvelope::new(
                                        "ui_seat.replay.reproof_required.v1",
                                        json!({"schema":PROTOCOL}),
                                    );
                                    let _ = send_authorized_async(
                                        &state,
                                        &session_id,
                                        &presentation_id,
                                        &detail,
                                        &live_sender,
                                        &reproof,
                                    )
                                    .await;
                                    return;
                                }
                                Err(error) => {
                                    tracing::warn!(error = %error, "authenticated live seat suffix failed");
                                    let invalidated = RouteStreamEnvelope::new(
                                        "ui_seat.replay.invalidated.v1",
                                        json!({"schema":PROTOCOL}),
                                    );
                                    let _ = send_authorized_async(
                                        &state,
                                        &session_id,
                                        &presentation_id,
                                        &detail,
                                        &live_sender,
                                        &invalidated,
                                    )
                                    .await;
                                    return;
                                }
                            }
                        }
                    });
                    return;
                }
            }
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

async fn send_authorized_async(
    state: &ryeos_app::state::AppState,
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
            Ok(false) => tokio::time::sleep(Duration::from_millis(5)).await,
            Err(_) => return false,
        }
    }
}

async fn send_reconciled_complete_authorized(
    state: &ryeos_app::state::AppState,
    session_id: &str,
    presentation_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
    cut: &crate::handlers::ui_seat::FixedTransferCut,
    replay: &ryeos_client_base::ui::composition_replay::UiCompositionReplay,
    sender: &tokio::sync::mpsc::Sender<RouteStreamEnvelope>,
    frame: &RouteStreamEnvelope,
    tip: &(String, u64),
) -> bool {
    loop {
        if sender.is_closed() {
            return false;
        }
        match try_send_reconciled_transfer_complete(
            state,
            session_id,
            presentation_id,
            &detail.chain_root_id,
            &detail.thread_id,
            &tip.0,
            tip.1,
            cut,
            replay,
            sender,
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

async fn send_replay_authorized(
    state: &ryeos_app::state::AppState,
    session_id: &str,
    presentation_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
    sender: &tokio::sync::mpsc::Sender<RouteStreamEnvelope>,
    frame: &RouteStreamEnvelope,
    expected_tip: Option<(&str, u64)>,
    paired_cut: Option<&crate::handlers::ui_seat::FixedTransferCut>,
) -> bool {
    loop {
        if sender.is_closed() {
            return false;
        }
        let result = match paired_cut {
            Some(cut) => {
                try_send_authorized_transfer_history_frame(state, session_id, cut, sender, frame)
            }
            None => try_send_authorized_history_frame(
                state,
                session_id,
                presentation_id,
                detail,
                sender,
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

fn send_authorized(
    state: &ryeos_app::state::AppState,
    session_id: &str,
    presentation_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
    sender: &tokio::sync::mpsc::Sender<RouteStreamEnvelope>,
    frame: &RouteStreamEnvelope,
    expected_tip: Option<(&str, u64)>,
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
            expected_tip,
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
        let interval = (0..=128_u64).collect::<Vec<_>>();
        let chunks = replay_event_chunks(&interval).unwrap();
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
        let records = vec!["x".repeat(1536 * 1024); 3];
        let chunks = replay_event_chunks(&records).unwrap();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
            [2, 1]
        );
    }
}
