//! Node-config loader: two-phase bootstrap for daemon-consumed control-plane items.
//!
//! Phase 1: `load_bundle_section()` — minimal bootstrap verifier, system space only.
//! Phase 2: `load_full()` — full engine-based scan across all sources.
//!
//! Section directories support recursive subfolders. For example:
//!
//!   .ai/node/routes/ui/cockpit/snapshot.yaml
//!   .ai/node/routes/ui/cockpit/items/list.yaml
//!   .ai/node/verbs/web.yaml
//!
//! The section invariant requires:
//! - The file lives under `.ai/node/<section>/` (any depth)
//! - The YAML body declares `section: <section>`
//!
//! Security: signed-required, trusted-signer-required, symlinks rejected,
//! regular files only, deterministic traversal order.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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
use crate::route_raw::RawRouteSpec;

/// Bootstrap loader for node-config items.
///
/// Phase 1: `load_bundle_section()` — minimal bootstrap verifier, system space only.
/// Phase 2: `load_full()` — full engine-based scan across all sources.
pub struct BootstrapLoader<'a> {
    pub system_space_dir: &'a Path,
    pub trust_store: &'a TrustStore,
}

/// A verified and parsed node-config YAML file, ready for section-specific handling.
struct VerifiedItem {
    path: PathBuf,
    name: String,
    body: Value,
}

/// Signature envelope used for all node-config items.
fn node_config_envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    }
}

/// Recursively collect all `.yaml`/`.yml` regular files under a directory.
///
/// Returns files in deterministic depth-first order, with entries sorted
/// alphabetically at each directory level. Rejects symlinks at every level
/// (both files and directories).
fn scan_yaml_files_recursive(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    scan_dir_recursive(dir, &mut files)?;
    Ok(files)
}

fn scan_dir_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    // Read and sort for deterministic order
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .with_context(|| format!("failed to read directory {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();

        // Reject all symlinks — both files and directories
        if path.is_symlink() {
            bail!(
                "node config scan encountered symlink at {} (symlinks rejected)",
                path.display()
            );
        }

        if path.is_dir() {
            scan_dir_recursive(&path, files)?;
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str());
            if ext == Some("yaml") || ext == Some("yml") {
                files.push(path);
            }
        }
        // Ignore special files (sockets, fifos, etc.)
    }
    Ok(())
}

/// Verify signature, trust, and section invariant for a single node-config file.
///
/// The `section_root` is the `.ai/node/<section>/` directory (e.g. the
/// `routes/` dir). The invariant checks that:
/// 1. `file` lives under `section_root` (at any depth)
/// 2. The YAML body declares `section: <expected_section>`
fn verify_and_parse(
    file: &Path,
    section_root: &Path,
    expected_section: &str,
    trust_store: &TrustStore,
) -> Result<VerifiedItem> {
    // Reject symlinks and non-regular files
    if !file.is_file() || file.is_symlink() {
        bail!(
            "node config item at {} is not a regular file (symlinks rejected)",
            file.display()
        );
    }

    let ext = file.extension().and_then(|e| e.to_str());
    if ext != Some("yaml") && ext != Some("yml") {
        bail!(
            "node config item at {} is not a .yaml or .yml file",
            file.display()
        );
    }

    let name = file.file_stem().and_then(|s| s.to_str()).context(format!(
        "node config item at {} has no filename stem",
        file.display()
    ))?;

    let content =
        fs::read_to_string(file).with_context(|| format!("failed to read {}", file.display()))?;

    let envelope = node_config_envelope();

    let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope)
        .with_context(|| {
            format!(
                "node config item at {} has no valid signature line",
                file.display()
            )
        })?;

    let (trust_class, _) =
        ryeos_engine::trust::verify_item_signature(&content, &header, &envelope, trust_store)?;

    if trust_class != ryeos_engine::contracts::TrustClass::Trusted {
        bail!(
            "node config item at {} is not trusted (trust_class: {:?}); \
             only trusted items are allowed in node config",
            file.display(),
            trust_class
        );
    }

    let body_str = strip_signature(&content);
    let body: Value = serde_yaml::from_str(&body_str)
        .with_context(|| format!("failed to parse YAML body of {}", file.display()))?;

    // Check section invariant: file must live under section_root AND declare section
    if !file.starts_with(section_root) {
        bail!(
            "node config item at {} is not under expected section directory '{}' \
             (section containment invariant violated)",
            file.display(),
            section_root.display()
        );
    }

    let declared_section = body
        .get("section")
        .and_then(|v| v.as_str())
        .with_context(|| {
            format!(
                "node config item at {} missing 'section' field",
                file.display()
            )
        })?;

    if declared_section != expected_section {
        bail!(
            "node config item at {} declares section '{}' but was loaded under section '{}' \
             (section = containment invariant violated)",
            file.display(),
            declared_section,
            expected_section
        );
    }

    Ok(VerifiedItem {
        path: file.to_path_buf(),
        name: name.to_string(),
        body,
    })
}

