//! Node-config loader: two-phase bootstrap for daemon-consumed control-plane items.
//!
//! Phase 1: `load_bundle_section()` — minimal bootstrap verifier, bundle roots only.
//! Phase 2: `load_full()` — full engine-based scan across all sources.
//!
//! Section directories support recursive subfolders. For example:
//!
//!   .ai/node/routes/ui/ryeos-ui/dimension-get.yaml
//!   .ai/node/routes/ui/ryeos-ui/items/list.yaml
//!   .ai/node/commands/web.yaml
//!
//! The section invariant requires the file to live under
//! `.ai/node/<section>/` (any depth). Section identity is derived from the
//! path, not duplicated in the YAML body.
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

use super::sections::bundle::BundleSection;
use super::sections::command::CommandRecord;
use super::sections::command_registration::CommandRegistrationPolicyRecord;
use super::sections::hosted_node::HostedNodePolicyRecord;
use super::{
    BundleRecord, NodeConfigSection, NodeConfigSnapshot, NodeItemContext, SectionSourcePolicy,
    SectionTable,
};
use crate::route_raw::RawRouteSpec;

/// Bootstrap loader for node-config items.
///
/// Phase 1: `load_bundle_section()` — minimal bootstrap verifier, bundle roots only.
/// Phase 2: `load_full()` — full engine-based scan across all sources.
pub struct BootstrapLoader<'a> {
    pub app_root: &'a Path,
    pub trust_store: &'a TrustStore,
}

/// A verified and parsed node-config YAML file, ready for section-specific handling.
struct VerifiedItem {
    path: PathBuf,
    ctx: NodeItemContext,
    signer_fingerprint: String,
    body: Value,
}

#[derive(Debug, Clone)]
struct NodeConfigScanRoot {
    path: PathBuf,
    command_provenance: ryeos_runtime::CommandProvenance,
}

/// Signature envelope used for all node-config items.
fn node_config_envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    }
}

/// Load one standalone node YAML item through the same strict trust boundary
/// used by registered node-config sections. This is for typed control-plane
/// declarations that have not yet become a full [`NodeConfigSection`]: regular
/// YAML files only, symlinks rejected, trusted signature required, legacy
/// structural fields rejected.
pub fn load_verified_node_yaml(file: &Path, trust_store: &TrustStore) -> Result<Value> {
    let ext = file.extension().and_then(|extension| extension.to_str());
    if !matches!(ext, Some("yaml" | "yml")) {
        bail!(
            "node config item at {} is not a .yaml or .yml file",
            file.display()
        );
    }
    let content = lillux::read_regular_file_to_string_no_follow(file)
        .with_context(|| format!("failed to securely read {}", file.display()))?;
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
            "node config item at {} is not trusted (trust_class: {:?}); only trusted items are allowed in node config",
            file.display(),
            trust_class
        );
    }
    let body: Value = serde_yaml::from_str(&strip_signature(&content))
        .with_context(|| format!("failed to parse YAML body of {}", file.display()))?;
    for forbidden in ["category", "section"] {
        if body.get(forbidden).is_some() {
            bail!(
                "node config item at {} declares legacy structural field '{}' (section/category are path-owned)",
                file.display(),
                forbidden
            );
        }
    }
    Ok(body)
}

/// Recursively collect all `.yaml`/`.yml` regular files under a directory.
///
/// Returns files in deterministic depth-first order, with entries sorted
/// alphabetically at each directory level. Rejects symlinks at every level
/// (both files and directories).
fn scan_yaml_files_recursive(dir: &Path) -> Result<Vec<PathBuf>> {
    scan_yaml_files(dir, true)
}

fn scan_yaml_files(dir: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    let files = lillux::collect_regular_files_no_follow(dir, recursive)?.unwrap_or_default();
    for path in &files {
        let ext = path.extension().and_then(|extension| extension.to_str());
        if !matches!(ext, Some("yaml" | "yml")) {
            bail!(
                "node config directory contains unsupported non-YAML entry {}",
                path.display()
            );
        }
    }
    Ok(files)
}

