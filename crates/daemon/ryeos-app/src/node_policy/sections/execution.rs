//! Node-owned execution admission and host-environment authority.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "execution";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExecutionAdmissionPolicy {
    pub schema: u32,
    pub max_live_fanout: u32,
    pub max_private_materialization_copy_bytes: u64,
    pub host_env_passthrough: Vec<String>,
}

impl NodeExecutionAdmissionPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node execution policy schema is not current");
        }
        if self.max_live_fanout == 0 {
            bail!("node execution max_live_fanout must be greater than zero");
        }
        if self.max_private_materialization_copy_bytes == 0 {
            bail!("node execution private materialization copy limit must be greater than zero");
        }
        let canonical = self
            .host_env_passthrough
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if canonical != self.host_env_passthrough {
            bail!("node execution host-env allowlist must be sorted and unique");
        }
        ryeos_engine::runtime::HostEnvBindings::from_allowlist(
            self.host_env_passthrough.iter().cloned(),
        )
        .map_err(anyhow::Error::from)
        .context("validate node execution host-env allowlist")?;
        Ok(())
    }

    pub fn host_env_bindings(&self) -> anyhow::Result<ryeos_engine::runtime::HostEnvBindings> {
        ryeos_engine::runtime::HostEnvBindings::from_allowlist(
            self.host_env_passthrough.iter().cloned(),
        )
        .map_err(anyhow::Error::from)
        .context("resolve node execution host-env bindings")
    }
}

impl TypedNodePolicy for NodeExecutionAdmissionPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct NodeExecutionPolicySection;

impl NodePolicySection for NodeExecutionPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: NodeExecutionAdmissionPolicy =
            serde_json::from_value(body.clone()).context("parse node execution policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_policy() -> NodeExecutionAdmissionPolicy {
        NodeExecutionAdmissionPolicy {
            schema: 1,
            max_live_fanout: 8,
            max_private_materialization_copy_bytes: 17_179_869_184,
            host_env_passthrough: Vec::new(),
        }
    }

    #[test]
    fn limits_are_explicit_and_positive() {
        let mut policy = valid_policy();
        assert!(policy.validate().is_ok());
        policy.max_live_fanout = 0;
        assert!(policy.validate().is_err());
        policy = valid_policy();
        policy.max_private_materialization_copy_bytes = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn host_environment_allowlist_is_canonical() {
        let mut policy = valid_policy();
        policy.host_env_passthrough = vec!["PATH".into(), "PATH".into()];
        assert!(policy.validate().is_err());
        policy.host_env_passthrough = vec!["Z_VALUE".into(), "A_VALUE".into()];
        assert!(policy.validate().is_err());
    }
}
