//! Node-owned declaration of provider-delivered environment names.
//!
//! The names are data in the signed node generation, never discovered from
//! the environment. The daemon takes exactly these names out of its own
//! environment during single-threaded startup and delivers them only through
//! authorized secret delivery. An empty list declares no such source.
//!
//! Every declared name carries its own recipient ceiling: the principals
//! that may receive it, and the capabilities whose holders may receive it.
//! A name with no recipient rule is refused; there is no default recipient.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "provider_environment";
pub const PROVIDER_ENVIRONMENT_POLICY_SCHEMA: u32 = 2;
pub const MAX_PROVIDER_ENVIRONMENT_NAMES: usize = 64;
pub const MAX_PROVIDER_ENVIRONMENT_RECIPIENTS: usize = 64;
pub const MAX_PROVIDER_ENVIRONMENT_RECIPIENT_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEnvironmentPolicy {
    pub schema: u32,
    pub names: Vec<ProviderEnvironmentName>,
}

/// One delivered name and who may receive it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEnvironmentName {
    pub name: String,
    pub recipients: ProviderEnvironmentRecipients,
}

/// Recipient ceiling for one delivered name. A requesting principal is a
/// recipient when it is listed in `principals`, or when its admitted
/// capabilities satisfy any one entry of `capabilities`. Both lists are
/// open strings, sorted and unique; at least one entry is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEnvironmentRecipients {
    pub principals: Vec<String>,
    pub capabilities: Vec<String>,
}

impl ProviderEnvironmentPolicy {
    /// Declared names in sorted order.
    pub fn declared_names(&self) -> Vec<String> {
        self.names.iter().map(|entry| entry.name.clone()).collect()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PROVIDER_ENVIRONMENT_POLICY_SCHEMA {
            bail!("node provider-environment policy schema is not current");
        }
        if self.names.len() > MAX_PROVIDER_ENVIRONMENT_NAMES {
            bail!(
                "node provider-environment policy declares more than {MAX_PROVIDER_ENVIRONMENT_NAMES} names"
            );
        }
        let mut previous: Option<&str> = None;
        for entry in &self.names {
            let name = entry.name.as_str();
            ryeos_vault::policy::validate_key_name(name)
                .with_context(|| format!("node provider-environment name `{name}` is refused"))?;
            // Admitted environment authority carries only canonical names.
            if !name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
            }) {
                bail!("node provider-environment name `{name}` is not a canonical uppercase name");
            }
            if previous.is_some_and(|candidate| candidate >= name) {
                bail!("node provider-environment names must be sorted and unique");
            }
            previous = Some(name);
            entry
                .recipients
                .validate()
                .with_context(|| format!("node provider-environment name `{name}` recipients"))?;
        }
        Ok(())
    }
}

impl ProviderEnvironmentRecipients {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.principals.is_empty() && self.capabilities.is_empty() {
            bail!("at least one recipient principal or capability is required");
        }
        validate_recipient_list("principal", &self.principals)?;
        validate_recipient_list("capability", &self.capabilities)?;
        for principal in &self.principals {
            if principal.chars().any(char::is_whitespace) {
                bail!("recipient principal `{principal}` contains whitespace");
            }
        }
        for capability in &self.capabilities {
            // A recipient capability names one exact capability. Holders of
            // a granted wildcard covering it still qualify; a wildcard here
            // would instead admit every holder of any matching grant.
            if capability.contains('*') || capability.contains('?') {
                bail!("recipient capability `{capability}` must not be a wildcard pattern");
            }
            ryeos_runtime::authorizer::validate_scope_pattern(capability).map_err(|error| {
                anyhow::anyhow!("recipient capability `{capability}` is refused: {error}")
            })?;
            if ryeos_runtime::authorizer::Capability::parse(capability).is_err() {
                bail!("recipient capability `{capability}` must name verb, kind and subject");
            }
        }
        Ok(())
    }
}

