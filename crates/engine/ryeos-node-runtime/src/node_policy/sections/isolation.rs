//! Node-owned subprocess isolation policy.
//!
//! The section adds only the node-policy envelope. Isolation semantics
//! and their validation remain owned by `ryeos-engine`'s
//! [`ryeos_engine::isolation::IsolationPolicyDocument`] contract.
//!
//! A publisher profile may declare several host classes in this section;
//! initialization narrows it to the one class the deployment names, and a
//! node's own policy generation declares exactly that one. The section
//! compiles only a single-class body into the runtime policy.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "isolation";
pub const POLICY_SCHEMA: u32 = 1;

/// Wire envelope around the engine-owned semantic isolation contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IsolationPolicyEnvelope {
    schema: u32,
    policy: ryeos_engine::isolation::IsolationPolicyDocument,
}

impl IsolationPolicyEnvelope {
    fn parse(body: &Value) -> anyhow::Result<Self> {
        let envelope: Self =
            serde_json::from_value(body.clone()).context("parse node isolation policy")?;
        if envelope.schema != POLICY_SCHEMA {
            bail!("node isolation policy schema is not current");
        }
        ryeos_engine::isolation::IsolationRuntime::validate_policy_document(&envelope.policy)
            .map_err(anyhow::Error::from)
            .context("validate node isolation policy")?;
        Ok(envelope)
    }
}

/// Host classes an isolation section body declares, after validating it.
pub fn declared_host_classes(body: &Value) -> anyhow::Result<Vec<String>> {
    Ok(IsolationPolicyEnvelope::parse(body)?
        .policy
        .host_class_names())
}

/// The isolation section body narrowed to the one named host class. A
/// class the body does not declare is refused.
pub fn select_host_class(body: &Value, host_class: &str) -> anyhow::Result<Value> {
    let envelope = IsolationPolicyEnvelope::parse(body)?;
    let narrowed = IsolationPolicyEnvelope {
        schema: envelope.schema,
        policy: envelope
            .policy
            .narrowed(host_class)
            .map_err(anyhow::Error::msg)?,
    };
    serde_json::to_value(narrowed).context("serialize the selected node isolation policy")
}

/// The single-class section body whose compiled policy is `policy`.
pub fn section_body(policy: ryeos_engine::isolation::IsolationPolicy) -> anyhow::Result<Value> {
    serde_json::to_value(IsolationPolicyEnvelope {
        schema: POLICY_SCHEMA,
        policy: ryeos_engine::isolation::IsolationPolicyDocument::from_selected_policy(policy),
    })
    .context("serialize node isolation policy")
}

pub struct IsolationPolicySection;

