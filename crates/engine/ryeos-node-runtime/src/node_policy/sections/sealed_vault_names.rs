//! Per-name ceilings on the node's sealed vault.
//!
//! The sealed vault is one store shared by every principal that holds the
//! vault service capabilities. This section bounds, per name, who may receive
//! the value into a launch (`recipients`) and who may set or delete it
//! (`writers`). Both are principals and exact capabilities, the same shape a
//! provider-delivered environment name uses.
//!
//! A name with no entry has no ceiling: only the node's configured local
//! operator may receive or write it. An empty list is therefore the safe
//! default, not an open one. Ceilings are explicit operator data; nothing
//! implies the operator inside a listed name, so an operator that should keep
//! writing a listed name lists itself.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::sections::recipients::NameRecipients;
use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "sealed_vault_names";
pub const SEALED_VAULT_NAMES_POLICY_SCHEMA: u32 = 1;
pub const MAX_SEALED_VAULT_NAMES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedVaultNamesPolicy {
    pub schema: u32,
    pub names: Vec<SealedVaultName>,
}

/// One sealed vault name and the principals that may act on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedVaultName {
    pub name: String,
    /// Who may receive the value into a launch.
    pub recipients: NameRecipients,
    /// Who may set or delete the value.
    pub writers: NameRecipients,
}

impl SealedVaultNamesPolicy {
    /// A policy that lists no names: every vault name is operator-only.
    pub fn operator_only() -> Self {
        Self {
            schema: SEALED_VAULT_NAMES_POLICY_SCHEMA,
            names: Vec::new(),
        }
    }

    pub fn entry(&self, name: &str) -> Option<&SealedVaultName> {
        self.names
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.names[index])
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != SEALED_VAULT_NAMES_POLICY_SCHEMA {
            bail!("node sealed-vault-names policy schema is not current");
        }
        if self.names.len() > MAX_SEALED_VAULT_NAMES {
            bail!("node sealed-vault-names policy lists more than {MAX_SEALED_VAULT_NAMES} names");
        }
        let mut previous: Option<&str> = None;
        for entry in &self.names {
            let name = entry.name.as_str();
            ryeos_vault::policy::validate_key_name(name)
                .with_context(|| format!("node sealed vault name `{name}` is refused"))?;
            if ryeos_vault::policy::is_internal_vault_key(name) {
                bail!("node sealed vault name `{name}` is an internal vault key");
            }
            if previous.is_some_and(|candidate| candidate >= name) {
                bail!("node sealed vault names must be sorted and unique");
            }
            previous = Some(name);
            entry
                .recipients
                .validate()
                .with_context(|| format!("node sealed vault name `{name}` recipients"))?;
            entry
                .writers
                .validate()
                .with_context(|| format!("node sealed vault name `{name}` writers"))?;
        }
        Ok(())
    }
}

