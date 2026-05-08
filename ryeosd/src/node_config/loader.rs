//! Node-config loader: two-phase bootstrap for daemon-consumed control-plane items.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use ryeos_engine::contracts::SignatureEnvelope;
use ryeos_engine::trust::TrustStore;

use super::sections::alias::AliasRecord;
use super::sections::bundle::BundleSection;
use super::sections::verb::VerbRecord;
use super::{
    BundleRecord, NodeConfigSection, NodeConfigSnapshot, SectionSourcePolicy, SectionTable,
};
use crate::routes::raw::RawRouteSpec;

/// Bootstrap loader for node-config items.
///
/// Phase 1: `load_bundle_section()` — minimal bootstrap verifier, system space only.
/// Phase 2: `load_full()` — full engine-based scan across all sources.
pub struct BootstrapLoader<'a> {
    pub system_space_dir: &'a Path,
    pub trust_store: &'a TrustStore,
}

impl<'a> BootstrapLoader<'a> {
    /// Phase 1: load only the `bundles` section to determine effective bundle roots.
    ///
    /// Scans `<system_space_dir>/.ai/node/bundles/`.
    /// Uses minimal bootstrap verifier (signature + hash, no full engine).
    pub fn load_bundle_section(&self) -> Result<Vec<BundleRecord>> {
        let section = BundleSection;
        let envelope = SignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        };

        let mut records: Vec<BundleRecord> = Vec::new();

