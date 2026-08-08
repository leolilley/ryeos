//! Canonical graph-action answers for dormant effect-record v2.
//!
//! This is the only live callback-envelope decoder allowed to construct a
//! graph v2 answer. It strips observation texture (thread snapshot and cost),
//! rejects state that replay cannot reconstruct, and leaves the authored
//! result byte-for-byte represented in the typed answer.

use serde::Deserialize;
use serde_json::Value;

use crate::callback_contract::CallbackDispatchResponse;
use crate::envelope::{RuntimeCost, RuntimeResultStatus};
use ryeos_state::objects::GraphNodeEffectAnswerV2;

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedGraphNodeEffectV2 {
    pub answer: GraphNodeEffectAnswerV2,
    /// Digest of the complete first response, retained as observation only.
    pub observed_response_digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeEnvelope {
    success: bool,
    status: RuntimeResultStatus,
    result: Value,
    outputs: Value,
    warnings: Vec<String>,
    cost: Value,
    #[serde(default)]
    replayed_from: Option<String>,
}

#[derive(Debug)]
struct RequiredNullableString(Option<String>);

impl<'de> Deserialize<'de> for RequiredNullableString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubprocessEnvelope {
    outcome_code: RequiredNullableString,
    result: Value,
    error: Value,
    artifacts: Vec<Value>,
    #[serde(default)]
    replayed_from: Option<String>,
}

pub fn normalize_graph_node_effect_v2(
    response: &Value,
) -> anyhow::Result<NormalizedGraphNodeEffectV2> {
    let callback: CallbackDispatchResponse = serde_json::from_value(response.clone())
        .map_err(|error| anyhow::anyhow!("invalid callback response for effect answer: {error}"))?;
    let observed_response_digest =
        ryeos_state::objects::effect_record_v2::canonical_value_digest(response)?;
    let result = callback.result;
    let answer = match result.as_object() {
        Some(object) if object.contains_key("success") || object.contains_key("status") => {
            let envelope: NativeEnvelope = serde_json::from_value(result).map_err(|error| {
                anyhow::anyhow!("invalid native effect response envelope: {error}")
            })?;
            if envelope.replayed_from.is_some() {
                anyhow::bail!("an already replayed native response cannot be recorded again");
            }
            if !envelope.success || envelope.status != RuntimeResultStatus::Completed {
                anyhow::bail!("only a successfully completed native response is recordable");
            }
            if !envelope.warnings.is_empty() {
                anyhow::bail!(
                    "native response warnings have no replay-safety classification; response is not recordable"
                );
            }
            if !envelope.cost.is_null() {
                let cost: RuntimeCost = serde_json::from_value(envelope.cost)
                    .map_err(|error| anyhow::anyhow!("invalid native response cost: {error}"))?;
                cost.validate()
                    .map_err(|error| anyhow::anyhow!("invalid native response cost: {error}"))?;
            }
            GraphNodeEffectAnswerV2::Native {
                result: envelope.result,
                outputs: envelope.outputs,
                warnings: Vec::new(),
            }
        }
        Some(object) if object.contains_key("outcome_code") => {
            let envelope: SubprocessEnvelope = serde_json::from_value(result).map_err(|error| {
                anyhow::anyhow!("invalid subprocess effect response envelope: {error}")
            })?;
            if envelope.replayed_from.is_some() {
                anyhow::bail!("an already replayed subprocess response cannot be recorded again");
            }
            if !envelope.error.is_null() {
                anyhow::bail!("a failed subprocess response is not recordable");
            }
            if !matches!(envelope.outcome_code.0.as_deref(), None | Some("exit:0")) {
                anyhow::bail!("a non-success subprocess outcome is not recordable");
            }
            if !envelope.artifacts.is_empty() {
                anyhow::bail!(
                    "subprocess response artifacts have no reconstructible effect-answer contract"
                );
            }
            GraphNodeEffectAnswerV2::Subprocess {
                result: envelope.result,
            }
        }
        _ => {
            if result
                .get("continuation_id")
                .is_some_and(|value| !value.is_null())
            {
                anyhow::bail!("a continuation-bearing response is not recordable");
            }
            if result.get("replayed_from").is_some() {
                anyhow::bail!("an already replayed bare response cannot be recorded again");
            }
            GraphNodeEffectAnswerV2::Bare { result }
        }
    };
    answer.validate()?;
    Ok(NormalizedGraphNodeEffectV2 {
        answer,
        observed_response_digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn thread_and_cost_observations_do_not_change_the_answer() {
        let response = |thread: &str, tokens: u64| {
            json!({
                "thread": {"thread_id": thread, "status": "completed"},
                "result": {
                    "success": true,
                    "status": "completed",
                    "result": {"value": 7},
                    "outputs": {"proof": true},
                    "warnings": [],
                    "cost": {
                        "input_tokens": tokens,
                        "output_tokens": 1,
                        "total_usd": "0.000000001"
                    }
                }
            })
        };
        let first = normalize_graph_node_effect_v2(&response("T-first", 10)).unwrap();
        let second = normalize_graph_node_effect_v2(&response("T-second", 99)).unwrap();
        assert_eq!(first.answer, second.answer);
        assert_eq!(
            first.answer.digest().unwrap(),
            second.answer.digest().unwrap()
        );
        assert_ne!(
            first.observed_response_digest,
            second.observed_response_digest
        );
    }

    #[test]
    fn unsafe_or_failed_envelopes_refuse() {
        for result in [
            json!({
                "success": true,
                "status": "completed",
                "result": {"ok": true},
                "outputs": {},
                "warnings": ["thread-local warning"],
                "cost": null
            }),
            json!({
                "outcome_code": null,
                "result": {"ok": true},
                "error": null,
                "artifacts": [{"uri": "file:///tmp/live"}]
            }),
            json!({
                "outcome_code": "exit:1",
                "result": null,
                "error": {"message": "failed"},
                "artifacts": []
            }),
        ] {
            assert!(
                normalize_graph_node_effect_v2(&json!({"thread": {}, "result": result})).is_err()
            );
        }
    }

    #[test]
    fn replay_wraps_bare_objects_without_mutating_authored_bytes() {
        let normalized = normalize_graph_node_effect_v2(&json!({
            "thread": {"thread_id": "T-historical"},
            "result": {"answer": 42}
        }))
        .unwrap();
        let replay = normalized
            .answer
            .replay_leaf_envelope(&"ab".repeat(32))
            .unwrap();
        assert_eq!(replay["result"], json!({"answer": 42}));
        assert!(replay["result"].get("replayed_from").is_none());
        assert_eq!(replay["replayed_from"], "ab".repeat(32));
    }
}
