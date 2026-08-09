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

pub struct ExternalContentImportPolicySection;

impl NodeConfigSection for ExternalContentImportPolicySection {
    fn source_policy(&self) -> SectionSourcePolicy {
        SectionSourcePolicy::SystemAndState
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
        let limits = &self.limits;
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

impl SectionRecord for ExternalContentImportPolicyRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn validate_root_name(value: &str) -> anyhow::Result<()> {
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
            }
        });
        assert!(serde_json::from_value::<ExternalContentImportPolicyRecord>(value).is_err());
    }
}
