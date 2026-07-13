use serde_json::{json, Value};

use crate::model::ErrorRecord;
use ryeos_runtime::checkpoint::CheckpointWriter;

use super::{follow_keys, Walker, GRAPH_CHECKPOINT_SCHEMA_VERSION};

struct CheckpointCursor<'a> {
    graph_run_id: &'a str,
    next_node: &'a str,
    next_step: u32,
    state: &'a Value,
    accounting: Value,
    suppressed_errors: &'a [ErrorRecord],
    retry_attempt: u32,
    written_at: &'a str,
}

/// Build the versioned cursor payload persisted after an advancing step.
fn checkpoint_payload(cursor: CheckpointCursor<'_>) -> Value {
    let CheckpointCursor {
        graph_run_id,
        next_node,
        next_step,
        state,
        accounting,
        suppressed_errors,
        retry_attempt,
        written_at,
    } = cursor;
    json!({
        "schema_version": GRAPH_CHECKPOINT_SCHEMA_VERSION,
        "graph_run_id": graph_run_id,
        "current_node": next_node,
        "step_count": next_step,
        "state": state,
        "accounting": accounting,
        "suppressed_errors": suppressed_errors,
        // Non-zero only when re-entering this same node after a failed attempt.
        "retry_attempt": retry_attempt,
        "written_at": written_at,
    })
}

/// Add the local pending-follow marker to a regular cursor payload. The marker
/// deliberately contains no child identity: daemon handoff owns that mapping.
fn follow_checkpoint_payload(
    cursor: CheckpointCursor<'_>,
    iteration_snapshot: Option<&[Value]>,
) -> Value {
    let graph_run_id = cursor.graph_run_id;
    let follow_node = cursor.next_node;
    let step = cursor.next_step;
    let mut payload = checkpoint_payload(cursor);
    let mut pending = serde_json::Map::new();
    pending.insert(follow_keys::FOLLOW_NODE.to_string(), json!(follow_node));
    pending.insert("step_count".to_string(), json!(step));
    pending.insert("graph_run_id".to_string(), json!(graph_run_id));
    if let Some(items) = iteration_snapshot {
        pending.insert("iteration_snapshot".to_string(), json!(items));
    }
    payload[follow_keys::PENDING_FOLLOW] = Value::Object(pending);
    payload
}

impl Walker {
    /// Write a checkpoint marking a follow suspend. The cursor points at the
    /// follow node itself so re-entry can idempotently re-drive the handoff.
    pub(super) async fn write_follow_checkpoint(
        &self,
        graph_run_id: &str,
        follow_node: &str,
        step: u32,
        state: &Value,
        suppressed_errors: &[ErrorRecord],
        iteration_snapshot: Option<&[Value]>,
    ) -> anyhow::Result<()> {
        let Some(writer) = &self.checkpoint else {
            return Ok(());
        };
        let accounting = {
            let acc = self.accounting.lock().unwrap();
            serde_json::to_value(&*acc).unwrap_or(Value::Null)
        };
        writer.write(&follow_checkpoint_payload(
            CheckpointCursor {
                graph_run_id,
                next_node: follow_node,
                next_step: step,
                state,
                accounting,
                suppressed_errors,
                retry_attempt: 0,
                written_at: &lillux::time::iso8601_now(),
            },
            iteration_snapshot,
        ))?;
        Ok(())
    }

    /// Persist the versioned next-node cursor and the accounting/error history
    /// needed to reconstruct a resumed run without under-counting prior work.
    pub(super) async fn write_checkpoint(
        &self,
        graph_run_id: &str,
        next_node: &str,
        next_step: u32,
        state: &Value,
        suppressed_errors: &[ErrorRecord],
        retry_attempt: u32,
    ) -> anyhow::Result<()> {
        let Some(writer) = &self.checkpoint else {
            return Ok(());
        };
        let accounting = {
            let acc = self.accounting.lock().unwrap();
            serde_json::to_value(&*acc).unwrap_or(Value::Null)
        };
        writer.write(&checkpoint_payload(CheckpointCursor {
            graph_run_id,
            next_node,
            next_step,
            state,
            accounting,
            suppressed_errors,
            retry_attempt,
            written_at: &lillux::time::iso8601_now(),
        }))?;

        // Production-inert crash injection used by the graph recovery e2e.
        if !CheckpointWriter::is_resume()
            && std::env::var("RYEOS_GRAPH_TEST_BLOCK_AFTER_CHECKPOINT")
                .ok()
                .as_deref()
                == Some(next_node)
        {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_payload_preserves_resume_state() {
        let errors = vec![ErrorRecord {
            step: 3,
            node: "previous".to_string(),
            error: "suppressed".to_string(),
        }];

        let state = json!({"answer": 42});
        let payload = checkpoint_payload(CheckpointCursor {
            graph_run_id: "run-1",
            next_node: "retrying",
            next_step: 4,
            state: &state,
            accounting: json!({"total": null, "nodes": []}),
            suppressed_errors: &errors,
            retry_attempt: 2,
            written_at: "2026-01-02T03:04:05Z",
        });

        assert_eq!(payload["schema_version"], GRAPH_CHECKPOINT_SCHEMA_VERSION);
        assert_eq!(payload["current_node"], "retrying");
        assert_eq!(payload["step_count"], 4);
        assert_eq!(payload["retry_attempt"], 2);
        assert_eq!(payload["state"], json!({"answer": 42}));
        assert_eq!(payload["suppressed_errors"][0]["error"], "suppressed");
    }

    #[test]
    fn follow_payload_repoints_cursor_without_child_identity() {
        let state = json!({});
        let payload = follow_checkpoint_payload(
            CheckpointCursor {
                graph_run_id: "run-2",
                next_node: "wait-for-child",
                next_step: 7,
                state: &state,
                accounting: Value::Null,
                suppressed_errors: &[],
                retry_attempt: 0,
                written_at: "2026-01-02T03:04:05Z",
            },
            None,
        );

        assert_eq!(payload["current_node"], "wait-for-child");
        assert_eq!(payload["retry_attempt"], 0);
        assert_eq!(
            payload[follow_keys::PENDING_FOLLOW],
            json!({
                "follow_node": "wait-for-child",
                "step_count": 7,
                "graph_run_id": "run-2",
            })
        );
        assert!(payload[follow_keys::PENDING_FOLLOW]
            .get("child_thread_id")
            .is_none());
    }
}
