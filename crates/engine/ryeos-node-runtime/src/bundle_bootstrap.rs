//! Trusted flat bundle-registration bootstrap, independent of App services.

use crate::node_config_source::{
    BundleRecord, NodeItemContext, check_bundle_collisions, scan_yaml_files, verify_and_parse,
};
use anyhow::{Context, Result, bail};
use ryeos_engine::trust::TrustStore;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

pub struct BundleBootstrapLoader<'a> {
    pub app_root: &'a Path,
    pub trust_store: &'a TrustStore,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundleRecord {
    kind: String,
    path: std::path::PathBuf,
    #[serde(default)]
    lane: Option<ryeos_engine::trust::BundleLane>,
}

pub fn parse_bundle(ctx: &NodeItemContext, body: &Value) -> Result<BundleRecord> {
    let raw: RawBundleRecord =
        serde_json::from_value(body.clone()).context("failed to parse bundle record")?;
    if raw.kind != "node" {
        bail!(
            "bundle '{}' declares kind {:?}, expected kind 'node'",
            ctx.id,
            raw.kind
        );
    }

    if !raw.path.is_absolute() {
        bail!(
            "bundle '{}' path must be absolute, got: {}",
            ctx.id,
            raw.path.display()
        );
    }

    if let Some(lane) = &raw.lane {
        lane.validate_shape()
            .map_err(|error| anyhow::anyhow!("bundle '{}' has an invalid lane: {error}", ctx.id))?;
    }

    Ok(BundleRecord {
        name: ctx.id.clone(),
        lane: raw.lane,
        path: raw.path,
        source_file: std::path::PathBuf::new(),
    })
}

/// A laned bundle the loader refused to load because its tree no longer
/// passes the lane limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedLaneBundle {
    pub name: String,
    /// The stable `bundle_lane_*` or `publication_lane_*` code.
    pub code: &'static str,
    pub message: String,
}

impl<'a> BundleBootstrapLoader<'a> {
    /// Phase 1: load only the `bundles` section to determine effective bundle roots.
    ///
    /// Scans `<app_root>/.ai/node/bundles/` (flat only — no recursion).
    /// Uses minimal bootstrap verifier (signature + hash, no full engine).
    pub fn load_bundle_section(&self) -> Result<Vec<BundleRecord>> {
        self.load_bundle_section_reporting_skipped()
            .map(|(records, _)| records)
    }

    /// As [`Self::load_bundle_section`], and also the laned bundles that were
    /// not loaded because their tree failed the lane limits. A failing laned
    /// bundle is left out and logged at error level; it never stops the node
    /// from loading the others.
    pub fn load_bundle_section_reporting_skipped(
        &self,
    ) -> Result<(Vec<BundleRecord>, Vec<SkippedLaneBundle>)> {
        let mut records: Vec<BundleRecord> = Vec::new();
        let mut skipped: Vec<SkippedLaneBundle> = Vec::new();

        let node_dir = self
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("bundles");
        // Bundles use one exact flat YAML namespace. Missing is empty; any
        // directory, symlink, special file, or non-YAML entry is an error.
        let files = scan_yaml_files(&node_dir, false)?;
        // Registrations are node state, written only with the node key
        // (`bundle_transaction`). A registration signed by any other pinned
        // key, a publisher's included, is refused.
        let node_fingerprint = if files.is_empty() {
            String::new()
        } else {
            crate::identity::node_identity_fingerprint(self.app_root)?
        };
        for path in files {
            let verified = verify_and_parse(&path, &node_dir, "bundles", self.trust_store)?;
            ryeos_engine::trust::require_document_signer(
                verified.path(),
                verified.signer_fingerprint(),
                &node_fingerprint,
                "node",
                "bundle registrations are written only by the node's own `ryeos bundle install`: remove the file and reinstall the bundle",
            )?;

            let mut record =
                parse_bundle(verified.context(), verified.body()).with_context(|| {
                    format!(
                        "failed to parse bundle record {}",
                        verified.path().display()
                    )
                })?;
            record.source_file = verified.path().to_path_buf();

            // Validate path: canonicalize, must exist as directory
            if !record.path.is_dir() {
                bail!(
                    "bundle '{}' path '{}' does not exist or is not a directory (declared in {})",
                    record.name,
                    record.path.display(),
                    record.source_file.display()
                );
            }

            let canonical = record.path.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize bundle '{}' path '{}'",
                    record.name,
                    record.path.display()
                )
            })?;
            if record.lane.is_some()
                && let Err(refusal) =
                    ryeos_bundle::lane::check_installed_lane_bundle(&canonical, &record.name)
            {
                tracing::error!(
                    bundle = %record.name,
                    code = refusal.code(),
                    error = %refusal,
                    "a publication-lane bundle no longer passes the lane limits; it is not loaded"
                );
                skipped.push(SkippedLaneBundle {
                    name: record.name.clone(),
                    code: refusal.code(),
                    message: refusal.to_string(),
                });
                continue;
            }
            record.path = canonical;

            records.push(record);
        }

        // Collision detection: by canonical path AND by name (fail-closed)
        check_bundle_collisions(&records)?;
        if records.is_empty() {
            bail!("node-config section `bundles` violates AtLeastOne cardinality with 0 record(s)");
        }

        Ok((records, skipped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> NodeItemContext {
        NodeItemContext {
            section: "bundles".to_string(),
            id: "pulled".to_string(),
            stem: "pulled".to_string(),
            rel_path: PathBuf::from("pulled.yaml"),
            source_file: PathBuf::from("/app/.ai/node/bundles/pulled.yaml"),
            signer_fingerprint: "f".repeat(64),
        }
    }

    fn lane_value() -> Value {
        serde_json::to_value(ryeos_engine::trust::BundleLane::publication(
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            "1.0.0",
        ))
        .unwrap()
    }

    #[test]
    fn registration_without_a_lane_parses_with_none() {
        let record =
            parse_bundle(&ctx(), &serde_json::json!({"kind": "node", "path": "/b"})).unwrap();
        assert!(record.lane.is_none());
    }

    #[test]
    fn registration_lane_is_carried_into_the_record() {
        let record = parse_bundle(
            &ctx(),
            &serde_json::json!({"kind": "node", "path": "/b", "lane": lane_value()}),
        )
        .unwrap();
        assert_eq!(record.lane.unwrap().name, "publication");
    }

    #[test]
    fn registration_with_a_malformed_lane_is_refused() {
        let mut lane = lane_value();
        lane["record_hash"] = serde_json::json!("not-hex");
        let error = parse_bundle(
            &ctx(),
            &serde_json::json!({"kind": "node", "path": "/b", "lane": lane}),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("record_hash"), "{error:#}");
    }
}