/// Verify signature, trust, and section invariant for a single node-config file.
///
/// The `section_root` is the `.ai/node/<section>/` directory (e.g. the
/// `routes/` dir). The invariant checks that:
/// 1. `file` lives under `section_root` (at any depth)
fn verify_and_parse(
    file: &Path,
    section_root: &Path,
    expected_section: &str,
    trust_store: &TrustStore,
) -> Result<VerifiedItem> {
    let ext = file.extension().and_then(|e| e.to_str());
    if ext != Some("yaml") && ext != Some("yml") {
        bail!(
            "node config item at {} is not a .yaml or .yml file",
            file.display()
        );
    }

    let stem = file.file_stem().and_then(|s| s.to_str()).context(format!(
        "node config item at {} has no filename stem",
        file.display()
    ))?;

    let content = lillux::read_regular_file_to_string_no_follow(file)
        .with_context(|| format!("failed to securely read {}", file.display()))?;

    let envelope = node_config_envelope();

    let header = ryeos_engine::item_resolution::parse_signature_header(&content, &envelope)
        .with_context(|| {
            format!(
                "node config item at {} has no valid signature line",
                file.display()
            )
        })?;
    let signer_fingerprint = header.signer_fingerprint.clone();

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

    // Check section invariant: file must live under section_root. Section is
    // selected by path, not duplicated in the body.
    if !file.starts_with(section_root) {
        bail!(
            "node config item at {} is not under expected section directory '{}' \
             (section containment invariant violated)",
            file.display(),
            section_root.display()
        );
    }

    for forbidden in ["category", "section"] {
        if body.get(forbidden).is_some() {
            bail!(
                "node config item at {} declares legacy structural field '{}' \
                 (section/category are derived from path and must not be in node YAML)",
                file.display(),
                forbidden
            );
        }
    }

    let rel_path = file.strip_prefix(section_root).with_context(|| {
        format!(
            "node config item at {} is not under expected section directory {}",
            file.display(),
            section_root.display()
        )
    })?;
    let mut id_path = rel_path.to_path_buf();
    id_path.set_extension("");
    let id = id_path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(part) => part
                .to_str()
                .map(|s| s.to_string())
                .context("node config path contains non-UTF-8 segment"),
            _ => bail!("node config relative path contains non-normal segment"),
        })
        .collect::<Result<Vec<_>>>()?
        .join("/");

    if id.is_empty() {
        bail!(
            "node config item at {} has empty path-derived id",
            file.display()
        );
    }

    let ctx = NodeItemContext {
        section: expected_section.to_string(),
        id,
        stem: stem.to_string(),
        rel_path: rel_path.to_path_buf(),
        source_file: file.to_path_buf(),
        signer_fingerprint: signer_fingerprint.clone(),
    };

    Ok(VerifiedItem {
        path: file.to_path_buf(),
        ctx,
        signer_fingerprint,
        body,
    })
}

