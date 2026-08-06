use ryeos_engine::resolution::KindComposedView;
use ryeos_handler_protocol::{
    EffectiveValidateRequest, EffectiveValidateResponse, HandlerResponse,
};

pub fn validate(request: EffectiveValidateRequest) -> HandlerResponse {
    if request.canonical_ref.split_once(':').map(|(kind, _)| kind) != Some("graph") {
        return invalid(
            "wrong_kind",
            "graph validator requires a graph canonical ref",
        );
    }
    if request.validator_config != serde_json::json!({}) && !request.validator_config.is_null() {
        return invalid(
            "invalid_config",
            "graph effective validator takes no config",
        );
    }
    let view = KindComposedView {
        composed: request.composed.composed,
        derived: request.composed.derived.into_iter().collect(),
        policy_facts: request.composed.policy_facts.into_iter().collect(),
    };
    match ryeos_graph_definition::validate_effective_graph(&view) {
        Ok(summary) => HandlerResponse::EffectiveValidate {
            response: EffectiveValidateResponse::Valid {
                normalized: serde_json::to_value(summary)
                    .expect("validation summary is serializable"),
            },
        },
        Err(error) => invalid("invalid_effective_graph", error.to_string()),
    }
}

pub fn wrong_request() -> HandlerResponse {
    invalid(
        "wrong_request",
        "graph effective validator accepts only effective_validate",
    )
}

fn invalid(code: impl Into<String>, message: impl Into<String>) -> HandlerResponse {
    HandlerResponse::EffectiveValidate {
        response: EffectiveValidateResponse::Invalid {
            code: code.into(),
            message: message.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use ryeos_engine::hooks::{
        EFFECTIVE_HOOK_PLAN_SCHEMA, EffectiveHookLayer, EffectiveHookPlan, HOOK_CONTEXT_SCHEMA,
        HookContextContract, HookEventContract, HookResultMode,
    };
    use ryeos_handler_protocol::LaunchComposedViewWire;

    use super::*;

    fn request() -> EffectiveValidateRequest {
        let empty = EffectiveHookLayer::empty();
        let plan = EffectiveHookPlan {
            schema: EFFECTIVE_HOOK_PLAN_SCHEMA.to_string(),
            owner_kind: "graph".to_string(),
            event_contracts: BTreeMap::from([(
                "graph_started".to_string(),
                HookEventContract {
                    context_contract: HookContextContract {
                        schema: HOOK_CONTEXT_SCHEMA.to_string(),
                        allowed_roots: BTreeSet::from(["event".to_string()]),
                    },
                    allowed_results: BTreeSet::from([
                        HookResultMode::Discard,
                        HookResultMode::Observation,
                    ]),
                },
            )]),
            authored: empty.clone(),
            builtin: empty.clone(),
            infrastructure: empty.clone(),
            context: empty.clone(),
            operator: empty.clone(),
            project: empty,
            sources: Vec::new(),
        };
        EffectiveValidateRequest {
            validator_config: serde_json::json!({}),
            canonical_ref: "graph:test/effective".to_string(),
            composed: LaunchComposedViewWire {
                composed: serde_json::json!({
                    "version": "1.0.0",
                    "category": "test",
                    "config": {
                        "start": "finish",
                        "nodes": {
                            "finish": {"node_type": "return", "output": "done"}
                        }
                    }
                }),
                derived: BTreeMap::from([(
                    "effective_hook_plan".to_string(),
                    plan.to_value().unwrap(),
                )]),
                policy_facts: BTreeMap::from([(
                    "effective_caps".to_string(),
                    serde_json::json!([]),
                )]),
            },
            ancestor_refs: vec!["graph:test/base".to_string()],
        }
    }

    #[test]
    fn handler_accepts_the_complete_composed_graph_and_returns_normalized_summary() {
        let HandlerResponse::EffectiveValidate {
            response: EffectiveValidateResponse::Valid { normalized },
        } = validate(request())
        else {
            panic!("effective graph should validate");
        };
        assert_eq!(
            normalized["schema"],
            ryeos_graph_definition::GRAPH_EFFECTIVE_VALIDATION_SCHEMA
        );
        assert_eq!(normalized["node_count"], 1);
        assert_eq!(normalized["edge_count"], 0);
    }

    #[test]
    fn handler_rejects_wrong_kind_and_nonempty_config_before_graph_decode() {
        let mut wrong_kind = request();
        wrong_kind.canonical_ref = "directive:test/effective".to_string();
        assert!(matches!(
            validate(wrong_kind),
            HandlerResponse::EffectiveValidate {
                response: EffectiveValidateResponse::Invalid { ref code, .. }
            } if code == "wrong_kind"
        ));

        let mut configured = request();
        configured.validator_config = serde_json::json!({"permissive": true});
        assert!(matches!(
            validate(configured),
            HandlerResponse::EffectiveValidate {
                response: EffectiveValidateResponse::Invalid { ref code, .. }
            } if code == "invalid_config"
        ));
    }
}
