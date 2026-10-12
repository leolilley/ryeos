//! Full domain node-config loader; bundle bootstrap belongs to Node Runtime.
//!
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

use std::collections::BTreeSet;
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
#[cfg(test)]
use serde_json::Value;

use ryeos_engine::trust::TrustStore;

use ryeos_node_runtime::node_config_source::{VerifiedNodeConfigItem, scan_yaml_files, scan_yaml_files_recursive, verify_and_parse, check_bundle_collisions};
#[cfg(test)]
use ryeos_node_runtime::node_config_source::strip_signature;
use super::sections::command::CommandRecord;
use super::{
    BundleRecord, NodeConfigSection, NodeConfigSnapshot, NodeConfigSourceScope, NodeConfigTable,
    SectionCardinality, SectionLoadPhase, SectionSignerPolicy, SectionTraversal,
};
use ryeos_node_runtime::node_policy::sections::command_registration::CommandRegistrationAuthority;
use crate::route_raw::RawRouteSpec;

/// Full loader for domain node-config items.
///
/// Phase 2: `load_full()` — full engine-based scan across all sources.
pub struct NodeConfigLoader<'a> {
    pub app_root: &'a Path,
    pub trust_store: &'a TrustStore,
}

#[derive(Debug, Clone)]
struct NodeConfigScanRoot {
    path: PathBuf,
    source: NodeConfigSource,
}

#[derive(Debug, Clone)]
enum NodeConfigSource {
    Node {
        command_registration_caps: Vec<String>,
    },
    Bundle {
        name: String,
        command_registration_caps: Vec<String>,
    },
}

impl NodeConfigSource {
    fn command_provenance(&self) -> Result<ryeos_runtime::CommandProvenance> {
        match self {
            Self::Node {
                command_registration_caps,
            } => Ok(ryeos_runtime::CommandProvenance {
                origin: ryeos_runtime::CommandOrigin::SystemSpace,
                command_registration_caps: command_registration_caps.clone(),
            }),
            Self::Bundle {
                name,
                command_registration_caps,
            } => {
                if name.is_empty() {
                    bail!("bundle source coordinate has an empty name");
                }
                Ok(ryeos_runtime::CommandProvenance {
                    origin: ryeos_runtime::CommandOrigin::InstalledBundle,
                    command_registration_caps: command_registration_caps.clone(),
                })
            }
        }
    }
}

pub(crate) struct NodeConfigAdmission {
    pub(crate) source_file: PathBuf,
    pub(crate) command_provenance: ryeos_runtime::CommandProvenance,
    /// Exact bytes verified by the loader, never a re-read of source_file.
    pub(crate) signed_source: std::sync::Arc<str>,
    pub(crate) signer_fingerprint: String,
    pub(crate) signer_verifying_key: [u8; 32],
}

pub(crate) struct NodeConfigSnapshotBuilder {
    external_execution: Vec<super::sections::external_execution::InstalledExternalExecutionBinding>,
    runtime_snapshot_production: Vec<
        super::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
    >,
    guest_runtime_materialization: Vec<
        super::sections::guest_runtime_materialization::InstalledGuestRuntimeMaterializationBinding,
    >,
    runtime_snapshot_qualification: Vec<
        super::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
    >,
    bundles: Vec<BundleRecord>,
    routes: Vec<RawRouteSpec>,
    commands: Vec<CommandRecord>,
    command_registration_authority: CommandRegistrationAuthority,
}

impl NodeConfigSnapshotBuilder {
    fn new(
        bundles: &[BundleRecord],
        command_registration_authority: CommandRegistrationAuthority,
    ) -> Result<Self> {
        Ok(Self {
            external_execution: Vec::new(),
            runtime_snapshot_production: Vec::new(),
            guest_runtime_materialization: Vec::new(),
            runtime_snapshot_qualification: Vec::new(),
            bundles: validate_prospective_bundle_records(bundles)?,
            routes: Vec::new(),
            commands: Vec::new(),
            command_registration_authority,
        })
    }

