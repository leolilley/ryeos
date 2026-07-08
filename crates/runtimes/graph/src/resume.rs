//! Replay-events resume fallback for the graph runtime.
//!
//! V5.5 D10 precedence:
//!
//! 1. `RYEOS_RESUME=1` + a writable local `CheckpointWriter` payload →
//!    `from_checkpoint_value` (the typed source of truth: cursor +
//!    state both come back atomically).
//! 2. `RYEOS_RESUME=1` + no local checkpoint → fall back to this
//!    module's `load_resume_state`, which reconstructs the **cursor
//!    only** by replaying the durable event log. Graph state cannot
//!    be reconstructed this way (state mutations don't surface in
//!    indexed events), so resumed runs lose the in-flight `state`
//!    facet — a deliberate v1 limitation, documented because it's
//!    the rare path (typically a daemon restart before the first
//!    checkpoint write).
//! 3. Both unavailable + resume requested → `main.rs` must fail
//!    loudly. Silent cold-start when `RYEOS_RESUME=1` is forbidden.
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
#[serde(deny_unknown_fields)]
pub struct ResumeState {
    pub current_node: String,
    pub step_count: u32,
    pub state: Value,
    pub graph_run_id: String,
    /// Raw `accounting` snapshot from the checkpoint payload (`None` for the
    /// event-replay path, which cannot reconstruct cost). The walker
    /// deserializes it into `GraphAccounting` to restore accumulated cost.
    pub accounting: Option<Value>,
    /// Raw `suppressed_errors` array from the checkpoint payload (`None` for the
    /// event-replay path). The walker deserializes it into `Vec<ErrorRecord>` to
    /// restore the pre-checkpoint suppressed-error history.
    pub suppressed_errors: Option<Value>,
    /// Present when the resume point is a follow node: local facts
    /// (`follow_node`, `step_count`, `graph_run_id`) recorded at suspend. No
    /// child IDs — the daemon owns the child relationship.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_follow: Option<Value>,
    /// The child's canonical terminal envelope, spliced into the successor's
    /// resume state by the follow-resume launcher. Consumed at the follow node
    /// instead of re-dispatching. Absent on a plain (non-follow) resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_result: Option<Value>,
    /// Per-step retry attempts already spent on `current_node` (checkpoint v2).
    /// `None` only on the event-replay path (no checkpoint) → the walker resumes
    /// with a zero attempt count. Pre-v2 checkpoints do NOT reach here: they are
    /// rejected at resume (a killed form), so a resumed checkpoint always carries
    /// this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_attempt: Option<u32>,
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
/// when `RYEOS_RESUME=1`.
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
        let payload_run_id = ev
            .payload
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
        // Event-replay resume cannot reconstruct cost accounting or the
        // suppressed-error history.
        accounting: None,
        suppressed_errors: None,
        // Follow resume relies on the checkpoint (which carries the marker + the
        // spliced child envelope); the event-replay path never resumes a follow.
        pending_follow: None,
        follow_result: None,
        // Event-replay cannot reconstruct the retry counter; resume from 0.
        retry_attempt: None,
    }))
}

/// Construct a `ResumeState` from a `CheckpointWriter` JSON payload.
///
/// Payload shape (written by `walker::write_checkpoint`):
/// ```json
/// {
///   "schema_version": 2,
///   "graph_run_id": "...",
///   "current_node": "<NEXT cursor>",
///   "step_count": N,
///   "state": {...},
///   "accounting": {"total": {...}|null, "nodes": [...]},
///   "suppressed_errors": [{"step": N, "node": "...", "error": "..."}],
///   "retry_attempt": 0,
///   "written_at": "<iso>"
/// }
/// ```
///
/// `schema_version` is required and must match
/// [`crate::walker::GRAPH_CHECKPOINT_SCHEMA_VERSION`] — an unknown version is
/// rejected rather than mis-read. `current_node` means "where to resume *into*":
/// the walker writes the NEXT node's name, not the just-completed one (R4 fix in
/// `walker.rs`).
pub fn from_checkpoint_value(value: &Value) -> Result<ResumeState> {
    // The payload is versioned; reject an unknown version rather than
    // mis-reading a future/foreign shape. The branch is unreleased, so there is
    // no legacy v0 to accept.
    let schema_version = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("checkpoint payload missing 'schema_version'"))?;
    if schema_version != crate::walker::GRAPH_CHECKPOINT_SCHEMA_VERSION as u64 {
        anyhow::bail!(
            "unsupported checkpoint schema_version {schema_version} (expected {})",
            crate::walker::GRAPH_CHECKPOINT_SCHEMA_VERSION
        );
    }
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
        accounting: value.get("accounting").cloned(),
        suppressed_errors: value.get("suppressed_errors").cloned(),
        pending_follow: value
            .get(crate::walker::follow_keys::PENDING_FOLLOW)
            .cloned(),
        follow_result: value
            .get(crate::walker::follow_keys::FOLLOW_RESULT)
            .cloned(),
        retry_attempt: value
            .get("retry_attempt")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
    })
}

