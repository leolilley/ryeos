//! Codex product-edge checks for existing guest-input descriptor delivery.
//!
//! The enclosing owner authenticates the retained consumer and selects these
//! descriptors. This module neither allocates a guest nor grants qualification.
//! Exact byte verification reuses the state's staged-realization verifier;
//! filesystem and descriptor mechanics remain in Lillux.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    GuestMountAccess, GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
    GuestProductManifestKind,
};
use ryeos_state::external_content::products::qualification::{
    ProductQualificationConsumerContentIdentity, ProductQualificationConsumerExecutionContext,
};
use ryeos_state::external_execution::admission::ExternalCandidateRequirement;
use ryeos_state::objects::ExternalContentKind;

use crate::QualificationExecutionEnvironment;

/// Prepared request with live descriptor custody, not a launched attempt.
/// The upstream staging owner must additionally retain its immutable content
/// leases and private-workspace writer authority through whole-scope settlement.
pub struct PreparedProductionNativeRequest<'a> {
    request: lillux::LinuxSandboxRequest,
    _content_descriptors: &'a BTreeMap<u32, lillux::InheritedDescriptorAuthority>,
    _private_descriptors: Vec<lillux::InheritedDescriptorAuthority>,
}

impl PreparedProductionNativeRequest<'_> {
    pub fn request(&self) -> &lillux::LinuxSandboxRequest {
        &self.request
    }
}

/// Fresh, verifier-private scratch layout, not a Worker candidate generation.
/// Sibling directories are created through the held unique occurrence root;
/// callers cannot substitute an immutable input as a writable view. The normal
/// verifier execution owner still supplies the private scratch authority and
/// must settle all writers before accepting outputs.
pub struct ProductionPrivateStaging {
    occurrence: lillux::PinnedDirectory,
    candidate: lillux::PinnedDirectory,
    runtime_views: BTreeMap<String, lillux::PinnedDirectory>,
}

impl ProductionPrivateStaging {
    /// Reuse bounded project capture for verifier-private output after the
    /// native owner has settled its namespace and excluded all candidate writers.
    /// This method does not establish that prerequisite or publish a candidate.
    pub(crate) fn capture_candidate_after_settlement(
        &self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ryeos_state::objects::ProjectTree> {
        self.recheck_bindings()?;
        ensure!(
            !deadline.has_elapsed(),
            "consumer candidate capture deadline expired"
        );
        let state_root = self
            .occurrence
            .create_child(std::ffi::OsStr::new("capture-state"), 0o700)?;
        let state = ryeos_state::StateDb::open(
            state_root.path(),
            std::sync::Arc::new(ryeos_state::TrustStore::new()),
        )?;
        let authority = state.pinned_authority()?;
        let guard = authority.acquire_shared_guard()?;
        drop(state);
        let policy = ryeos_state::objects::ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            BTreeMap::new(),
        )?;
        ryeos_project_capture::ingest_project_tree_bounded(
            &authority,
            &guard,
            &self.candidate,
            &policy,
            ryeos_project_capture::ProjectCaptureBudget {
                max_bytes: 1024 * 1024,
                deadline,
            },
        )
    }

    pub fn create(
        private_scratch: &lillux::PinnedDirectory,
        content: &ProductQualificationConsumerContentIdentity,
        consumer: &ProductQualificationConsumerExecutionContext,
    ) -> Result<Self> {
        QualificationExecutionEnvironment::from_retained_production_consumer(content, consumer)?;
        private_scratch.require_owner_private_directory()?;
        private_scratch.ensure_path_binding()?;
        let (_, occurrence) = private_scratch.create_unique_child("production-verifier", 0o700)?;
        let candidate = occurrence.create_child(std::ffi::OsStr::new("candidate"), 0o700)?;
        let views = occurrence.create_child(std::ffi::OsStr::new("runtime-views"), 0o700)?;
        let mut runtime_views = BTreeMap::new();
        for (name, value) in &content.process_environment {
            if matches!(
                value,
                ryeos_state::objects::SessionProcessEnvironmentValue::RuntimeViewDirectory { .. }
            ) {
                runtime_views.insert(
                    name.clone(),
                    views.create_child(std::ffi::OsStr::new(name), 0o700)?,
                );
            }
        }
        Ok(Self {
            occurrence,
            candidate,
            runtime_views,
        })
    }