    fn admit(
        &mut self,
        record: Box<dyn super::CompiledNodeConfigItem>,
        verified: &VerifiedNodeConfigItem,
        source: &NodeConfigSource,
    ) -> Result<()> {
        let context = verified.context();
        if record.section_name() != context.section {
            bail!(
                "node-config compiler `{}` returned record for section `{}`",
                context.section,
                record.section_name()
            );
        }
        let admission = NodeConfigAdmission {
            source_file: context.source_file.clone(),
            command_provenance: source.command_provenance()?,
            signed_source: std::sync::Arc::clone(verified.signed_source()),
            signer_fingerprint: verified.signer_fingerprint().to_owned(),
            signer_verifying_key: verified.signer_verifying_key(),
        };
        record.admit(self, &admission)
    }

    pub(crate) fn push_route(&mut self, record: RawRouteSpec) {
        self.routes.push(record);
    }

    pub(crate) fn push_external_execution(
        &mut self,
        record: super::sections::external_execution::InstalledExternalExecutionBinding,
    ) -> Result<()> {
        if self
            .external_execution
            .iter()
            .any(|prior| prior.id() == record.id())
        {
            bail!("duplicate external execution binding identity");
        }
        self.external_execution.push(record);
        Ok(())
    }

    pub(crate) fn push_runtime_snapshot_production(
        &mut self,
        record: super::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
    ) -> Result<()> {
        if self
            .runtime_snapshot_production
            .iter()
            .any(|prior| prior.id() == record.id())
        {
            bail!("duplicate runtime snapshot production binding identity");
        }
        self.runtime_snapshot_production.push(record);
        Ok(())
    }

    pub(crate) fn push_guest_runtime_materialization(
        &mut self,
        record: super::sections::guest_runtime_materialization::InstalledGuestRuntimeMaterializationBinding,
    ) -> Result<()> {
        if self
            .guest_runtime_materialization
            .iter()
            .any(|prior| prior.id() == record.id())
        {
            bail!("duplicate guest runtime materialization binding identity");
        }
        self.guest_runtime_materialization.push(record);
        Ok(())
    }

    pub(crate) fn push_runtime_snapshot_qualification(
        &mut self,
        record: super::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
    ) -> Result<()> {
        if self
            .runtime_snapshot_qualification
            .iter()
            .any(|prior| prior.id() == record.id())
        {
            bail!("duplicate runtime snapshot qualification binding identity");
        }
        self.runtime_snapshot_qualification.push(record);
        Ok(())
    }

    pub(crate) fn push_command(&mut self, record: CommandRecord) {
        self.commands.push(record);
    }

    fn finish(self) -> Result<NodeConfigSnapshot> {
        check_bundle_collisions(&self.bundles)?;
        for qualification in &self.runtime_snapshot_qualification {
            ensure!(
                self.runtime_snapshot_production.iter().any(|producer| {
                    producer.id() == qualification.production_binding_id()
                        && producer.digest() == qualification.production_binding_digest()
                }),
                "snapshot qualification has no exact current producer binding"
            );
        }
        ryeos_runtime::CommandRegistry::from_records(
            &self.commands,
            &self.command_registration_authority.runtime_policy(),
        )
        .context("validate loaded command registry")?;

        Ok(NodeConfigSnapshot {
            external_execution: self.external_execution,
            runtime_snapshot_production: self.runtime_snapshot_production,
            guest_runtime_materialization: self.guest_runtime_materialization,
            runtime_snapshot_qualification: self.runtime_snapshot_qualification,
            bundles: self.bundles,
            routes: self.routes,
            commands: self.commands,
        })
    }
}

