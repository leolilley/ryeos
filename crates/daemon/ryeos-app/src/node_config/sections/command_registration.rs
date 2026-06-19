use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::node_config::{NodeConfigSection, NodeItemContext, SectionRecord, SectionSourcePolicy};

#[derive(Debug, Clone)]
pub struct CommandRegistrationPolicyRecord {
    pub name: String,
    pub policy: ryeos_runtime::CommandRegistrationPolicy,
    pub source_file: std::path::PathBuf,
}

impl Default for CommandRegistrationPolicyRecord {
    fn default() -> Self {
        Self {
            name: "default".into(),
            policy: ryeos_runtime::CommandRegistrationPolicy::default(),
            source_file: std::path::PathBuf::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommandRegistrationPolicyRecord {
    #[serde(default)]
    claim_rules: Vec<ryeos_runtime::CommandRegistrationRule>,
    #[serde(default)]
    system_source_caps: Vec<String>,
}

pub struct CommandRegistrationSection;

impl NodeConfigSection for CommandRegistrationSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::SystemAndState
    }

    fn parse(&self, ctx: &NodeItemContext, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let raw: RawCommandRegistrationPolicyRecord = serde_json::from_value(body.clone())
            .context("failed to parse command registration policy record")?;

        Ok(Box::new(CommandRegistrationPolicyRecord {
            name: ctx.id.clone(),
            policy: ryeos_runtime::CommandRegistrationPolicy {
                claim_rules: raw.claim_rules,
                system_source_caps: raw.system_source_caps,
            },
            source_file: std::path::PathBuf::new(),
        }))
    }
}

impl SectionRecord for CommandRegistrationPolicyRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