fn validate_recipient_list(label: &str, values: &[String]) -> anyhow::Result<()> {
    if values.len() > MAX_PROVIDER_ENVIRONMENT_RECIPIENTS {
        bail!("more than {MAX_PROVIDER_ENVIRONMENT_RECIPIENTS} recipient {label} entries");
    }
    let mut previous: Option<&str> = None;
    for value in values {
        if value.is_empty() || value.trim() != value {
            bail!("recipient {label} entries must be non-empty and trimmed");
        }
        if value.len() > MAX_PROVIDER_ENVIRONMENT_RECIPIENT_BYTES {
            bail!(
                "recipient {label} `{value}` exceeds {MAX_PROVIDER_ENVIRONMENT_RECIPIENT_BYTES} bytes"
            );
        }
        if value.chars().any(char::is_control) {
            bail!("recipient {label} entries must not contain control characters");
        }
        if previous.is_some_and(|candidate| candidate >= value.as_str()) {
            bail!("recipient {label} entries must be sorted and unique");
        }
        previous = Some(value);
    }
    Ok(())
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

    fn principal_recipient() -> ProviderEnvironmentRecipients {
        ProviderEnvironmentRecipients {
            principals: vec!["fp:operator".to_owned()],
            capabilities: Vec::new(),
        }
    }

    fn policy(names: &[&str]) -> ProviderEnvironmentPolicy {
        ProviderEnvironmentPolicy {
            schema: PROVIDER_ENVIRONMENT_POLICY_SCHEMA,
            names: names
                .iter()
                .map(|name| ProviderEnvironmentName {
                    name: (*name).to_owned(),
                    recipients: principal_recipient(),
                })
                .collect(),
        }
    }

    fn with_recipients(principals: &[&str], capabilities: &[&str]) -> ProviderEnvironmentPolicy {
        ProviderEnvironmentPolicy {
            schema: PROVIDER_ENVIRONMENT_POLICY_SCHEMA,
            names: vec![ProviderEnvironmentName {
                name: "OPENAI_API_KEY".to_owned(),
                recipients: ProviderEnvironmentRecipients {
                    principals: principals.iter().map(|value| (*value).to_owned()).collect(),
                    capabilities: capabilities.iter().map(|value| (*value).to_owned()).collect(),
                },
            }],
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
        let mut previous = policy(&[]);
        previous.schema = 1;
        assert!(previous.validate().is_err());
        let names = (0..=MAX_PROVIDER_ENVIRONMENT_NAMES)
            .map(|index| format!("NAME_{index:03}"))
            .collect::<Vec<_>>();
        let names = names.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(policy(&names).validate().is_err());
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
    fn every_name_requires_a_bounded_recipient_rule() {
        let error = with_recipients(&[], &[]).validate().unwrap_err();
        assert!(
            format!("{error:#}").contains("at least one recipient"),
            "{error:#}"
        );
        with_recipients(&["fp:a", "fp:b"], &[]).validate().unwrap();
        with_recipients(&[], &["ryeos.receive.provider_env.openai_api_key"])
            .validate()
            .unwrap();
        with_recipients(&["fp:a"], &["ryeos.receive.provider_env.openai_api_key"])
            .validate()
            .unwrap();
        for (principals, capabilities) in [
            (&["fp:b", "fp:a"][..], &[][..]),
            (&["fp:a", "fp:a"][..], &[][..]),
            (&[""][..], &[][..]),
            (&[" fp:a"][..], &[][..]),
            (&["fp a"][..], &[][..]),
            (&[][..], &["*"][..]),
            (&[][..], &["ryeos.receive.provider_env.*"][..]),
            (&[][..], &["receive.provider_env"][..]),
            (&[][..], &["ryeos.receive.provider_env"][..]),
            (&[][..], &["ryeos.receive.provider_env.OPENAI_API_KEY"][..]),
        ] {
            assert!(
                with_recipients(principals, capabilities).validate().is_err(),
                "{principals:?} {capabilities:?}"
            );
        }
        let principals = (0..=MAX_PROVIDER_ENVIRONMENT_RECIPIENTS)
            .map(|index| format!("fp:{index:03}"))
            .collect::<Vec<_>>();
        let principals = principals.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(with_recipients(&principals, &[]).validate().is_err());
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
            .parse(&context, &serde_json::json!({"schema": 2, "names": []}))
            .unwrap();
        section
            .parse(
                &context,
                &serde_json::json!({"schema": 2, "names": [{
                    "name": "OPENAI_API_KEY",
                    "recipients": {"principals": ["fp:operator"], "capabilities": []}
                }]}),
            )
            .unwrap();
        assert!(section.parse(&context, &serde_json::json!({"schema": 2})).is_err());
        assert!(
            section
                .parse(
                    &context,
                    &serde_json::json!({"schema": 2, "names": [], "values": {}})
                )
                .is_err()
        );
        // The previous bare-name shape is not read.
        assert!(
            section
                .parse(
                    &context,
                    &serde_json::json!({"schema": 2, "names": ["OPENAI_API_KEY"]})
                )
                .is_err()
        );
        assert!(
            section
                .parse(
                    &context,
                    &serde_json::json!({"schema": 2, "names": [{"name": "OPENAI_API_KEY"}]})
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