        let scan_roots = [self.system_space_dir];
        for root in &scan_roots {
            let node_dir = root.join(".ai").join("node").join("bundles");
            if !node_dir.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&node_dir)
                .with_context(|| format!("failed to read node config dir {}", node_dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();

                // Reject symlinks and non-regular files
                if !path.is_file() || path.is_symlink() {
                    bail!(
                        "node config item at {} is not a regular file (symlinks rejected)",
                        path.display()
                    );
                }

                let ext = path.extension().and_then(|e| e.to_str());
                if ext != Some("yaml") && ext != Some("yml") {
                    continue;
                }

                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .context("node config item has no filename stem")?;

                // Read and verify
                let content = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;

                let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope)
                    .context(format!("node config item at {} has no valid signature line", path.display()))?;

                // Verify signature against trust store
                let (trust_class, _) = ryeos_engine::trust::verify_item_signature(
                    &content,
                    &header,
                    &envelope,
                    self.trust_store,
                )?;

                if trust_class != ryeos_engine::contracts::TrustClass::Trusted {
                    bail!(
                        "node config item at {} is not trusted (trust_class: {:?}); \
                         only trusted items are allowed in node config",
                        path.display(),
                        trust_class
                    );
                }

                // Strip signature and parse YAML body
                let body_str = strip_signature(&content);
                let body: Value = serde_yaml::from_str(&body_str)
                    .with_context(|| format!("failed to parse YAML body of {}", path.display()))?;

                // Check path = section invariant
                let declared_section = body
                    .get("section")
                    .and_then(|v| v.as_str())
                    .context(format!("node config item at {} missing 'section' field", path.display()))?;

                let parent_dir_name = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .context(format!("node config item at {} has no parent directory", path.display()))?;

                if declared_section != parent_dir_name {
                    bail!(
                        "node config item at {} declares section '{}' but lives under directory '{}' \
                         (path = section invariant violated)",
                        path.display(),
                        declared_section,
                        parent_dir_name
                    );
                }

                // Parse as bundle record
                let record = section
                    .parse(name, &body)
                    .with_context(|| format!("failed to parse bundle record {}", path.display()))?;
                let mut record: BundleRecord = record
                    .as_any()
                    .downcast_ref::<BundleRecord>()
                    .context("BundleSection::parse returned wrong type")?
                    .clone();
                record.source_file = path.clone();

                // Validate path: canonicalize, must exist as directory
                if !record.path.is_dir() {
                    bail!(
                        "bundle '{}' path '{}' does not exist or is not a directory (declared in {})",
                        name,
                        record.path.display(),
                        path.display()
                    );
                }

                let canonical = record
                    .path
                    .canonicalize()
                    .with_context(|| format!("failed to canonicalize bundle '{}' path '{}'", name, record.path.display()))?;
                record.path = canonical;

                records.push(record);
            }
        }

        // Collision detection: by canonical path AND by name (fail-closed)
        check_bundle_collisions(&records)?;

        Ok(records)
    }

    /// Phase 2: full node-config scan across all sections and sources.
    pub fn load_full(
        &self,
        section_table: &SectionTable,
        bundles: &[BundleRecord],
    ) -> Result<NodeConfigSnapshot> {
        let mut loaded_bundles: Vec<BundleRecord> = Vec::new();
        let mut routes: Vec<RawRouteSpec> = Vec::new();
        let mut verbs: Vec<VerbRecord> = Vec::new();
        let mut aliases: Vec<AliasRecord> = Vec::new();

        for section_name in section_table.section_names() {
            let section = section_table
                .get(section_name)
                .context(format!("section '{}' registered but handler missing", section_name))?;

            let scan_roots = match section.source_policy() {
                SectionSourcePolicy::SystemAndState => {
                    vec![self.system_space_dir.to_path_buf()]
                }
                SectionSourcePolicy::EffectiveBundleRootsAndState => {
                    let mut roots = vec![self.system_space_dir.to_path_buf()];
                    for b in bundles {
                        if !roots.iter().any(|r| r == &b.path) {
                            roots.push(b.path.clone());
                        }
                    }
                    roots
                }
            };

            let envelope = SignatureEnvelope {
                prefix: "#".into(),
                suffix: None,
                after_shebang: false,
            };

            for root in &scan_roots {
                let node_section_dir = root
                    .join(".ai")
                    .join("node")
                    .join(section_name);
                if !node_section_dir.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(&node_section_dir)
                    .with_context(|| {
                        format!("failed to read node config section dir {}", node_section_dir.display())
                    })?
                {
                    let entry = entry?;
                    let path = entry.path();

                    if !path.is_file() || path.is_symlink() {
                        bail!(
                            "node config item at {} is not a regular file (symlinks rejected)",
                            path.display()
                        );
                    }

                    let ext = path.extension().and_then(|e| e.to_str());
                    if ext != Some("yaml") && ext != Some("yml") {
                        continue;
                    }

                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .context("node config item has no filename stem")?;

                    let content = fs::read_to_string(&path)
                        .with_context(|| format!("failed to read {}", path.display()))?;

                    let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope)
                        .context(format!(
                            "node config item at {} has no valid signature line",
                            path.display()
                        ))?;

                    let (trust_class, _) = ryeos_engine::trust::verify_item_signature(
                        &content,
                        &header,
                        &envelope,
                        self.trust_store,
                    )?;

                    if trust_class != ryeos_engine::contracts::TrustClass::Trusted {
                        bail!(
                            "node config item at {} is not trusted (trust_class: {:?})",
                            path.display(),
                            trust_class
                        );
                    }

                    let body_str = strip_signature(&content);
                    let body: Value = serde_yaml::from_str(&body_str)
                        .with_context(|| format!("failed to parse YAML body of {}", path.display()))?;

                    let declared_section = body
                        .get("section")
                        .and_then(|v| v.as_str())
                        .context(format!(
                            "node config item at {} missing 'section' field",
                            path.display()
                        ))?;

                    let parent_dir_name = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("");

                    if declared_section != parent_dir_name {
                        bail!(
                            "node config item at {} declares section '{}' but lives under '{}'",
                            path.display(),
                            declared_section,
                            parent_dir_name
                        );
                    }

                    if section_name == "bundles" {
                        let record = section
                            .parse(name, &body)
                            .with_context(|| format!("failed to parse bundle record {}", path.display()))?;
                        let mut record: BundleRecord = record
                            .as_any()
                            .downcast_ref::<BundleRecord>()
                            .context("BundleSection::parse returned wrong type")?
                            .clone();
                        record.source_file = path.clone();
                        if !record.path.is_dir() {
                            bail!(
                                "bundle '{}' path '{}' does not exist or is not a directory (declared in {})",
                                name,
                                record.path.display(),
                                path.display()
                            );
                        }
                        let canonical = record.path.canonicalize().with_context(|| {
                            format!(
                                "failed to canonicalize bundle '{}' path '{}'",
                                name,
                                record.path.display()
                            )
                        })?;
                        record.path = canonical;
                        loaded_bundles.push(record);
                    } else if section_name == "routes" {
                        let record = section
                            .parse(name, &body)
                            .with_context(|| format!("failed to parse route record {}", path.display()))?;
                        let mut record: RawRouteSpec = record
                            .as_any()
                            .downcast_ref::<RawRouteSpec>()
                            .context("RouteSection::parse returned wrong type")?
                            .clone();
                        record.source_file = path.clone();
                        routes.push(record);
                    } else if section_name == "verbs" {
                        let record = section
                            .parse(name, &body)
                            .with_context(|| format!("failed to parse verb record {}", path.display()))?;
                        let mut record: VerbRecord = record
                            .as_any()
                            .downcast_ref::<VerbRecord>()
                            .context("VerbSection::parse returned wrong type")?
                            .clone();
                        record.source_file = path.clone();
                        verbs.push(record);
                    } else if section_name == "aliases" {
                        let record = section
                            .parse(name, &body)
                            .with_context(|| format!("failed to parse alias record {}", path.display()))?;
                        let mut record: AliasRecord = record
                            .as_any()
                            .downcast_ref::<AliasRecord>()
                            .context("AliasSection::parse returned wrong type")?
                            .clone();
                        record.source_file = path.clone();
                        aliases.push(record);
                    }
                }
            }

            if section_name == "bundles" {
                check_bundle_collisions(&loaded_bundles)?;
            }
        }

        Ok(NodeConfigSnapshot {
            bundles: loaded_bundles,
            routes,
            verbs,
            aliases,
        })
    }
}

