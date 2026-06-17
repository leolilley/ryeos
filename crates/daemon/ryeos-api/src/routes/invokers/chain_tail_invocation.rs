//! Chain-tail invoker — the conversation feed.
//!
//! Replays a chain's braid from the `Last-Event-ID` cursor, then follows
//! it live across thread boundaries: a chained-resume turn is a NEW
//! thread in the same chain, so the tail re-subscribes to each new head
//! as the chain advances. Live is just replay arriving now — both paths
//! emit the same persisted records, cursored by `chain_seq`.
//!
//! Unlike the thread tail, a chain tail never terminates on a thread's
//! terminal event: chains continue. Between turns (head settled, next
//! turn not yet launched) the tail waits on a short replay poll — the
//! only place bounded polling is acceptable, because nothing is
//! executing to publish events.

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteEventStream, RouteInvocationContext,
    RouteInvocationContract, RouteInvocationOutput, RouteInvocationResult,
};
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::stream_envelope::RouteStreamEnvelope;

use super::stream_helpers::*;
use super::subscription_stream_invocation::parse_last_event_id;

pub struct CompiledChainTailInvocation {
    pub keep_alive_secs: u64,
}

static CHAIN_TAIL_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Stream,
    principal: PrincipalPolicy::Required,
};

const BETWEEN_TURNS_POLL_MS: u64 = 1500;

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledChainTailInvocation {
    fn contract(&self) -> &'static RouteInvocationContract {
        &CHAIN_TAIL_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        let chain_root_id = ctx
            .input
            .get("chain_root_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RouteDispatchError::BadRequest("missing chain_root_id in request".into())
            })?
            .to_string();

        let last_event_id = parse_last_event_id(&ctx.headers)?;

        // Ownership: only the chain root's creator can tail it.
        let principal = ctx
            .principal
            .as_ref()
            .ok_or(RouteDispatchError::Unauthorized)?;
        let root_detail = ctx
            .state
            .state_store
            .get_thread(&chain_root_id)
            .map_err(|e| RouteDispatchError::Internal(e.to_string()))?
            .ok_or(RouteDispatchError::NotFound)?;
        let requested_by = root_detail
            .requested_by
            .as_deref()
            .ok_or(RouteDispatchError::NotFound)?;
        if principal.id != requested_by {
            return Err(RouteDispatchError::NotFound);
        }

        let hub = ctx.state.event_streams.clone();
        let events_svc = ctx.state.events.clone();
        let keep_alive_secs = self.keep_alive_secs;

        let stream = async_stream::stream! {
            yield Ok(RouteStreamEnvelope::new(
                "stream_started",
                serde_json::json!({ "chain_root_id": chain_root_id }),
            ));

            let mut cursor: i64 = last_event_id.unwrap_or(0);
            // The thread the live subscription currently follows.
            let mut head_thread = chain_root_id.clone();
            let mut sub = ryeos_app::event_stream::HubSubscription::new(hub, &head_thread);

            loop {
                // Replay everything past the cursor, chain-scoped. This
                // both backfills and discovers chain advancement (new
                // turn threads appear in the chain's braid).
                let mut drained = false;
                while !drained {
                    let page = events_svc.replay(&EventReplayParams {
                        chain_root_id: Some(chain_root_id.clone()),
                        thread_id: None,
                        after_chain_seq: Some(cursor),
                        limit: REPLAY_BATCH_SIZE,
                    });
                    match page {
                        Ok(page) => {
                            if page.events.is_empty() {
                                drained = true;
                            }
                            for ev in &page.events {
                                cursor = cursor.max(ev.chain_seq);
                                if ev.thread_id != head_thread {
                                    // The chain advanced: follow the new
                                    // head's live events.
                                    head_thread = ev.thread_id.clone();
                                    sub.follow(&head_thread);
                                }
                                yield Ok(envelope_for_persisted(ev));
                            }
                            if page.next_cursor.is_none() {
                                drained = true;
                            }
                        }
                        Err(e) => {
                            yield Ok(error_envelope(
                                "replay_failed",
                                &format!("chain replay failed: {e}"),
                            ));
                            return;
                        }
                    }
                }

                // Live-follow the current head until it goes quiet
                // (terminal/lag/closed), then loop back to replay — which
                // either drains stragglers or discovers the next turn.
                loop {
                    let received = tokio::time::timeout(
                        std::time::Duration::from_millis(BETWEEN_TURNS_POLL_MS),
                        sub.recv(),
                    )
                    .await;
                    match received {
                        Ok(Ok(ev)) => {
                            if is_ephemeral(&ev) {
                                yield Ok(envelope_for_persisted(&ev));
                                continue;
                            }
                            if ev.chain_seq <= cursor {
                                continue;
                            }
                            cursor = ev.chain_seq;
                            yield Ok(envelope_for_persisted(&ev));
                            if is_terminal(&ev.event_type) {
                                // Thread settled; chain may continue —
                                // fall back to replay-discovery.
                                break;
                            }
                        }
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                            // Lag recovery = the outer replay loop.
                            break;
                        }
                        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                            break;
                        }
                        Err(_elapsed) => {
                            // Quiet head: re-check the chain for a new
                            // turn (bounded between-turns poll).
                            break;
                        }
                    }
                }
            }
        };

        Ok(RouteInvocationResult::Stream(RouteEventStream {
            events: Box::pin(stream),
            keep_alive_secs,
        }))
    }
}
