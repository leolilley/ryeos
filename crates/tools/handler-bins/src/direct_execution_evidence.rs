//! Evidence interpretation for an admitted callback-free direct protocol.
//!
//! The signed protocol selects this handler. The daemon separately proves the
//! exact protocol identity, callback-free contract, terminal snapshot and
//! execution realization. This pure projection therefore claims only that the
//! successful terminal value is the result and that the contract describes no
//! child calls.

use ryeos_handler_protocol::{
    ExecutionEvidenceDescribeRequest, ExecutionEvidenceDescribeResponse,
    ExecutionEvidenceProjectRequest, ExecutionEvidenceProjectResponse, HandlerResponse,
};

pub fn describe(request: ExecutionEvidenceDescribeRequest) -> HandlerResponse {
    let response = match require_direct_program(&request) {
        Ok(()) => ExecutionEvidenceDescribeResponse::Described {
            required_calls: Vec::new(),
        },
        Err(message) => ExecutionEvidenceDescribeResponse::Refused { message },
    };
    HandlerResponse::ExecutionEvidenceDescribe { response }
}

pub fn project(request: ExecutionEvidenceProjectRequest) -> HandlerResponse {
    let response = match project_inner(request) {
        Ok(result) => ExecutionEvidenceProjectResponse::Projected {
            result,
            calls: Vec::new(),
        },
        Err(message) => ExecutionEvidenceProjectResponse::Refused { message },
    };
    HandlerResponse::ExecutionEvidenceProject { response }
}

pub fn wrong_request() -> HandlerResponse {
    HandlerResponse::ExecutionEvidenceProject {
        response: ExecutionEvidenceProjectResponse::Refused {
            message: "direct execution-evidence handler accepts only evidence requests".into(),
        },
    }
}

fn project_inner(request: ExecutionEvidenceProjectRequest) -> Result<serde_json::Value, String> {
    require_empty_config(&request.config)?;
    require_no_executable_hooks(&request.effective_program)?;
    if request.terminal.status != "completed"
        || !request.terminal.error.is_null()
        || request.terminal.result.is_null()
    {
        return Err("direct evidence requires one successful terminal result".into());
    }
    if request.events.iter().any(|event| {
        matches!(
            event.event_type.as_str(),
            ryeos_state::event_types::TOOL_CALL_START
                | ryeos_state::event_types::TOOL_CALL_RESULT
                | ryeos_state::event_types::CHILD_THREAD_SPAWNED
        )
    }) {
        return Err("callback-free direct evidence contains a child call occurrence".into());
    }
    Ok(request.terminal.result)
}

fn require_direct_program(request: &ExecutionEvidenceDescribeRequest) -> Result<(), String> {
    require_empty_config(&request.config)?;
    require_no_executable_hooks(&request.effective_program)
}

fn require_no_executable_hooks(
    program: &ryeos_handler_protocol::ExecutionEvidenceProgramWire,
) -> Result<(), String> {
    let Some(value) = program
        .composed
        .derived
        .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
    else {
        return Ok(());
    };
    let plan = ryeos_engine::hooks::EffectiveHookPlan::from_value(value)
        .map_err(|error| error.to_string())?;
    if plan.iter_layers().any(|(_, layer)| !layer.hooks.is_empty()) {
        return Err("callback-free direct evidence cannot contain executable hooks".into());
    }
    Ok(())
}