    fn recheck_bindings(&self) -> Result<()> {
        self.occurrence.ensure_path_binding()?;
        self.candidate.ensure_path_binding()?;
        for directory in self.runtime_views.values() {
            directory.ensure_path_binding()?;
        }
        Ok(())
    }
}

/// Build the complete native mount request from verified production delivery.
/// Fresh private directories come from the verifier staging owner. Content is
/// reverified after scratch creation, so scratch inserted beneath an immutable
/// input cannot silently alter that input's exact manifest.
pub fn prepare_production_native_request<'a>(
    content: &ProductQualificationConsumerContentIdentity,
    consumer: &ProductQualificationConsumerExecutionContext,
    requirement: &ExternalCandidateRequirement,
    inputs: &[GuestMountInput],
    descriptors: &'a BTreeMap<u32, lillux::InheritedDescriptorAuthority>,
    private: &ProductionPrivateStaging,
) -> Result<PreparedProductionNativeRequest<'a>> {
    private.recheck_bindings()?;
    let candidate = &private.candidate;
    let runtime_views = &private.runtime_views;
    let environment =
        verify_production_descriptor_inputs(content, consumer, requirement, inputs, descriptors)?;
    let effective = environment.expected_guest_environment(requirement)?;
    let expected_views: BTreeSet<_> = content
        .process_environment
        .iter()
        .filter_map(|(name, value)| {
            matches!(
                value,
                ryeos_state::objects::SessionProcessEnvironmentValue::RuntimeViewDirectory { .. }
            )
            .then_some(name.as_str())
        })
        .collect();
    ensure!(
        runtime_views
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == expected_views,
        "production native runtime views differ from admitted environment"
    );
    let mut mounts: Vec<_> = inputs
        .iter()
        .map(|input| lillux::LinuxSandboxMount {
            source_fd: input.descriptor,
            destination: input.destination.clone().into(),
            access: lillux::LinuxSandboxMountAccess::ReadOnly,
            layer: 0,
        })
        .collect();
    let mut private_descriptors = Vec::new();
    let candidate_descriptor = candidate.inherited_descriptor_authority()?;
    mounts.push(lillux::LinuxSandboxMount {
        source_fd: candidate_descriptor
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?,
        destination: "/workspace".into(),
        access: lillux::LinuxSandboxMountAccess::Writable,
        layer: 0,
    });
    private_descriptors.push(candidate_descriptor);
    for (name, directory) in runtime_views {
        let descriptor = directory.inherited_descriptor_authority()?;
        mounts.push(lillux::LinuxSandboxMount {
            source_fd: descriptor
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            destination: ryeos_state::objects::runtime_view_mount_destination(name)?,
            access: lillux::LinuxSandboxMountAccess::Writable,
            layer: 0,
        });
        private_descriptors.push(descriptor);
    }
    // Obvious backing-object aliases are refused here. Distinct directory
    // identities do not prove absence of ancestor/descendant overlap; that
    // stronger fact still belongs to the private staging owner.
    for (index, private) in private_descriptors.iter().enumerate() {
        for other in private_descriptors
            .iter()
            .skip(index + 1)
            .chain(descriptors.values())
        {
            ensure!(
                !private.same_file_identity(other)?,
                "production private mount aliases another retained input"
            );
        }
    }
    let request = crate::native_guest::native_request(
        &requirement.runtime_recipe,
        effective.environment,
        mounts,
    )?;
    private.recheck_bindings()?;
    Ok(PreparedProductionNativeRequest {
        request,
        _content_descriptors: descriptors,
        _private_descriptors: private_descriptors,
    })
}

