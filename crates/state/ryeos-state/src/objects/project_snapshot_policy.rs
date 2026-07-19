//! Immutable effective policy captured with a project snapshot.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

use super::thread_snapshot::validate_canonical_hash;
use crate::ignore::{IgnoreConfig, IgnoreMatcher};
use crate::project_sync::ProjectSyncScope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSnapshotPolicy {
    pub sync_scope: ProjectSyncScope,
    pub language_version: u32,
    pub ryeos_floor_version: u32,
    pub ryeos_floor_rules: Vec<String>,
    pub project_exclusions: Vec<String>,
    pub node_additions: Vec<String>,
    pub source_hashes: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectSnapshotPolicyWire {
    kind: String,
    schema: u32,
    sync_scope: ProjectSyncScope,
    language_version: u32,
    ryeos_floor_version: u32,
    ryeos_floor_rules: Vec<String>,
    project_exclusions: Vec<String>,
    node_additions: Vec<String>,
    source_hashes: BTreeMap<String, String>,
}

impl ProjectSnapshotPolicy {
    pub const SCHEMA: u32 = 1;
    pub const LANGUAGE_VERSION: u32 = 1;
    pub const RYEOS_FLOOR_VERSION: u32 = 1;

    pub fn new(
        sync_scope: ProjectSyncScope,
        project_exclusions: Vec<String>,
        node_additions: Vec<String>,
        source_hashes: BTreeMap<String, String>,
    ) -> anyhow::Result<Self> {
        let policy = Self {
            sync_scope,
            language_version: Self::LANGUAGE_VERSION,
            ryeos_floor_version: Self::RYEOS_FLOOR_VERSION,
            ryeos_floor_rules: crate::project_sync::snapshot_floor_rules(),
            project_exclusions: normalize_patterns(project_exclusions)?,
            node_additions: normalize_patterns(node_additions)?,
            source_hashes,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn from_matcher(
        sync_scope: ProjectSyncScope,
        matcher: &IgnoreMatcher,
    ) -> anyhow::Result<Self> {
        Self::new(
            sync_scope,
            Vec::new(),
            matcher.canonical_patterns().to_vec(),
            crate::project_sync::absent_project_snapshot_source_hashes(matcher)?,
        )
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.language_version != Self::LANGUAGE_VERSION {
            anyhow::bail!(
                "project_snapshot_policy language_version mismatch: expected {}, got {}",
                Self::LANGUAGE_VERSION,
                self.language_version
            );
        }
        if self.ryeos_floor_version != Self::RYEOS_FLOOR_VERSION {
            anyhow::bail!(
                "project_snapshot_policy floor_version mismatch: expected {}, got {}",
                Self::RYEOS_FLOOR_VERSION,
                self.ryeos_floor_version
            );
        }
        if self.ryeos_floor_rules != crate::project_sync::snapshot_floor_rules() {
            anyhow::bail!("project_snapshot_policy does not match the current safety floor");
        }
        ensure_canonical_patterns("project exclusions", &self.project_exclusions)?;
        ensure_canonical_patterns("node additions", &self.node_additions)?;
        for (source, hash) in &self.source_hashes {
            super::validate_trimmed_control_free("policy source label", source, false)?;
            validate_canonical_hash("policy source hash", hash)?;
        }
        Ok(())
    }

    pub fn matcher(&self) -> anyhow::Result<IgnoreMatcher> {
        let mut patterns = self.project_exclusions.clone();
        patterns.extend(self.node_additions.iter().cloned());
        IgnoreMatcher::from_config(&IgnoreConfig { patterns })
    }

    pub fn to_value(&self) -> Value {
        json!({
            "kind": "project_snapshot_policy",
            "schema": Self::SCHEMA,
            "sync_scope": self.sync_scope,
            "language_version": self.language_version,
            "ryeos_floor_version": self.ryeos_floor_version,
            "ryeos_floor_rules": self.ryeos_floor_rules,
            "project_exclusions": self.project_exclusions,
            "node_additions": self.node_additions,
            "source_hashes": self.source_hashes,
        })
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let wire: ProjectSnapshotPolicyWire = serde_json::from_value(value.clone())
            .context("failed to deserialize project_snapshot_policy schema 1")?;
        if wire.kind != "project_snapshot_policy" {
            anyhow::bail!(
                "project_snapshot_policy kind mismatch: expected project_snapshot_policy, got {}",
                wire.kind
            );
        }
        if wire.schema != Self::SCHEMA {
            anyhow::bail!(
                "project_snapshot_policy schema mismatch: expected {}, got {}",
                Self::SCHEMA,
                wire.schema
            );
        }
        let policy = Self {
            sync_scope: wire.sync_scope,
            language_version: wire.language_version,
            ryeos_floor_version: wire.ryeos_floor_version,
            ryeos_floor_rules: wire.ryeos_floor_rules,
            project_exclusions: wire.project_exclusions,
            node_additions: wire.node_additions,
            source_hashes: wire.source_hashes,
        };
        policy.validate()?;
        Ok(policy)
    }
}

fn normalize_patterns(patterns: Vec<String>) -> anyhow::Result<Vec<String>> {
    IgnoreMatcher::from_config(&IgnoreConfig {
        patterns: patterns.clone(),
    })?;
    let mut patterns = patterns;
    patterns.sort();
    patterns.dedup();
    Ok(patterns)
}

fn ensure_canonical_patterns(label: &str, patterns: &[String]) -> anyhow::Result<()> {
    if normalize_patterns(patterns.to_vec())? != patterns {
        anyhow::bail!("project_snapshot_policy {label} must be sorted and deduplicated");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_patterns_are_canonical_sets() {
        let policy = ProjectSnapshotPolicy::new(
            ProjectSyncScope::FullProject,
            vec!["target/".into(), ".venv/".into(), "target/".into()],
            vec!["*.pyc".into()],
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(policy.project_exclusions, vec![".venv/", "target/"]);
        assert!(ProjectSnapshotPolicy::from_value(&policy.to_value()).is_ok());
    }
}