fn require_empty_config(config: &serde_json::Value) -> Result<(), String> {
    if config != &serde_json::json!({}) {
        return Err("direct execution-evidence handler takes no configuration".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ryeos_handler_protocol::{
        ExecutionEvidenceEventWire, ExecutionEvidenceProgramWire, ExecutionEvidenceTerminalWire,
        LaunchComposedViewWire,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    use super::*;

    fn program() -> ExecutionEvidenceProgramWire {
        ExecutionEvidenceProgramWire {
            canonical_ref: "tool:test/verifier".into(),
            effective_definition_digest: "a".repeat(64),
            composed: LaunchComposedViewWire {
                composed: json!({"execution_protocol":"protocol:ryeos/core/opaque"}),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            ancestor_requested_ids: Vec::new(),
        }
    }

    #[test]
    fn direct_projection_has_no_calls_and_returns_the_exact_terminal_value() {
        let HandlerResponse::ExecutionEvidenceDescribe {
            response: ExecutionEvidenceDescribeResponse::Described { required_calls },
        } = describe(ExecutionEvidenceDescribeRequest {
            config: json!({}),
            effective_program: program(),
        })
        else {
            panic!("direct description refused")
        };
        assert!(required_calls.is_empty());

        let result = json!({"schema":"qualified.v1","claims":{"abi":"x"}});
        let HandlerResponse::ExecutionEvidenceProject {
            response:
                ExecutionEvidenceProjectResponse::Projected {
                    result: projected,
                    calls,
                },
        } = project(ExecutionEvidenceProjectRequest {
            config: json!({}),
            effective_program: program(),
            terminal: ExecutionEvidenceTerminalWire {
                status: "completed".into(),
                result: result.clone(),
                error: serde_json::Value::Null,
                artifacts: Vec::new(),
            },
            events: Vec::new(),
        })
        else {
            panic!("direct projection refused")
        };
        assert_eq!(projected, result);
        assert!(calls.is_empty());
    }

    #[test]
    fn direct_projection_refuses_failed_missing_and_configured_results() {
        for (status, result, error) in [
            ("failed", json!({}), serde_json::Value::Null),
            (
                "completed",
                serde_json::Value::Null,
                serde_json::Value::Null,
            ),
            ("completed", json!({}), json!({"message":"failed"})),
        ] {
            assert!(matches!(
                project(ExecutionEvidenceProjectRequest {
                    config: json!({}),
                    effective_program: program(),
                    terminal: ExecutionEvidenceTerminalWire {
                        status: status.into(),
                        result,
                        error,
                        artifacts: Vec::new(),
                    },
                    events: Vec::new(),
                }),
                HandlerResponse::ExecutionEvidenceProject {
                    response: ExecutionEvidenceProjectResponse::Refused { .. }
                }
            ));
        }
        assert!(matches!(
            describe(ExecutionEvidenceDescribeRequest {
                config: json!({"permissive":true}),
                effective_program: program(),
            }),
            HandlerResponse::ExecutionEvidenceDescribe {
                response: ExecutionEvidenceDescribeResponse::Refused { .. }
            }
        ));
    }

    #[test]
    fn direct_projection_refuses_hidden_hooks_and_child_events() {
        let mut hooked = program();
        let plan = ryeos_engine::hooks::EffectiveHookPlan {
            schema: ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_SCHEMA.into(),
            owner_kind: "tool".into(),
            event_contracts: BTreeMap::from([(
                "tool_started".into(),
                ryeos_engine::hooks::HookEventContract {
                    context_contract: ryeos_engine::hooks::HookContextContract {
                        schema: ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA.into(),
                        allowed_roots: std::collections::BTreeSet::from(["event".into()]),
                    },
                    allowed_results: std::collections::BTreeSet::from([
                        ryeos_engine::hooks::HookResultMode::Discard,
                    ]),
                },
            )]),
            authored: ryeos_engine::hooks::EffectiveHookLayer {
                hooks: vec![ryeos_engine::hooks::HookDefinition {
                    id: "hidden".into(),
                    event: "tool_started".into(),
                    result: ryeos_engine::hooks::HookResultMode::Discard,
                    condition: ryeos_engine::hooks::ExpressionCondition::Absent,
                    action: json!({"item_id":"tool:test/hidden"}),
                }],
                dispatch_caps: vec!["ryeos.execute.tool.test/hidden".into()],
            },
            builtin: ryeos_engine::hooks::EffectiveHookLayer::empty(),
            infrastructure: ryeos_engine::hooks::EffectiveHookLayer::empty(),
            context: ryeos_engine::hooks::EffectiveHookLayer::empty(),
            operator: ryeos_engine::hooks::EffectiveHookLayer::empty(),
            project: ryeos_engine::hooks::EffectiveHookLayer::empty(),
            sources: Vec::new(),
        };
        hooked.composed.derived.insert(
            ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY.into(),
            plan.to_value().unwrap(),
        );
        assert!(matches!(
            describe(ExecutionEvidenceDescribeRequest {
                config: json!({}),
                effective_program: hooked,
            }),
            HandlerResponse::ExecutionEvidenceDescribe {
                response: ExecutionEvidenceDescribeResponse::Refused { .. }
            }
        ));

        assert!(matches!(
            project(ExecutionEvidenceProjectRequest {
                config: json!({}),
                effective_program: program(),
                terminal: ExecutionEvidenceTerminalWire {
                    status: "completed".into(),
                    result: json!({"accepted":true}),
                    error: serde_json::Value::Null,
                    artifacts: Vec::new(),
                },
                events: vec![ExecutionEvidenceEventWire {
                    thread_seq: 1,
                    event_type: ryeos_state::event_types::CHILD_THREAD_SPAWNED.into(),
                    payload: json!({}),
                }],
            }),
            HandlerResponse::ExecutionEvidenceProject {
                response: ExecutionEvidenceProjectResponse::Refused { .. }
            }
        ));
    }
}
