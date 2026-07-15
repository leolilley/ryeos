//! Fail-closed graph checkpoint resume.
//!
//! rye-expr/1 checkpoints pin the exact signed graph definition and language
//! that produced their state. Event replay cannot reconstruct that state or
//! prove the definition identity, so it is not a resume source after the clean
//! expression-language cutover.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::GraphDefinition;

pub const RESTART_REQUIRED: &str = "restart_required_after_expression_language_cutover";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingFollow {
    pub follow_node: String,
    pub step_count: u32,
    pub graph_run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration_snapshot: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeState {
    pub definition_ref: String,
    pub definition_hash: String,
    pub expression_language: String,
    pub current_node: String,
    pub step_count: u32,
    pub state: Value,
    pub graph_run_id: String,
    /// Accounting snapshot validated against the walker's closed accounting
    /// type before this DTO is accepted.
    pub accounting: Value,
    /// Suppressed-error history validated as `Vec<ErrorRecord>` before this DTO
    /// is accepted.
    pub suppressed_errors: Value,
    /// Local pending-follow facts recorded at suspend. Child identity remains
    /// daemon-owned and is not stored here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_follow: Option<PendingFollow>,
    /// Canonical terminal child envelope spliced by the follow-resume launcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_result: Option<Value>,
    /// Per-step retry attempts already spent on `current_node`.
    pub retry_attempt: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointPayload {
    schema_version: u32,
    definition_ref: String,
    definition_hash: String,
    expression_language: String,
    current_node: String,
    step_count: u32,
    state: Value,
    graph_run_id: String,
    accounting: Value,
    suppressed_errors: Value,
    #[serde(default)]
    pending_follow: Option<PendingFollow>,
    #[serde(default)]
    follow_result: Option<Value>,
    retry_attempt: u32,
    written_at: String,
}

fn restart_required(detail: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{RESTART_REQUIRED}: {detail}; start a new graph run")
}

fn require_non_empty(value: &str, field: &str) -> Result<()> {
    if value.is_empty() {
        return Err(restart_required(format!(
            "resume state missing non-empty `{field}`"
        )));
    }
    Ok(())
}

fn reject_explicit_null(value: &Value, field: &str) -> Result<()> {
    if value.get(field).is_some_and(Value::is_null) {
        return Err(restart_required(format!(
            "resume state `{field}` must be omitted rather than null"
        )));
    }
    if field == crate::walker::follow_keys::PENDING_FOLLOW
        && value
            .get(field)
            .and_then(Value::as_object)
            .and_then(|pending| pending.get("iteration_snapshot"))
            .is_some_and(Value::is_null)
    {
        return Err(restart_required(
            "resume state `pending_follow.iteration_snapshot` must be omitted rather than null",
        ));
    }
    Ok(())
}

impl ResumeState {
    fn validate(&self, definition: &GraphDefinition) -> Result<()> {
        require_non_empty(&self.definition_ref, "definition_ref")?;
        require_non_empty(&self.definition_hash, "definition_hash")?;
        require_non_empty(&self.expression_language, "expression_language")?;
        require_non_empty(&self.current_node, "current_node")?;
        require_non_empty(&self.graph_run_id, "graph_run_id")?;

        if self.expression_language != crate::walker::EXPRESSION_LANGUAGE {
            return Err(restart_required(format!(
                "resume expression language `{}` does not match `{}`",
                self.expression_language,
                crate::walker::EXPRESSION_LANGUAGE
            )));
        }
        if self.definition_ref != definition.definition_ref {
            return Err(restart_required(format!(
                "resume definition_ref `{}` does not match resolved `{}`",
                self.definition_ref, definition.definition_ref
            )));
        }
        if self.definition_hash != definition.definition_hash {
            return Err(restart_required(format!(
                "resume definition hash `{}` does not match resolved `{}`",
                self.definition_hash, definition.definition_hash
            )));
        }

        let Some(current_node_definition) = definition.config.nodes.get(&self.current_node) else {
            return Err(restart_required(format!(
                "resume current_node `{}` does not exist in resolved graph `{}`",
                self.current_node, definition.definition_ref
            )));
        };
        if self.step_count > definition.config.max_steps {
            return Err(restart_required(format!(
                "resume step_count {} exceeds graph max_steps {}",
                self.step_count, definition.config.max_steps
            )));
        }
        if self.retry_attempt > self.step_count {
            return Err(restart_required(format!(
                "resume retry_attempt {} exceeds completed step_count {}",
                self.retry_attempt, self.step_count
            )));
        }
        match &current_node_definition.retry {
            None if self.retry_attempt != 0 => {
                return Err(restart_required(format!(
                    "resume retry_attempt {} is invalid for node `{}` without retry policy",
                    self.retry_attempt, self.current_node
                )));
            }
            Some(retry) if self.retry_attempt >= retry.attempts => {
                return Err(restart_required(format!(
                    "resume retry_attempt {} must be below configured attempts {} for node `{}`",
                    self.retry_attempt, retry.attempts, self.current_node
                )));
            }
            _ => {}
        }
        if !self.state.is_object() {
            return Err(restart_required(format!(
                "resume `state` must be a JSON object, received {}",
                json_type(&self.state)
            )));
        }
        crate::evaluation::validate_runtime_shape(&self.state, "resume state")
            .map_err(restart_required)?;
        crate::walker::validate_checkpoint_snapshots(
            &self.accounting,
            &self.suppressed_errors,
            self.step_count,
            definition,
        )
        .map_err(restart_required)?;

        let Some(pending) = &self.pending_follow else {
            if self.follow_result.is_some() {
                return Err(restart_required(
                    "resume contains `follow_result` without a valid `pending_follow` marker",
                ));
            }
            return Ok(());
        };

        require_non_empty(&pending.follow_node, "pending_follow.follow_node")?;
        require_non_empty(&pending.graph_run_id, "pending_follow.graph_run_id")?;
        if pending.follow_node != self.current_node {
            return Err(restart_required(format!(
                "resume pending_follow follow_node `{}` does not match current_node `{}`",
                pending.follow_node, self.current_node
            )));
        }
        if pending.step_count != self.step_count {
            return Err(restart_required(format!(
                "resume pending_follow step_count {} does not match top-level {}",
                pending.step_count, self.step_count
            )));
        }
        if pending.graph_run_id != self.graph_run_id {
            return Err(restart_required(format!(
                "resume pending_follow graph_run_id `{}` does not match top-level `{}`",
                pending.graph_run_id, self.graph_run_id
            )));
        }
        if !current_node_definition.follow {
            return Err(restart_required(format!(
                "resume pending_follow targets `{}`, but that node is not a follow node",
                self.current_node
            )));
        }
        match (
            current_node_definition.over.is_some(),
            pending.iteration_snapshot.as_ref(),
        ) {
            (true, None) => {
                return Err(restart_required(format!(
                    "follow-fanout resume for `{}` requires `iteration_snapshot`",
                    self.current_node
                )));
            }
            (false, Some(_)) => {
                return Err(restart_required(format!(
                    "single-follow resume for `{}` must not contain `iteration_snapshot`",
                    self.current_node
                )));
            }
            _ => {}
        }
        if let Some(snapshot) = &pending.iteration_snapshot {
            crate::evaluation::validate_runtime_array_shape(
                snapshot,
                "resume pending_follow iteration_snapshot",
            )
            .map_err(restart_required)?;
            if snapshot.is_empty() {
                return Err(restart_required(
                    "resume pending_follow iteration_snapshot must contain at least one item",
                ));
            }
        }
        if let Some(result) = &self.follow_result {
            crate::evaluation::validate_runtime_shape(result, "resume follow_result")
                .map_err(restart_required)?;
            if let Some(snapshot) = pending.iteration_snapshot.as_ref() {
                crate::dispatch::classify_follow_fanout_envelope(result.clone(), snapshot.len())
                    .map_err(restart_required)?;
            } else {
                crate::dispatch::classify_follow_envelope(result.clone())
                    .map_err(restart_required)?;
            }
        }
        Ok(())
    }
}

/// Parse and verify the exact identity-bearing DTO injected into the walker.
/// Any present value is either a complete, valid resume or a terminal
/// preflight error; callers must never reinterpret failure as a cold start.
pub(crate) fn from_injected_value(
    value: &Value,
    definition: &GraphDefinition,
) -> Result<ResumeState> {
    crate::evaluation::validate_runtime_shape(value, "graph resume_state")
        .map_err(restart_required)?;
    reject_explicit_null(value, crate::walker::follow_keys::PENDING_FOLLOW)?;
    reject_explicit_null(value, crate::walker::follow_keys::FOLLOW_RESULT)?;
    let resume: ResumeState = serde_json::from_value(value.clone())
        .map_err(|error| restart_required(format!("invalid resume state: {error}")))?;
    resume.validate(definition)?;
    Ok(resume)
}

fn checkpoint_to_resume(payload: CheckpointPayload) -> Result<ResumeState> {
    if payload.schema_version != crate::walker::GRAPH_CHECKPOINT_SCHEMA_VERSION {
        return Err(restart_required(format!(
            "checkpoint schema {} does not match required schema {}",
            payload.schema_version,
            crate::walker::GRAPH_CHECKPOINT_SCHEMA_VERSION
        )));
    }
    require_non_empty(&payload.written_at, "written_at")?;
    Ok(ResumeState {
        definition_ref: payload.definition_ref,
        definition_hash: payload.definition_hash,
        expression_language: payload.expression_language,
        current_node: payload.current_node,
        step_count: payload.step_count,
        state: payload.state,
        graph_run_id: payload.graph_run_id,
        accounting: payload.accounting,
        suppressed_errors: payload.suppressed_errors,
        pending_follow: payload.pending_follow,
        follow_result: payload.follow_result,
        retry_attempt: payload.retry_attempt,
    })
}

fn reject_checkpoint_nulls(value: &Value) -> Result<()> {
    for field in [
        crate::walker::follow_keys::PENDING_FOLLOW,
        crate::walker::follow_keys::FOLLOW_RESULT,
    ] {
        if value.get(field).is_some_and(Value::is_null) {
            return Err(restart_required(format!(
                "checkpoint `{field}` must be omitted rather than null"
            )));
        }
    }
    if value
        .get(crate::walker::follow_keys::PENDING_FOLLOW)
        .and_then(Value::as_object)
        .and_then(|pending| pending.get("iteration_snapshot"))
        .is_some_and(Value::is_null)
    {
        return Err(restart_required(
            "checkpoint `pending_follow.iteration_snapshot` must be omitted rather than null",
        ));
    }
    Ok(())
}

/// Parse and verify a schema-v3 checkpoint against the exact graph definition
/// resolved for this launch. No older shape or alternate expression marker is
/// accepted.
pub fn from_checkpoint_value(value: &Value, definition: &GraphDefinition) -> Result<ResumeState> {
    crate::evaluation::validate_runtime_shape(value, "graph checkpoint")
        .map_err(restart_required)?;
    reject_checkpoint_nulls(value)?;
    let payload: CheckpointPayload = serde_json::from_value(value.clone())
        .map_err(|error| restart_required(format!("invalid checkpoint payload: {error}")))?;
    let resume = checkpoint_to_resume(payload)?;
    resume.validate(definition)?;
    Ok(resume)
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResumeSource {
    ColdStart,
    LocalCheckpoint,
    /// Resume was explicitly requested but no identity-bearing local
    /// checkpoint exists. Event replay is intentionally not a fallback.
    NoSourceAvailable,
}

pub fn decide_resume_source(
    resume_requested: bool,
    local_checkpoint_present: bool,
) -> ResumeSource {
    if !resume_requested {
        ResumeSource::ColdStart
    } else if local_checkpoint_present {
        ResumeSource::LocalCheckpoint
    } else {
        ResumeSource::NoSourceAvailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definition() -> GraphDefinition {
        GraphDefinition::from_yaml(
            r#"
version: "1"
category: test
config:
  start: wait
  nodes:
    wait:
      follow: true
      over: "${state.jobs}"
      as: job
      parallel: true
      action: {item_id: "directive:test/child", params: {job: "${job}"}}
      collect: results
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
            Some("example.yaml"),
        )
        .unwrap()
    }

    fn single_follow_definition() -> GraphDefinition {
        GraphDefinition::from_yaml(
            r#"
version: "1"
category: test
config:
  start: wait
  nodes:
    wait:
      follow: true
      action: {item_id: "directive:test/child"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
            Some("single-follow.yaml"),
        )
        .unwrap()
    }

    fn checkpoint(definition: &GraphDefinition) -> Value {
        json!({
            "schema_version": crate::walker::GRAPH_CHECKPOINT_SCHEMA_VERSION,
            "definition_ref": definition.definition_ref,
            "definition_hash": definition.definition_hash,
            "expression_language": crate::walker::EXPRESSION_LANGUAGE,
            "graph_run_id": "run-1",
            "current_node": "done",
            "step_count": 4,
            "state": {"answer": 42},
            "accounting": {"total": null, "nodes": [], "hooks": []},
            "suppressed_errors": [],
            "retry_attempt": 0,
            "written_at": "2026-01-02T03:04:05Z"
        })
    }

    fn injected(definition: &GraphDefinition) -> Value {
        serde_json::to_value(from_checkpoint_value(&checkpoint(definition), definition).unwrap())
            .unwrap()
    }

    fn follow_terminal(status: ryeos_runtime::envelope::RuntimeResultStatus) -> Value {
        let result = if status.is_success() {
            json!({"ok": true})
        } else {
            json!({"error": "child failed"})
        };
        json!({
            "success": status.is_success(),
            "status": status,
            "result": result,
            "outputs": null,
            "warnings": [],
            "cost": null,
        })
    }

    #[test]
    fn parses_identity_pinned_checkpoint() {
        let definition = definition();
        let state = from_checkpoint_value(&checkpoint(&definition), &definition).unwrap();
        assert_eq!(state.definition_ref, definition.definition_ref);
        assert_eq!(state.definition_hash, definition.definition_hash);
        assert_eq!(state.expression_language, "rye-expr/1");
        assert_eq!(state.current_node, "done");
        assert_eq!(state.state, json!({"answer": 42}));
    }

    #[test]
    fn rejects_old_or_missing_schema_with_restart_diagnostic() {
        let definition = definition();
        for value in [
            json!({}),
            json!({"schema_version": 2}),
            json!({"schema_version": 99}),
        ] {
            let error = from_checkpoint_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED));
        }
    }

    #[test]
    fn rejects_definition_or_language_mismatch() {
        let definition = definition();
        for (key, replacement) in [
            ("definition_ref", json!("graph:other/item")),
            ("definition_hash", json!("sha256:other")),
            ("expression_language", json!("rye-expr/2")),
        ] {
            let mut value = checkpoint(&definition);
            value[key] = replacement;
            let error = from_checkpoint_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED));
        }
    }

    #[test]
    fn rejects_missing_or_corrupt_resume_history() {
        let definition = definition();
        for (key, replacement) in [
            (
                "accounting",
                json!({"total": "not-a-cost", "nodes": [], "hooks": []}),
            ),
            ("suppressed_errors", json!({"not": "an array"})),
        ] {
            let mut value = checkpoint(&definition);
            value[key] = replacement;
            let error = from_checkpoint_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED));
        }

        for key in ["accounting", "suppressed_errors", "retry_attempt"] {
            let mut value = checkpoint(&definition);
            value.as_object_mut().unwrap().remove(key);
            let error = from_checkpoint_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED));
        }
    }

    #[test]
    fn rejects_non_object_state_and_unknown_current_node() {
        let definition = definition();
        for (key, replacement, expected) in [
            ("state", json!([1, 2, 3]), "state` must be a JSON object"),
            (
                "current_node",
                json!("removed-node"),
                "does not exist in resolved graph",
            ),
        ] {
            let mut value = checkpoint(&definition);
            value[key] = replacement;
            let error = from_checkpoint_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn validates_pending_follow_against_top_level_cursor() {
        let definition = definition();
        let valid = json!({
            "follow_node": "wait",
            "step_count": 4,
            "graph_run_id": "run-1",
            "iteration_snapshot": ["a", "b"],
        });
        let mut value = checkpoint(&definition);
        value["current_node"] = json!("wait");
        value["state"] = json!({"jobs": ["a", "b"]});
        value[crate::walker::follow_keys::PENDING_FOLLOW] = valid.clone();
        let follow_result = json!({
            "fanout": true,
            "expected": 2,
            "failed": 1,
            "statuses": [
                crate::model::FanoutItemStatus::Completed,
                crate::model::FanoutItemStatus::Failed,
            ],
            "items": [
                follow_terminal(ryeos_runtime::envelope::RuntimeResultStatus::Completed),
                follow_terminal(ryeos_runtime::envelope::RuntimeResultStatus::Failed),
            ],
        });
        value[crate::walker::follow_keys::FOLLOW_RESULT] = follow_result.clone();
        let parsed = from_checkpoint_value(&value, &definition).unwrap();
        assert_eq!(
            parsed.pending_follow,
            Some(PendingFollow {
                follow_node: "wait".to_string(),
                step_count: 4,
                graph_run_id: "run-1".to_string(),
                iteration_snapshot: Some(vec![json!("a"), json!("b")]),
            })
        );
        assert_eq!(parsed.follow_result, Some(follow_result));

        for (pending, expected) in [
            (json!(null), "must be omitted rather than null"),
            (json!({}), "missing field `follow_node`"),
            (
                json!({
                    "follow_node": "",
                    "step_count": 4,
                    "graph_run_id": "run-1",
                }),
                "missing non-empty `follow_node`",
            ),
            (
                json!({
                    "follow_node": "other",
                    "step_count": 4,
                    "graph_run_id": "run-1",
                }),
                "does not match current_node",
            ),
            (
                json!({
                    "follow_node": "wait",
                    "graph_run_id": "run-1",
                }),
                "missing field `step_count`",
            ),
            (
                json!({
                    "follow_node": "wait",
                    "step_count": 5,
                    "graph_run_id": "run-1",
                }),
                "does not match top-level",
            ),
            (
                json!({
                    "follow_node": "wait",
                    "step_count": 4,
                    "graph_run_id": "",
                }),
                "missing non-empty `graph_run_id`",
            ),
            (
                json!({
                    "follow_node": "wait",
                    "step_count": 4,
                    "graph_run_id": "other-run",
                }),
                "does not match top-level",
            ),
            (
                json!({
                    "follow_node": "wait",
                    "step_count": 4,
                    "graph_run_id": "run-1",
                    "iteration_snapshot": null,
                }),
                "iteration_snapshot` must be omitted rather than null",
            ),
        ] {
            let mut value = checkpoint(&definition);
            value["current_node"] = json!("wait");
            value[crate::walker::follow_keys::PENDING_FOLLOW] = pending;
            let error = from_checkpoint_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn rejects_pending_follow_on_a_non_follow_node() {
        let definition = definition();
        let mut value = checkpoint(&definition);
        value[crate::walker::follow_keys::PENDING_FOLLOW] = json!({
            "follow_node": "done",
            "step_count": 4,
            "graph_run_id": "run-1",
        });
        let error = from_checkpoint_value(&value, &definition)
            .unwrap_err()
            .to_string();
        assert!(error.contains(RESTART_REQUIRED), "{error}");
        assert!(error.contains("not a follow node"), "{error}");
    }

    #[test]
    fn rejects_follow_result_without_valid_pending_marker() {
        let definition = definition();
        let mut value = checkpoint(&definition);
        value[crate::walker::follow_keys::FOLLOW_RESULT] = json!({"result": "orphaned"});
        let error = from_checkpoint_value(&value, &definition)
            .unwrap_err()
            .to_string();
        assert!(error.contains(RESTART_REQUIRED), "{error}");
        assert!(
            error.contains("without a valid `pending_follow`"),
            "{error}"
        );
    }

    #[test]
    fn follow_result_must_match_the_exact_single_or_cohort_contract() {
        let single = single_follow_definition();
        let mut canonical_failure = checkpoint(&single);
        canonical_failure["current_node"] = json!("wait");
        canonical_failure[crate::walker::follow_keys::PENDING_FOLLOW] = json!({
            "follow_node": "wait",
            "step_count": 4,
            "graph_run_id": "run-1",
        });
        canonical_failure[crate::walker::follow_keys::FOLLOW_RESULT] =
            follow_terminal(ryeos_runtime::envelope::RuntimeResultStatus::Failed);
        from_checkpoint_value(&canonical_failure, &single).unwrap();

        let mut malformed_single = canonical_failure;
        malformed_single[crate::walker::follow_keys::FOLLOW_RESULT] =
            json!({"result": {"ok": true}});
        let error = from_checkpoint_value(&malformed_single, &single)
            .unwrap_err()
            .to_string();
        assert!(error.contains(RESTART_REQUIRED), "{error}");
        assert!(
            error.contains("malformed follow result envelope"),
            "{error}"
        );

        let fanout = definition();
        let mut malformed_cohort = checkpoint(&fanout);
        malformed_cohort["current_node"] = json!("wait");
        malformed_cohort[crate::walker::follow_keys::PENDING_FOLLOW] = json!({
            "follow_node": "wait",
            "step_count": 4,
            "graph_run_id": "run-1",
            "iteration_snapshot": ["a"],
        });
        malformed_cohort[crate::walker::follow_keys::FOLLOW_RESULT] = json!({
            "fanout": true,
            "expected": 2,
            "failed": 0,
            "statuses": ["completed"],
            "items": [follow_terminal(ryeos_runtime::envelope::RuntimeResultStatus::Completed)],
        });
        let error = from_checkpoint_value(&malformed_cohort, &fanout)
            .unwrap_err()
            .to_string();
        assert!(error.contains(RESTART_REQUIRED), "{error}");
        assert!(error.contains("cardinality"), "{error}");

        let mut overflow_cohort = checkpoint(&fanout);
        overflow_cohort["current_node"] = json!("wait");
        overflow_cohort[crate::walker::follow_keys::PENDING_FOLLOW] = json!({
            "follow_node": "wait",
            "step_count": 4,
            "graph_run_id": "run-1",
            "iteration_snapshot": ["a", "b"],
        });
        let mut first = follow_terminal(ryeos_runtime::envelope::RuntimeResultStatus::Completed);
        first["cost"] = json!({
            "input_tokens": i64::MAX as u64,
            "output_tokens": 0,
            "total_usd": 0.0,
        });
        let mut second = follow_terminal(ryeos_runtime::envelope::RuntimeResultStatus::Completed);
        second["cost"] = json!({
            "input_tokens": 1,
            "output_tokens": 0,
            "total_usd": 0.0,
        });
        overflow_cohort[crate::walker::follow_keys::FOLLOW_RESULT] = json!({
            "fanout": true,
            "expected": 2,
            "failed": 0,
            "statuses": ["completed", "completed"],
            "items": [first, second],
        });
        let error = from_checkpoint_value(&overflow_cohort, &fanout)
            .unwrap_err()
            .to_string();
        assert!(error.contains(RESTART_REQUIRED), "{error}");
        assert!(error.contains("aggregate cost"), "{error}");
    }

    #[test]
    fn follow_snapshot_presence_matches_follow_shape_exactly() {
        let fanout = definition();
        let mut missing_snapshot = checkpoint(&fanout);
        missing_snapshot["current_node"] = json!("wait");
        missing_snapshot[crate::walker::follow_keys::PENDING_FOLLOW] = json!({
            "follow_node": "wait",
            "step_count": 4,
            "graph_run_id": "run-1",
        });
        let error = from_checkpoint_value(&missing_snapshot, &fanout)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires `iteration_snapshot`"), "{error}");

        let single = single_follow_definition();
        let mut unexpected_snapshot = checkpoint(&single);
        unexpected_snapshot["current_node"] = json!("wait");
        unexpected_snapshot[crate::walker::follow_keys::PENDING_FOLLOW] = json!({
            "follow_node": "wait",
            "step_count": 4,
            "graph_run_id": "run-1",
            "iteration_snapshot": [],
        });
        let error = from_checkpoint_value(&unexpected_snapshot, &single)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("must not contain `iteration_snapshot`"),
            "{error}"
        );
    }

    #[test]
    fn injected_resume_rejects_partial_unknown_and_out_of_range_fields() {
        let definition = definition();
        let valid = injected(&definition);
        from_injected_value(&valid, &definition).unwrap();

        let mut cases = Vec::new();

        let mut missing_identity = valid.clone();
        missing_identity
            .as_object_mut()
            .unwrap()
            .remove("definition_hash");
        cases.push(missing_identity);

        let mut unknown = valid.clone();
        unknown["legacy_cursor"] = json!(true);
        cases.push(unknown);

        let mut oversized_step = valid.clone();
        oversized_step["step_count"] = json!(u64::from(u32::MAX) + 1);
        cases.push(oversized_step);

        let mut negative_retry = valid;
        negative_retry["retry_attempt"] = json!(-1);
        cases.push(negative_retry);

        for value in cases {
            let error = from_injected_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED), "{error}");
        }
    }

    #[test]
    fn injected_resume_rejects_identity_drift_and_corrupt_history() {
        let definition = definition();
        let valid = injected(&definition);
        for (path, replacement) in [
            ("definition_ref", json!("graph:test/other")),
            ("definition_hash", json!("sha256:other")),
            ("expression_language", json!("rye-expr/2")),
            ("current_node", json!("")),
            ("graph_run_id", json!("")),
            (
                "step_count",
                json!(definition.config.max_steps.saturating_add(1)),
            ),
            ("retry_attempt", json!(1)),
            ("state", json!([])),
            (
                "accounting",
                json!({"total": null, "nodes": [], "hooks": [], "extra": true}),
            ),
            (
                "suppressed_errors",
                json!([{"step": -1, "node": "x", "error": "bad"}]),
            ),
        ] {
            let mut value = valid.clone();
            value[path] = replacement;
            let error = from_injected_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED), "{path}: {error}");
        }
    }

    #[test]
    fn checkpoint_rejects_unknown_top_level_and_pending_fields() {
        let definition = definition();
        let mut top_level = checkpoint(&definition);
        top_level["legacy_cursor"] = json!("done");
        let error = from_checkpoint_value(&top_level, &definition)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `legacy_cursor`"), "{error}");

        let mut pending = checkpoint(&definition);
        pending["current_node"] = json!("wait");
        pending[crate::walker::follow_keys::PENDING_FOLLOW] = json!({
            "follow_node": "wait",
            "step_count": 4,
            "graph_run_id": "run-1",
            "iteration_snapshot": [],
            "child_thread_id": "legacy-child",
        });
        let error = from_checkpoint_value(&pending, &definition)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `child_thread_id`"), "{error}");
    }

    #[test]
    fn retry_cursor_must_leave_a_configured_attempt_available() {
        let definition = GraphDefinition::from_yaml(
            r#"
version: "1"
category: test
config:
  start: work
  nodes:
    work:
      action: {item_id: "tool:test/work"}
      retry: {attempts: 2, backoff_ms: 1}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
            Some("retry.yaml"),
        )
        .unwrap();
        let mut value = checkpoint(&definition);
        value["current_node"] = json!("work");
        value["retry_attempt"] = json!(2);

        let error = from_checkpoint_value(&value, &definition)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("must be below configured attempts 2"),
            "{error}"
        );
    }

    #[test]
    fn retry_cursor_cannot_claim_more_attempts_than_completed_steps() {
        let definition = GraphDefinition::from_yaml(
            r#"
version: "1"
category: test
config:
  start: work
  nodes:
    work:
      action: {item_id: "tool:test/work"}
      retry: {attempts: 2, backoff_ms: 1}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
            Some("retry-step-count.yaml"),
        )
        .unwrap();
        let mut value = checkpoint(&definition);
        value["current_node"] = json!("work");
        value["step_count"] = json!(0);
        value["retry_attempt"] = json!(1);

        let error = from_checkpoint_value(&value, &definition)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds completed step_count 0"), "{error}");
    }

    #[test]
    fn checkpoint_history_must_match_rollup_and_precede_cursor() {
        let definition = definition();
        let node = json!({
            "node": "wait",
            "step": 1,
            "item_id": "directive:test/work",
            "cost": {
                "input_tokens": 3,
                "output_tokens": 5,
                "total_usd": 0.25
            }
        });
        let total = json!({
            "input_tokens": 3,
            "output_tokens": 5,
            "total_usd": 0.25,
            "basis": ryeos_runtime::envelope::COST_BASIS_ROLLUP
        });

        let mut valid = checkpoint(&definition);
        valid["accounting"] = json!({"total": total, "nodes": [node.clone()], "hooks": []});
        from_checkpoint_value(&valid, &definition).unwrap();

        let mut missing_total = checkpoint(&definition);
        missing_total["accounting"] = json!({"total": null, "nodes": [node], "hooks": []});

        let mut contradictory_total = valid.clone();
        contradictory_total["accounting"]["total"]["input_tokens"] = json!(4);

        let mut future_cost = valid.clone();
        future_cost["accounting"]["nodes"][0]["step"] = json!(4);

        let mut duplicate_cost_step = valid.clone();
        duplicate_cost_step["accounting"]["nodes"] = json!([node.clone(), node.clone()]);
        duplicate_cost_step["accounting"]["total"] = json!({
            "input_tokens": 6,
            "output_tokens": 10,
            "total_usd": 0.5,
            "basis": ryeos_runtime::envelope::COST_BASIS_ROLLUP
        });

        let mut unknown_cost_node = valid.clone();
        unknown_cost_node["accounting"]["nodes"][0]["node"] = json!("removed");

        let mut negative_cost = valid.clone();
        negative_cost["accounting"]["nodes"][0]["cost"]["total_usd"] = json!(-0.25);

        let mut future_error = checkpoint(&definition);
        future_error["suppressed_errors"] = json!([{"step": 4, "node": "wait", "error": "future"}]);

        let mut reversed_errors = checkpoint(&definition);
        reversed_errors["suppressed_errors"] = json!([
            {"step": 2, "node": "wait", "error": "later"},
            {"step": 1, "node": "wait", "error": "earlier"}
        ]);

        let mut unknown_error_node = checkpoint(&definition);
        unknown_error_node["suppressed_errors"] =
            json!([{"step": 1, "node": "removed", "error": "unknown"}]);

        for value in [
            missing_total,
            contradictory_total,
            future_cost,
            duplicate_cost_step,
            unknown_cost_node,
            negative_cost,
            future_error,
            reversed_errors,
            unknown_error_node,
        ] {
            let error = from_checkpoint_value(&value, &definition)
                .unwrap_err()
                .to_string();
            assert!(error.contains(RESTART_REQUIRED), "{error}");
        }
    }

    #[test]
    fn resume_requires_local_checkpoint() {
        assert_eq!(decide_resume_source(false, false), ResumeSource::ColdStart);
        assert_eq!(
            decide_resume_source(true, true),
            ResumeSource::LocalCheckpoint
        );
        assert_eq!(
            decide_resume_source(true, false),
            ResumeSource::NoSourceAvailable
        );
    }
}