impl TypedNodePolicy for SealedVaultNamesPolicy {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

pub struct SealedVaultNamesPolicySection;

impl NodePolicySection for SealedVaultNamesPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: SealedVaultNamesPolicy =
            serde_json::from_value(body.clone()).context("parse node sealed-vault-names policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipients(principals: &[&str], capabilities: &[&str]) -> NameRecipients {
        NameRecipients {
            principals: principals.iter().map(|value| (*value).to_owned()).collect(),
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    fn entry(name: &str) -> SealedVaultName {
        SealedVaultName {
            name: name.to_owned(),
            recipients: recipients(&["fp:operator"], &[]),
            writers: recipients(&["fp:operator"], &[]),
        }
    }

    fn policy(names: Vec<SealedVaultName>) -> SealedVaultNamesPolicy {
        SealedVaultNamesPolicy {
            schema: SEALED_VAULT_NAMES_POLICY_SCHEMA,
            names,
        }
    }

    #[test]
    fn sealed_vault_names_rejects_wildcard_capability() {
        for pattern in ["ryeos.receive.vault.*", "*", "ryeos.receive.vault.api_?"] {
            let mut recipient_wildcard = entry("API_KEY");
            recipient_wildcard.recipients = recipients(&[], &[pattern]);
            assert!(
                policy(vec![recipient_wildcard]).validate().is_err(),
                "{pattern}"
            );
            let mut writer_wildcard = entry("API_KEY");
            writer_wildcard.writers = recipients(&[], &[pattern]);
            assert!(
                policy(vec![writer_wildcard]).validate().is_err(),
                "{pattern}"
            );
        }
        let mut exact = entry("API_KEY");
        exact.recipients = recipients(&[], &["ryeos.receive.vault.api_key"]);
        exact.writers = recipients(&[], &["ryeos.write.vault.api_key"]);
        policy(vec![exact]).validate().unwrap();
    }

    #[test]
    fn sealed_vault_names_requires_some_recipient() {
        let mut no_recipient = entry("API_KEY");
        no_recipient.recipients = recipients(&[], &[]);
        let error = policy(vec![no_recipient]).validate().unwrap_err();
        assert!(format!("{error:#}").contains("recipients"), "{error:#}");
        let mut no_writer = entry("API_KEY");
        no_writer.writers = recipients(&[], &[]);
        let error = policy(vec![no_writer]).validate().unwrap_err();
        assert!(format!("{error:#}").contains("writers"), "{error:#}");
    }

    #[test]
    fn sealed_vault_names_sorted_unique() {
        policy(Vec::new()).validate().unwrap();
        policy(vec![entry("A_KEY"), entry("B_KEY")])
            .validate()
            .unwrap();
        assert!(
            policy(vec![entry("B_KEY"), entry("A_KEY")])
                .validate()
                .is_err()
        );
        assert!(
            policy(vec![entry("A_KEY"), entry("A_KEY")])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn sealed_vault_names_refuse_protected_and_internal_names() {
        for name in ["", "PATH", "NAME=VALUE", "WITH SPACE"] {
            assert!(policy(vec![entry(name)]).validate().is_err(), "{name:?}");
        }
        let internal = format!(
            "{}bundle__ns__KEY",
            ryeos_vault::policy::INTERNAL_RUNTIME_VAULT_PREFIX
        );
        assert!(policy(vec![entry(&internal)]).validate().is_err());
    }

    #[test]
    fn sealed_vault_names_policy_is_bounded_and_versioned() {
        let mut stale = policy(Vec::new());
        stale.schema = 0;
        assert!(stale.validate().is_err());
        let names = (0..=MAX_SEALED_VAULT_NAMES)
            .map(|index| entry(&format!("NAME_{index:04}")))
            .collect();
        assert!(policy(names).validate().is_err());
    }

    #[test]
    fn the_section_is_mandatory_and_rejects_unknown_fields() {
        let section = SealedVaultNamesPolicySection;
        let context = NodePolicyContext {
            section: SECTION_NAME.to_owned(),
            source_file: std::path::PathBuf::from("sealed_vault_names.yaml"),
            signer_fingerprint: String::new(),
        };
        section
            .parse(&context, &serde_json::json!({"schema": 1, "names": []}))
            .unwrap();
        section
            .parse(
                &context,
                &serde_json::json!({"schema": 1, "names": [{
                    "name": "API_KEY",
                    "recipients": {"principals": ["fp:operator"], "capabilities": []},
                    "writers": {"principals": ["fp:operator"], "capabilities": []}
                }]}),
            )
            .unwrap();
        assert!(
            section
                .parse(&context, &serde_json::json!({"schema": 1}))
                .is_err()
        );
        assert!(
            section
                .parse(
                    &context,
                    &serde_json::json!({"schema": 1, "names": [], "default": "open"})
                )
                .is_err()
        );
        // A name must carry its writers; there is no implied default.
        assert!(
            section
                .parse(
                    &context,
                    &serde_json::json!({"schema": 1, "names": [{
                        "name": "API_KEY",
                        "recipients": {"principals": ["fp:operator"], "capabilities": []}
                    }]})
                )
                .is_err()
        );
    }

    #[test]
    fn entries_are_found_by_name() {
        let policy = policy(vec![entry("A_KEY"), entry("C_KEY")]);
        assert!(policy.entry("A_KEY").is_some());
        assert!(policy.entry("B_KEY").is_none());
        assert!(
            SealedVaultNamesPolicy::operator_only()
                .entry("A_KEY")
                .is_none()
        );
    }
}
