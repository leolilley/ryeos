//! Immutable effective policy captured with a project snapshot.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Value, json};

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
    pub node_patterns: Vec<String>,
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
    node_patterns: Vec<String>,
    source_hashes: BTreeMap<String, String>,
}

impl ProjectSnapshotPolicy {
    pub const SCHEMA: u32 = 2;
    pub const LANGUAGE_VERSION: u32 = 1;
    pub const RYEOS_FLOOR_VERSION: u32 = 1;

    pub fn new(
        sync_scope: ProjectSyncScope,
        project_exclusions: Vec<String>,
        node_patterns: Vec<String>,
        mut source_hashes: BTreeMap<String, String>,
    ) -> anyhow::Result<Self> {
        let node_patterns = normalize_patterns(node_patterns)?;
        source_hashes.insert(
            crate::project_sync::NODE_PATTERNS_POLICY_SOURCE.to_owned(),
            crate::project_sync::project_snapshot_node_patterns_hash(&node_patterns)?,
        );
        source_hashes
            .entry(crate::project_sync::PROJECT_CONFIG_POLICY_SOURCE.to_owned())
            .or_insert_with(crate::project_sync::absent_project_snapshot_config_hash);
        let policy = Self {
            sync_scope,
            language_version: Self::LANGUAGE_VERSION,
            ryeos_floor_version: Self::RYEOS_FLOOR_VERSION,
            ryeos_floor_rules: crate::project_sync::snapshot_floor_rules(),
            project_exclusions: normalize_patterns(project_exclusions)?,
            node_patterns,
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
        ensure_canonical_patterns("node patterns", &self.node_patterns)?;
        let expected_node_patterns_hash =
            crate::project_sync::project_snapshot_node_patterns_hash(&self.node_patterns)?;
        if self
            .source_hashes
            .get(crate::project_sync::NODE_PATTERNS_POLICY_SOURCE)
            != Some(&expected_node_patterns_hash)
        {
            anyhow::bail!(
                "project_snapshot_policy node-pattern provenance does not match its patterns"
            );
        }
        let expected_sources = [
            crate::project_sync::NODE_PATTERNS_POLICY_SOURCE,
            crate::project_sync::PROJECT_CONFIG_POLICY_SOURCE,
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        let observed_sources = self
            .source_hashes
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        if observed_sources != expected_sources {
            anyhow::bail!(
                "project_snapshot_policy source hashes must contain exactly project_config and node_patterns"
            );
        }
        for (source, hash) in &self.source_hashes {
            super::validate_trimmed_control_free("policy source label", source, false)?;
            validate_canonical_hash("policy source hash", hash)?;
        }
        Ok(())
    }

    pub fn matcher(&self) -> anyhow::Result<IgnoreMatcher> {
        let mut patterns = self.project_exclusions.clone();
        patterns.extend(self.node_patterns.iter().cloned());
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
            "node_patterns": self.node_patterns,
            "source_hashes": self.source_hashes,
        })
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let wire: ProjectSnapshotPolicyWire = serde_json::from_value(value.clone())
            .context("failed to deserialize project_snapshot_policy schema 2")?;
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
            node_patterns: wire.node_patterns,
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

    #[test]
    fn predecessor_additions_shape_is_not_accepted_as_current_policy() {
        let policy = ProjectSnapshotPolicy::new(
            ProjectSyncScope::FullProject,
            Vec::new(),
            vec!["target/".into()],
            BTreeMap::new(),
        )
        .unwrap();
        let mut wire = policy.to_value();
        wire["schema"] = Value::from(1);
        wire["node_additions"] = wire["node_patterns"].take();
        wire.as_object_mut().unwrap().remove("node_patterns");
        assert!(ProjectSnapshotPolicy::from_value(&wire).is_err());
    }

    #[test]
    fn decoded_policy_rejects_substituted_node_pattern_provenance() {
        let policy = ProjectSnapshotPolicy::new(
            ProjectSyncScope::FullProject,
            Vec::new(),
            vec!["target/".into()],
            BTreeMap::new(),
        )
        .unwrap();
        let mut wire = policy.to_value();
        wire["source_hashes"][crate::project_sync::NODE_PATTERNS_POLICY_SOURCE] =
            Value::from("ab".repeat(32));
        assert!(ProjectSnapshotPolicy::from_value(&wire).is_err());
    }

    #[test]
    fn decoded_policy_rejects_missing_or_extra_policy_sources() {
        let policy = ProjectSnapshotPolicy::new(
            ProjectSyncScope::FullProject,
            Vec::new(),
            vec!["target/".into()],
            BTreeMap::new(),
        )
        .unwrap();

        let mut missing = policy.to_value();
        missing["source_hashes"]
            .as_object_mut()
            .unwrap()
            .remove(crate::project_sync::PROJECT_CONFIG_POLICY_SOURCE);
        assert!(ProjectSnapshotPolicy::from_value(&missing).is_err());

        let mut extra = policy.to_value();
        extra["source_hashes"]["unowned"] = Value::from("ab".repeat(32));
        assert!(ProjectSnapshotPolicy::from_value(&extra).is_err());
    }
}