impl<'a> NodeConfigLoader<'a> {
    /// Phase 2: full node-config scan across all sections and sources.
    ///
    /// Sections explicitly select app-root-only or app-root-plus-bundle scan scope.
    pub fn load_full(
        &self,
        config_table: &NodeConfigTable,
        bundles: &[BundleRecord],
        command_registration_authority: &CommandRegistrationAuthority,
        policy_table: &ryeos_node_runtime::node_policy::NodePolicyTable,
    ) -> Result<NodeConfigSnapshot> {
        let node_fingerprint = ryeos_node_runtime::identity::node_identity_fingerprint(self.app_root)?;
        command_registration_authority
            .require_bundle_sources(bundles.iter().map(|bundle| bundle.name.as_str()))?;
        let mut builder =
            NodeConfigSnapshotBuilder::new(bundles, command_registration_authority.clone())?;
        reject_disallowed_bundle_section_contributions(config_table, policy_table, bundles)?;

        for section in config_table.sections() {
            let section_name = section.name();
            let spec = section.load_spec();
            if spec.phase != SectionLoadPhase::Full {
                continue;
            }
            let mut count = 0usize;
            let scan_roots = section_scan_roots(
                section,
                self.app_root,
                bundles,
                command_registration_authority,
            )?;
            for scan_root in scan_roots {
                let section_dir = scan_root
                    .path
                    .join(ryeos_engine::AI_DIR)
                    .join("node")
                    .join(section_name);
                let yaml_files = match spec.traversal {
                    SectionTraversal::Recursive => scan_yaml_files_recursive(&section_dir)
                        .with_context(|| {
                            format!(
                                "failed to scan node config section '{}' recursively in {}",
                                section_name,
                                section_dir.display()
                            )
                        })?,
                    SectionTraversal::Flat => scan_yaml_files(&section_dir, false)?,
                };
                for path in yaml_files {
                    let verified =
                        verify_and_parse(&path, &section_dir, section_name, self.trust_store)
                            .with_context(|| {
                                format!(
                                    "failed to verify node config item {} in section '{}'",
                                    path.path().display(),
                                    section_name
                                )
                            })?;
                    verify_section_signer(section, &verified, &node_fingerprint)?;
                    let record =
                        section
                            .parse(verified.context(), verified.body())
                            .with_context(|| {
                                format!(
                                    "failed to parse node config item {} in section '{}'",
                                    verified.path().display(),
                                    section_name
                                )
                            })?;
                    builder.admit(record, &verified, &scan_root.source)?;
                    count = count.saturating_add(1);
                }
            }
            validate_section_cardinality(section, count)?;
        }

        builder.finish()
    }
}


fn system_scan_root(app_root: &Path, policy: &CommandRegistrationAuthority) -> NodeConfigScanRoot {
    NodeConfigScanRoot {
        path: app_root.to_path_buf(),
        source: NodeConfigSource::Node {
            command_registration_caps: policy.system_source_caps.clone(),
        },
    }
}

fn bundle_scan_root(
    bundle: &BundleRecord,
    policy: &CommandRegistrationAuthority,
) -> Result<NodeConfigScanRoot> {
    let command_registration_caps = policy
        .bundle_source_caps
        .get(&bundle.name)
        .with_context(|| {
            format!(
                "command registration policy has no explicit authority for active bundle `{}`",
                bundle.name
            )
        })?
        .clone();
    Ok(NodeConfigScanRoot {
        path: bundle.path.clone(),
        source: NodeConfigSource::Bundle {
            name: bundle.name.clone(),
            command_registration_caps,
        },
    })
}