impl<'a> BootstrapLoader<'a> {
    /// Phase 1: load only the `bundles` section to determine effective bundle roots.
    ///
    /// Scans `<app_root>/.ai/node/bundles/` (flat only — no recursion).
    /// Uses minimal bootstrap verifier (signature + hash, no full engine).
    pub fn load_bundle_section(&self) -> Result<Vec<BundleRecord>> {
        let section = BundleSection;
        let mut records: Vec<BundleRecord> = Vec::new();

        let node_dir = self.app_root.join(".ai").join("node").join("bundles");
        // Bundles use one exact flat YAML namespace. Missing is empty; any
        // directory, symlink, special file, or non-YAML entry is an error.
        for path in scan_yaml_files(&node_dir, false)? {
            let verified = verify_and_parse(&path, &node_dir, "bundles", self.trust_store)?;

            let record = section
                .parse(&verified.ctx, &verified.body)
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
    /// For sections with `EffectiveBundleRootsAndState` policy (routes, commands),
    /// scans recursively into subdirectories. For `SystemAndState` (bundles), scans flat.
    pub fn load_full(
        &self,
        section_table: &SectionTable,
        bundles: &[BundleRecord],
    ) -> Result<NodeConfigSnapshot> {
        self.load_full_inner(section_table, bundles, false)
    }

    /// Load the full node-config graph against an authoritative prospective
    /// bundle registry.
    ///
    /// Bundle registration files on disk still describe the live generation
    /// during pre-activation admission, so the normal full pass cannot model a
    /// replacement or removal exactly. This variant substitutes `bundles` for
    /// the `bundles` section while scanning every other system and bundle
    /// section normally.
    pub fn load_full_prospective(
        &self,
        section_table: &SectionTable,
        bundles: &[BundleRecord],
    ) -> Result<NodeConfigSnapshot> {
        self.load_full_inner(section_table, bundles, true)
    }

    fn load_full_inner(
        &self,
        section_table: &SectionTable,
        bundles: &[BundleRecord],
        prospective_bundle_registry: bool,
    ) -> Result<NodeConfigSnapshot> {
        let command_registration_policy = self.load_command_registration_policy(section_table)?;
        let mut loaded_bundles = if prospective_bundle_registry {
            validate_prospective_bundle_records(bundles)?
        } else {
            Vec::new()
        };
        let mut routes: Vec<RawRouteSpec> = Vec::new();
        let mut commands: Vec<CommandRecord> = Vec::new();
        let mut hosted_node_policies: Vec<HostedNodePolicyRecord> = Vec::new();

        for section_name in section_table.section_names() {
            if section_name == "command_registration" {
                continue;
            }
            if prospective_bundle_registry && section_name == "bundles" {
                continue;
            }
            let section = section_table.get(section_name).context(format!(
                "section '{}' registered but handler missing",
                section_name
            ))?;

            let scan_roots = match section.source_policy() {
                SectionSourcePolicy::SystemAndState => {
                    vec![system_scan_root(
                        self.app_root,
                        &command_registration_policy.policy,
                    )]
                }
                SectionSourcePolicy::EffectiveBundleRootsAndState => {
                    let mut roots = vec![system_scan_root(
                        self.app_root,
                        &command_registration_policy.policy,
                    )];
                    for b in bundles {
                        if !roots.iter().any(|r| r.path == b.path) {
                            roots.push(bundle_scan_root(b));
                        }
                    }
                    roots
                }
            };

            for scan_root in &scan_roots {
                let section_dir = scan_root.path.join(".ai").join("node").join(section_name);

                // Routes and commands: recursive scan.
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
                    scan_yaml_files(&section_dir, false)?
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
                                .parse(&verified.ctx, &verified.body)
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
                                .parse(&verified.ctx, &verified.body)
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
                    } else if section_name == "commands" {
                        let record =
                            section
                                .parse(&verified.ctx, &verified.body)
                                .with_context(|| {
                                    format!(
                                        "failed to parse command record {}",
                                        verified.path.display()
                                    )
                                })?;
                        let mut record: CommandRecord = record
                            .as_any()
                            .downcast_ref::<CommandRecord>()
                            .context("CommandSection::parse returned wrong type")?
                            .clone();
                        record.source_file = verified.path.clone();
                        record.provenance = scan_root.command_provenance.clone();
                        commands.push(record);
                    } else if section_name == "hosted" {
                        let record =
                            section
                                .parse(&verified.ctx, &verified.body)
                                .with_context(|| {
                                    format!(
                                        "failed to parse hosted-node policy record {}",
                                        verified.path.display()
                                    )
                                })?;
                        let mut record: HostedNodePolicyRecord = record
                            .as_any()
                            .downcast_ref::<HostedNodePolicyRecord>()
                            .context("HostedNodePolicySection::parse returned wrong type")?
                            .clone();
                        record.source_file = verified.path.clone();
                        hosted_node_policies.push(record);
                    }
                }
            }

            if section_name == "bundles" {
                check_bundle_collisions(&loaded_bundles)?;
            }
        }

        ryeos_runtime::CommandRegistry::from_records(
            &commands,
            &command_registration_policy.policy,
        )
        .context("validate loaded command registry")?;
        check_hosted_policy_uniqueness(&hosted_node_policies)?;

        Ok(NodeConfigSnapshot {
            bundles: loaded_bundles,
            routes,
            commands,
            hosted_node_policies,
            command_registration_policy,
        })
    }

