use serde_json::{json, Value};

use crate::evaluation::validate_runtime_shape;
use crate::model::ErrorRecord;
use ryeos_runtime::checkpoint::CheckpointWriter;

use super::{follow_keys, Walker, EXPRESSION_LANGUAGE, GRAPH_CHECKPOINT_SCHEMA_VERSION};

struct CheckpointCursor<'a> {
    definition_ref: &'a str,
    definition_hash: &'a str,
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
        definition_ref,
        definition_hash,
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
        "definition_ref": definition_ref,
        "definition_hash": definition_hash,
        "expression_language": EXPRESSION_LANGUAGE,
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
    item_refs: &[String],
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
    pending.insert("item_refs".to_string(), json!(item_refs));
    if let Some(items) = iteration_snapshot {
        pending.insert("iteration_snapshot".to_string(), json!(items));
    }
    payload[follow_keys::PENDING_FOLLOW] = Value::Object(pending);
    payload
}

/// Enforce one aggregate rye-expr/1 envelope limit across state, accounting,
/// suppressed errors, and any pending-follow snapshot before persistence. Each
/// constituent is bounded as it is produced; this final borrowed pass catches
/// a checkpoint whose individually valid parts are too large in combination.
fn validate_checkpoint_payload(payload: &Value) -> anyhow::Result<()> {
    validate_runtime_shape(payload, "graph checkpoint payload").map_err(|error| {
        anyhow::anyhow!("graph checkpoint payload exceeded rye-expr/1 bounds: {error}")
    })
}

fn validate_follow_item_refs(
    item_refs: &[String],
    iteration_snapshot: Option<&[Value]>,
) -> anyhow::Result<()> {
    let expected = iteration_snapshot.map_or(1, <[Value]>::len);
    if item_refs.len() != expected || expected == 0 {
        anyhow::bail!(
            "pending follow item_refs cardinality {} does not match required {expected}",
            item_refs.len()
        );
    }
    for (index, item_ref) in item_refs.iter().enumerate() {
        ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).map_err(|error| {
            anyhow::anyhow!("pending follow item_refs[{index}] is not canonical: {error}")
        })?;
    }
    Ok(())
}

/// Ordered child identity plus the optional fanout snapshot written into one
/// pending-follow checkpoint. Keeping these coupled prevents call sites from
/// supplying a cohort snapshot without the matching rendered refs (or vice
/// versa), while keeping the checkpoint writer below Clippy's argument limit.
pub(super) struct FollowCheckpointChildren<'a> {
    item_refs: &'a [String],
    iteration_snapshot: Option<&'a [Value]>,
}

impl<'a> FollowCheckpointChildren<'a> {
    pub(super) fn single(item_refs: &'a [String]) -> Self {
        Self {
            item_refs,
            iteration_snapshot: None,
        }
    }

    pub(super) fn fanout(item_refs: &'a [String], iteration_snapshot: &'a [Value]) -> Self {
        Self {
            item_refs,
            iteration_snapshot: Some(iteration_snapshot),
        }
    }
}

impl Walker {
    /// Fail checkpoint persistence deterministically after `successful_writes`
    /// writes. The seam exists only in graph-runtime unit-test builds and is
    /// deliberately below every checkpoint payload constructor so ordinary and
    /// pending-follow writes exercise the same failure boundary.
    #[cfg(test)]
    pub(super) fn fail_checkpoint_writes_after(&self, successful_writes: usize) {
        *self.checkpoint_writes_before_failure.lock().unwrap() = Some(successful_writes);
    }

    /// Crash deterministically after `successful_writes` additional atomic
    /// checkpoint replacements and before the completed commit can advance.
    /// A panic is intentional here: unlike a rejected write, this models an
    /// abrupt process loss after durable authority has already moved.
    #[cfg(test)]
    pub(super) fn crash_after_checkpoint_writes(&self, successful_writes: usize) {
        *self.checkpoint_writes_before_crash.lock().unwrap() = Some(successful_writes);
    }

    #[cfg(test)]
    fn inject_checkpoint_write_failure(&self) -> anyhow::Result<()> {
        let mut writes_before_failure = self.checkpoint_writes_before_failure.lock().unwrap();
        let Some(remaining) = writes_before_failure.as_mut() else {
            return Ok(());
        };
        if *remaining == 0 {
            anyhow::bail!("injected checkpoint persistence failure");
        }
        *remaining -= 1;
        Ok(())
    }

    #[cfg(test)]
    fn inject_post_checkpoint_crash(&self) {
        let should_crash = {
            let mut writes_before_crash = self.checkpoint_writes_before_crash.lock().unwrap();
            let Some(remaining) = writes_before_crash.as_mut() else {
                return;
            };
            if *remaining == 0 {
                *writes_before_crash = None;
                true
            } else {
                *remaining -= 1;
                false
            }
        };
        assert!(!should_crash, "injected crash after checkpoint persistence");
    }

