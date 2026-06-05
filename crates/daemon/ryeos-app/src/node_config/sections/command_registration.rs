use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::node_config::{NodeConfigSection, SectionRecord, SectionSourcePolicy};

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
    #[allow(dead_code)]
    section: String,
    #[serde(default)]
    name: Option<String>,
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

    fn parse(&self, name: &str, body: &Value) -> Result<Box<dyn SectionRecord>> {
        let raw: RawCommandRegistrationPolicyRecord = serde_json::from_value(body.clone())
            .context("failed to parse command registration policy record")?;
        let declared_name = raw.name.unwrap_or_else(|| name.to_string());
        if declared_name != name {
            bail!(
                "command registration policy declares name '{}' but filename is '{}'",
                declared_name,
                name
            );
        }

        Ok(Box::new(CommandRegistrationPolicyRecord {
            name: declared_name,
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
