use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

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
    /// Every bundle selected for initialization must have an explicit entry,
    /// even when that bundle receives no command-registration capabilities.
    pub fn require_bundle_sources<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        for name in names {
            if !self.bundle_source_caps.contains_key(name) {
                anyhow::bail!(
                    "command registration policy has no explicit authority for active bundle `{name}`"
                );
            }
        }
        Ok(())
    }

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
        anyhow::ensure!(
            raw.schema == 1,
            "command registration policy schema is not current"
        );
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
            let canonical = caps
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
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

#[cfg(test)]
mod tests {
    use super::CommandRegistrationAuthority;

    #[test]
    fn selected_bundle_requires_explicit_entry_even_when_caps_are_empty() {
        let authority = CommandRegistrationAuthority {
            claim_rules: Vec::new(),
            system_source_caps: Vec::new(),
            bundle_source_caps: [("core".to_owned(), Vec::new())].into(),
        };
        authority.require_bundle_sources(["core"]).unwrap();
        let error = authority
            .require_bundle_sources(["core", "render-sandbox"])
            .unwrap_err();
        assert!(
            error.to_string().contains("active bundle `render-sandbox`"),
            "{error:#}"
        );
    }
}
