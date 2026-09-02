//! Node-owned aggregate resource policy for persistent subprocess sessions.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};
use crate::persistent_session::PersistentSessionPoolLimits;

pub const SECTION_NAME: &str = "persistent_sessions";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionPolicy {
    pub schema: u32,
    pub enabled: bool,
    pub limits: Option<PersistentSessionPoolLimits>,
}

impl PersistentSessionPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("persistent-session node policy schema is not current");
        }
        match (self.enabled, self.limits.as_ref()) {
            (true, Some(limits)) => limits.validate(),
            (false, None) => Ok(()),
            (true, None) => bail!("enabled persistent sessions require exact node limits"),
            (false, Some(_)) => bail!("disabled persistent sessions must not retain latent limits"),
        }
    }

    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            schema: 1,
            enabled: false,
            limits: None,
        }
    }
}

pub struct PersistentSessionPolicySection;

impl TypedNodePolicy for PersistentSessionPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl NodePolicySection for PersistentSessionPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: PersistentSessionPolicy =
            serde_json::from_value(body.clone()).context("parse persistent-session node policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_value() -> Value {
        serde_json::to_value(PersistentSessionPolicy {
            schema: 1,
            enabled: true,
            limits: Some(PersistentSessionPoolLimits::default()),
        })
        .unwrap()
    }

    #[test]
    fn policy_is_node_owned_and_parses_exact_limits() {
        let section = PersistentSessionPolicySection;
        let context = NodePolicyContext {
            section: "persistent_sessions".to_owned(),
            source_file: "/node/policies/persistent_sessions.yaml".into(),
            signer_fingerprint: "ab".repeat(32),
        };
        let parsed = section.parse(&context, &policy_value()).unwrap();
        let record = parsed
            .as_any()
            .downcast_ref::<PersistentSessionPolicy>()
            .unwrap();
        assert!(record.enabled);
        assert_eq!(record.limits, Some(PersistentSessionPoolLimits::default()));
    }

    #[test]
    fn policy_rejects_unknown_schema_and_incoherent_limits() {
        let mut unknown_schema = policy_value();
        unknown_schema["schema"] = Value::from(2);
        let record: PersistentSessionPolicy = serde_json::from_value(unknown_schema).unwrap();
        assert!(record.validate().is_err());

        let mut incoherent = policy_value();
        incoherent["limits"]["max_total_processes"] = Value::from(0);
        let record: PersistentSessionPolicy = serde_json::from_value(incoherent).unwrap();
        assert!(record.validate().is_err());

        assert!(PersistentSessionPolicy::disabled().validate().is_ok());
    }
}
use std::sync::Arc;
