//! Replay-events resume fallback for the graph runtime.
//!
//! V5.5 D10 precedence:
//!
//! 1. `RYE_RESUME=1` + a writable local `CheckpointWriter` payload →
//!    `from_checkpoint_value` (the typed source of truth: cursor +
//!    state both come back atomically).
//! 2. `RYE_RESUME=1` + no local checkpoint → fall back to this
//!    module's `load_resume_state`, which reconstructs the **cursor
//!    only** by replaying the durable event log. Graph state cannot
//!    be reconstructed this way (state mutations don't surface in
//!    indexed events), so resumed runs lose the in-flight `state`
//!    facet — a deliberate v1 limitation, documented because it's
//!    the rare path (typically a daemon restart before the first
//!    checkpoint write).
//! 3. Both unavailable + resume requested → `main.rs` must fail
//!    loudly. Silent cold-start when `RYE_RESUME=1` is forbidden.
//!
//! The previous facet-keyed implementation (`graph_checkpoint:*`,
//! `graph_ref:*`) has been removed — facets are runtime-thread
//! storage that doesn't survive daemon restart, so it never
//! delivered on its promise as a resume source.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_runtime::callback_client::CallbackClient;
#[cfg(test)]
use ryeos_runtime::ReplayedEventRecord;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    pub current_node: String,
    pub step_count: u32,
    pub state: Value,
    pub graph_run_id: String,
}

/// Reconstruct a [`ResumeState`] for the thread by scanning its
/// durable event log.
///
/// Walks the persisted events looking for the latest
/// `graph_step_started` — regardless of `graph_run_id`. Events are
/// already partitioned by `thread_id`, so the latest `graph_step_started`
/// for that thread IS the cursor we want. The matched event payload's
/// `graph_run_id` is reconstructed into the returned `ResumeState`.
///
/// D12: replay resume keys on `thread_id` only, not `graph_run_id`.
/// The launcher doesn't supply graph_run_id, so the old filter always
/// matched `""` — making replay effectively dead.
///
/// Returns `Ok(None)` when no `graph_step_started` exists.
/// `main.rs` is responsible for turning that into a hard failure
/// when `RYE_RESUME=1`.
pub async fn load_resume_state(
    callback: &CallbackClient,
    thread_id: &str,
) -> Result<Option<ResumeState>> {
    let response = callback.replay_events_for(thread_id).await?;

    let mut latest: Option<(String, u32, String)> = None;
    for ev in &response.events {
        if ev.event_type != "graph_step_started" {
            continue;
        }
        let payload_run_id = ev.payload
            .get("graph_run_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let node = match ev.payload.get("node").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let step = match ev.payload.get("step").and_then(|v| v.as_u64()) {
            Some(s) => s as u32,
            None => continue,
        };
        latest = Some((node, step, payload_run_id));
    }

    Ok(latest.map(|(node, step, graph_run_id)| ResumeState {
        current_node: node,
        step_count: step,
        // V1 limitation: replay-resume cannot recover state.
        // CheckpointWriter is the primary resume source.
        state: Value::Object(Default::default()),
        graph_run_id,
    }))
}

/// Construct a `ResumeState` from a `CheckpointWriter` JSON payload.
///
/// Payload shape (written by `walker::write_checkpoint`):
/// ```json
/// {
///   "graph_run_id": "...",
///   "current_node": "<NEXT cursor>",
///   "step_count": N,
///   "state": {...},
///   "written_at": "<iso>"
/// }
/// ```
///
/// Note: `current_node` here means "where to resume *into*" — the
/// walker writes the NEXT node's name, not the just-completed one
/// (R4 fix in `walker.rs`).
pub fn from_checkpoint_value(value: &Value) -> Result<ResumeState> {
    let current_node = value
        .get("current_node")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("checkpoint payload missing 'current_node'"))?
        .to_string();
    let step_count = value
        .get("step_count")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("checkpoint payload missing 'step_count'"))?
        as u32;
    let state = value
        .get("state")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("checkpoint payload missing 'state'"))?;
    let graph_run_id = value
        .get("graph_run_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(ResumeState {
        current_node,
        step_count,
        state,
        graph_run_id,
    })
}

