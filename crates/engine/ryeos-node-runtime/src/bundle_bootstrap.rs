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

    Ok(BundleRecord {
        name: ctx.id.clone(),
        path: raw.path,
        source_file: std::path::PathBuf::new(),
    })
}

impl<'a> BundleBootstrapLoader<'a> {
    /// Phase 1: load only the `bundles` section to determine effective bundle roots.
    ///
    /// Scans `<app_root>/.ai/node/bundles/` (flat only — no recursion).
    /// Uses minimal bootstrap verifier (signature + hash, no full engine).
    pub fn load_bundle_section(&self) -> Result<Vec<BundleRecord>> {
        let mut records: Vec<BundleRecord> = Vec::new();

        let node_dir = self
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("bundles");
        // Bundles use one exact flat YAML namespace. Missing is empty; any
        // directory, symlink, special file, or non-YAML entry is an error.
        for path in scan_yaml_files(&node_dir, false)? {
            let verified = verify_and_parse(&path, &node_dir, "bundles", self.trust_store)?;

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
            record.path = canonical;

            records.push(record);
        }

        // Collision detection: by canonical path AND by name (fail-closed)
        check_bundle_collisions(&records)?;
        if records.is_empty() {
            bail!("node-config section `bundles` violates AtLeastOne cardinality with 0 record(s)");
        }

        Ok(records)
    }
}
