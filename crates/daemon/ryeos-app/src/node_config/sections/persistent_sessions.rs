//! Node-owned aggregate resource policy for persistent subprocess sessions.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, NodeItemContext, SectionRecord, SectionSourcePolicy};
use crate::persistent_session::PersistentSessionPoolLimits;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionPolicyRecord {
    pub schema: u32,
    pub limits: PersistentSessionPoolLimits,
}

impl PersistentSessionPolicyRecord {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("persistent-session node policy schema is not current");
        }
        self.limits.validate()
    }
}

pub struct PersistentSessionPolicySection;

impl NodeConfigSection for PersistentSessionPolicySection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::SystemAndState
    }

    fn operator_policy_item_id(&self) -> Option<&'static str> {
        Some("policy")
    }

    fn parse(&self, ctx: &NodeItemContext, body: &Value) -> anyhow::Result<Box<dyn SectionRecord>> {
        if ctx.id != "policy" {
            bail!(
                "persistent-session policy filename must be `policy`, got `{}`",
                ctx.id
            );
        }
        let record: PersistentSessionPolicyRecord =
            serde_json::from_value(body.clone()).context("parse persistent-session node policy")?;
        record.validate()?;
        Ok(Box::new(record))
    }
}

impl SectionRecord for PersistentSessionPolicyRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_value() -> Value {
        serde_json::to_value(PersistentSessionPolicyRecord {
            schema: 1,
            limits: PersistentSessionPoolLimits::default(),
        })
        .unwrap()
    }

    #[test]
    fn policy_is_node_owned_and_parses_exact_limits() {
        let section = PersistentSessionPolicySection;
        assert_eq!(section.source_policy(), SectionSourcePolicy::SystemAndState);
        assert_eq!(section.operator_policy_item_id(), Some("policy"));
        let context = NodeItemContext {
            section: "persistent_sessions".to_owned(),
            id: "policy".to_owned(),
            stem: "policy".to_owned(),
            rel_path: "policy.yaml".into(),
            source_file: "/node/persistent_sessions/policy.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        };
        let parsed = section.parse(&context, &policy_value()).unwrap();
        let record = parsed
            .as_any()
            .downcast_ref::<PersistentSessionPolicyRecord>()
            .unwrap();
        assert_eq!(record.limits, PersistentSessionPoolLimits::default());
    }

    #[test]
    fn policy_rejects_unknown_schema_and_incoherent_limits() {
        let mut unknown_schema = policy_value();
        unknown_schema["schema"] = Value::from(2);
        let record: PersistentSessionPolicyRecord = serde_json::from_value(unknown_schema).unwrap();
        assert!(record.validate().is_err());

        let mut incoherent = policy_value();
        incoherent["limits"]["max_total_processes"] = Value::from(0);
        let record: PersistentSessionPolicyRecord = serde_json::from_value(incoherent).unwrap();
        assert!(record.validate().is_err());
    }
}