impl TypedNodePolicy for ryeos_engine::isolation::IsolationPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl NodePolicySection for IsolationPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let envelope = IsolationPolicyEnvelope::parse(body)?;
        let policy = envelope
            .policy
            .into_selected_policy()
            .map_err(anyhow::Error::msg)
            .context("compile node isolation policy")?;
        Ok(Arc::new(policy))
    }
}
use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> NodePolicyContext {
        NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: format!("/node/policies/{SECTION_NAME}.yaml").into(),
            signer_fingerprint: "ab".repeat(32),
        }
    }

    fn disabled_policy_value() -> Value {
        section_body(ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring()).unwrap()
    }

    /// The single class of a section body built by `disabled_policy_value`.
    fn class(body: &mut Value) -> &mut Value {
        &mut body["policy"]["host_classes"][ryeos_engine::isolation::AUTHORING_HOST_CLASS]
    }

    #[test]
    fn section_is_registered_policy_authority() {
        let section = IsolationPolicySection;
        assert_eq!(section.name(), SECTION_NAME);
        assert!(section.parse(&context(), &disabled_policy_value()).is_ok());
    }

    #[test]
    fn parses_and_retains_the_engine_owned_policy_exactly() {
        let section = IsolationPolicySection;
        let parsed = section.parse(&context(), &disabled_policy_value()).unwrap();
        assert_eq!(
            parsed
                .as_any()
                .downcast_ref::<ryeos_engine::isolation::IsolationPolicy>()
                .unwrap(),
            &ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring()
        );
    }

    #[test]
    fn rejects_unknown_fields_and_unknown_schema() {
        let section = IsolationPolicySection;
        let mut unknown = disabled_policy_value();
        unknown["unexpected"] = Value::Bool(true);
        assert!(section.parse(&context(), &unknown).is_err());

        let mut unknown_nested = disabled_policy_value();
        unknown_nested["policy"]["unexpected"] = Value::Bool(true);
        assert!(section.parse(&context(), &unknown_nested).is_err());

        let mut missing_trusted_lane = disabled_policy_value();
        class(&mut missing_trusted_lane)
            .as_object_mut()
            .unwrap()
            .remove("trusted_process_group_sessions");
        assert!(section.parse(&context(), &missing_trusted_lane).is_err());

        assert!(
            section
                .parse(&context(), &serde_json::json!({"schema": 1}))
                .is_err()
        );

        let mut stale = disabled_policy_value();
        stale["schema"] = Value::from(POLICY_SCHEMA + 1);
        assert!(
            section
                .parse(&context(), &stale)
                .err()
                .unwrap()
                .to_string()
                .contains("schema is not current")
        );
    }

    #[test]
    fn refuses_the_previous_policy_version_and_the_removed_nested_sandbox_key() {
        let section = IsolationPolicySection;
        let mut previous = disabled_policy_value();
        previous["policy"]["version"] =
            Value::from(ryeos_engine::isolation::ISOLATION_POLICY_VERSION - 1);
        let error = section
            .parse(&context(), &previous)
            .err()
            .unwrap()
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(": ");
        assert!(
            error.contains("unsupported node isolation policy version"),
            "got: {error}"
        );

        let mut removed_key = disabled_policy_value();
        class(&mut removed_key)["process_scopes"] = serde_json::json!({
            "mode": "required", "control_timeout_ms": 1000, "nested_sandbox": true,
        });
        let error = section
            .parse(&context(), &removed_key)
            .err()
            .unwrap()
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(": ");
        assert!(error.contains("nested_sandbox"), "got: {error}");
    }

    #[test]
    fn floor_entries_carry_accepted_grades_and_providers_through_the_section() {
        let section = IsolationPolicySection;
        let mut declared = disabled_policy_value();
        class(&mut declared)["requires_capabilities"] = serde_json::json!([
            {"any_of": ["accounting", "residency:swap_absent"]},
            "storage:durable_root",
            {"name": "termination:process_group", "accept": ["trusted"]},
            {"name": "time", "providers": ["host"]},
        ]);
        let parsed = section.parse(&context(), &declared).unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<ryeos_engine::isolation::IsolationPolicy>()
            .unwrap();
        assert_eq!(policy.requires_capabilities.len(), 4);
        assert_eq!(
            policy.requires_capabilities[2].accepted_grades(),
            ["trusted"]
        );
        assert_eq!(policy.requires_capabilities[3].providers(), ["host"]);
        assert!(policy.requires_capabilities[1].accepted_grades().is_empty());

        let mut refused_grade = declared.clone();
        class(&mut refused_grade)["requires_capabilities"][3]["accept"] =
            serde_json::json!(["peer_attested"]);
        assert!(section.parse(&context(), &refused_grade).is_err());

        let mut unknown_key = declared;
        class(&mut unknown_key)["requires_capabilities"][3]["grade"] = Value::from("measured");
        assert!(section.parse(&context(), &unknown_key).is_err());
    }

    #[test]
    fn delegates_semantic_validation_to_the_engine_contract() {
        let section = IsolationPolicySection;
        let mut invalid = disabled_policy_value();
        class(&mut invalid)["mode"] = Value::String("enforce".to_owned());
        class(&mut invalid)["backend"] = Value::Null;

        let error = section
            .parse(&context(), &invalid)
            .err()
            .unwrap()
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(": ");
        assert!(
            error.contains("enforced isolation requires an explicit backend selection"),
            "got: {error}"
        );
    }

    #[test]
    fn a_profile_body_declares_several_classes_and_init_selects_one() {
        let section = IsolationPolicySection;
        let mut profile = disabled_policy_value();
        let local = class(&mut profile).clone();
        let mut hosted = local.clone();
        hosted["requires_capabilities"] = serde_json::json!([
            {"any_of": ["accounting", "residency:swap_absent"]},
            {"name": "termination:enclosure_trusted_init", "accept": ["measured", "trusted"]},
            "time",
        ]);
        profile["policy"]["host_classes"] = serde_json::json!({
            "hosted": hosted,
            "local": local,
        });
        assert_eq!(
            declared_host_classes(&profile).unwrap(),
            ["hosted", "local"]
        );

        // A node runs under one class: the unselected body does not compile.
        let error = format!("{:#}", section.parse(&context(), &profile).err().unwrap());
        assert!(error.contains("exactly one host class"), "{error}");
        assert!(error.contains("hosted, local"), "{error}");

        let selected = select_host_class(&profile, "hosted").unwrap();
        assert_eq!(declared_host_classes(&selected).unwrap(), ["hosted"]);
        let parsed = section.parse(&context(), &selected).unwrap();
        let policy = parsed
            .as_any()
            .downcast_ref::<ryeos_engine::isolation::IsolationPolicy>()
            .unwrap();
        assert_eq!(policy.host_class, "hosted");
        assert_eq!(policy.requires_capabilities.len(), 3);

        let refused = format!("{:#}", select_host_class(&profile, "edge").unwrap_err());
        assert!(refused.contains("`edge` is not declared"), "{refused}");
    }
}
