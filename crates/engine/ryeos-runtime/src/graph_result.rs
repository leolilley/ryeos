//! Canonical graph result wire contract shared by producers and retained-result consumers.
//!
//! This decoder preserves the complete durable result and validates the parent
//! return value; it does not compile or execute graph definitions.

use crate::envelope::RuntimeCost;
use crate::events::RuntimeEventType;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphRunStatus {
    Valid,
    Invalid,
    Completed,
    CompletedWithErrors,
    Continued,
    Error,
    MaxStepsExceeded,
    Cancelled,
    Killed,
}

impl GraphRunStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::Completed => "completed",
            Self::CompletedWithErrors => "completed_with_errors",
            Self::Continued => "continued",
            Self::Error => "error",
            Self::MaxStepsExceeded => "max_steps_exceeded",
            Self::Cancelled => "cancelled",
            Self::Killed => "killed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphResult {
    pub success: bool,
    pub graph_id: String,
    pub definition_ref: String,
    pub effective_definition_digest: String,
    pub graph_run_id: String,
    pub status: GraphRunStatus,
    pub steps: u32,
    pub state: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors_suppressed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<ErrorRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<RuntimeCost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_costs: Vec<NodeCostRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hook_costs: Vec<HookCostRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorRecord {
    pub step: u32,
    pub node: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCostRecord {
    pub node: String,
    pub step: u32,
    pub item_id: String,
    pub cost: RuntimeCost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookCostRecord {
    pub event: RuntimeEventType,
    pub step: Option<u32>,
    pub cost: RuntimeCost,
}

/// Project a graph runtime's complete durable result to the value exposed to
/// an authored parent action. The complete state remains on the child thread;
/// follow transport carries only this return value.
pub fn project_graph_action_result(result: Value, outputs: Value) -> Result<Value, String> {
    if !outputs.is_null() {
        return Err("graph runtime envelope must carry null `outputs`".to_string());
    }

    let graph_result: GraphResult = serde_json::from_value(result)
        .map_err(|error| format!("graph runtime returned malformed GraphResult: {error}"))?;
    let definition_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(
        &graph_result.definition_ref,
    )
    .map_err(|error| {
        format!(
            "graph runtime returned GraphResult with invalid definition_ref `{}`: {error}",
            graph_result.definition_ref
        )
    })?;
    if definition_ref.kind != "graph" {
        return Err(format!(
            "graph runtime returned GraphResult with non-graph definition_ref `{}`",
            graph_result.definition_ref
        ));
    }
    let successful_status = matches!(
        graph_result.status,
        GraphRunStatus::Valid | GraphRunStatus::Completed | GraphRunStatus::CompletedWithErrors
    );
    if !graph_result.success || !successful_status {
        return Err(format!(
            "graph runtime returned success envelope with contradictory GraphResult success={} status=`{}`",
            graph_result.success,
            graph_result.status.as_str()
        ));
    }

    Ok(graph_result.result.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn completed_result() -> Value {
        json!({
            "success": true,
            "graph_id": "test/report",
            "definition_ref": "graph:test/report",
            "effective_definition_digest": "digest",
            "graph_run_id": "run-1",
            "status": "completed",
            "steps": 1,
            "state": {"private_child_state": "not a parent return"},
            "result": {"report": [1, 2, 3]}
        })
    }

    #[test]
    fn parent_receives_only_the_authored_return_for_supported_success_statuses() {
        for status in ["valid", "completed", "completed_with_errors"] {
            let mut result = completed_result();
            result["status"] = json!(status);
            assert_eq!(
                project_graph_action_result(result, Value::Null).unwrap(),
                json!({"report": [1, 2, 3]}),
                "status {status}"
            );
        }
        for returned in [Value::Null, json!(false), json!(0), json!(["a", "b"])] {
            let mut result = completed_result();
            result["result"] = returned.clone();
            assert_eq!(
                project_graph_action_result(result, Value::Null).unwrap(),
                returned
            );
        }
        let mut result = completed_result();
        result.as_object_mut().unwrap().remove("result");
        assert_eq!(
            project_graph_action_result(result, Value::Null).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn contradictory_success_and_nonterminal_statuses_are_not_parent_results() {
        for status in [
            "invalid",
            "continued",
            "error",
            "max_steps_exceeded",
            "cancelled",
            "killed",
        ] {
            let mut result = completed_result();
            result["status"] = json!(status);
            assert!(
                project_graph_action_result(result, Value::Null)
                    .unwrap_err()
                    .contains("contradictory"),
                "status {status}"
            );
        }
        for status in ["valid", "completed", "completed_with_errors"] {
            let mut result = completed_result();
            result["status"] = json!(status);
            result["success"] = json!(false);
            assert!(
                project_graph_action_result(result, Value::Null)
                    .unwrap_err()
                    .contains("contradictory"),
                "status {status}"
            );
        }
    }

    #[test]
    fn projection_requires_a_canonical_graph_reference() {
        for definition_ref in [
            "",
            "test/report",
            "graph:",
            "graph:../report",
            "graph:test//report",
            "graph:test/report@unknown",
        ] {
            let mut result = completed_result();
            result["definition_ref"] = json!(definition_ref);
            assert!(
                project_graph_action_result(result, Value::Null)
                    .unwrap_err()
                    .contains("invalid definition_ref"),
                "ref {definition_ref}"
            );
        }
        let mut result = completed_result();
        result["definition_ref"] = json!("tool:test/report");
        assert!(
            project_graph_action_result(result, Value::Null)
                .unwrap_err()
                .contains("non-graph definition_ref")
        );
    }

    #[test]
    fn projection_refuses_extra_envelope_outputs_and_malformed_results() {
        for outputs in [
            json!({}),
            json!([]),
            json!(false),
            json!(0),
            json!("output"),
        ] {
            assert!(
                project_graph_action_result(completed_result(), outputs)
                    .unwrap_err()
                    .contains("null `outputs`")
            );
        }
        let mut malformed = Vec::new();
        for field in [
            "success",
            "graph_id",
            "definition_ref",
            "effective_definition_digest",
            "graph_run_id",
            "status",
            "steps",
            "state",
        ] {
            let mut result = completed_result();
            result.as_object_mut().unwrap().remove(field);
            malformed.push(result);
        }
        for (field, value) in [
            ("status", json!("unknown")),
            ("steps", json!(-1)),
            ("success", json!("true")),
            ("unexpected", json!(true)),
        ] {
            let mut result = completed_result();
            result[field] = value;
            malformed.push(result);
        }
        malformed.extend([Value::Null, json!([])]);
        for result in malformed {
            assert!(
                project_graph_action_result(result, Value::Null)
                    .unwrap_err()
                    .contains("malformed GraphResult")
            );
        }
    }

    #[test]
    fn nested_records_keep_strict_decoding_and_optional_collections_round_trip() {
        let cost = json!({"input_tokens": 1, "output_tokens": 2, "total_usd": "0.000000003"});
        let records = [
            (
                "errors",
                json!({"step": 1, "node": "report", "error": "suppressed"}),
            ),
            (
                "node_costs",
                json!({"node": "report", "step": 1, "item_id": "tool:test/report", "cost": cost.clone()}),
            ),
            (
                "hook_costs",
                json!({"event": "thread_completed", "step": null, "cost": cost}),
            ),
        ];
        for (field, record) in records {
            let mut result = completed_result();
            result[field] = json!([record.clone()]);
            assert!(project_graph_action_result(result.clone(), Value::Null).is_ok());
            result[field][0]["unexpected"] = json!(true);
            assert!(
                project_graph_action_result(result, Value::Null)
                    .unwrap_err()
                    .contains("malformed GraphResult"),
                "record {field}"
            );
        }
        let decoded: GraphResult = serde_json::from_value(completed_result()).unwrap();
        assert!(decoded.node_costs.is_empty());
        assert!(decoded.hook_costs.is_empty());
        assert!(decoded.errors.is_none());
        let encoded = serde_json::to_value(decoded).unwrap();
        for field in [
            "errors",
            "error",
            "errors_suppressed",
            "cost",
            "node_costs",
            "hook_costs",
        ] {
            assert!(encoded.get(field).is_none(), "omitted field {field}");
        }
        assert_eq!(
            project_graph_action_result(encoded, Value::Null).unwrap(),
            json!({"report": [1, 2, 3]})
        );
    }
}
