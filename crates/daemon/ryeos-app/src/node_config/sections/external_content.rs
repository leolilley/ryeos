//! Node-owned external-content import roots and resource policy.
//!
//! This section is system/state-only. Bundle and project content cannot add a
//! host path, loosen capture exclusions, or increase a node storage budget.

use std::collections::BTreeMap;
use std::path::{Component, PathBuf};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_config::{NodeConfigSection, NodeItemContext, SectionRecord, SectionSourcePolicy};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentImportPolicyRecord {
    pub schema: u32,
    pub roots: BTreeMap<String, ExternalContentImportRoot>,
    pub limits: ExternalContentImportLimits,
    /// Optional node-owned permission and ceilings for activation from signed
    /// trusted-bundle declarations. Absence disables managed acquisition while
    /// preserving the independent local named-root import primitive.
    #[serde(default)]
    pub managed_activation: Option<ManagedExternalContentActivationPolicy>,
    #[serde(skip)]
    pub source_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentImportRoot {
    pub path: PathBuf,
    pub containing_device: u64,
    pub root_inode: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentImportLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub store_budget_bytes: u64,
    pub minimum_free_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedExternalContentActivationPolicy {
    pub allow_online: bool,
    #[serde(default)]
    pub allowed_https_hosts: Vec<String>,
    /// Redirects are denied when older or deliberately strict node policy
    /// omits this field. Adding the field cannot widen existing authority.
    #[serde(default)]
    pub max_redirects: usize,
    pub max_archives: usize,
    pub max_compressed_bytes: u64,
    pub max_expanded_bytes: u64,
    pub max_members: usize,
    pub max_member_bytes: u64,
    /// V1 deliberately serializes managed activation. The field is retained
    /// in the node-owned contract so a future protocol can widen it without
    /// conflating that change with bundle authority.
    pub max_concurrent_activations: usize,
    pub cache_budget_bytes: u64,
    pub store_budget_bytes: u64,
    pub minimum_free_bytes: u64,
    pub max_attempts: u64,
}

pub struct ExternalContentImportPolicySection;

impl NodeConfigSection for ExternalContentImportPolicySection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::SystemAndState
    }

    fn operator_policy_item_id(&self) -> Option<&'static str> {
        Some("policy")
    }

    fn parse(&self, ctx: &NodeItemContext, body: &Value) -> anyhow::Result<Box<dyn SectionRecord>> {
        if ctx.id != "policy" {
            bail!(
                "external-content import policy filename must be `policy`, got `{}`",
                ctx.id
            );
        }
        let mut record: ExternalContentImportPolicyRecord =
            serde_json::from_value(body.clone()).context("parse external-content import policy")?;
        record.source_file = ctx.source_file.clone();
        record.validate()?;
        Ok(Box::new(record))
    }
}

impl ExternalContentImportPolicyRecord {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("external-content import policy schema is not current");
        }
        for (name, root) in &self.roots {
            validate_root_name(name)?;
            if !root.path.is_absolute()
                || root.path.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::CurDir | Component::Prefix(_)
                    )
                })
            {
                bail!("external-content import root `{name}` must be an absolute normalized path");
            }
            if root.containing_device == 0 || root.root_inode == 0 {
                bail!("external-content import root `{name}` has an invalid filesystem identity");
            }
        }
        self.limits.validate()?;
        if let Some(managed) = self.managed_activation.as_ref() {
            managed.validate()?;
        }
        Ok(())
    }
}

impl ExternalContentImportLimits {
    pub fn validate(&self) -> anyhow::Result<()> {
        let limits = self;
        if limits.max_depth == 0 || limits.max_depth > 256 {
            bail!("external-content import max_depth is outside the supported range");
        }
        if limits.max_entries == 0
            || limits.max_entries > ryeos_state::objects::MAX_EXTERNAL_CONTENT_ENTRIES
        {
            bail!("external-content import max_entries is outside the manifest bound");
        }
        if limits.max_file_bytes == 0
            || limits.max_file_bytes > ryeos_state::objects::MAX_LARGE_CONTENT_FILE_BYTES
        {
            bail!("external-content import max_file_bytes is outside the manifest bound");
        }
        if limits.max_total_bytes == 0
            || limits.max_total_bytes > ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES
            || limits.max_file_bytes > limits.max_total_bytes
        {
            bail!("external-content import max_total_bytes is incoherent");
        }
        if limits.store_budget_bytes == 0
            || limits.max_total_bytes > limits.store_budget_bytes
            || limits.minimum_free_bytes == 0
        {
            bail!("external-content import storage budget is incoherent");
        }
        Ok(())
    }
}

impl ManagedExternalContentActivationPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.allowed_https_hosts.len() > 64 {
            bail!("managed external-content host allowlist exceeds 64 entries");
        }
        let mut seen = std::collections::BTreeSet::new();
        for host in &self.allowed_https_hosts {
            if host.is_empty()
                || host.len() > 253
                || host.bytes().any(|byte| {
                    !(byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'-'))
                })
                || host.starts_with('.')
                || host.ends_with('.')
                || host.contains("..")
                || !seen.insert(host)
            {
                bail!("managed external-content HTTPS host is not canonical");
            }
        }
        if self.allow_online && self.allowed_https_hosts.is_empty() {
            bail!("online managed external-content activation requires an HTTPS host allowlist");
        }
        if self.max_redirects > 4 {
            bail!("managed external-content max_redirects exceeds 4");
        }
        if self.max_archives == 0 || self.max_archives > 8 {
            bail!("managed external-content max_archives is outside 1..=8");
        }
        let maximum_archive_entries =
            (ryeos_state::objects::MAX_EXTERNAL_CONTENT_ENTRIES + 1).saturating_mul(8);
        if self.max_members == 0 || self.max_members > maximum_archive_entries {
            bail!("managed external-content max_members is outside 1..={maximum_archive_entries}");
        }
        if self.max_compressed_bytes == 0
            || self.max_expanded_bytes == 0
            || self.max_member_bytes == 0
            || self.max_compressed_bytes > self.cache_budget_bytes
            || self.max_member_bytes > self.max_expanded_bytes
            || self.max_expanded_bytes
                > ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES.saturating_mul(8)
        {
            bail!("managed external-content byte ceilings are incoherent");
        }
        if self.cache_budget_bytes == 0
            || self.store_budget_bytes == 0
            || self.max_expanded_bytes > self.store_budget_bytes
            || self.minimum_free_bytes == 0
        {
            bail!("managed external-content storage reserve is incoherent");
        }
        if self.max_concurrent_activations != 1 {
            bail!("managed external-content v1 requires exactly one concurrent activation");
        }
        if self.max_attempts == 0 || self.max_attempts > 16 {
            bail!("managed external-content max_attempts is outside 1..=16");
        }
        Ok(())
    }
}

impl SectionRecord for ExternalContentImportPolicyRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(crate) fn validate_root_name(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("external-content import root name `{value}` is invalid");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_rejects_relative_roots_and_unbounded_limits() {
        assert_eq!(
            ExternalContentImportPolicySection.operator_policy_item_id(),
            Some("policy")
        );
        let policy = ExternalContentImportPolicyRecord {
            schema: 1,
            roots: BTreeMap::from([(
                "source".to_owned(),
                ExternalContentImportRoot {
                    path: PathBuf::from("relative"),
                    containing_device: 1,
                    root_inode: 1,
                },
            )]),
            limits: ExternalContentImportLimits {
                max_depth: 8,
                max_entries: 8,
                max_file_bytes: 1024,
                max_total_bytes: 1024,
                store_budget_bytes: 2048,
                minimum_free_bytes: 1024,
            },
            managed_activation: None,
            source_file: PathBuf::new(),
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn policy_requires_an_exact_root_filesystem_identity() {
        let value = serde_json::json!({
            "schema": 1,
            "roots": {
                "source": {"path": "/srv/source", "containing_device": 7}
            },
            "limits": {
                "max_depth": 8,
                "max_entries": 8,
                "max_file_bytes": 1024,
                "max_total_bytes": 1024,
                "store_budget_bytes": 2048,
                "minimum_free_bytes": 1024
            },
            "managed_activation": null
        });
        assert!(serde_json::from_value::<ExternalContentImportPolicyRecord>(value).is_err());
    }

    #[test]
    fn managed_activation_requires_explicit_bounded_online_hosts() {
        let policy = ManagedExternalContentActivationPolicy {
            allow_online: true,
            allowed_https_hosts: Vec::new(),
            max_redirects: 0,
            max_archives: 1,
            max_compressed_bytes: 1024,
            max_expanded_bytes: 2048,
            max_members: 4,
            max_member_bytes: 1024,
            max_concurrent_activations: 1,
            cache_budget_bytes: 4096,
            store_budget_bytes: 8192,
            minimum_free_bytes: 1024,
            max_attempts: 3,
        };
        assert!(policy.validate().is_err());
        let mut admitted = policy;
        admitted.allowed_https_hosts = vec!["releases.example.test".to_owned()];
        admitted.validate().unwrap();
        admitted.max_concurrent_activations = 2;
        assert!(admitted.validate().is_err());
    }
}