/// Result of evaluating the V5.5 D10 resume precedence.
///
/// `main.rs` reduces the (`RYE_RESUME=1`?, local-checkpoint?, replay?)
/// triple into one of these four variants. Hoisting the decision into
/// a pure function keeps the precedence rule unit-testable without
/// having to spin up a real `CheckpointWriter` + callback transport.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumeSource {
    /// `RYE_RESUME` was unset → cold start. Walker boots from
    /// `cfg.start`.
    ColdStart,
    /// Local `CheckpointWriter` payload was present. Wins over
    /// replay; carries cursor + state both.
    LocalCheckpoint,
    /// No local checkpoint, replay-events reconstruction succeeded.
    /// Carries cursor only — graph state is lost on this path
    /// (documented v1 limitation).
    ReplayFallback,
    /// `RYE_RESUME=1` but neither source is available. `main.rs`
    /// MUST surface this as a hard error — silent cold-start when
    /// resume is requested is forbidden by D10.
    NoSourceAvailable,
}

/// Pure precedence rule: given (resume requested?, local checkpoint
/// available?, replay reconstruction succeeded?) return the chosen
/// source. The caller (`main.rs`) is responsible for actually loading
/// the payload — this function makes no I/O.
///
/// V5.5 D10:
/// 1. `RYE_RESUME=1` + local checkpoint → `LocalCheckpoint`
/// 2. `RYE_RESUME=1` + no local + replay hit → `ReplayFallback`
/// 3. `RYE_RESUME=1` + neither → `NoSourceAvailable` (caller MUST fail loud)
/// 4. `RYE_RESUME` unset → `ColdStart`
pub fn decide_resume_source(
    resume_requested: bool,
    local_checkpoint_present: bool,
    replay_succeeded: bool,
) -> ResumeSource {
    if !resume_requested {
        return ResumeSource::ColdStart;
    }
    if local_checkpoint_present {
        ResumeSource::LocalCheckpoint
    } else if replay_succeeded {
        ResumeSource::ReplayFallback
    } else {
        ResumeSource::NoSourceAvailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest, ReplayResponse, RuntimeCallbackAPI};
    use serde_json::json;
    use std::sync::Arc;

    struct ReplayMock {
        events: Vec<ReplayedEventRecord>,
    }

    #[async_trait]
    impl RuntimeCallbackAPI for ReplayMock {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_event(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(serde_json::to_value(ReplayResponse { events: self.events.clone() }).unwrap())
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
    }

    fn make_callback(events: Vec<ReplayedEventRecord>) -> CallbackClient {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(ReplayMock { events });
        CallbackClient::from_inner(inner, "T-test", "/tmp/test")
    }

    #[tokio::test]
    async fn replay_resume_returns_none_when_log_empty() {
        let callback = make_callback(vec![]);
        let result = load_resume_state(&callback, "T-test").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn replay_resume_loads_from_events() {
        let events = vec![
            ReplayedEventRecord {
                event_type: "graph_started".to_string(),
                payload: json!({"graph_run_id": "gr-target", "graph_id": "g1"}),
            },
            ReplayedEventRecord {
                event_type: "graph_step_started".to_string(),
                payload: json!({"graph_run_id": "gr-target", "node": "step1", "step": 0}),
            },
            ReplayedEventRecord {
                event_type: "graph_step_completed".to_string(),
                payload: json!({"graph_run_id": "gr-target", "node": "step1", "step": 0, "status": "ok"}),
            },
            ReplayedEventRecord {
                event_type: "graph_step_started".to_string(),
                payload: json!({"graph_run_id": "gr-target", "node": "step2", "step": 1}),
            },
        ];
        let callback = make_callback(events);
        let rs = load_resume_state(&callback, "T-test")
            .await
            .unwrap()
            .expect("resume state must be present");
        assert_eq!(rs.current_node, "step2");
        assert_eq!(rs.step_count, 1);
        assert_eq!(rs.graph_run_id, "gr-target");
        // V1 limitation: state is empty on replay-resume.
        assert!(rs.state.as_object().map_or(false, |m| m.is_empty()));
    }

    /// D12: replay picks the latest graph_step_started for the thread,
    /// ignoring graph_run_id mismatch. The old filter-by-graph_run_id
    /// path was dead because the launcher never supplied graph_run_id.
    #[tokio::test]
    async fn replay_resume_picks_latest_step_started_ignoring_run_id() {
        let events = vec![
            ReplayedEventRecord {
                event_type: "graph_step_started".to_string(),
                payload: json!({"graph_run_id": "run-a", "node": "step1", "step": 0}),
            },
            ReplayedEventRecord {
                event_type: "graph_step_started".to_string(),
                payload: json!({"graph_run_id": "run-b", "node": "step3", "step": 2}),
            },
            ReplayedEventRecord {
                event_type: "graph_step_started".to_string(),
                payload: json!({"graph_run_id": "run-a", "node": "step5", "step": 4}),
            },
        ];
        let callback = make_callback(events);
        let rs = load_resume_state(&callback, "T-test")
            .await
            .unwrap()
            .expect("resume state must be present");
        // Picks the LAST graph_step_started regardless of run_id.
        assert_eq!(rs.current_node, "step5");
        assert_eq!(rs.step_count, 4);
        assert_eq!(rs.graph_run_id, "run-a", "graph_run_id reconstructed from matched event");
    }

    #[tokio::test]
    async fn replay_resume_returns_none_when_no_step_started_events() {
        let events = vec![ReplayedEventRecord {
            event_type: "graph_started".to_string(),
            payload: json!({"graph_run_id": "gr-target", "graph_id": "g1"}),
        }];
        let callback = make_callback(events);
        let rs = load_resume_state(&callback, "T-test")
            .await
            .unwrap();
        assert!(rs.is_none());
    }

    #[test]
    fn from_checkpoint_value_parses_valid_payload() {
        let payload = json!({
            "graph_run_id": "gr-test",
            "current_node": "step4",
            "step_count": 7,
            "state": {"counter": 42},
            "written_at": "2026-01-01T00:00:00Z",
        });
        let state = from_checkpoint_value(&payload).unwrap();
        assert_eq!(state.current_node, "step4");
        assert_eq!(state.step_count, 7);
        assert_eq!(state.graph_run_id, "gr-test");
        assert_eq!(state.state["counter"], 42);
    }

    #[test]
    fn from_checkpoint_value_rejects_missing_fields() {
        let missing_node = json!({"step_count": 1, "state": {}});
        assert!(from_checkpoint_value(&missing_node).is_err());

        let missing_step = json!({"current_node": "x", "state": {}});
        assert!(from_checkpoint_value(&missing_step).is_err());

        let missing_state = json!({"current_node": "x", "step_count": 1});
        assert!(from_checkpoint_value(&missing_state).is_err());
    }

    // ── V5.5 D10 precedence (decide_resume_source) ────────────────────

    #[test]
    fn decide_resume_cold_start_when_resume_unset() {
        // Without RYE_RESUME=1, presence of either source MUST NOT
        // cause an unintended resume — the walker is supposed to
        // cold-start.
        assert_eq!(
            decide_resume_source(false, false, false),
            ResumeSource::ColdStart,
        );
        assert_eq!(
            decide_resume_source(false, true, false),
            ResumeSource::ColdStart,
            "checkpoint presence MUST be ignored when RYE_RESUME unset",
        );
        assert_eq!(
            decide_resume_source(false, true, true),
            ResumeSource::ColdStart,
        );
    }

    #[test]
    fn checkpoint_wins_over_replay_when_both_present() {
        // D10 step 1: local checkpoint always wins when both sources
        // are available. The replay path is the explicit fallback
        // path, never co-equal.
        assert_eq!(
            decide_resume_source(true, true, true),
            ResumeSource::LocalCheckpoint,
        );
        assert_eq!(
            decide_resume_source(true, true, false),
            ResumeSource::LocalCheckpoint,
            "checkpoint wins regardless of replay availability",
        );
    }

    #[test]
    fn explicit_replay_fallback_when_no_local_checkpoint() {
        // D10 step 2: no local checkpoint and replay reconstruction
        // succeeded → explicit replay fallback. Documented v1
        // limitation: state cannot be reconstructed (cursor only).
        assert_eq!(
            decide_resume_source(true, false, true),
            ResumeSource::ReplayFallback,
        );
    }

    #[test]
    fn no_source_available_fails_loud() {
        // D10 step 3: RYE_RESUME=1 but neither source has a payload.
        // `main.rs` MUST translate this into a hard `bail!`. Pure
        // function returns the variant; caller surfaces the error.
        assert_eq!(
            decide_resume_source(true, false, false),
            ResumeSource::NoSourceAvailable,
        );
    }
}