/// Strip the signature line(s) from the top of a file.
fn strip_signature(content: &str) -> String {
    content
        .lines()
        .skip_while(|l| l.starts_with("# ryeos:signed:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_string()
}

/// Fail-closed collision detection for bundle records.
///
/// Errors if two records share either:
/// - the same canonical path (the engine would mount the same bundle twice), or
/// - the same name (accidental duplicate registration).
///
/// All `BundleRecord::path` values must already be canonicalized before this
/// is called.
fn check_bundle_collisions(records: &[BundleRecord]) -> Result<()> {
    let mut by_path: HashMap<&Path, &BundleRecord> = HashMap::new();
    let mut by_name: HashMap<&str, &BundleRecord> = HashMap::new();

    for record in records {
        if let Some(prev) = by_name.get(record.name.as_str()) {
            bail!(
                "node config section 'bundles' has duplicate name '{}': \
                 first registered from '{}' (path: {}), second from '{}' (path: {})",
                record.name,
                prev.source_file.display(),
                prev.path.display(),
                record.source_file.display(),
                record.path.display(),
            );
        }
        if let Some(prev) = by_path.get(record.path.as_path()) {
            bail!(
                "node config section 'bundles' has duplicate canonical path '{}': \
                 first registered as '{}' (from {}), second as '{}' (from {})",
                record.path.display(),
                prev.name,
                prev.source_file.display(),
                record.name,
                record.source_file.display(),
            );
        }
        by_name.insert(record.name.as_str(), record);
        by_path.insert(record.path.as_path(), record);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_signature_removes_signed_line() {
        let content = "# ryeos:signed:2026-01-01T00:00:00Z:abc123:sig456:fp789\nsection: bundles\npath: /foo\n";
        let stripped = strip_signature(content);
        assert!(stripped.starts_with("section: bundles"));
        assert!(!stripped.contains("ryeos:signed:"));
    }

    #[test]
    fn strip_signature_preserves_body() {
        let content = "# ryeos:signed:2026-01-01T00:00:00Z:abc:sig:fp\nsection: bundles\npath: /foo/bar\n";
        let stripped = strip_signature(content);
        let parsed: Value = serde_yaml::from_str(&stripped).unwrap();
        assert_eq!(parsed["section"], "bundles");
        assert_eq!(parsed["path"], "/foo/bar");
    }

    fn make_record(name: &str, path: &str, source: &str) -> BundleRecord {
        BundleRecord {
            name: name.into(),
            path: std::path::PathBuf::from(path),
            source_file: std::path::PathBuf::from(source),
        }
    }

    #[test]
    fn check_bundle_collisions_different_names_same_canonical_path_errors() {
        let records = vec![
            make_record("alpha", "/var/lib/ryeos/bundles/core", "/etc/ryeos/.ai/node/bundles/alpha.yaml"),
            make_record("beta", "/var/lib/ryeos/bundles/core", "/etc/ryeos/.ai/node/bundles/beta.yaml"),
        ];
        let err = check_bundle_collisions(&records).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("duplicate canonical path"), "got: {}", msg);
        assert!(msg.contains("/var/lib/ryeos/bundles/core"), "got: {}", msg);
        assert!(msg.contains("alpha"), "got: {}", msg);
        assert!(msg.contains("beta"), "got: {}", msg);
    }

    #[test]
    fn check_bundle_collisions_same_name_errors() {
        let records = vec![
            make_record("core", "/var/lib/ryeos/bundles/core-a", "/etc/ryeos/.ai/node/bundles/core.yaml"),
            make_record("core", "/var/lib/ryeos/bundles/core-b", "/var/lib/ryeos/.ai/node/bundles/core.yaml"),
        ];
        let err = check_bundle_collisions(&records).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("duplicate name"), "got: {}", msg);
        assert!(msg.contains("'core'"), "got: {}", msg);
    }

    #[test]
    fn check_bundle_collisions_different_names_different_paths_ok() {
        let records = vec![
            make_record("core", "/var/lib/ryeos/bundles/core", "/etc/ryeos/.ai/node/bundles/core.yaml"),
            make_record("standard", "/var/lib/ryeos/bundles/standard", "/etc/ryeos/.ai/node/bundles/standard.yaml"),
        ];
        check_bundle_collisions(&records).unwrap();
    }

    #[test]
    fn check_bundle_collisions_empty_ok() {
        check_bundle_collisions(&[]).unwrap();
    }
}
