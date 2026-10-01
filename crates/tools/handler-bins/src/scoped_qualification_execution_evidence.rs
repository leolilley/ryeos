//! Pure projection for a qualification-only scoped callback verifier.
//!
//! This handler interprets the terminal's claimed subordinate coordinate. It
//! does not attest execution: the daemon independently joins that coordinate
//! to its exact owner, recipe, process, scope and retained CAS observation.

use ryeos_handler_protocol::{
    ExecutionEvidenceCandidateScopedAttemptWire, ExecutionEvidenceDescribeRequest,
    ExecutionEvidenceDescribeResponse, ExecutionEvidenceProjectRequest,
    ExecutionEvidenceProjectResponse, HandlerResponse,
};
use ryeos_state::external_content::products::qualification::ProductQualificationResult;

pub fn describe(request: ExecutionEvidenceDescribeRequest) -> HandlerResponse {
    let response = match require_program(&request.config, &request.effective_program) {
        Ok(()) => ExecutionEvidenceDescribeResponse::Described {
            required_calls: Vec::new(),
        },
        Err(message) => ExecutionEvidenceDescribeResponse::Refused { message },
    };
    HandlerResponse::ExecutionEvidenceDescribe { response }
}

pub fn project(request: ExecutionEvidenceProjectRequest) -> HandlerResponse {
    let response = match project_inner(request) {
        Ok((result, subordinate_attempt)) => ExecutionEvidenceProjectResponse::Projected {
            result,
            calls: Vec::new(),
            subordinate_attempt: Some(subordinate_attempt),
        },
        Err(message) => ExecutionEvidenceProjectResponse::Refused { message },
    };
    HandlerResponse::ExecutionEvidenceProject { response }
}

pub fn wrong_request() -> HandlerResponse {
    HandlerResponse::ExecutionEvidenceProject {
        response: ExecutionEvidenceProjectResponse::Refused {
            message: "scoped qualification evidence handler accepts only evidence requests".into(),
        },
    }
}

fn project_inner(
    request: ExecutionEvidenceProjectRequest,
) -> Result<
    (
        serde_json::Value,
        ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire,
    ),
    String,
> {
    require_program(&request.config, &request.effective_program)?;
    if request.terminal.status != "completed"
        || !request.terminal.error.is_null()
        || request.terminal.result.is_null()
        || request.events.iter().any(|event| {
            matches!(
                event.event_type.as_str(),
                ryeos_state::event_types::TOOL_CALL_START
                    | ryeos_state::event_types::TOOL_CALL_RESULT
                    | ryeos_state::event_types::CHILD_THREAD_SPAWNED
            )
        })
    {
        return Err(
            "scoped qualification requires one successful terminal without managed child calls"
                .into(),
        );
    }
    let result = ProductQualificationResult::from_value(&request.terminal.result)
        .map_err(|error| error.to_string())?;
    use ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire;
    let candidate = if request.config == serde_json::json!({"subordinate":"remote_consumer"}) {
        if result.probe_evidence.get("scoped_attempt").is_some() {
            return Err("remote consumer terminal contains scoped producer evidence".into());
        }
        let claimed = result
            .probe_evidence
            .get("remote_consumer_attempt")
            .ok_or_else(|| "remote consumer terminal omits exact retained references".to_owned())?;
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RemoteReferences {
            operation_id: String,
            evidence_sha256: String,
            termination_operation_id: String,
            terminal_observation_sha256: String,
        }
        let references: RemoteReferences =
            serde_json::from_value(claimed.clone()).map_err(|error| error.to_string())?;
        ExecutionEvidenceCandidateSubordinateAttemptWire::RemoteConsumer {
            operation_id: references.operation_id,
            evidence_sha256: references.evidence_sha256,
            termination_operation_id: references.termination_operation_id,
            terminal_observation_sha256: references.terminal_observation_sha256,
        }
    } else {
        if result
            .probe_evidence
            .get("remote_consumer_attempt")
            .is_some()
        {
            return Err("scoped producer terminal contains remote consumer evidence".into());
        }
        let claimed = result.probe_evidence.get("scoped_attempt").ok_or_else(|| {
            "scoped qualification terminal omits its attempt coordinate".to_owned()
        })?;
        let coordinate: ExecutionEvidenceCandidateScopedAttemptWire =
            serde_json::from_value(claimed.clone()).map_err(|error| error.to_string())?;
        ExecutionEvidenceCandidateSubordinateAttemptWire::ScopedProducer { coordinate }
    };
    Ok((request.terminal.result, candidate))
}