/// Recheck a complete production input inventory before native staging.
/// Authorities are borrowed: the existing transport/staging owner must retain
/// their custody through execution and settlement. This return value establishes
/// neither writer exclusion nor an applied launch.
pub fn verify_production_descriptor_inputs(
    content: &ProductQualificationConsumerContentIdentity,
    consumer: &ProductQualificationConsumerExecutionContext,
    requirement: &ExternalCandidateRequirement,
    inputs: &[GuestMountInput],
    descriptors: &BTreeMap<u32, lillux::InheritedDescriptorAuthority>,
) -> Result<QualificationExecutionEnvironment> {
    let environment =
        QualificationExecutionEnvironment::from_retained_production_consumer(content, consumer)?;
    requirement.validate()?;
    let qualified_use = content
        .qualification_use
        .as_ref()
        .context("production descriptor input has no retained use")?;
    ensure!(
        requirement.runtime_product_declaration_id == content.runtime_realization.id
            && requirement.runtime_recipe.executable_relative_path
                == content.runtime_member.relative_path
            && requirement.qualification_requirement_digest()? == qualified_use.requirement_digest,
        "production descriptor recipe differs from retained consumer use"
    );
    let destinations = ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment::expected_destinations(
        requirement, &environment.realizations,
    )?;
    ensure!(
        inputs.len() == environment.realizations.iter().len(),
        "production descriptor inventory is incomplete or extra"
    );
    let mut ids = BTreeSet::new();
    let mut used_descriptors = BTreeSet::new();
    for input in inputs {
        ensure!(
            ids.insert(input.authority_id.as_str()),
            "production descriptor identity is duplicated"
        );
        let realized = environment
            .realizations
            .iter()
            .find(|entry| entry.id == input.authority_id)
            .context("production descriptor has an unadmitted identity")?;
        ensure!(
            input.role == GuestMountRole::Product
                && input.access == GuestMountAccess::ReadOnly
                && destinations.get(&input.authority_id) == Some(&input.destination)
                && input.bytes == realized.total_bytes
                && match realized.kind {
                    ExternalContentKind::Tree =>
                        input.kind == GuestMountKind::Directory && input.normalized_mode.is_none(),
                    ExternalContentKind::File =>
                        input.kind == GuestMountKind::RegularFile
                            && input.normalized_mode == Some(0o755),
                },
            "production descriptor mount differs from retained realization"
        );
        let GuestMountContentAuthority::ProductManifest {
            manifest_kind,
            manifest_hash,
            manifest_descriptor,
            manifest_bytes,
        } = &input.content_authority
        else {
            anyhow::bail!("production descriptor is not manifest-bound content");
        };
        ensure!(
            manifest_hash == &realized.manifest_hash,
            "production descriptor manifest differs from retained content"
        );
        for descriptor in [input.descriptor, *manifest_descriptor] {
            ensure!(
                descriptor > 2 && used_descriptors.insert(descriptor),
                "production descriptor custody is aliased or invalid"
            );
        }
        let source = descriptors
            .get(&input.descriptor)
            .context("production content descriptor is absent")?;
        let manifest = descriptors
            .get(manifest_descriptor)
            .context("production manifest descriptor is absent")?;
        ensure!(
            source.inherited_descriptor().map_err(anyhow::Error::msg)? == input.descriptor
                && manifest
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    == *manifest_descriptor,
            "production descriptor coordinates differ from retained custody"
        );
        ryeos_state::external_content::realization_verification::verify_staged_external_realization(
            source, manifest,
            match manifest_kind {
                GuestProductManifestKind::Content => ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
                GuestProductManifestKind::LargeContent => ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            },
            manifest_hash, *manifest_bytes, realized.kind, realized.total_bytes,
        )?;
        let (manifest_bytes, _) = manifest.read_regular_file_stable_bounded(
            ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64,
        )?;
        ensure!(
            lillux::sha256_hex(&manifest_bytes) == *manifest_hash,
            "production manifest changed during runtime member join"
        );
        let manifest_value: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        let entry_count = match manifest_kind {
            GuestProductManifestKind::Content => {
                ryeos_state::objects::ExternalContentManifestObject::from_value(&manifest_value)?
                    .entry_count
            }
            GuestProductManifestKind::LargeContent => {
                ryeos_state::objects::ExternalLargeContentManifestObject::from_value(
                    &manifest_value,
                )?
                .entry_count
            }
        };
        ensure!(
            entry_count == realized.entry_count,
            "production manifest entry count differs from retained identity"
        );
        if realized.id == content.runtime_realization.id {
            ensure!(
                ryeos_state::external_content::runtime_member::exact_runtime_member_hash(
                    &manifest_value,
                    &content.runtime_member.relative_path,
                )? == content.runtime_member.executable_sha256,
                "production runtime member differs from retained executable"
            );
        }
    }
    ensure!(
        descriptors.len() == used_descriptors.len(),
        "production descriptor inventory contains unselected authority"
    );
    Ok(environment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::external_content::products::qualification::{
        ProductQualificationBundleDefinitionIdentity,
        ProductQualificationConsumerDefinitionIdentity,
        ProductQualificationConsumerRuntimeMemberIdentity,
        QualificationConsumerDeclarationAuthority,
    };
    use ryeos_state::objects::{
        ExecutableSearchPathEntry, ExternalContentMode, ExternalContentMountRoot,
        ExternalContentRealization, ExternalContentRealizationSet,
    };
    use std::ffi::OsStr;

    struct Fixture {
        _temporary: tempfile::TempDir,
        root: lillux::PinnedDirectory,
        content: ProductQualificationConsumerContentIdentity,
        consumer: ProductQualificationConsumerExecutionContext,
        requirement: ExternalCandidateRequirement,
        inputs: Vec<GuestMountInput>,
        descriptors: BTreeMap<u32, lillux::InheritedDescriptorAuthority>,
    }

    fn fixture(large_runtime: bool) -> Fixture {
        let temporary = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let profile: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../bundles/codex/.ai/workers/codex/lib/hosted/external-authoring.profile.json"
        ))
        .unwrap();
        let requirement: ExternalCandidateRequirement =
            serde_json::from_value(profile["external_candidate"].clone()).unwrap();
        let mut inputs = Vec::new();
        let mut descriptors = BTreeMap::new();
        let mut realizations = Vec::new();
        for (id, kind, mount) in [
            (
                "authoring-tools",
                ExternalContentKind::Tree,
                "authoring-tools",
            ),
            ("codex", ExternalContentKind::File, "codex"),
            (
                "codex-bwrap",
                ExternalContentKind::File,
                "codex-resources/bwrap",
            ),
            (
                "codex-code-mode-host",
                ExternalContentKind::File,
                "codex-code-mode-host",
            ),
            ("codex-rg", ExternalContentKind::File, "codex-path/rg"),
            (
                "codex-zsh",
                ExternalContentKind::File,
                "codex-resources/zsh/bin/zsh",
            ),
            ("guest-runtime", ExternalContentKind::Tree, "guest-runtime"),
        ] {
            let (source, manifest) = if kind == ExternalContentKind::Tree {
                let tree = root.create_child(OsStr::new(id), 0o755).unwrap();
                let bin = tree.create_child(OsStr::new("bin"), 0o755).unwrap();
                let members: &[&str] = if id == "guest-runtime" {
                    &["codex"]
                } else {
                    &["rg", "zsh"]
                };
                for member in members {
                    bin.atomic_create_pinned_regular(
                        OsStr::new(member),
                        b"fixture-executable",
                        0o755,
                    )
                    .unwrap()
                    .unwrap();
                }
                let manifest = ryeos_state::observe_external_content_tree_exact(&tree).unwrap();
                (tree.inherited_descriptor_authority().unwrap(), manifest)
            } else {
                root.atomic_create_pinned_regular(OsStr::new(id), b"fixture-executable", 0o755)
                    .unwrap()
                    .unwrap();
                let source = root
                    .open_inherited_regular(OsStr::new(id), false)
                    .unwrap()
                    .unwrap();
                let manifest = ryeos_state::single_file_manifest_from_verified_blob(
                    &lillux::sha256_hex(b"fixture-executable"),
                    18,
                    0o755,
                )
                .unwrap();
                (source, manifest)
            };
            let mut value = serde_json::to_value(&manifest).unwrap();
            let manifest_kind = if id == "guest-runtime" && large_runtime {
                value["kind"] =
                    serde_json::json!(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND);
                value["schema"] =
                    serde_json::json!(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA);
                GuestProductManifestKind::LargeContent
            } else {
                GuestProductManifestKind::Content
            };
            let bytes = lillux::canonical_json(&value).unwrap();
            let manifest_hash = lillux::sha256_hex(bytes.as_bytes());
            let manifest_authority =
                lillux::sealed_memfd(c"production-test-manifest", bytes.as_bytes()).unwrap();
            let descriptor = source.inherited_descriptor().unwrap();
            let manifest_descriptor = manifest_authority.inherited_descriptor().unwrap();
            inputs.push(GuestMountInput {
                role: GuestMountRole::Product,
                authority_id: id.into(),
                descriptor,
                destination: if id == "guest-runtime" {
                    "/runtime".into()
                } else {
                    format!("/ryeos/realizations/{mount}")
                },
                kind: if kind == ExternalContentKind::Tree {
                    GuestMountKind::Directory
                } else {
                    GuestMountKind::RegularFile
                },
                access: GuestMountAccess::ReadOnly,
                normalized_mode: if kind == ExternalContentKind::File {
                    Some(0o755)
                } else {
                    None
                },
                content_authority: GuestMountContentAuthority::ProductManifest {
                    manifest_kind,
                    manifest_hash: manifest_hash.clone(),
                    manifest_descriptor,
                    manifest_bytes: bytes.len() as u64,
                },
                bytes: manifest.total_bytes,
            });
            descriptors.insert(descriptor, source);
            descriptors.insert(manifest_descriptor, manifest_authority);
            realizations.push(ExternalContentRealization {
                id: id.into(),
                kind,
                mode: ExternalContentMode::Pinned,
                manifest_hash,
                entry_count: manifest.entry_count,
                total_bytes: manifest.total_bytes,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: mount.into(),
            });
        }
        let all = ExternalContentRealizationSet::new(realizations).unwrap();
        let consumer = ProductQualificationConsumerExecutionContext {
            worker_ref: "worker:codex/external-hosted-authoring".into(),
            product_declaration_id: "guest-runtime".into(),
            environment_ref: "config:codex/environments/external-authoring".into(),
            worker_execution_ref: "worker_execution:codex/bounded-turn".into(),
            environment_binding: "environment".into(),
        };
        let definition = |reference: &str| ProductQualificationBundleDefinitionIdentity {
            canonical_ref: reference.into(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
        };
        let search = vec![ExecutableSearchPathEntry {
            realization_id: "authoring-tools".into(),
            relative_directory: "bin".into(),
        }];
        let process_environment = BTreeMap::from([(
            "TMPDIR".into(),
            ryeos_state::objects::SessionProcessEnvironmentValue::RuntimeViewDirectory {
                relative_path: "scratch".into(),
            },
        )]);
        let content = ProductQualificationConsumerContentIdentity {
            qualification_use: Some(ryeos_state::external_execution::admission::ExternalCandidateQualificationUse {
                schema: ryeos_state::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA.into(),
                requirement_digest: requirement.qualification_requirement_digest().unwrap(),
                profile_hash: "d".repeat(64), source_binding_hash: "e".repeat(64), source_content_manifest_hash: "f".repeat(64),
                provider_executable_manifest_hash: all.iter().find(|r| r.id == "codex").unwrap().manifest_hash.clone(),
                execution_environment_digest: ryeos_state::external_execution::admission::ExternalCandidateQualificationUse::admitted_execution_environment_digest(&all, &search, &process_environment).unwrap(),
            }),
            definitions: ProductQualificationConsumerDefinitionIdentity {
                bundle_generation_identity: "descriptor-fixture".into(), worker: definition(&consumer.worker_ref),
                environment: definition(&consumer.environment_ref), worker_execution: definition(&consumer.worker_execution_ref),
            },
            declaration_authority: QualificationConsumerDeclarationAuthority::CapturedProduct { relationship_definition: definition("config:codex/guest-runtime-products") },
            worker_source: ryeos_state::objects::EffectiveSourceClosureProjection {
                schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA, binding_hash: "e".repeat(64), content_manifest_hash: "f".repeat(64), owner_key: "a".repeat(64), file_count: 1, total_bytes: 1,
            }, worker_profile_hash: "d".repeat(64), worker_preselection_effective_definition_digest: "b".repeat(64),
            worker_literals: ExternalContentRealizationSet::new(all.iter().filter(|r| r.id.starts_with("codex")).cloned().collect()).unwrap(),
            runtime_realization: all.iter().find(|r| r.id == "guest-runtime").unwrap().clone(),
            environment_realized_effective_definition_digest: "b".repeat(64),
            environment_realizations: ExternalContentRealizationSet::new(all.iter().filter(|r| r.id == "authoring-tools").cloned().collect()).unwrap(),
            executable_search: search, process_environment,
            runtime_member: ProductQualificationConsumerRuntimeMemberIdentity {
                product_declaration_id: "guest-runtime".into(), relative_path: "bin/codex".into(), executable_sha256: lillux::sha256_hex(b"fixture-executable"),
            },
        };
        Fixture {
            _temporary: temporary,
            root,
            content,
            consumer,
            requirement,
            inputs,
            descriptors,
        }
    }

    fn verify(f: &Fixture) -> Result<QualificationExecutionEnvironment> {
        verify_production_descriptor_inputs(
            &f.content,
            &f.consumer,
            &f.requirement,
            &f.inputs,
            &f.descriptors,
        )
    }

    #[test]
    fn private_candidate_capture_is_bounded_and_keeps_product_inputs_unchanged() {
        let f = fixture(false);
        let scratch = f
            .root
            .create_child(OsStr::new("capture-scratch"), 0o700)
            .unwrap();
        let private = ProductionPrivateStaging::create(&scratch, &f.content, &f.consumer).unwrap();
        private
            .candidate
            .atomic_create_pinned_regular(
                OsStr::new("candidate-strategy.txt"),
                b"candidate\n",
                0o644,
            )
            .unwrap()
            .unwrap();
        let before = private.occurrence.entries_no_follow_bounded(8).unwrap();
        assert!(
            private
                .capture_candidate_after_settlement(lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::ZERO
                ),)
                .is_err()
        );
        assert_eq!(
            private.occurrence.entries_no_follow_bounded(8).unwrap(),
            before
        );
        // Synthetic byte-capture fixture only: it provides no native execution
        // or settlement proof and cannot qualify the runtime.
        let captured = private
            .capture_candidate_after_settlement(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(10),
            ))
            .unwrap();
        let file = ryeos_state::objects::ProjectFile {
            blob_hash: lillux::sha256_hex(b"candidate\n"),
            size: 10,
            normalized_mode: 0o644,
        };
        assert_eq!(
            captured.files,
            BTreeMap::from([(
                "candidate-strategy.txt".into(),
                ryeos_state::objects::canonical_value_digest(&file.to_value()).unwrap()
            )])
        );
        verify(&f).unwrap();
    }

    #[test]
    fn native_request_preserves_every_production_mount_and_private_view() {
        let f = fixture(true);
        let scratch = f
            .root
            .create_child(OsStr::new("private-scratch"), 0o700)
            .unwrap();
        let mut private =
            ProductionPrivateStaging::create(&scratch, &f.content, &f.consumer).unwrap();
        let prepared = prepare_production_native_request(
            &f.content,
            &f.consumer,
            &f.requirement,
            &f.inputs,
            &f.descriptors,
            &private,
        )
        .unwrap();
        let request = prepared.request();
        assert_eq!(request.mounts.len(), 9);
        for input in &f.inputs {
            let mount = request
                .mounts
                .iter()
                .find(|mount| mount.destination == std::path::Path::new(&input.destination))
                .unwrap();
            assert_eq!(mount.source_fd, input.descriptor);
            assert_eq!(mount.access, lillux::LinuxSandboxMountAccess::ReadOnly);
        }
        assert_eq!(
            request.executable,
            std::path::Path::new("/runtime/bin/codex")
        );
        assert_eq!(
            request.environment.get(OsStr::new("PATH")).unwrap(),
            OsStr::new("/ryeos/realizations/authoring-tools/bin")
        );
        assert_eq!(
            request.environment.get(OsStr::new("TMPDIR")).unwrap(),
            OsStr::new("/ryeos/runtime-views/TMPDIR")
        );
        private.runtime_views.clear();
        assert!(
            prepare_production_native_request(
                &f.content,
                &f.consumer,
                &f.requirement,
                &f.inputs,
                &f.descriptors,
                &private,
            )
            .err()
            .unwrap()
            .to_string()
            .contains("runtime views")
        );
        let tools = f
            .root
            .open_child_directory(OsStr::new("authoring-tools"))
            .unwrap()
            .unwrap();
        let before = tools.entries_no_follow_bounded(8).unwrap();
        assert!(ProductionPrivateStaging::create(&tools, &f.content, &f.consumer).is_err());
        assert_eq!(tools.entries_no_follow_bounded(8).unwrap(), before);
        verify(&f).unwrap();
        // Privacy alone is not disjointness. A private child under an
        // immutable product still invalidates the exact product manifest.
        let nested = tools
            .create_child(OsStr::new("private-under-product"), 0o700)
            .unwrap();
        let private = ProductionPrivateStaging::create(&nested, &f.content, &f.consumer).unwrap();
        assert!(
            prepare_production_native_request(
                &f.content,
                &f.consumer,
                &f.requirement,
                &f.inputs,
                &f.descriptors,
                &private,
            )
            .err()
            .unwrap()
            .to_string()
            .contains("undeclared entry")
        );
    }

    #[test]
    fn exact_descriptors_support_both_manifest_tiers_and_refuse_tampering() {
        for large in [false, true] {
            let mut f = fixture(large);
            verify(&f).unwrap();
            f.inputs[0].access = GuestMountAccess::PrivateWritable;
            assert!(verify(&f).is_err());
            f.inputs[0].access = GuestMountAccess::ReadOnly;
            f.inputs[1].descriptor = f.inputs[0].descriptor;
            assert!(verify(&f).unwrap_err().to_string().contains("aliased"));
            let mut f = fixture(large);
            f.content.runtime_member.executable_sha256 = "a".repeat(64);
            assert!(verify(&f).is_err());
            let mut f = fixture(large);
            f.inputs.pop();
            assert!(verify(&f).is_err());
            let f = fixture(large);
            let tools = f
                .root
                .open_child_directory(OsStr::new("authoring-tools"))
                .unwrap()
                .unwrap();
            tools
                .atomic_create_pinned_regular(OsStr::new("ambient"), b"undeclared", 0o644)
                .unwrap()
                .unwrap();
            assert!(verify(&f).is_err());
            // Fresh exact fixture avoids treating an arbitrary descriptor repair
            // as authority; changed delivered bytes must fail independently.
            let f = fixture(large);
            // Test-only external writer mutates the open file's inode. Atomic
            // replacement would leave descriptor custody on the original bytes.
            std::fs::write(f.root.path().join("codex"), b"changed-executable").unwrap();
            assert!(verify(&f).is_err());
        }
    }
}