/// Result of evaluating the V5.5 D10 resume precedence.
///
/// `main.rs` reduces the (`RYEOS_RESUME=1`?, local-checkpoint?, replay?)
/// triple into one of these four variants. Hoisting the decision into
/// a pure function keeps the precedence rule unit-testable without
/// having to spin up a real `CheckpointWriter` + callback transport.
#[derive(Debug, PartialEq, Eq)]
pub enum ResumeSource {
    /// `RYEOS_RESUME` was unset → cold start. Walker boots from
    /// `cfg.start`.
    ColdStart,
    /// Local `CheckpointWriter` payload was present. Wins over
    /// replay; carries cursor + state both.
    LocalCheckpoint,
    /// No local checkpoint, replay-events reconstruction succeeded.
    /// Carries cursor only — graph state is lost on this path
    /// (documented v1 limitation).
    ReplayFallback,
    /// `RYEOS_RESUME=1` but neither source is available. `main.rs`
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
/// 1. `RYEOS_RESUME=1` + local checkpoint → `LocalCheckpoint`
/// 2. `RYEOS_RESUME=1` + no local + replay hit → `ReplayFallback`
/// 3. `RYEOS_RESUME=1` + neither → `NoSourceAvailable` (caller MUST fail loud)
/// 4. `RYEOS_RESUME` unset → `ColdStart`
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
    use ryeos_runtime::callback::{
        CallbackError, DispatchActionRequest, ReplayResponse, RuntimeCallbackAPI,
    };
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
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            _: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn request_continuation(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: Value) -> Result<Value, CallbackError> {
            Ok(serde_json::to_value(ReplayResponse {
                events: self.events.clone(),
                next_cursor: None,
            })
            .unwrap())
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: i64,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
    }

    fn make_callback(events: Vec<ReplayedEventRecord>) -> CallbackClient {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(ReplayMock { events });
        CallbackClient::from_inner(inner, "T-test", "/tmp/test", "tat-test")
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
        assert!(rs.state.as_object().is_some_and(|m| m.is_empty()));
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
        assert_eq!(
            rs.graph_run_id, "run-a",
            "graph_run_id reconstructed from matched event"
        );
    }

    #[tokio::test]
    async fn replay_resume_returns_none_when_no_step_started_events() {
        let events = vec![ReplayedEventRecord {
            event_type: "graph_started".to_string(),
            payload: json!({"graph_run_id": "gr-target", "graph_id": "g1"}),
        }];
        let callback = make_callback(events);
        let rs = load_resume_state(&callback, "T-test").await.unwrap();
        assert!(rs.is_none());
    }

    #[test]
    fn from_checkpoint_value_parses_valid_payload() {
        let payload = json!({
            "schema_version": 2,
            "graph_run_id": "gr-test",
            "current_node": "step4",
            "step_count": 7,
            "state": {"counter": 42},
            "accounting": {"total": null, "nodes": []},
            "suppressed_errors": [{"step": 2, "node": "n1", "error": "boom"}],
            "retry_attempt": 2,
            "written_at": "2026-01-01T00:00:00Z",
        });
        let state = from_checkpoint_value(&payload).unwrap();
        assert_eq!(state.current_node, "step4");
        assert_eq!(state.step_count, 7);
        assert_eq!(state.graph_run_id, "gr-test");
        assert_eq!(state.state["counter"], 42);
        assert_eq!(
            state.retry_attempt,
            Some(2),
            "retry_attempt carried through from the v2 checkpoint"
        );
        assert_eq!(
            state.accounting,
            Some(json!({"total": null, "nodes": []})),
            "accounting carried through verbatim"
        );
        assert_eq!(
            state.suppressed_errors,
            Some(json!([{"step": 2, "node": "n1", "error": "boom"}])),
            "suppressed_errors carried through verbatim"
        );
    }

    #[test]
    fn from_checkpoint_value_rejects_bad_schema_version() {
        // Missing schema_version → rejected (no legacy v0 on this branch).
        let no_version = json!({"current_node": "x", "step_count": 1, "state": {}});
        assert!(from_checkpoint_value(&no_version).is_err());
        // Unknown future version → rejected, not mis-read.
        let future = json!({
            "schema_version": 999,
            "current_node": "x",
            "step_count": 1,
            "state": {},
        });
        assert!(from_checkpoint_value(&future).is_err());
        // The prior v1 shape (pre-retry_attempt) is a killed form — rejected,
        // never read as a v2 payload with a defaulted counter.
        let v1 = json!({
            "schema_version": 1,
            "current_node": "x",
            "step_count": 1,
            "state": {},
        });
        assert!(from_checkpoint_value(&v1).is_err());
    }

    #[test]
    fn from_checkpoint_value_rejects_missing_fields() {
        let missing_node = json!({"schema_version": 2, "step_count": 1, "state": {}});
        assert!(from_checkpoint_value(&missing_node).is_err());

        let missing_step = json!({"schema_version": 2, "current_node": "x", "state": {}});
        assert!(from_checkpoint_value(&missing_step).is_err());

        let missing_state = json!({"schema_version": 2, "current_node": "x", "step_count": 1});
        assert!(from_checkpoint_value(&missing_state).is_err());
    }

    // ── V5.5 D10 precedence (decide_resume_source) ────────────────────

    #[test]
    fn decide_resume_cold_start_when_resume_unset() {
        // Without RYEOS_RESUME=1, presence of either source MUST NOT
        // cause an unintended resume — the walker is supposed to
        // cold-start.
        assert_eq!(
            decide_resume_source(false, false, false),
            ResumeSource::ColdStart,
        );
        assert_eq!(
            decide_resume_source(false, true, false),
            ResumeSource::ColdStart,
            "checkpoint presence MUST be ignored when RYEOS_RESUME unset",
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
        // D10 step 3: RYEOS_RESUME=1 but neither source has a payload.
        // `main.rs` MUST translate this into a hard `bail!`. Pure
        // function returns the variant; caller surfaces the error.
        assert_eq!(
            decide_resume_source(true, false, false),
            ResumeSource::NoSourceAvailable,
        );
    }
}