fn require_program(
    config: &serde_json::Value,
    program: &ryeos_handler_protocol::ExecutionEvidenceProgramWire,
) -> Result<(), String> {
    let expected_protocol = if config == &serde_json::json!({}) {
        "protocol:ryeos/core/qualification_scoped_callback"
    } else if config == &serde_json::json!({"subordinate":"remote_consumer"}) {
        "protocol:ryeos/core/qualification_consumer_callback"
    } else {
        return Err("qualification projector configuration is not exact".into());
    };
    if program.composed.composed.get("execution_protocol")
        != Some(&serde_json::json!(expected_protocol))
    {
        return Err(
            "scoped qualification projector requires its exact protocol and empty configuration"
                .into(),
        );
    }
    if let Some(value) = program
        .composed
        .derived
        .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
    {
        let plan = ryeos_engine::hooks::EffectiveHookPlan::from_value(value)
            .map_err(|error| error.to_string())?;
        if plan.iter_layers().any(|(_, layer)| !layer.hooks.is_empty()) {
            return Err("scoped qualification verifier cannot have executable hooks".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ryeos_handler_protocol::{
        ExecutionEvidenceEventWire, ExecutionEvidenceProgramWire, ExecutionEvidenceTerminalWire,
        LaunchComposedViewWire,
    };
    use serde_json::json;

    use super::*;

    fn program() -> ExecutionEvidenceProgramWire {
        ExecutionEvidenceProgramWire {
            canonical_ref: "tool:test/scoped-verifier".into(),
            effective_definition_digest: "a".repeat(64),
            composed: LaunchComposedViewWire {
                composed: json!({"execution_protocol":"protocol:ryeos/core/qualification_scoped_callback"}),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            ancestor_requested_ids: Vec::new(),
        }
    }

    fn request() -> ExecutionEvidenceProjectRequest {
        ExecutionEvidenceProjectRequest {
            config: json!({}),
            effective_program: program(),
            terminal: ExecutionEvidenceTerminalWire {
                status: "completed".into(),
                result: json!({
                    "schema": ryeos_state::external_content::products::qualification::PRODUCT_QUALIFICATION_RESULT_SCHEMA,
                    "subject_manifest_hash": "a".repeat(64),
                    "claims": ["command_probe"],
                    "probe_evidence": {
                        "scoped_attempt": {
                            "attempt_id": format!("scoped-{}", "b".repeat(64)),
                            "scenario_id": "native_codex",
                            "observation_object_hash": "c".repeat(64),
                        }
                    }
                }),
                error: serde_json::Value::Null,
                artifacts: Vec::new(),
            },
            events: Vec::new(),
        }
    }

    #[test]
    fn remote_projection_requires_its_protocol_and_exclusive_references() {
        let mut valid = request();
        valid.config = json!({"subordinate":"remote_consumer"});
        valid.effective_program.composed.composed["execution_protocol"] =
            json!("protocol:ryeos/core/qualification_consumer_callback");
        valid.terminal.result["probe_evidence"] = json!({"remote_consumer_attempt":{
            "operation_id":"a".repeat(64), "evidence_sha256":"b".repeat(64),
            "termination_operation_id":"c".repeat(64), "terminal_observation_sha256":"d".repeat(64),
        }});
        assert!(matches!(project(valid.clone()), HandlerResponse::ExecutionEvidenceProject {
            response: ExecutionEvidenceProjectResponse::Projected {
                subordinate_attempt: Some(ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire::RemoteConsumer { .. }), ..
            }
        }));
        let mut mixed = valid.clone();
        mixed.terminal.result["probe_evidence"]["scoped_attempt"] = json!({});
        assert!(matches!(
            project(mixed),
            HandlerResponse::ExecutionEvidenceProject {
                response: ExecutionEvidenceProjectResponse::Refused { .. }
            }
        ));
        let mut wrong_protocol = valid.clone();
        wrong_protocol.effective_program = program();
        assert!(matches!(
            project(wrong_protocol),
            HandlerResponse::ExecutionEvidenceProject {
                response: ExecutionEvidenceProjectResponse::Refused { .. }
            }
        ));
        valid.terminal.result["probe_evidence"]["remote_consumer_attempt"]["retry"] = json!(true);
        assert!(matches!(
            project(valid),
            HandlerResponse::ExecutionEvidenceProject {
                response: ExecutionEvidenceProjectResponse::Refused { .. }
            }
        ));
    }

    #[test]
    fn projects_only_exact_scoped_terminal_coordinate() {
        let HandlerResponse::ExecutionEvidenceDescribe {
            response: ExecutionEvidenceDescribeResponse::Described { required_calls },
        } = describe(ExecutionEvidenceDescribeRequest {
            config: json!({}),
            effective_program: program(),
        })
        else {
            panic!("signed scoped program refused");
        };
        assert!(required_calls.is_empty());
        let HandlerResponse::ExecutionEvidenceProject {
            response:
                ExecutionEvidenceProjectResponse::Projected {
                    calls,
                    subordinate_attempt: Some(ryeos_handler_protocol::ExecutionEvidenceCandidateSubordinateAttemptWire::ScopedProducer { coordinate: scoped }),
                    ..
                },
        } = project(request())
        else {
            panic!("exact scoped terminal refused");
        };
        assert!(calls.is_empty());
        assert_eq!(scoped.scenario_id, "native_codex");
    }

    #[test]
    fn refuses_unaccounted_managed_child_and_missing_scoped_coordinate() {
        let mut managed = request();
        managed.events.push(ExecutionEvidenceEventWire {
            thread_seq: 1,
            event_type: ryeos_state::event_types::CHILD_THREAD_SPAWNED.into(),
            payload: json!({}),
        });
        assert!(matches!(
            project(managed),
            HandlerResponse::ExecutionEvidenceProject {
                response: ExecutionEvidenceProjectResponse::Refused { .. }
            }
        ));
        let mut missing = request();
        missing.terminal.result["probe_evidence"] = json!({});
        assert!(matches!(
            project(missing),
            HandlerResponse::ExecutionEvidenceProject {
                response: ExecutionEvidenceProjectResponse::Refused { .. }
            }
        ));
    }
}
