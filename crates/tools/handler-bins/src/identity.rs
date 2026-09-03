use std::collections::{HashMap, HashSet};

use ryeos_handler_protocol::{
    ComposeRequest, ComposeSuccess, ComposerFieldRequirement, ComposerFieldSemantics,
    ResolutionStepNameWire,
};
use serde_json::Value;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityConfig {
    #[serde(default)]
    policy_facts: Vec<IdentityPolicyFact>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityPolicyFact {
    name: String,
    path: Vec<String>,
    expect: IdentityPolicyFactShape,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum IdentityPolicyFactShape {
    ArrayOfStrings,
}

pub fn validate_config(config: &Value) -> Result<(), String> {
    if config.is_null() {
        return Ok(());
    }
    let parsed: IdentityConfig = serde_json::from_value(config.clone()).map_err(|error| {
        format!("identity composer config must be a strict policy-fact object: {error}")
    })?;
    if parsed.policy_facts.len() > 16 {
        return Err("identity composer policy-fact count exceeds 16".to_owned());
    }
    let mut names = HashSet::new();
    for fact in parsed.policy_facts {
        if fact.name.is_empty()
            || fact.name.len() > 128
            || !names.insert(fact.name)
            || fact.path.is_empty()
            || fact.path.len() > 16
            || fact.path.iter().any(|part| {
                part.is_empty() || part.len() > 128 || part.chars().any(char::is_control)
            })
        {
            return Err("identity composer policy fact is not canonical and bounded".to_owned());
        }
    }
    Ok(())
}

/// Validate exact-value composition requirements for the identity composer.
///
/// Identity returns the root parsed value unchanged, so it can promise root
/// preservation but cannot promise ancestor inheritance.
pub fn validate_field_requirements(
    requirements: &[ComposerFieldRequirement],
) -> Result<(), String> {
    for requirement in requirements {
        if requirement.path.len() != 1 || requirement.path[0].is_empty() {
            return Err(
                "identity composer field requirement must contain one non-empty path segment"
                    .to_string(),
            );
        }
        if requirement.semantics != ComposerFieldSemantics::RootVerbatim {
            return Err(format!(
                "identity composer cannot provide {:?} semantics for path `{}`; it only preserves root fields verbatim",
                requirement.semantics,
                requirement.path.join(".")
            ));
        }
    }
    Ok(())
}

pub fn compose(
    config: &Value,
    request: &ComposeRequest,
) -> Result<ComposeSuccess, (ResolutionStepNameWire, String)> {
    validate_config(config).map_err(|reason| (ResolutionStepNameWire::PipelineInit, reason))?;
    let parsed = if config.is_null() {
        IdentityConfig {
            policy_facts: Vec::new(),
        }
    } else {
        serde_json::from_value(config.clone()).map_err(|error| {
            (
                ResolutionStepNameWire::PipelineInit,
                format!("decode validated identity composer config: {error}"),
            )
        })?
    };
    let mut policy_facts = HashMap::new();
    for fact in parsed.policy_facts {
        let mut value = &request.root.parsed;
        for part in &fact.path {
            value = value.get(part).ok_or_else(|| {
                (
                    ResolutionStepNameWire::PipelineInit,
                    format!("identity policy fact `{}` path is absent", fact.name),
                )
            })?;
        }
        match fact.expect {
            IdentityPolicyFactShape::ArrayOfStrings
                if value.as_array().is_some_and(|items| {
                    items.iter().all(|item| {
                        item.as_str().is_some_and(|text| {
                            !text.is_empty()
                                && text.len() <= 1024
                                && !text.chars().any(char::is_control)
                        })
                    })
                }) => {}
            _ => {
                return Err((
                    ResolutionStepNameWire::PipelineInit,
                    format!("identity policy fact `{}` has the wrong shape", fact.name),
                ));
            }
        }
        policy_facts.insert(fact.name, value.clone());
    }
    Ok(ComposeSuccess {
        composed: request.root.parsed.clone(),
        derived: HashMap::new(),
        policy_facts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_handler_protocol::{ComposeInput, ComposeItemContext, TrustClassWire};
    use serde_json::json;

    fn root_input(parsed: Value) -> ComposeInput {
        ComposeInput {
            item: ComposeItemContext {
                requested_id: "x:r".into(),
                resolved_ref: "x:r".into(),
                trust_class: TrustClassWire::TrustedBundle,
            },
            parsed,
        }
    }

    #[test]
    fn returns_root_parsed_verbatim() {
        let view = compose(
            &Value::Null,
            &ComposeRequest {
                composer_config: Value::Null,
                root: root_input(json!({ "k": 1 })),
                ancestors: vec![],
            },
        )
        .unwrap();
        assert_eq!(view.composed, json!({ "k": 1 }));
        assert!(view.derived.is_empty());
        assert!(view.policy_facts.is_empty());
    }

    #[test]
    fn ignores_ancestors() {
        let view = compose(
            &Value::Null,
            &ComposeRequest {
                composer_config: Value::Null,
                root: root_input(json!({ "x": 1 })),
                ancestors: vec![root_input(json!({ "anything": 1 }))],
            },
        )
        .unwrap();
        assert_eq!(view.composed, json!({ "x": 1 }));
    }

    #[test]
    fn validate_config_accepts_null() {
        validate_config(&Value::Null).unwrap();
    }

    #[test]
    fn validate_config_accepts_empty_object() {
        validate_config(&json!({})).unwrap();
    }

    #[test]
    fn validate_config_rejects_non_empty_object() {
        let err = validate_config(&json!({ "anything": 1 })).unwrap_err();
        assert!(err.contains("strict policy-fact object"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_array() {
        let err = validate_config(&json!([1, 2, 3])).unwrap_err();
        assert!(err.contains("strict policy-fact object"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_string() {
        let err = validate_config(&json!("nope")).unwrap_err();
        assert!(err.contains("strict policy-fact object"), "got: {err}");
    }

    #[test]
    fn extracts_only_declared_typed_policy_facts_without_changing_content() {
        let config = json!({"policy_facts":[{
            "name":"effective_caps",
            "path":["requires","capabilities","declared"],
            "expect":"array_of_strings"
        }]});
        let root = json!({
            "requires":{"capabilities":{"declared":["cap:a"]}},
            "config":{"opaque":true}
        });
        let view = compose(
            &config,
            &ComposeRequest {
                composer_config: config.clone(),
                root: root_input(root.clone()),
                ancestors: vec![],
            },
        )
        .unwrap();
        assert_eq!(view.composed, root);
        assert_eq!(view.policy_facts["effective_caps"], json!(["cap:a"]));
    }

    #[test]
    fn exact_field_requirements_are_root_only() {
        validate_field_requirements(&[ComposerFieldRequirement {
            path: vec!["policy".into()],
            semantics: ComposerFieldSemantics::RootVerbatim,
        }])
        .unwrap();
        let error = validate_field_requirements(&[ComposerFieldRequirement {
            path: vec!["policy".into()],
            semantics: ComposerFieldSemantics::InheritOrReplace,
        }])
        .unwrap_err();
        assert!(error.contains("only preserves root fields verbatim"));
    }
}