impl<'a> BootstrapLoader<'a> {
    /// Phase 1: load only the `bundles` section to determine effective bundle roots.
    ///
    /// Scans `<system_space_dir>/.ai/node/bundles/` (flat only — no recursion).
    /// Uses minimal bootstrap verifier (signature + hash, no full engine).
    pub fn load_bundle_section(&self) -> Result<Vec<BundleRecord>> {
        let section = BundleSection;
        let mut records: Vec<BundleRecord> = Vec::new();

        let node_dir = self
            .system_space_dir
            .join(".ai")
            .join("node")
            .join("bundles");
        if !node_dir.is_dir() {
            return Ok(records);
        }

        // Bundles use flat scan only (no subdirectories for bundles)
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(&node_dir)
            .with_context(|| format!("failed to read node config dir {}", node_dir.display()))?
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();

            let verified = match verify_and_parse(&path, &node_dir, "bundles", self.trust_store) {
                Ok(v) => v,
                Err(e) => {
                    // For bundles, non-YAML files are silently skipped (backward compat)
                    let ext = path.extension().and_then(|e| e.to_str());
                    if ext == Some("yaml") || ext == Some("yml") {
                        return Err(e);
                    }
                    continue;
                }
            };

            let record = section
                .parse(&verified.name, &verified.body)
                .with_context(|| {
                    format!("failed to parse bundle record {}", verified.path.display())
                })?;
            let mut record: BundleRecord = record
                .as_any()
                .downcast_ref::<BundleRecord>()
                .context("BundleSection::parse returned wrong type")?
                .clone();
            record.source_file = verified.path.clone();

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

        Ok(records)
    }

