use std::sync::Arc;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "command_registration";

#[derive(Debug, Clone)]
pub struct CommandRegistrationAuthority {
    pub claim_rules: Vec<ryeos_runtime::CommandRegistrationRule>,
    pub system_source_caps: Vec<String>,
    pub bundle_source_caps: BTreeMap<String, Vec<String>>,
}

impl CommandRegistrationAuthority {
    pub fn runtime_policy(&self) -> ryeos_runtime::CommandRegistrationPolicy {
        ryeos_runtime::CommandRegistrationPolicy {
            claim_rules: self.claim_rules.clone(),
            system_source_caps: self.system_source_caps.clone(),
        }
    }
}

impl TypedNodePolicy for CommandRegistrationAuthority {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandRegistrationPolicyDocument {
    schema: u32,
    claim_rules: Vec<ryeos_runtime::CommandRegistrationRule>,
    system_source_caps: Vec<String>,
    bundle_source_caps: BTreeMap<String, Vec<String>>,
}

pub struct CommandRegistrationPolicySection;

impl NodePolicySection for CommandRegistrationPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> Result<Arc<dyn ErasedNodePolicy>> {
        let raw: CommandRegistrationPolicyDocument = serde_json::from_value(body.clone())
            .context("failed to parse command registration policy record")?;
        anyhow::ensure!(raw.schema == 1, "command registration policy schema is not current");
        let canonical_system_caps = raw
            .system_source_caps
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        anyhow::ensure!(
            canonical_system_caps == raw.system_source_caps,
            "command registration system_source_caps must be sorted and unique"
        );
        for (bundle, caps) in &raw.bundle_source_caps {
            crate::node_policy::generation::validate_policy_name("bundle policy", bundle)?;
            let canonical = caps.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
            anyhow::ensure!(
                &canonical == caps,
                "command registration caps for bundle `{bundle}` must be sorted and unique"
            );
        }

        Ok(Arc::new(CommandRegistrationAuthority {
            claim_rules: raw.claim_rules,
            system_source_caps: raw.system_source_caps,
            bundle_source_caps: raw.bundle_source_caps,
        }))
    }
}
