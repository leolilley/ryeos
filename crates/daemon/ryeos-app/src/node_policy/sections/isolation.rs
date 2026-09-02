//! Node-owned subprocess isolation policy.
//!
//! The section adds only the node-policy envelope. Isolation semantics
//! and their validation remain owned by `ryeos-engine`'s existing
//! [`ryeos_engine::isolation::IsolationPolicy`] contract.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "isolation";
pub const POLICY_SCHEMA: u32 = 1;

/// Wire envelope around the engine-owned semantic isolation contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IsolationPolicyDocument {
    schema: u32,
    policy: ryeos_engine::isolation::IsolationPolicy,
}

impl IsolationPolicyDocument {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != POLICY_SCHEMA {
            bail!("node isolation policy schema is not current");
        }
        ryeos_engine::isolation::IsolationRuntime::validate_policy(&self.policy)
            .map_err(anyhow::Error::from)
            .context("validate node isolation policy")
    }
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
        let document: IsolationPolicyDocument =
            serde_json::from_value(body.clone()).context("parse node isolation policy")?;
        document.validate()?;
        Ok(Arc::new(document.policy))
    }
}

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
        serde_json::to_value(IsolationPolicyDocument {
            schema: POLICY_SCHEMA,
            policy: ryeos_engine::isolation::IsolationPolicy::default_disabled(),
        })
        .unwrap()
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
            &ryeos_engine::isolation::IsolationPolicy::default_disabled()
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
    fn delegates_semantic_validation_to_the_engine_contract() {
        let section = IsolationPolicySection;
        let mut invalid = disabled_policy_value();
        invalid["policy"]["mode"] = Value::String("enforce".to_owned());
        invalid["policy"]["backend"] = Value::Null;

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
}
use std::sync::Arc;