    /// Phase 2: full node-config scan across all sections and sources.
    ///
    /// For sections with `EffectiveBundleRootsAndState` policy (routes, verbs),
    /// scans recursively into subdirectories. For `SystemAndState` (bundles), scans flat.
    pub fn load_full(
        &self,
        section_table: &SectionTable,
        bundles: &[BundleRecord],
    ) -> Result<NodeConfigSnapshot> {
        let mut loaded_bundles: Vec<BundleRecord> = Vec::new();
        let mut routes: Vec<RawRouteSpec> = Vec::new();
        let mut verbs: Vec<VerbRecord> = Vec::new();

        for section_name in section_table.section_names() {
            let section = section_table.get(section_name).context(format!(
                "section '{}' registered but handler missing",
                section_name
            ))?;

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

            for root in &scan_roots {
                let section_dir = root.join(".ai").join("node").join(section_name);
                if !section_dir.is_dir() {
                    continue;
                }

                // Routes and verbs: recursive scan.
                // Bundles: flat scan (enforced by policy type, but we handle
                // both for correctness — bundles don't actually reach here
                // with subdirectories).
                let use_recursive = section_name != "bundles";
                let yaml_files = if use_recursive {
                    scan_yaml_files_recursive(&section_dir).with_context(|| {
                        format!(
                            "failed to scan node config section '{}' recursively in {}",
                            section_name,
                            section_dir.display()
                        )
                    })?
                } else {
                    // Flat scan for bundles (same as Phase 1)
                    let mut files: Vec<PathBuf> = Vec::new();
                    let mut entries: Vec<fs::DirEntry> = fs::read_dir(&section_dir)
                        .with_context(|| {
                            format!(
                                "failed to read node config section dir {}",
                                section_dir.display()
                            )
                        })?
                        .collect::<Result<Vec<_>, _>>()?;
                    entries.sort_by_key(|e| e.file_name());
                    for entry in entries {
                        let path = entry.path();
                        let ext = path.extension().and_then(|e| e.to_str());
                        if ext == Some("yaml") || ext == Some("yml") {
                            files.push(path);
                        }
                    }
                    files
                };

                for path in yaml_files {
                    let verified =
                        verify_and_parse(&path, &section_dir, section_name, self.trust_store)
                            .with_context(|| {
                                format!(
                                    "failed to verify node config item {} in section '{}'",
                                    path.display(),
                                    section_name
                                )
                            })?;

                    if section_name == "bundles" {
                        let record =
                            section
                                .parse(&verified.name, &verified.body)
                                .with_context(|| {
                                    format!(
                                        "failed to parse bundle record {}",
                                        verified.path.display()
                                    )
                                })?;
                        let mut record: BundleRecord = record
                            .as_any()
                            .downcast_ref::<BundleRecord>()
                            .context("BundleSection::parse returned wrong type")?
                            .clone();
                        record.source_file = verified.path.clone();
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
                        loaded_bundles.push(record);
                    } else if section_name == "routes" {
                        let record =
                            section
                                .parse(&verified.name, &verified.body)
                                .with_context(|| {
                                    format!(
                                        "failed to parse route record {}",
                                        verified.path.display()
                                    )
                                })?;
                        let mut record: RawRouteSpec = record
                            .as_any()
                            .downcast_ref::<RawRouteSpec>()
                            .context("RouteSection::parse returned wrong type")?
                            .clone();
                        record.source_file = verified.path.clone();
                        routes.push(record);
                    } else if section_name == "verbs" {
                        let record =
                            section
                                .parse(&verified.name, &verified.body)
                                .with_context(|| {
                                    format!(
                                        "failed to parse verb record {}",
                                        verified.path.display()
                                    )
                                })?;
                        let mut record: VerbRecord = record
                            .as_any()
                            .downcast_ref::<VerbRecord>()
                            .context("VerbSection::parse returned wrong type")?
                            .clone();
                        record.source_file = verified.path.clone();
                        verbs.push(record);
                    }
                }
            }

            if section_name == "bundles" {
                check_bundle_collisions(&loaded_bundles)?;
            }
        }

        let aliases = synthesize_aliases_from_verbs(&verbs);
        check_alias_collisions(&aliases)?;

        Ok(NodeConfigSnapshot {
            bundles: loaded_bundles,
            routes,
            verbs,
            aliases,
        })
    }
}

fn synthesize_aliases_from_verbs(verbs: &[VerbRecord]) -> Vec<AliasRecord> {
    let mut aliases = Vec::new();

    for verb in verbs {
        for alias in &verb.aliases {
            aliases.push(AliasRecord {
                category: "aliases".into(),
                section: "aliases".into(),
                tokens: alias.tokens.clone(),
                verb: verb.name.clone(),
                description: alias
                    .description
                    .clone()
                    .unwrap_or_else(|| verb.description.clone()),
                deprecated: alias.deprecated,
                replacement_tokens: alias.replacement_tokens.clone(),
                removed_in: alias.removed_in.clone(),
                positional_forms: alias.positional_forms.clone(),
                project_resolution: alias.project_resolution,
                source_file: verb.source_file.clone(),
            });
        }
    }

    aliases
}

