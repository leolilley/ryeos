//! Node-owned declaration of provider-delivered environment names.
//!
//! The names are data in the signed node generation, never discovered from
//! the environment. The daemon takes exactly these names out of its own
//! environment during single-threaded startup and delivers them only through
//! authorized secret delivery. An empty list declares no such source.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "provider_environment";
pub const MAX_PROVIDER_ENVIRONMENT_NAMES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEnvironmentPolicy {
    pub schema: u32,
    pub names: Vec<String>,
}

impl ProviderEnvironmentPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("node provider-environment policy schema is not current");
        }
        if self.names.len() > MAX_PROVIDER_ENVIRONMENT_NAMES {
            bail!(
                "node provider-environment policy declares more than {MAX_PROVIDER_ENVIRONMENT_NAMES} names"
            );
        }
        let mut previous: Option<&str> = None;
        for name in &self.names {
            ryeos_vault::policy::validate_key_name(name)
                .with_context(|| format!("node provider-environment name `{name}` is refused"))?;
            // Admitted environment authority carries only canonical names.
            if !name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
            }) {
                bail!("node provider-environment name `{name}` is not a canonical uppercase name");
            }
            if previous.is_some_and(|candidate| candidate >= name.as_str()) {
                bail!("node provider-environment names must be sorted and unique");
            }
            previous = Some(name);
        }
        Ok(())
    }
}

impl TypedNodePolicy for ProviderEnvironmentPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct ProviderEnvironmentPolicySection;

impl NodePolicySection for ProviderEnvironmentPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: ProviderEnvironmentPolicy = serde_json::from_value(body.clone())
            .context("parse node provider-environment policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(names: &[&str]) -> ProviderEnvironmentPolicy {
        ProviderEnvironmentPolicy {
            schema: 1,
            names: names.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    #[test]
    fn declared_names_are_canonical_and_bounded() {
        policy(&[]).validate().unwrap();
        policy(&["ANTHROPIC_API_KEY", "OPENAI_API_KEY"])
            .validate()
            .unwrap();
        assert!(policy(&["OPENAI_API_KEY", "ANTHROPIC_API_KEY"]).validate().is_err());
        assert!(policy(&["OPENAI_API_KEY", "OPENAI_API_KEY"]).validate().is_err());
        let mut current = policy(&[]);
        current.schema = 2;
        assert!(current.validate().is_err());
        let names = (0..=MAX_PROVIDER_ENVIRONMENT_NAMES)
            .map(|index| format!("NAME_{index:03}"))
            .collect::<Vec<_>>();
        let oversized = ProviderEnvironmentPolicy { schema: 1, names };
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn protected_and_malformed_names_are_refused() {
        for name in [
            "",
            "PATH",
            "HOME",
            "LD_PRELOAD",
            "NAME=VALUE",
            "WITH SPACE",
            "lower_case",
            "1_LEADING_DIGIT",
        ] {
            assert!(policy(&[name]).validate().is_err(), "{name:?}");
        }
    }

    #[test]
    fn the_section_is_mandatory_and_rejects_unknown_fields() {
        let section = ProviderEnvironmentPolicySection;
        let context = NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: std::path::PathBuf::from("provider_environment.yaml"),
            signer_fingerprint: String::new(),
        };
        section
            .parse(&context, &serde_json::json!({"schema": 1, "names": []}))
            .unwrap();
        assert!(section.parse(&context, &serde_json::json!({"schema": 1})).is_err());
        assert!(
            section
                .parse(
                    &context,
                    &serde_json::json!({"schema": 1, "names": [], "values": {}})
                )
                .is_err()
        );
        assert!(
            crate::node_policy::NodePolicyTable::new()
                .get(SECTION_NAME)
                .is_some()
        );
    }
}