fn section_scan_roots(
    section: &dyn NodeConfigSection,
    app_root: &Path,
    bundles: &[BundleRecord],
    policy: &CommandRegistrationAuthority,
) -> Result<Vec<NodeConfigScanRoot>> {
    let mut roots = vec![system_scan_root(app_root, policy)];
    if section.source_scope() == NodeConfigSourceScope::AppRootAndBundleRoots {
        roots.extend(
            bundles
                .iter()
                .map(|bundle| bundle_scan_root(bundle, policy))
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(roots)
}

fn verify_section_signer(
    section: &dyn NodeConfigSection,
    item: &VerifiedNodeConfigItem,
    node_fingerprint: &str,
) -> Result<()> {
    if section.load_spec().signer == SectionSignerPolicy::CurrentNode
        && item.signer_fingerprint() != node_fingerprint
    {
        bail!(
            "node-config item {} in section `{}` must be signed by current node {}; got {}",
            item.path().display(),
            section.name(),
            node_fingerprint,
            item.signer_fingerprint()
        );
    }
    Ok(())
}

fn validate_section_cardinality(section: &dyn NodeConfigSection, count: usize) -> Result<()> {
    let valid = match section.load_spec().cardinality {
        SectionCardinality::Any => true,
        SectionCardinality::AtLeastOne => count >= 1,
        SectionCardinality::AtMostOne => count <= 1,
        SectionCardinality::ExactlyOne => count == 1,
    };
    if !valid {
        bail!(
            "node-config section `{}` violates {:?} cardinality with {count} record(s)",
            section.name(),
            section.load_spec().cardinality
        );
    }
    Ok(())
}

fn reject_disallowed_bundle_section_contributions(
    config_table: &NodeConfigTable,
    policy_table: &ryeos_node_runtime::node_policy::NodePolicyTable,
    bundles: &[BundleRecord],
) -> Result<()> {
    let mut forbidden_directories = BTreeSet::new();
    for section in config_table.sections() {
        if section.source_scope() != NodeConfigSourceScope::AppRootOnly {
            continue;
        }
        forbidden_directories.insert(section.name());
    }
    forbidden_directories.insert(ryeos_node_runtime::node_policy::generation::POLICIES_DIRECTORY);
    for section in policy_table.sections() {
        forbidden_directories.insert(section.name());
    }

    for bundle in bundles {
        let bundle_root = lillux::PinnedDirectory::open(&bundle.path)?
            .with_context(|| format!("bundle root disappeared: {}", bundle.path.display()))?;
        let Some(ai_root) =
            bundle_root.open_child_directory(std::ffi::OsStr::new(ryeos_engine::AI_DIR))?
        else {
            continue;
        };
        let Some(node_root) = ai_root.open_child_directory(std::ffi::OsStr::new("node"))? else {
            continue;
        };
        for directory in &forbidden_directories {
            if node_root
                .entry_no_follow(std::ffi::OsStr::new(directory))?
                .is_some()
            {
                bail!(
                    "bundle `{}` contributes forbidden node-owned namespace `{}` at {}",
                    bundle.name,
                    directory,
                    node_root.path().join(directory).display()
                );
            }
        }
    }
    Ok(())
}

/// Strip the signature line(s) from the top of a file.
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_binding_admission_is_node_owned_and_rotation_preserves_capacity() {
        use super::super::sections::external_execution::{ExternalExecutionSection, SECTION_NAME};
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .unwrap()
            .to_path_buf();
        let trust = TrustStore::load_from_dir(
            &workspace.join("crates/bin/daemon/tests/fixtures/trusted_signers"),
        )
        .unwrap();
        let system = temp_system_with_command_registration_policy(&workspace);
        let identity = ryeos_node_runtime::identity::NodeIdentity::load(
            &system.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let dir = system.path().join(".ai/node").join(SECTION_NAME);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("primary.yaml");
        // The current signed binding shape; the loader signs it as this
        // node's own item.
        let mut body = super::super::sections::external_execution::RetainedExternalExecutionBinding::test_fixture_document_value();
        // Byte budgets whose admitted bounds the refusals below sit next to.
        body["max_workspace_bytes"] = Value::from(1_048_576_u64);
        body["max_export_bytes"] = Value::from(524_288_u64);
        body["max_transfer_bytes"] = Value::from(2_097_152_u64);
        let write = |body: &Value| {
            fs::write(
                &path,
                ryeos_node_runtime::node_document::render_signed_item(SECTION_NAME, "primary", body, &identity)
                    .unwrap(),
            )
            .unwrap()
        };
        write(&body);
        let loader = NodeConfigLoader {
            app_root: system.path(),
            trust_store: &trust,
        };
        let load = || {
            loader.load_full(
                &NodeConfigTable::new(),
                &[],
                &command_policy([]),
                &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
            )
        };
        let first = load().unwrap();
        let binding = &first.external_execution[0];
        assert_eq!(binding.id(), "primary");
        assert!(binding.credential_access().is_ok());
        let runtime_recipe =
            ryeos_state::external_execution::admission::ExternalCandidateRuntimeRecipe {
                schema: 2,
                runtime_mount_destination: "/runtime".into(),
                executable_relative_path: "bin/codex".into(),
                argv0: "codex".into(),
                arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
                cwd: "/workspace".into(),
                environment: std::collections::BTreeMap::new(),
                max_stdout_bytes: 1024 * 1024,
                max_stderr_bytes: 1024 * 1024,
                proc_filesystem:
                    ryeos_state::external_execution::admission::ExternalCandidateProcFilesystem::PidNamespaceNested,
                contain_process_group: false,
                nested_sandbox: true,
            };
        let runtime_recipe_digest = runtime_recipe.digest().unwrap();
        let requirement =
            ryeos_state::external_execution::admission::ExternalCandidateRequirement {
                schema: 7,
                required_lifecycle_capabilities: Default::default(),
                protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
                connector_protocol:
                    ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
                execution_route: ryeos_state::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
                provider_declaration_id: "codex-hosted".into(),
                provider_configuration_destination: "environments.toml".into(),
                runtime_product_declaration_id: "runtime".into(),
                runtime_authority: ryeos_state::external_execution::admission::ExternalCandidateRuntimeAuthority::CapturedProduct,
                runtime_recipe,
            };
        let qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        let program =
            ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram {
                requirement,
                qualification_use,
                runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
                runtime_manifest_hash: "b".repeat(64),
                runtime_source: ryeos_state::external_execution::admission::ExternalCandidateRuntimeSource::CapturedProduct { witness_hash: "1".repeat(64) },
                qualification_attestation_hash: "2".repeat(64),
                selection_identity_digest: "c".repeat(64),
                runtime_recipe_digest,
            };
        binding.check_program(&program).unwrap();
        for field in [
            "runtime",
            "selection",
            "protocol",
            "connector_protocol",
            "provider_configuration_destination",
            "requirement_schema",
            "malformed",
        ] {
            let mut changed = program.clone();
            match field {
                "runtime" => changed.runtime_manifest_hash = "f".repeat(64),
                "selection" => changed.selection_identity_digest = "f".repeat(64),
                "protocol" => changed.requirement.protocol = "other".into(),
                "connector_protocol" => changed.requirement.connector_protocol = "other".into(),
                "provider_configuration_destination" => {
                    changed.requirement.provider_configuration_destination = "other.toml".into()
                }
                "requirement_schema" => changed.requirement.schema = 2,
                _ => changed.runtime_source = ryeos_state::external_execution::admission::ExternalCandidateRuntimeSource::CapturedProduct { witness_hash: "not-a-hash".into() },
            }
            assert!(binding.check_program(&changed).is_err());
        }
        assert!(!format!("{binding:?}").contains(&"a".repeat(64)));

        for (field, value) in [
            (
                "/controller_transport/https_origin",
                serde_json::json!("http://controller.example:7443"),
            ),
            (
                "/controller_transport/https_origin",
                serde_json::json!("https://controller.example:7443/path"),
            ),
            (
                "/controller_transport/route_contract",
                serde_json::json!("ryeos.external-channel.other"),
            ),
            (
                "/controller_transport/tls_root_bundle_digest",
                serde_json::json!("not-a-digest"),
            ),
            (
                "/controller_transport/connect_timeout_ms",
                serde_json::json!(0),
            ),
            (
                "/controller_transport/request_timeout_ms",
                serde_json::json!(30_001),
            ),
            (
                "/controller_transport/maximum_response_bytes",
                serde_json::json!(4_095),
            ),
            ("/workload/connector_protocol", serde_json::json!("other")),
            (
                "/workload/connector_artifact_hash",
                serde_json::json!("not-a-digest"),
            ),
            ("/workload/connector_artifact_bytes", serde_json::json!(0)),
        ] {
            let mut invalid = body.clone();
            *invalid.pointer_mut(field).unwrap() = value;
            write(&invalid);
            assert!(
                load().is_err(),
                "invalid controller transport field {field}"
            );
        }
        write(&body);

        // A valid direct workload has no provider/runtime-selection fields and
        // grants no candidate export. Loading its signed shape is not direct
        // execution admission: the existing session check must refuse it.
        let mut direct = body.clone();
        direct["workload"] = serde_json::json!({"kind":"direct_command"});
        direct["max_export_bytes"] = serde_json::json!(0);
        write(&direct);
        let direct_loaded = load().unwrap();
        let error = direct_loaded.external_execution[0]
            .check_program(&program)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot authorize a structured session")
        );
        assert_ne!(
            binding.digest(),
            direct_loaded.external_execution[0].digest()
        );
        assert_eq!(
            binding.capacity_owner(),
            direct_loaded.external_execution[0].capacity_owner()
        );
        direct["workload"]["provider_declaration_id"] = serde_json::json!("codex-hosted");
        write(&direct);
        assert!(load().is_err());
        write(&body);

        let rotated_tls_roots = vec![base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"rotated fixture controller TLS root",
        )];
        body["controller_tls_root_certificates_der_base64"] = serde_json::json!(rotated_tls_roots);
        body["controller_transport"]["tls_root_bundle_digest"] = Value::String(
            ryeos_state::external_execution::transport::external_tls_root_bundle_digest(
                &rotated_tls_roots,
            )
            .unwrap(),
        );
        write(&body);
        let transport_rotated = load().unwrap();
        assert_ne!(
            binding.digest(),
            transport_rotated.external_execution[0].digest()
        );
        assert_eq!(
            binding.capacity_owner(),
            transport_rotated.external_execution[0].capacity_owner()
        );
        body["credential_generation"] = Value::String("e".repeat(64));
        write(&body);
        let second = load().unwrap();
        assert_ne!(
            transport_rotated.external_execution[0].digest(),
            second.external_execution[0].digest()
        );
        assert_eq!(
            binding.capacity_owner(),
            second.external_execution[0].capacity_owner()
        );
        let section = ExternalExecutionSection;
        assert_eq!(section.source_scope(), NodeConfigSourceScope::AppRootOnly);
        let files = scan_yaml_files(&dir, false).unwrap();
        let verified = verify_and_parse(&files[0], &dir, SECTION_NAME, &trust).unwrap();
        assert!(verify_section_signer(&section, &verified, &"f".repeat(64)).is_err());
        let record = section.parse(verified.context(), verified.body()).unwrap();
        let mut builder = NodeConfigSnapshotBuilder::new(&[], command_policy([])).unwrap();
        let source = NodeConfigSource::Node {
            command_registration_caps: Vec::new(),
        };
        builder.admit(record, &verified, &source).unwrap();
        assert!(
            builder
                .admit(
                    section.parse(verified.context(), verified.body()).unwrap(),
                    &verified,
                    &source
                )
                .is_err()
        );
        fs::copy(&path, dir.join("primary.yml")).unwrap();
        assert!(load().is_err());
        fs::remove_file(dir.join("primary.yml")).unwrap();
        let bundle = tempfile::tempdir().unwrap();
        let bundle_section = bundle.path().join(".ai/node").join(SECTION_NAME);
        fs::create_dir_all(&bundle_section).unwrap();
        fs::copy(&path, bundle_section.join("primary.yaml")).unwrap();
        let contributed = BundleRecord {
            name: "test".into(),
            path: bundle.path().into(),
            source_file: path.clone(),
        };
        assert!(
            loader
                .load_full(
                    &NodeConfigTable::new(),
                    &[contributed],
                    &command_policy([("test", Vec::new())]),
                    &ryeos_node_runtime::node_policy::NodePolicyTable::new()
                )
                .is_err()
        );
        for (field, invalid) in [
            ("max_active", 0),
            ("max_active", 65),
            ("timeout_seconds", 0),
            ("timeout_seconds", 3601),
            ("contact_timeout_seconds", 0),
            ("contact_timeout_seconds", 61),
            ("observation_timeout_seconds", 0),
            ("observation_timeout_seconds", 301),
            ("cleanup_timeout_seconds", 0),
            ("cleanup_timeout_seconds", 601),
        ] {
            let original = body[field].clone();
            body[field] = Value::from(invalid);
            write(&body);
            assert!(load().is_err());
            body[field] = original;
        }
        for (active, timeout) in [(1, 1), (64, 3600)] {
            body["max_active"] = Value::from(active);
            body["timeout_seconds"] = Value::from(timeout);
            write(&body);
            assert_eq!(
                load().unwrap().external_execution[0].capacity_owner(),
                binding.capacity_owner()
            );
        }
        for field in ["network_policy", "storage_policy", "cleanup_proof"] {
            let original = body[field].clone();
            body[field] = Value::String("unsupported".into());
            write(&body);
            assert!(load().is_err(), "unsupported {field} was admitted");
            body[field] = original;
        }
        for (field, invalid) in [
            ("max_workspace_bytes", 0_u64),
            ("max_workspace_bytes", (1_u64 << 40) + 1),
            ("max_export_bytes", 0),
            ("max_export_bytes", 1_048_577),
            ("max_transfer_bytes", 524_287),
            ("max_transfer_bytes", (1_u64 << 40) + 1),
            ("max_guest_package_regular_bytes", 0),
            ("max_guest_package_regular_bytes", 2_097_152),
            ("max_guest_package_framed_bytes", 1_048_576),
            ("max_guest_package_framed_bytes", (4_u64 << 30) + 1),
        ] {
            let original = body[field].clone();
            body[field] = Value::from(invalid);
            write(&body);
            assert!(load().is_err(), "invalid {field} budget was admitted");
            body[field] = original;
        }
        for field in [
            "max_guest_package_regular_bytes",
            "max_guest_package_framed_bytes",
        ] {
            let original = body.as_object_mut().unwrap().remove(field).unwrap();
            write(&body);
            assert!(load().is_err(), "missing {field} budget was admitted");
            body[field] = original;
        }
        for (regular, framed) in [
            (1_048_576_u64, 2_097_152_u64),
            ((4_u64 << 30) - 1, 4_u64 << 30),
        ] {
            body["max_guest_package_regular_bytes"] = Value::from(regular);
            body["max_guest_package_framed_bytes"] = Value::from(framed);
            write(&body);
            let admitted = load().unwrap();
            let contract = admitted.external_execution[0].backend_contract();
            assert_eq!(contract.max_guest_package_regular_bytes, regular);
            assert_eq!(contract.max_guest_package_framed_bytes, framed);
        }
        body["max_guest_package_regular_bytes"] = Value::from(1_048_576_u64);
        body["max_guest_package_framed_bytes"] = Value::from(2_097_152_u64);
        for field in ["backend", "account", "capacity_group"] {
            let original = body[field].clone();
            body[field] = Value::String("other".into());
            write(&body);
            assert_ne!(
                load().unwrap().external_execution[0].capacity_owner(),
                binding.capacity_owner()
            );
            body[field] = original;
        }
        body["secret"] = Value::String("must-not-be-accepted".into());
        write(&body);
        assert!(load().is_err());
    }

    #[test]
    fn admission_retains_verified_bytes_after_source_replacement() {
        #[derive(Debug, Clone)]
        struct CheckSource {
            expected: std::sync::Arc<str>,
            signer: String,
        }
        impl super::super::CompiledNodeConfigItem for CheckSource {
            fn section_name(&self) -> &'static str {
                "routes"
            }
            fn admit(
                self: Box<Self>,
                _: &mut NodeConfigSnapshotBuilder,
                admission: &NodeConfigAdmission,
            ) -> Result<()> {
                assert_eq!(admission.signed_source, self.expected);
                assert_eq!(admission.signer_fingerprint, self.signer);
                assert_ne!(
                    fs::read_to_string(&admission.source_file)?,
                    self.expected.as_ref()
                );
                Ok(())
            }
        }
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("bundles").is_dir())
            .unwrap()
            .to_path_buf();
        let trust = TrustStore::load_from_dir(
            &workspace.join("crates/bin/daemon/tests/fixtures/trusted_signers"),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current.yaml");
        fs::copy(
            workspace.join("bundles/ryeos-ui/.ai/node/routes/ui/session/current.yaml"),
            &path,
        )
        .unwrap();
        let original = fs::read_to_string(&path).unwrap();
        let expected_signer = ryeos_engine::item_resolution::parse_signature_header(
            &original,
            &ryeos_engine::contracts::SignatureEnvelope {
                prefix: "#".into(),
                suffix: None,
                after_shebang: false,
            },
        )
        .unwrap()
        .signer_fingerprint;
        let files = scan_yaml_files(dir.path(), false).unwrap();
        let verified = verify_and_parse(&files[0], dir.path(), "routes", &trust).unwrap();
        let check = CheckSource {
            expected: original.into(),
            signer: expected_signer,
        };
        fs::write(&path, "replaced after verification").unwrap();
        let mut builder = NodeConfigSnapshotBuilder::new(
            &[],
            CommandRegistrationAuthority {
                claim_rules: Vec::new(),
                system_source_caps: Vec::new(),
                bundle_source_caps: Default::default(),
            },
        )
        .unwrap();
        builder
            .admit(
                Box::new(check.clone()),
                &verified,
                &NodeConfigSource::Node {
                    command_registration_caps: Vec::new(),
                },
            )
            .unwrap();
        let replacement = dir.path().join("replacement.yaml");
        fs::write(&replacement, "atomic replacement after verification").unwrap();
        fs::rename(&replacement, &path).unwrap();
        builder
            .admit(
                Box::new(check),
                &verified,
                &NodeConfigSource::Node {
                    command_registration_caps: Vec::new(),
                },
            )
            .unwrap();
    }

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
            .map(|f| f.path().file_name().unwrap().to_string_lossy().to_string())
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
            .map(|f| f.path().file_name().unwrap().to_string_lossy().to_string())
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
                f.path()
                    .strip_prefix(&routes_dir)
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
            source_file: workspace.join("bundles/core/.ai/node/bundles/ryeos-ui.yaml"),
        }];
        let command_policy = command_policy([("ryeos-ui", Vec::new())]);

        let loader = NodeConfigLoader {
            app_root: system.path(),
            trust_store: &trust_store,
        };
        let snapshot = loader
            .load_full(
                &NodeConfigTable::new(),
                &bundles,
                &command_policy,
                &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
            )
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
        assert!(
            snapshot
                .routes
                .iter()
                .any(|route| route.path == "/ui/api/invocations/dispatch"),
            "browser-session invocation route should load from nested route directory"
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
        dir
    }

    fn command_policy<const N: usize>(
        bundles: [(&str, Vec<String>); N],
    ) -> CommandRegistrationAuthority {
        CommandRegistrationAuthority {
            claim_rules: Vec::new(),
            system_source_caps: Vec::new(),
            bundle_source_caps: bundles
                .into_iter()
                .map(|(name, caps)| (name.to_owned(), caps))
                .collect(),
        }
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
    fn command_registration_caps_follow_policy_and_registration_not_bundle_name() {
        let system = std::path::PathBuf::from("/system");
        let mut policy = command_policy([
            ("core", Vec::new()),
            (
                "standard",
                vec!["ryeos.register.command.root.standard".into()],
            ),
        ]);
        policy.system_source_caps = vec!["ryeos.register.command.root.execute".into()];
        let core = BundleRecord {
            name: "core".into(),
            path: std::path::PathBuf::from("/system/.ai/bundles/core"),
            source_file: std::path::PathBuf::from("/system/.ai/node/bundles/core.yaml"),
        };
        let standard = BundleRecord {
            name: "standard".into(),
            path: std::path::PathBuf::from("/system/.ai/bundles/standard"),
            source_file: std::path::PathBuf::from("/system/.ai/node/bundles/standard.yaml"),
        };

        assert_eq!(
            system_scan_root(&system, &policy)
                .source
                .command_provenance()
                .unwrap()
                .command_registration_caps,
            vec!["ryeos.register.command.root.execute"]
        );
        assert_eq!(
            bundle_scan_root(&core, &policy)
                .unwrap()
                .source
                .command_provenance()
                .unwrap()
                .command_registration_caps,
            Vec::<String>::new()
        );
        assert_eq!(
            bundle_scan_root(&standard, &policy)
                .unwrap()
                .source
                .command_provenance()
                .unwrap()
                .command_registration_caps,
            vec!["ryeos.register.command.root.standard"]
        );
    }
}