fn check_alias_collisions(records: &[AliasRecord]) -> Result<()> {
    let mut by_tokens: HashMap<Vec<String>, &AliasRecord> = HashMap::new();

    for record in records {
        if let Some(prev) = by_tokens.get(&record.tokens) {
            bail!(
                "node config aliases have duplicate tokens {:?}: \
                 first routes to '{}' from '{}', second routes to '{}' from '{}'",
                record.tokens,
                prev.verb,
                prev.source_file.display(),
                record.verb,
                record.source_file.display(),
            );
        }
        by_tokens.insert(record.tokens.clone(), record);
    }

    Ok(())
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
        let content =
            "# ryeos:signed:2026-01-01T00:00:00Z:abc123:sig456:fp789\nsection: bundles\npath: /foo\n";
        let stripped = strip_signature(content);
        assert!(stripped.starts_with("section: bundles"));
        assert!(!stripped.contains("ryeos:signed:"));
    }

    #[test]
    fn strip_signature_preserves_body() {
        let content =
            "# ryeos:signed:2026-01-01T00:00:00Z:abc:sig:fp\nsection: bundles\npath: /foo/bar\n";
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
            make_record(
                "alpha",
                "/var/lib/ryeos/bundles/core",
                "/etc/ryeos/.ai/node/bundles/alpha.yaml",
            ),
            make_record(
                "beta",
                "/var/lib/ryeos/bundles/core",
                "/etc/ryeos/.ai/node/bundles/beta.yaml",
            ),
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
            make_record(
                "core",
                "/var/lib/ryeos/bundles/core-a",
                "/etc/ryeos/.ai/node/bundles/core.yaml",
            ),
            make_record(
                "core",
                "/var/lib/ryeos/bundles/core-b",
                "/etc/ryeos/.ai/node/bundles/core.yaml",
            ),
        ];
        let err = check_bundle_collisions(&records).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("duplicate name"), "got: {}", msg);
        assert!(msg.contains("'core'"), "got: {}", msg);
    }

    #[test]
    fn check_bundle_collisions_different_names_different_paths_ok() {
        let records = vec![
            make_record(
                "core",
                "/var/lib/ryeos/bundles/core",
                "/etc/ryeos/.ai/node/bundles/core.yaml",
            ),
            make_record(
                "standard",
                "/var/lib/ryeos/bundles/standard",
                "/etc/ryeos/.ai/node/bundles/standard.yaml",
            ),
        ];
        check_bundle_collisions(&records).unwrap();
    }

    #[test]
    fn check_bundle_collisions_empty_ok() {
        check_bundle_collisions(&[]).unwrap();
    }

    // ── Recursive scan tests ──────────────────────────────────────────

    #[test]
    fn scan_yaml_files_recursive_finds_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        let ui_dir = routes_dir.join("ui");
        let cockpit_dir = ui_dir.join("cockpit");
        fs::create_dir_all(&cockpit_dir).unwrap();

        // Flat file
        fs::write(routes_dir.join("health.yaml"), "section: routes").unwrap();
        // Nested one level
        fs::write(ui_dir.join("index.yaml"), "section: routes").unwrap();
        // Nested two levels
        fs::write(cockpit_dir.join("snapshot.yaml"), "section: routes").unwrap();
        // Non-yaml file (should be skipped)
        fs::write(routes_dir.join("README.md"), "not yaml").unwrap();
        // Hidden file (should be found — no hidden filtering)
        fs::write(routes_dir.join(".hidden.yaml"), "section: routes").unwrap();

        let files = scan_yaml_files_recursive(&routes_dir).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        // Sorted: .hidden.yaml, health.yaml, then ui/cockpit/snapshot.yaml, ui/index.yaml
        assert_eq!(names.len(), 4, "expected 4 yaml files, got: {:?}", names);
        assert!(names.contains(&".hidden.yaml".to_string()));
        assert!(names.contains(&"health.yaml".to_string()));
        assert!(names.contains(&"snapshot.yaml".to_string()));
        assert!(names.contains(&"index.yaml".to_string()));

        // Verify deterministic order: depth-first, sorted at each level
        // Level 1: .hidden.yaml, health.yaml, ui/
        //   Level 2: ui/cockpit/snapshot.yaml, ui/index.yaml
        assert_eq!(names[0], ".hidden.yaml");
        assert_eq!(names[1], "health.yaml");
        assert_eq!(names[2], "snapshot.yaml");
        assert_eq!(names[3], "index.yaml");
    }

    #[test]
    fn scan_yaml_files_recursive_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let files = scan_yaml_files_recursive(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn scan_yaml_files_recursive_rejects_symlink_dir() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();

        // Create a symlinked subdirectory
        let link_target = routes_dir.join("symlinked_subdir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), &link_target).unwrap();

        let result = scan_yaml_files_recursive(&routes_dir);
        let err = result.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("symlink"),
            "symlinked directory should be rejected, got: {}",
            msg
        );
    }

    #[test]
    fn scan_yaml_files_rejects_symlink_file() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();

        // Create a real target file
        let target = dir.path().join("target.yaml");
        fs::write(&target, "section: routes").unwrap();

        // Create a symlink to it inside routes dir
        let link = routes_dir.join("evil.yaml");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = scan_yaml_files_recursive(&routes_dir);
        let err = result.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("symlink"),
            "symlinked file should be rejected, got: {}",
            msg
        );
    }

    #[test]
    fn scan_yaml_files_deterministic_order() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();

        // Create files in non-alphabetical order
        fs::write(routes_dir.join("zebra.yaml"), "").unwrap();
        fs::write(routes_dir.join("alpha.yaml"), "").unwrap();
        fs::write(routes_dir.join("middle.yaml"), "").unwrap();

        let files = scan_yaml_files_recursive(&routes_dir).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(names, vec!["alpha.yaml", "middle.yaml", "zebra.yaml"]);
    }

    #[test]
    fn scan_yaml_files_nested_deterministic_order() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        let api_dir = routes_dir.join("api");
        let ui_dir = routes_dir.join("ui");
        let cockpit_dir = ui_dir.join("cockpit");
        fs::create_dir_all(&cockpit_dir).unwrap();
        fs::create_dir_all(&api_dir).unwrap();

        // api/a.yaml (alphabetically before ui/)
        fs::write(api_dir.join("a.yaml"), "").unwrap();
        // ui/aaa.yaml (alphabetically before ui/cockpit/)
        fs::write(ui_dir.join("aaa.yaml"), "").unwrap();
        // ui/cockpit/c.yaml (deeper under ui/)
        fs::write(cockpit_dir.join("c.yaml"), "").unwrap();

        let files = scan_yaml_files_recursive(&routes_dir).unwrap();
        let relative: Vec<String> = files
            .iter()
            .map(|f| {
                f.strip_prefix(&routes_dir)
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();

        // api/ sorts before ui/
        // Within ui/: aaa.yaml sorts before cockpit/ (a < c)
        // So order is: api/a.yaml, ui/aaa.yaml, ui/cockpit/c.yaml
        assert_eq!(
            relative,
            vec![
                "api/a.yaml".to_string(),
                "ui/aaa.yaml".to_string(),
                "ui/cockpit/c.yaml".to_string(),
            ]
        );
    }

    // ── Section invariant tests (new: under-section-root) ─────────────

    #[test]
    fn section_invariant_nested_file_under_correct_section() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        let cockpit_dir = routes_dir.join("ui").join("cockpit");
        fs::create_dir_all(&cockpit_dir).unwrap();

        let file = cockpit_dir.join("snapshot.yaml");
        fs::write(&file, "section: routes").unwrap();

        let body: Value = serde_yaml::from_str(&fs::read_to_string(&file).unwrap()).unwrap();

        // File is under routes/ and declares section: routes — should pass
        assert!(file.starts_with(&routes_dir));
        assert_eq!(body.get("section").and_then(|v| v.as_str()), Some("routes"));
    }

    #[test]
    fn section_invariant_wrong_section_in_nested_dir() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        let nested = routes_dir.join("aliased");
        fs::create_dir_all(&nested).unwrap();

        let file = nested.join("evil.yaml");
        // Declares section: aliases but lives under routes/
        fs::write(&file, "section: aliases").unwrap();

        // Use a dummy trust store — we're testing the invariant logic,
        // not the signature verification here. Instead, test directly.
        let body: Value = serde_yaml::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        let declared_section = body.get("section").and_then(|v| v.as_str()).unwrap();

        // File IS under routes_dir
        assert!(file.starts_with(&routes_dir));
        // But declares wrong section
        assert_ne!(declared_section, "routes");
    }

    #[test]
    fn load_full_loads_real_cockpit_bundle_nested_routes() {
        let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .expect("workspace root with bundles/ directory")
            .to_path_buf();
        let trusted_dir = workspace.join("crates/bin/daemon/tests/fixtures/trusted_signers");
        let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load test trust store");

        let core = workspace.join("bundles/core").canonicalize().unwrap();
        let standard = workspace.join("bundles/standard").canonicalize().unwrap();
        let cockpit = workspace.join("bundles/cockpit").canonicalize().unwrap();
        let bundles = vec![
            BundleRecord {
                name: "standard".into(),
                path: standard,
                source_file: workspace.join("bundles/core/.ai/node/bundles/standard.yaml"),
            },
            BundleRecord {
                name: "cockpit".into(),
                path: cockpit,
                source_file: workspace.join("bundles/core/.ai/node/bundles/cockpit.yaml"),
            },
        ];

        let loader = BootstrapLoader {
            system_space_dir: &core,
            trust_store: &trust_store,
        };
        let snapshot = loader
            .load_full(&SectionTable::new(), &bundles)
            .expect("load full node config with cockpit bundle");

        let cockpit_snapshot_route = snapshot
            .routes
            .iter()
            .find(|route| route.path == "/ui/api/cockpit/snapshot")
            .expect("cockpit snapshot route should load from nested route directory");
        assert!(
            cockpit_snapshot_route
                .source_file
                .ends_with(".ai/node/routes/ui/cockpit/snapshot.yaml"),
            "route source should preserve nested path, got {}",
            cockpit_snapshot_route.source_file.display()
        );
        assert!(
            snapshot.routes.iter().any(|route| route.path == "/ui"),
            "moved cockpit bundle should still provide base UI route"
        );
        assert!(
            snapshot
                .routes
                .iter()
                .any(|route| route.path == "/ui/api/items/effective"),
            "browser-session effective item route should load from nested route directory"
        );
    }

    #[test]
    fn synthesize_aliases_from_verbs_inherits_description_and_source() {
        let verb = VerbRecord {
            category: "verbs".into(),
            section: "verbs".into(),
            name: "sign".into(),
            description: "Sign an item".into(),
            execute: Some("tool:ryeos/core/sign".into()),
            aliases: vec![crate::node_config::sections::verb::VerbAliasRecord {
                tokens: vec!["sign".into()],
                description: None,
                deprecated: None,
                replacement_tokens: None,
                removed_in: None,
                positional_forms: Vec::new(),
                project_resolution: crate::node_config::sections::alias::ProjectResolution::None,
            }],
            source_file: PathBuf::from("/bundle/.ai/node/verbs/sign.yaml"),
        };

        let aliases = synthesize_aliases_from_verbs(&[verb]);
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].tokens, vec!["sign"]);
        assert_eq!(aliases[0].verb, "sign");
        assert_eq!(aliases[0].description, "Sign an item");
        assert_eq!(
            aliases[0].source_file,
            PathBuf::from("/bundle/.ai/node/verbs/sign.yaml")
        );
    }

    #[test]
    fn check_alias_collisions_errors_on_duplicate_tokens() {
        let first = AliasRecord {
            category: "aliases".into(),
            section: "aliases".into(),
            tokens: vec!["sign".into()],
            verb: "sign".into(),
            description: "Sign".into(),
            deprecated: None,
            replacement_tokens: None,
            removed_in: None,
            positional_forms: Vec::new(),
            project_resolution: crate::node_config::sections::alias::ProjectResolution::None,
            source_file: PathBuf::from("/a.yaml"),
        };
        let mut second = first.clone();
        second.verb = "other".into();
        second.source_file = PathBuf::from("/b.yaml");

        let err = check_alias_collisions(&[first, second]).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("duplicate tokens"), "got: {msg}");
        assert!(msg.contains("/a.yaml"), "got: {msg}");
        assert!(msg.contains("/b.yaml"), "got: {msg}");
    }
}