    /// Write a checkpoint marking a follow suspend. The cursor points at the
    /// follow node itself so re-entry can idempotently re-drive the handoff.
    pub(super) async fn write_follow_checkpoint(
        &self,
        graph_run_id: &str,
        follow_node: &str,
        step: u32,
        state: &Value,
        suppressed_errors: &[ErrorRecord],
        children: FollowCheckpointChildren<'_>,
    ) -> anyhow::Result<()> {
        let FollowCheckpointChildren {
            item_refs,
            iteration_snapshot,
        } = children;
        self.ensure_run_history_bounded()?;
        validate_follow_item_refs(item_refs, iteration_snapshot)?;
        let Some(writer) = &self.checkpoint else {
            return Ok(());
        };
        let accounting = {
            let acc = self.accounting.lock().unwrap();
            serde_json::to_value(&*acc)
                .map_err(|error| anyhow::anyhow!("serialize graph accounting: {error}"))?
        };
        let payload = follow_checkpoint_payload(
            CheckpointCursor {
                definition_ref: &self.graph.definition_ref,
                definition_hash: &self.graph.definition_hash,
                graph_run_id,
                next_node: follow_node,
                next_step: step,
                state,
                accounting,
                suppressed_errors,
                retry_attempt: 0,
                written_at: &lillux::time::iso8601_now(),
            },
            item_refs,
            iteration_snapshot,
        );
        validate_checkpoint_payload(&payload)?;
        #[cfg(test)]
        self.inject_checkpoint_write_failure()?;
        writer.write(&payload)?;
        #[cfg(test)]
        self.inject_post_checkpoint_crash();
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
        self.ensure_run_history_bounded()?;
        let Some(writer) = &self.checkpoint else {
            return Ok(());
        };
        let accounting = {
            let acc = self.accounting.lock().unwrap();
            serde_json::to_value(&*acc)
                .map_err(|error| anyhow::anyhow!("serialize graph accounting: {error}"))?
        };
        let payload = checkpoint_payload(CheckpointCursor {
            definition_ref: &self.graph.definition_ref,
            definition_hash: &self.graph.definition_hash,
            graph_run_id,
            next_node,
            next_step,
            state,
            accounting,
            suppressed_errors,
            retry_attempt,
            written_at: &lillux::time::iso8601_now(),
        });
        validate_checkpoint_payload(&payload)?;
        #[cfg(test)]
        self.inject_checkpoint_write_failure()?;
        writer.write(&payload)?;
        #[cfg(test)]
        self.inject_post_checkpoint_crash();

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
            definition_ref: "graph:test/example",
            definition_hash: "sha256:test-definition",
            graph_run_id: "run-1",
            next_node: "retrying",
            next_step: 4,
            state: &state,
            accounting: json!({"total": null, "nodes": [], "hooks": []}),
            suppressed_errors: &errors,
            retry_attempt: 2,
            written_at: "2026-01-02T03:04:05Z",
        });

        assert_eq!(payload["schema_version"], GRAPH_CHECKPOINT_SCHEMA_VERSION);
        assert_eq!(payload["definition_ref"], "graph:test/example");
        assert_eq!(payload["definition_hash"], "sha256:test-definition");
        assert_eq!(payload["expression_language"], EXPRESSION_LANGUAGE);
        assert_eq!(payload["current_node"], "retrying");
        assert_eq!(payload["step_count"], 4);
        assert_eq!(payload["retry_attempt"], 2);
        assert_eq!(payload["state"], json!({"answer": 42}));
        assert_eq!(payload["suppressed_errors"][0]["error"], "suppressed");
    }

    #[test]
    fn follow_payload_repoints_cursor_without_child_identity() {
        let state = json!({});
        let item_refs = vec!["directive:test/child".to_string()];
        let payload = follow_checkpoint_payload(
            CheckpointCursor {
                definition_ref: "graph:test/example",
                definition_hash: "sha256:test-definition",
                graph_run_id: "run-2",
                next_node: "wait-for-child",
                next_step: 7,
                state: &state,
                accounting: json!({"total": null, "nodes": [], "hooks": []}),
                suppressed_errors: &[],
                retry_attempt: 0,
                written_at: "2026-01-02T03:04:05Z",
            },
            &item_refs,
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
                "item_refs": ["directive:test/child"],
            })
        );
        assert!(payload[follow_keys::PENDING_FOLLOW]
            .get("child_thread_id")
            .is_none());
    }

    #[test]
    fn checkpoint_rejects_parts_that_exceed_the_combined_payload_budget() {
        let chunk = "x".repeat(700 * 1024);
        let state = json!({
            "parts": [chunk.clone(), chunk.clone(), chunk.clone()],
        });
        let errors = (0..3)
            .map(|step| ErrorRecord {
                step,
                node: "previous".to_string(),
                error: chunk.clone(),
            })
            .collect::<Vec<_>>();
        let error_value = serde_json::to_value(&errors).unwrap();
        assert!(validate_runtime_shape(&state, "test state").is_ok());
        assert!(validate_runtime_shape(&error_value, "test errors").is_ok());

        let payload = checkpoint_payload(CheckpointCursor {
            definition_ref: "graph:test/example",
            definition_hash: "sha256:test-definition",
            graph_run_id: "run-large",
            next_node: "next",
            next_step: 2,
            state: &state,
            accounting: json!({"total": null, "nodes": [], "hooks": []}),
            suppressed_errors: &errors,
            retry_attempt: 0,
            written_at: "2026-01-02T03:04:05Z",
        });

        let error = validate_checkpoint_payload(&payload).unwrap_err();
        assert!(error.to_string().contains("JSON byte limit"));
    }
}