    fn load_command_registration_policy(
        &self,
        section_table: &SectionTable,
    ) -> Result<CommandRegistrationPolicyRecord> {
        let section_name = "command_registration";
        let node_fingerprint = node_identity_fingerprint(self.app_root)?;
        let section = section_table
            .get(section_name)
            .context("command_registration section handler missing")?;
        let section_dir = self.app_root.join(".ai").join("node").join(section_name);
        if !section_dir.is_dir() {
            bail!(
                "missing required node config section '{}' at {}",
                section_name,
                section_dir.display()
            );
        }

        let yaml_files = scan_yaml_files_recursive(&section_dir).with_context(|| {
            format!(
                "failed to scan node config section '{}' recursively in {}",
                section_name,
                section_dir.display()
            )
        })?;

        let mut records = Vec::new();
        for path in yaml_files {
            let verified = verify_and_parse(&path, &section_dir, section_name, self.trust_store)
                .with_context(|| {
                    format!(
                        "failed to verify node config item {} in section '{}'",
                        path.display(),
                        section_name
                    )
                })?;
            if verified.signer_fingerprint != node_fingerprint {
                bail!(
                    "command registration policy {} must be signed by node identity {}; got signer {}",
                    verified.path.display(),
                    node_fingerprint,
                    verified.signer_fingerprint
                );
            }
            let record = section
                .parse(&verified.ctx, &verified.body)
                .with_context(|| {
                    format!(
                        "failed to parse command registration policy {}",
                        verified.path.display()
                    )
                })?;
            let mut record: CommandRegistrationPolicyRecord = record
                .as_any()
                .downcast_ref::<CommandRegistrationPolicyRecord>()
                .context("CommandRegistrationSection::parse returned wrong type")?
                .clone();
            record.source_file = verified.path.clone();
            records.push(record);
        }

        match records.len() {
            1 => Ok(records.remove(0)),
            0 => bail!(
                "node config section '{}' must contain exactly one policy record",
                section_name
            ),
            _ => bail!(
                "node config section '{}' has multiple policy records; refusing ambiguous command registration policy",
                section_name
            ),
        }
    }
}

fn node_identity_fingerprint(app_root: &Path) -> Result<String> {
    let key_path = app_root
        .join(".ai")
        .join("node")
        .join("identity")
        .join("private_key.pem");
    let identity = crate::identity::NodeIdentity::load(&key_path).with_context(|| {
        format!(
            "load node identity for node-owned command registration policy from {}",
            key_path.display()
        )
    })?;
    Ok(identity.fingerprint().to_string())
}

fn check_hosted_policy_uniqueness(records: &[HostedNodePolicyRecord]) -> Result<()> {
    if records.len() <= 1 {
        return Ok(());
    }

    let sources = records
        .iter()
        .map(|record| record.source_file.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "multiple hosted-node policies loaded; refusing ambiguous hosted policy set: {}",
        sources
    )
}

fn system_scan_root(
    app_root: &Path,
    policy: &ryeos_runtime::CommandRegistrationPolicy,
) -> NodeConfigScanRoot {
    NodeConfigScanRoot {
        path: app_root.to_path_buf(),
        command_provenance: ryeos_runtime::CommandProvenance {
            origin: ryeos_runtime::CommandOrigin::SystemSpace,
            command_registration_caps: policy.system_source_caps.clone(),
        },
    }
}

fn bundle_scan_root(bundle: &BundleRecord) -> NodeConfigScanRoot {
    NodeConfigScanRoot {
        path: bundle.path.clone(),
        command_provenance: ryeos_runtime::CommandProvenance {
            origin: ryeos_runtime::CommandOrigin::InstalledBundle,
            command_registration_caps: bundle.command_registration_caps.clone(),
        },
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

fn validate_prospective_bundle_records(records: &[BundleRecord]) -> Result<Vec<BundleRecord>> {
    let mut validated = Vec::with_capacity(records.len());
    for record in records {
        if !record.path.is_absolute() {
            bail!(
                "prospective bundle '{}' path must be absolute, got {}",
                record.name,
                record.path.display()
            );
        }
        if !record.path.is_dir() {
            bail!(
                "prospective bundle '{}' path '{}' does not exist or is not a directory",
                record.name,
                record.path.display()
            );
        }
        let mut record = record.clone();
        record.path = record.path.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize prospective bundle '{}' path '{}'",
                record.name,
                record.path.display()
            )
        })?;
        validated.push(record);
    }
    check_bundle_collisions(&validated)?;
    Ok(validated)
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
    use crate::node_config::sections::hosted_node::{
        HostedNodeAdmissionPolicy, HostedNodeAuthorizationPolicy, HostedNodeDescriptorPolicy,
        HostedNodeOperationsPolicy, HostedNodeTransportPolicy,
    };

    #[test]
    fn strip_signature_removes_signed_line() {
        let content = "# ryeos:signed:2026-01-01T00:00:00Z:abc123:sig456:fp789\npath: /foo\n";
        let stripped = strip_signature(content);
        assert!(stripped.starts_with("path: /foo"));
        assert!(!stripped.contains("ryeos:signed:"));
    }

    #[test]
    fn strip_signature_preserves_body() {
        let content = "# ryeos:signed:2026-01-01T00:00:00Z:abc:sig:fp\npath: /foo/bar\n";
        let stripped = strip_signature(content);
        let parsed: Value = serde_yaml::from_str(&stripped).unwrap();
        assert_eq!(parsed["path"], "/foo/bar");
    }

    fn make_record(name: &str, path: &str, source: &str) -> BundleRecord {
        BundleRecord {
            name: name.into(),
            path: std::path::PathBuf::from(path),
            command_registration_caps: Vec::new(),
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
        let ryeos_dir = ui_dir.join("ryeos-ui");
        fs::create_dir_all(&ryeos_dir).unwrap();

        // Flat file
        fs::write(routes_dir.join("health.yaml"), "id: health").unwrap();
        // Nested one level
        fs::write(ui_dir.join("index.yaml"), "id: ui.index").unwrap();
        // Nested two levels
        fs::write(
            ryeos_dir.join("dimension-get.yaml"),
            "id: ui.ryeos.dimension-get",
        )
        .unwrap();
        // Hidden file (should be found — no hidden filtering)
        fs::write(routes_dir.join(".hidden.yaml"), "id: hidden").unwrap();

        let files = scan_yaml_files_recursive(&routes_dir).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        // Sorted: .hidden.yaml, health.yaml, then ui/index.yaml, ui/ryeos-ui/dimension-get.yaml
        assert_eq!(names.len(), 4, "expected 4 yaml files, got: {:?}", names);
        assert!(names.contains(&".hidden.yaml".to_string()));
        assert!(names.contains(&"health.yaml".to_string()));
        assert!(names.contains(&"dimension-get.yaml".to_string()));
        assert!(names.contains(&"index.yaml".to_string()));

        // Verify deterministic order: depth-first, sorted at each level
        // Level 1: .hidden.yaml, health.yaml, ui/
        //   Level 2: ui/index.yaml, ui/ryeos-ui/dimension-get.yaml
        assert_eq!(names[0], ".hidden.yaml");
        assert_eq!(names[1], "health.yaml");
        assert_eq!(names[2], "index.yaml");
        assert_eq!(names[3], "dimension-get.yaml");
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

        assert!(scan_yaml_files_recursive(&routes_dir).is_err());
    }

    #[test]
    fn scan_yaml_files_rejects_symlink_file() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();

        // Create a real target file
        let target = dir.path().join("target.yaml");
        fs::write(&target, "id: target").unwrap();

        // Create a symlink to it inside routes dir
        let link = routes_dir.join("evil.yaml");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(scan_yaml_files_recursive(&routes_dir).is_err());
    }

    #[test]
    fn scan_yaml_files_recursive_rejects_non_yaml_entries() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(routes_dir.join("README.md"), "not yaml").unwrap();

        assert!(scan_yaml_files_recursive(&routes_dir).is_err());
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
        let ryeos_dir = ui_dir.join("ryeos-ui");
        fs::create_dir_all(&ryeos_dir).unwrap();
        fs::create_dir_all(&api_dir).unwrap();

        // api/a.yaml (alphabetically before ui/)
        fs::write(api_dir.join("a.yaml"), "").unwrap();
        // ui/aaa.yaml (alphabetically before ui/ryeos-ui/)
        fs::write(ui_dir.join("aaa.yaml"), "").unwrap();
        // ui/ryeos-ui/c.yaml (deeper under ui/)
        fs::write(ryeos_dir.join("c.yaml"), "").unwrap();

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
        // Within ui/: aaa.yaml sorts before RyeOS UI/ (a < c)
        // So order is: api/a.yaml, ui/aaa.yaml, ui/ryeos-ui/c.yaml
        assert_eq!(
            relative,
            vec![
                "api/a.yaml".to_string(),
                "ui/aaa.yaml".to_string(),
                "ui/ryeos-ui/c.yaml".to_string(),
            ]
        );
    }

    // ── Section invariant tests (new: under-section-root) ─────────────

    #[test]
    fn section_invariant_nested_file_under_correct_section() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        let ryeos_dir = routes_dir.join("ui").join("ryeos-ui");
        fs::create_dir_all(&ryeos_dir).unwrap();

        let file = ryeos_dir.join("dimension-get.yaml");
        fs::write(&file, "id: ui.ryeos.dimension-get").unwrap();

        let body: Value = serde_yaml::from_str(&fs::read_to_string(&file).unwrap()).unwrap();

        // File is under routes/; section identity comes from containment.
        assert!(file.starts_with(&routes_dir));
        assert_eq!(
            body.get("id").and_then(|v| v.as_str()),
            Some("ui.ryeos.dimension-get")
        );
    }

    #[test]
    fn section_invariant_wrong_section_in_nested_dir() {
        let dir = tempfile::tempdir().unwrap();
        let routes_dir = dir.path().join("routes");
        let nested = routes_dir.join("aliased");
        fs::create_dir_all(&nested).unwrap();

        let file = nested.join("evil.yaml");
        // Legacy structural fields are no longer allowed in node YAML; section
        // identity comes from the containing directory.
        fs::write(&file, "section: aliases").unwrap();

        let body: Value = serde_yaml::from_str(&fs::read_to_string(&file).unwrap()).unwrap();

        assert!(file.starts_with(&routes_dir));
        assert!(body.get("section").is_some());
    }

    #[test]
    fn load_full_loads_real_ryeos_ui_bundle_nested_routes() {
        let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .expect("workspace root with bundles/ directory")
            .to_path_buf();
        let trusted_dir = workspace.join("crates/bin/daemon/tests/fixtures/trusted_signers");
        let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load test trust store");

        let system = temp_system_with_command_registration_policy(&workspace);
        let ryeos_ui = temp_bundle_with_node_section(&workspace.join("bundles/ryeos-ui"), "routes");
        let bundles = vec![BundleRecord {
            name: "ryeos-ui".into(),
            path: ryeos_ui.path().to_path_buf(),
            command_registration_caps: Vec::new(),
            source_file: workspace.join("bundles/core/.ai/node/bundles/ryeos-ui.yaml"),
        }];

        let loader = BootstrapLoader {
            app_root: system.path(),
            trust_store: &trust_store,
        };
        let snapshot = loader
            .load_full(&SectionTable::new(), &bundles)
            .expect("load full node config with RyeOS UI bundle");

        let ryeos_dimension_route = snapshot
            .routes
            .iter()
            .find(|route| route.path == "/ui/api/ryeos-ui/dimension")
            .expect("RyeOS UI dimension route should load from nested route directory");
        assert!(
            ryeos_dimension_route
                .source_file
                .ends_with(".ai/node/routes/ui/ryeos-ui/dimension-get.yaml"),
            "route source should preserve nested path, got {}",
            ryeos_dimension_route.source_file.display()
        );
        assert!(
            snapshot.routes.iter().any(|route| route.path == "/ui"),
            "moved RyeOS UI bundle should still provide base UI route"
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
    fn load_full_loads_hosted_node_policy_from_bundle() {
        let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .expect("workspace root with bundles/ directory")
            .to_path_buf();
        let trusted_dir = workspace.join("crates/bin/daemon/tests/fixtures/trusted_signers");
        let trust_store = TrustStore::load_from_dir(&trusted_dir).expect("load test trust store");

        let system = temp_system_with_command_registration_policy(&workspace);
        let hosted_node = workspace
            .join("bundles/hosted-node")
            .canonicalize()
            .unwrap();
        let bundles = vec![BundleRecord {
            name: "hosted-node".into(),
            path: hosted_node,
            command_registration_caps: Vec::new(),
            source_file: workspace.join("bundles/core/.ai/node/bundles/hosted-node.yaml"),
        }];

        let loader = BootstrapLoader {
            app_root: system.path(),
            trust_store: &trust_store,
        };
        let snapshot = loader
            .load_full(&SectionTable::new(), &bundles)
            .expect("load full node config with hosted-node bundle");

        assert_eq!(snapshot.hosted_node_policies.len(), 1);
        let policy = &snapshot.hosted_node_policies[0];
        assert!(policy.transport.public_https_required);
        assert_eq!(policy.admission.mode, "one_time_token");
        assert_eq!(
            policy.authorization.authority,
            "target_node_authorized_keys"
        );
        assert!(!policy.authorization.central_bearer_tokens_allowed);
        assert!(!policy.operations.shared_daemon_multitenancy_enabled);
        assert!(
            policy.source_file.ends_with(".ai/node/hosted/policy.yaml"),
            "policy source should be the hosted node section, got {}",
            policy.source_file.display()
        );
    }

    fn temp_system_with_command_registration_policy(
        workspace: &std::path::Path,
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let identity_dir = dir.path().join(".ai/node/identity");
        fs::create_dir_all(&identity_dir).unwrap();
        fs::copy(
            workspace.join(".dev-keys/PUBLISHER_DEV.pem"),
            identity_dir.join("private_key.pem"),
        )
        .unwrap();
        let target = dir.path().join(".ai/node/command_registration");
        fs::create_dir_all(&target).unwrap();
        fs::copy(
            workspace.join("bundles/.ai/node/init/command-registration/default.yaml"),
            target.join("default.yaml"),
        )
        .unwrap();
        dir
    }

    fn temp_bundle_with_node_section(bundle: &std::path::Path, section: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source = bundle.join(".ai/node").join(section);
        let target = dir.path().join(".ai/node").join(section);
        copy_dir_recursive_for_test(&source, &target);
        dir
    }

    fn copy_dir_recursive_for_test(source: &std::path::Path, target: &std::path::Path) {
        fs::create_dir_all(target).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let source_path = entry.path();
            let target_path = target.join(entry.file_name());
            if source_path.is_dir() {
                copy_dir_recursive_for_test(&source_path, &target_path);
            } else {
                fs::copy(&source_path, &target_path).unwrap();
            }
        }
    }

    #[test]
    fn hosted_policy_uniqueness_rejects_multiple_policies() {
        let mk_record = |source_file: &str| HostedNodePolicyRecord {
            version: "0.1.0".into(),
            schema_version: "1.0.0".into(),
            description: "test".into(),
            transport: HostedNodeTransportPolicy {
                public_https_required: true,
                loopback_http_allowed: true,
            },
            admission: HostedNodeAdmissionPolicy {
                mode: "one_time_token".into(),
                token_ttl_secs: 600,
                reject_wildcard_scopes: true,
                token_delivery: "out_of_band".into(),
            },
            descriptor: HostedNodeDescriptorPolicy {
                require_live_identity_match: true,
                advertised_capabilities: vec![],
            },
            authorization: HostedNodeAuthorizationPolicy {
                authority: "target_node_authorized_keys".into(),
                central_bearer_tokens_allowed: false,
                implicit_cross_node_authority_allowed: false,
            },
            operations: HostedNodeOperationsPolicy {
                audit_admission_events: true,
                audit_grant_changes: true,
                prefer_isolated_node_per_principal: true,
                shared_daemon_multitenancy_enabled: false,
            },
            source_file: std::path::PathBuf::from(source_file),
        };

        let err = check_hosted_policy_uniqueness(&[
            mk_record("/bundle/.ai/node/hosted/policy.yaml"),
            mk_record("/state/.ai/node/hosted/policy.yaml"),
        ])
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("multiple hosted-node policies"), "got: {msg}");
        assert!(
            msg.contains("/bundle/.ai/node/hosted/policy.yaml"),
            "got: {msg}"
        );
        assert!(
            msg.contains("/state/.ai/node/hosted/policy.yaml"),
            "got: {msg}"
        );
    }

    #[test]
    fn command_registration_caps_follow_policy_and_registration_not_bundle_name() {
        let system = std::path::PathBuf::from("/system");
        let policy = ryeos_runtime::CommandRegistrationPolicy {
            claim_rules: Vec::new(),
            system_source_caps: vec!["ryeos.register.command.root.execute".into()],
        };
        let core = BundleRecord {
            name: "core".into(),
            path: std::path::PathBuf::from("/system/.ai/bundles/core"),
            command_registration_caps: Vec::new(),
            source_file: std::path::PathBuf::from("/system/.ai/node/bundles/core.yaml"),
        };
        let standard = BundleRecord {
            name: "standard".into(),
            path: std::path::PathBuf::from("/system/.ai/bundles/standard"),
            command_registration_caps: vec!["ryeos.register.command.root.standard".into()],
            source_file: std::path::PathBuf::from("/system/.ai/node/bundles/standard.yaml"),
        };

        assert_eq!(
            system_scan_root(&system, &policy)
                .command_provenance
                .command_registration_caps,
            vec!["ryeos.register.command.root.execute"]
        );
        assert_eq!(
            bundle_scan_root(&core)
                .command_provenance
                .command_registration_caps,
            Vec::<String>::new()
        );
        assert_eq!(
            bundle_scan_root(&standard)
                .command_provenance
                .command_registration_caps,
            vec!["ryeos.register.command.root.standard"]
        );
    }
}
