//! Exact signed-bundle composition for external execution artifacts.
//!
//! This module is the only startup path from bundle declarations to executable
//! authority. It runs inside the engine's checked bundle-generation read and
//! never searches PATH, daemon siblings, project content, or unsigned source
//! manifests.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use ryeos_engine::binary_resolver::{
    BundlePayloadIdentity, BundlePayloadSourceProof, CapturedExecutable, ResolvedBinary,
    capture_bundle_binary_ref, verify_retained_bundle_payload_proof,
};
use ryeos_external_execution_contract::{
    ExternalLifecycleAdapterDeclaration, LifecycleProviderSpecIdentity,
    MAX_LIFECYCLE_PROVIDER_SPEC_BYTES,
};
use ryeos_state::objects::{
    RetainedLifecycleArtifacts, RetainedLifecycleExecutable, RetainedLifecycleExecutableRole,
    RetainedLifecycleSpec, RetainedLifecycleSpecRole,
};

use crate::external_placement::{
    ExternalCandidateConnectorRegistry, ExternalPlacementBackend, ExternalPlacementBackendRegistry,
    ExternalProviderConfigurationRegistry, InstalledExternalProviderConfiguration,
};

#[derive(Debug)]
pub struct ResolvedExternalLifecycleArtifacts {
    pub declaration: ExternalLifecycleAdapterDeclaration,
    pub bundle_name: String,
    pub bundle_manifest_digest: String,
    pub signer_fingerprint: String,
    /// Exact historically signed declaration, not a later installed read.
    pub signed_bundle_manifest: Vec<u8>,
    /// Public verifier captured from the checked node trust store. It may
    /// verify old bytes after revocation but never grants fresh admission.
    pub bundle_verifying_key: [u8; 32],
    pub adapter: CapturedExecutable,
    pub supervisor: CapturedExecutable,
    pub launcher: CapturedExecutable,
    pub restoration_verifier: Option<CapturedExecutable>,
    pub provider_spec: CapturedLifecycleProviderSpec,
    pub snapshot_production_spec: Option<CapturedLifecycleProviderSpec>,
}

#[derive(Debug)]
pub struct CapturedLifecycleProviderSpec {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub authority: lillux::InheritedDescriptorAuthority,
}

/// Borrowed, already-admitted startup capture for one cleanup-only CAS escrow.
/// A later installed Bundle generation must never be consulted to fill gaps.
pub(crate) struct LifecycleEscrowCapture<'a> {
    pub declaration: &'a ExternalLifecycleAdapterDeclaration,
    pub bundle_name: &'a str,
    pub bundle_manifest_digest: &'a str,
    pub signer_fingerprint: &'a str,
    pub signed_bundle_manifest: &'a [u8],
    pub bundle_verifying_key: [u8; 32],
    pub executables: Vec<LifecycleEscrowExecutable<'a>>,
    pub provider_spec: &'a CapturedLifecycleProviderSpec,
    pub snapshot_production_spec: Option<&'a CapturedLifecycleProviderSpec>,
}

pub(crate) struct LifecycleEscrowExecutable<'a> {
    pub role: RetainedLifecycleExecutableRole,
    pub hash: &'a str,
    pub bytes: u64,
    pub handle: &'a lillux::InheritedDescriptorAuthority,
    pub proof: &'a BundlePayloadSourceProof,
}

/// Only the durable publisher can mint this link capability after complete
/// closure verification and `finish_admitted` have succeeded.
pub(crate) struct PublishedLifecycleArtifacts {
    root_hash: String,
}

impl PublishedLifecycleArtifacts {
    pub(crate) fn root_hash(&self) -> &str {
        &self.root_hash
    }
}

#[derive(Debug)]
pub struct ResolvedExternalExecutionArtifacts {
    pub connectors: ExternalCandidateConnectorRegistry,
    pub provider_configurations: ExternalProviderConfigurationRegistry,
    pub placement_backends: ExternalPlacementBackendRegistry,
}

/// Rejoin a historical escrow root to the exact signed declaration it claims.
/// This validates role selection, not the executor proof/payload chains or
/// cleanup authority; those are separate required checks before use.
pub(crate) fn verify_retained_lifecycle_declaration(
    source: &RetainedLifecycleArtifacts,
    signed_manifest: &[u8],
) -> Result<ExternalLifecycleAdapterDeclaration> {
    source.validate()?;
    ensure!(
        lillux::sha256_hex(signed_manifest) == source.signed_bundle_manifest_blob_hash,
        "retained lifecycle signed Bundle bytes differ from CAS identity"
    );
    let encoded = source
        .signer_verifying_key
        .strip_prefix("ed25519:")
        .context("retained lifecycle public verifier is not Ed25519")?;
    let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(encoded)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("retained lifecycle public verifier length changed"))?;
    let verifier = lillux::crypto::VerifyingKey::from_bytes(&key_bytes)?;
    let verified = ryeos_bundle::manifest::verify_retained_manifest_bytes(
        signed_manifest,
        &source.bundle_name,
        &source.signer_fingerprint,
        &verifier,
    )?;
    ensure!(
        verified.body_digest == source.bundle_manifest_body_digest,
        "retained lifecycle Bundle body differs from escrow identity"
    );
    let declaration = verified
        .manifest
        .external_lifecycle_adapters
        .into_iter()
        .find(|candidate| candidate.id == source.declaration_id)
        .context("retained lifecycle declaration is absent from signed Bundle")?;
    let selected_target = &source.executables[0].target_triple;
    for executable in &source.executables {
        ensure!(
            executable.target_triple == *selected_target
                && declaration.targets.contains(&executable.target_triple),
            "retained lifecycle executable target is not signed for this adapter"
        );
        let declared_name = match executable.role {
            RetainedLifecycleExecutableRole::Adapter => Some(declaration.adapter.as_str()),
            RetainedLifecycleExecutableRole::Supervisor => Some(declaration.supervisor.as_str()),
            RetainedLifecycleExecutableRole::Launcher => Some(declaration.launcher.as_str()),
            RetainedLifecycleExecutableRole::RestorationVerifier => {
                declaration.restoration_verifier.as_deref()
            }
        }
        .context("retained lifecycle executable role is not signed")?;
        ensure!(
            executable.item_ref == format!("bin/{}/{}", executable.target_triple, declared_name),
            "retained lifecycle executable role differs from signed Bundle"
        );
    }
    ensure!(
        source.executables.len() == 3 + usize::from(declaration.restoration_verifier.is_some()),
        "retained lifecycle executable inventory differs from signed Bundle"
    );
    for spec in &source.specs {
        let declared = match spec.role {
            RetainedLifecycleSpecRole::Provider => Some(&declaration.provider_spec),
            RetainedLifecycleSpecRole::SnapshotProduction => {
                declaration.snapshot_production_spec.as_ref()
            }
        }
        .context("retained lifecycle spec role is not signed")?;
        ensure!(
            spec.path == declared.path && spec.blob_hash == declared.sha256,
            "retained lifecycle spec differs from signed Bundle"
        );
    }
    ensure!(
        source.specs.len() == 1 + usize::from(declaration.snapshot_production_spec.is_some()),
        "retained lifecycle spec inventory differs from signed Bundle"
    );
    Ok(declaration)
}

/// Verify all bytes reached by one retained lifecycle artifact root. This
/// deliberately does not authorize provider contact: the caller must join the
/// root to the original bootstrap journal and historical credential generation
/// and expose it only through an operation-bound cleanup facade.
pub(crate) fn verify_retained_lifecycle_closure(
    cas: &lillux::CasStore,
    root_hash: &str,
) -> Result<RetainedLifecycleArtifacts> {
    let root = cas
        .get_object_bounded(root_hash, 64 * 1024)?
        .context("retained lifecycle artifact root is absent")?;
    let source = RetainedLifecycleArtifacts::from_value(&root)?;
    let mut limits = ryeos_state::object_closure::ObjectClosureLimits::default();
    limits.max_blob_bytes = 1024 * 1024 * 1024;
    limits.max_total_blob_bytes = 4 * 1024 * 1024 * 1024 + 16 * 1024 * 1024;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        cas,
        [root_hash.to_owned()],
        limits,
    )?;
    ensure!(
        closure.is_complete(),
        "retained lifecycle artifact CAS closure is incomplete"
    );
    let signed_manifest = cas
        .get_blob_bounded(&source.signed_bundle_manifest_blob_hash, 256 * 1024)?
        .context("retained lifecycle signed Bundle manifest is absent")?;
    verify_retained_lifecycle_declaration(&source, &signed_manifest)?;
    let encoded = source
        .signer_verifying_key
        .strip_prefix("ed25519:")
        .context("retained lifecycle verifier is not Ed25519")?;
    let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(encoded)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("retained lifecycle verifier length changed"))?;
    let verifier = lillux::crypto::VerifyingKey::from_bytes(&key_bytes)?;
    for executable in &source.executables {
        let signed_manifest_ref = cas
            .get_blob_bounded(
                &executable.signed_manifest_ref_blob_hash,
                ryeos_engine::executor_resolution::MAX_EXECUTOR_MANIFEST_REF_BYTES,
            )?
            .context("retained lifecycle signed executor ref is absent")?;
        let manifest_bytes = cas
            .get_blob_bounded(&executable.manifest_object_blob_hash, 1024 * 1024)?
            .context("retained lifecycle executor manifest is absent")?;
        let manifest_object: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        ensure!(
            lillux::canonical_json(&manifest_object)?.as_bytes() == manifest_bytes,
            "retained lifecycle executor manifest is not canonical JSON"
        );
        let item_source_object = cas
            .get_object_bounded(&executable.item_source_object_hash, 64 * 1024)?
            .context("retained lifecycle executor ItemSource is absent")?;
        let signed_sidecar = cas
            .get_blob_bounded(&executable.signed_sidecar_blob_hash, 1024 * 1024)?
            .context("retained lifecycle signed executor sidecar is absent")?;
        let proof = BundlePayloadSourceProof {
            selected_item_ref: executable.item_ref.clone(),
            signed_manifest_ref,
            manifest_object,
            item_source_object,
            signed_sidecar,
        };
        let identity = BundlePayloadIdentity {
            content_hash: executable.payload_blob_hash.clone(),
            manifest_hash: executable.manifest_object_blob_hash.clone(),
            item_source_hash: executable.item_source_object_hash.clone(),
            signer_fingerprint: source.signer_fingerprint.clone(),
            target_triple: executable.target_triple.clone(),
        };
        verify_retained_bundle_payload_proof(&proof, &identity, executable.mode, &verifier)?;
        ensure!(
            cas.verify_blob_bounded(&executable.payload_blob_hash, 1024 * 1024 * 1024)?
                == Some(executable.payload_bytes),
            "retained lifecycle executable payload bytes differ"
        );
    }
    for spec in &source.specs {
        let bytes = cas
            .get_blob_bounded(&spec.blob_hash, 1024 * 1024)?
            .context("retained lifecycle provider spec is absent")?;
        ensure!(
            bytes.len() as u64 == spec.bytes,
            "retained lifecycle provider spec size changed"
        );
    }
    Ok(source)
}

/// Publish the exact captured signed adapter generation before any bootstrap
/// provider claim. The CAS object is a historical cleanup closure, not a
/// current execution grant. Large native payloads are streamed in bounded
/// chunks from sealed descriptors instead of materialized in daemon memory.
pub(crate) fn publish_retained_lifecycle_artifacts(
    state_store: &crate::state_store::StateStore,
    owner_principal: &str,
    capture: &LifecycleEscrowCapture<'_>,
) -> Result<PublishedLifecycleArtifacts> {
    ensure!(
        capture.executables.len() == 3 || capture.executables.len() == 4,
        "captured lifecycle executable inventory is incomplete"
    );
    let verifier = lillux::crypto::VerifyingKey::from_bytes(&capture.bundle_verifying_key)?;
    ensure!(
        lillux::crypto::fingerprint(&verifier) == capture.signer_fingerprint,
        "captured lifecycle verifier differs from admitted signer"
    );
    let target = lillux::platform::current_binary_target()?;
    let mut executables = Vec::with_capacity(capture.executables.len());
    for item in &capture.executables {
        let observation = item.handle.regular_file_observation()?;
        let mode = observation.permission_mode()?;
        ensure!(
            observation.size() == item.bytes,
            "captured lifecycle executable size changed"
        );
        let manifest_bytes = lillux::canonical_json(&item.proof.manifest_object)?;
        let item_source_bytes = lillux::canonical_json(&item.proof.item_source_object)?;
        let identity = BundlePayloadIdentity {
            content_hash: item.hash.to_owned(),
            manifest_hash: lillux::sha256_hex(manifest_bytes.as_bytes()),
            item_source_hash: lillux::sha256_hex(item_source_bytes.as_bytes()),
            signer_fingerprint: capture.signer_fingerprint.to_owned(),
            target_triple: target.to_owned(),
        };
        verify_retained_bundle_payload_proof(item.proof, &identity, mode, &verifier)?;
        executables.push(RetainedLifecycleExecutable {
            role: item.role,
            item_ref: item.proof.selected_item_ref.clone(),
            target_triple: target.to_owned(),
            payload_blob_hash: item.hash.to_owned(),
            payload_bytes: item.bytes,
            mode,
            signed_manifest_ref_blob_hash: lillux::sha256_hex(&item.proof.signed_manifest_ref),
            manifest_object_blob_hash: identity.manifest_hash,
            item_source_object_hash: identity.item_source_hash,
            signed_sidecar_blob_hash: lillux::sha256_hex(&item.proof.signed_sidecar),
        });
    }
    let mut spec_sources = vec![(RetainedLifecycleSpecRole::Provider, capture.provider_spec)];
    if let Some(spec) = capture.snapshot_production_spec {
        spec_sources.push((RetainedLifecycleSpecRole::SnapshotProduction, spec));
    }
    let mut specs = Vec::with_capacity(spec_sources.len());
    let mut spec_bytes = Vec::with_capacity(spec_sources.len());
    for (role, spec) in spec_sources {
        let (bytes, observation) = spec
            .authority
            .read_regular_file_stable_bounded(1024 * 1024)?;
        ensure!(
            observation.size() == spec.bytes && lillux::sha256_hex(&bytes) == spec.sha256,
            "captured lifecycle spec differs from admitted bytes"
        );
        specs.push(RetainedLifecycleSpec {
            role,
            path: spec.path.clone(),
            blob_hash: spec.sha256.clone(),
            bytes: spec.bytes,
        });
        spec_bytes.push(bytes);
    }
    let source = RetainedLifecycleArtifacts {
        schema: ryeos_state::objects::RETAINED_LIFECYCLE_ARTIFACTS_SCHEMA,
        kind: ryeos_state::objects::RETAINED_LIFECYCLE_ARTIFACTS_KIND.to_owned(),
        bundle_name: capture.bundle_name.to_owned(),
        signed_bundle_manifest_blob_hash: lillux::sha256_hex(capture.signed_bundle_manifest),
        bundle_manifest_body_digest: capture.bundle_manifest_digest.to_owned(),
        signer_fingerprint: capture.signer_fingerprint.to_owned(),
        signer_verifying_key: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(capture.bundle_verifying_key)
        ),
        declaration_id: capture.declaration.id.clone(),
        executables,
        specs,
    };
    verify_retained_lifecycle_declaration(&source, capture.signed_bundle_manifest)?;
    let root_hash = source.digest()?;
    let authority = state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let key = ryeos_state::DurableCasPublicationKey::retained_lifecycle_artifacts(
        capture.bundle_manifest_digest,
        &root_hash,
    )?;
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(
            &guard,
            owner_principal,
            "retained-lifecycle-artifacts",
            &key,
            None,
        )?;
    ensure!(
        stage.store_blob(&guard, &cas, capture.signed_bundle_manifest)?
            == source.signed_bundle_manifest_blob_hash,
        "signed Bundle escrow digest changed"
    );
    for (item, retained) in capture.executables.iter().zip(&source.executables) {
        let proof = item.proof;
        ensure!(
            stage.store_blob(&guard, &cas, &proof.signed_manifest_ref)?
                == retained.signed_manifest_ref_blob_hash,
            "signed executor-ref escrow digest changed"
        );
        let manifest = proof.manifest_object.clone();
        let item_source = proof.item_source_object.clone();
        ensure!(
            stage.store_object(&guard, &cas, &manifest)? == retained.manifest_object_blob_hash
                && stage.store_object(&guard, &cas, &item_source)?
                    == retained.item_source_object_hash
                && stage.store_blob(&guard, &cas, &proof.signed_sidecar)?
                    == retained.signed_sidecar_blob_hash,
            "signed executor escrow digest changed"
        );
        let mut reader =
            item.handle
                .stable_regular_reader_exact(item.bytes, item.hash, 1024 * 1024 * 1024)?;
        let mut offset = 0_u64;
        let mut already_stored = false;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            if !already_stored {
                already_stored = stage.store_blob_chunk(
                    &guard,
                    &cas,
                    item.hash,
                    item.bytes,
                    offset,
                    &buffer[..read],
                )?;
            }
            offset += u64::try_from(read)?;
        }
        reader.finish()?;
        ensure!(
            already_stored && offset == item.bytes,
            "lifecycle payload escrow is incomplete"
        );
    }
    for (spec, bytes) in source.specs.iter().zip(spec_bytes) {
        ensure!(
            stage.store_blob(&guard, &cas, &bytes)? == spec.blob_hash,
            "lifecycle spec escrow digest changed"
        );
    }
    ensure!(
        stage.store_object(&guard, &cas, &source.to_value()?)? == root_hash,
        "retained lifecycle root changed during CAS publication"
    );
    stage.protect_cas_closure(&guard, [root_hash.as_str()], std::iter::empty())?;
    ensure!(
        verify_retained_lifecycle_closure(&cas, &root_hash)? == source,
        "retained lifecycle CAS closure failed its historical verifier"
    );
    stage.finish_admitted(&guard, &root_hash)?;
    authority.ensure_guard(&guard)?;
    Ok(PublishedLifecycleArtifacts { root_hash })
}

/// Reconstruct one historically signed adapter from an operation-linked CAS
/// closure. Never insert this backend into the fresh-work registry.
pub(crate) fn recover_retained_lifecycle_backend(
    cas: &lillux::CasStore,
    root_hash: &str,
) -> Result<crate::external_lifecycle_adapter::ExecutableExternalPlacementBackend> {
    let source = verify_retained_lifecycle_closure(cas, root_hash)?;
    let signed_manifest = cas
        .get_blob_bounded(&source.signed_bundle_manifest_blob_hash, 256 * 1024)?
        .context("retained signed Bundle manifest is absent")?;
    let declaration = verify_retained_lifecycle_declaration(&source, &signed_manifest)?;
    let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(
            source
                .signer_verifying_key
                .strip_prefix("ed25519:")
                .context("retained Bundle verifier is not Ed25519")?,
        )?
        .try_into()
        .map_err(|_| anyhow::anyhow!("retained Bundle verifier has wrong length"))?;
    let mut executables = Vec::with_capacity(source.executables.len());
    for item in &source.executables {
        let (mut payload, bytes) = cas
            .open_blob(&item.payload_blob_hash)?
            .context("retained lifecycle executable payload is absent")?;
        ensure!(
            bytes == item.payload_bytes,
            "retained executable size changed"
        );
        let handle = lillux::sealed_executable_memfd_from_reader(
            c"ryeos-retained-lifecycle-executable",
            &mut payload,
            item.payload_bytes,
            &item.payload_blob_hash,
            1024 * 1024 * 1024,
        )
        .map_err(anyhow::Error::msg)?;
        let manifest_bytes = cas
            .get_blob_bounded(&item.manifest_object_blob_hash, 1024 * 1024)?
            .context("retained executor manifest is absent")?;
        let item_source = cas
            .get_object_bounded(&item.item_source_object_hash, 64 * 1024)?
            .context("retained executor ItemSource is absent")?;
        executables.push(CapturedExecutable {
            identity: ResolvedBinary {
                // Diagnostic only; execution binds this same immutable descriptor.
                absolute_path: handle.path().to_path_buf(),
                content_hash: item.payload_blob_hash.clone(),
                manifest_hash: item.manifest_object_blob_hash.clone(),
                item_source_hash: item.item_source_object_hash.clone(),
                signer_fingerprint: source.signer_fingerprint.clone(),
                target_triple: item.target_triple.clone(),
            },
            source_proof: BundlePayloadSourceProof {
                selected_item_ref: item.item_ref.clone(),
                signed_manifest_ref: cas
                    .get_blob_bounded(
                        &item.signed_manifest_ref_blob_hash,
                        ryeos_engine::executor_resolution::MAX_EXECUTOR_MANIFEST_REF_BYTES,
                    )?
                    .context("retained signed executor ref is absent")?,
                manifest_object: serde_json::from_slice(&manifest_bytes)?,
                item_source_object: item_source,
                signed_sidecar: cas
                    .get_blob_bounded(&item.signed_sidecar_blob_hash, 1024 * 1024)?
                    .context("retained signed sidecar is absent")?,
            },
            handle,
        });
    }
    let mut specs = Vec::with_capacity(source.specs.len());
    for spec in &source.specs {
        let bytes = cas
            .get_blob_bounded(&spec.blob_hash, 1024 * 1024)?
            .context("retained lifecycle spec is absent")?;
        ensure!(
            bytes.len() as u64 == spec.bytes,
            "retained lifecycle spec size changed"
        );
        specs.push(CapturedLifecycleProviderSpec {
            path: spec.path.clone(),
            sha256: spec.blob_hash.clone(),
            bytes: spec.bytes,
            authority: lillux::sealed_memfd(c"ryeos-retained-lifecycle-spec", &bytes)
                .map_err(anyhow::Error::msg)?,
        });
    }
    let mut executable = executables.into_iter();
    let adapter = executable.next().context("retained adapter is absent")?;
    let supervisor = executable.next().context("retained supervisor is absent")?;
    let launcher = executable.next().context("retained launcher is absent")?;
    let restoration_verifier = executable.next();
    let mut spec = specs.into_iter();
    let provider_spec = spec.next().context("retained provider spec is absent")?;
    let snapshot_production_spec = spec.next();
    let artifacts = ResolvedExternalLifecycleArtifacts {
        declaration,
        bundle_name: source.bundle_name,
        bundle_manifest_digest: source.bundle_manifest_body_digest,
        signer_fingerprint: source.signer_fingerprint,
        signed_bundle_manifest: signed_manifest,
        bundle_verifying_key: key_bytes,
        adapter,
        supervisor,
        launcher,
        restoration_verifier,
        provider_spec,
        snapshot_production_spec,
    };
    crate::external_lifecycle_adapter::ExecutableExternalPlacementBackend::new(artifacts)
}

/// Recovery-only facade. It intentionally exposes no fresh allocation,
/// bootstrap create, snapshot production, or worker placement method.
pub(crate) struct RecoveredBootstrapLifecycle {
    backend: crate::external_lifecycle_adapter::ExecutableExternalPlacementBackend,
}

impl RecoveredBootstrapLifecycle {
    pub(crate) fn from_operation(
        state: &crate::state::AppState,
        record: &crate::runtime_db::runtime_snapshot_bootstrap::SnapshotBootstrapRecord,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &crate::vault::placement::PlacementCredential,
    ) -> Result<Self> {
        let root = record
            .lifecycle_artifacts_root
            .as_deref()
            .context("bootstrap recovery lost retained lifecycle artifact root")?;
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let backend = recover_retained_lifecycle_backend(&authority.cas_store()?, root)?;
        authority.ensure_guard(&guard)?;
        backend.preflight_bootstrap_recovery(binding, credential, &record.intent)?;
        let current = state
            .state_store
            .snapshot_bootstrap_operation(&record.intent.operation_id)?
            .context("bootstrap recovery lost operation journal")?;
        ensure!(
            current.intent == record.intent
                && current.lifecycle_artifacts_root.as_deref() == Some(root),
            "bootstrap recovery closure differs from authoritative operation"
        );
        Ok(Self { backend })
    }

    pub(crate) fn observe(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &crate::vault::placement::PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapReadinessRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<crate::external_placement::ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapReadinessAdapterResponse>>{
        self.backend
            .observe_snapshot_bootstrap_source(binding, credential, request, deadline)
    }

    pub(crate) fn terminate(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &crate::vault::placement::PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapTerminationAdapterRequest,
        first_contact: bool,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<crate::external_placement::ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapTerminationAdapterResponse>>{
        self.backend.terminate_snapshot_bootstrap_source(
            binding,
            credential,
            request,
            first_contact,
            deadline,
        )
    }

    pub(crate) fn upload_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &crate::vault::placement::PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotUploadAdapterRequest,
        upload: &lillux::InheritedDescriptorAuthority,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        crate::external_placement::ExternalLifecycleObservation<
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotUploadReceipt,
        >,
    > {
        self.backend
            .upload_runtime_snapshot(binding, credential, request, upload, deadline)
    }

    pub(crate) fn create_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &crate::vault::placement::PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotCreateAdapterRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        crate::external_placement::ExternalLifecycleObservation<
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotCreateResult,
        >,
    > {
        self.backend
            .create_runtime_snapshot(binding, credential, request, deadline)
    }

    pub(crate) fn observe_snapshot_readiness(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &crate::vault::placement::PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotReadinessRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<crate::external_placement::ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotReadinessObservation>>{
        self.backend
            .observe_runtime_snapshot_readiness(binding, credential, request, deadline)
    }
}

pub fn resolve_external_execution_artifacts(
    bundle_roots: &[PathBuf],
    node_trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<ResolvedExternalExecutionArtifacts> {
    let target = lillux::platform::current_binary_target()?;
    let mut connectors = Vec::new();
    let mut configurations = Vec::new();
    let mut lifecycle_adapters = Vec::new();
    let mut provider_owners = BTreeMap::new();
    let mut lifecycle_owners = BTreeMap::new();

    for root in bundle_roots {
        let name = root
            .file_name()
            .and_then(|value| value.to_str())
            .context("registered bundle root has no UTF-8 name")?;
        let verified = ryeos_bundle::manifest::load_verified_manifest(
            &root.join(ryeos_engine::AI_DIR),
            name,
            node_trust_store,
        )
        .with_context(|| format!("verify external execution bundle `{name}`"))?;

        for declaration in &verified.manifest.external_providers {
            declaration.validate()?;
            if !declaration
                .targets
                .iter()
                .any(|candidate| candidate == target)
            {
                continue;
            }
            ensure!(
                provider_owners
                    .insert(declaration.id.clone(), name.to_owned())
                    .is_none(),
                "external provider declaration `{}` has more than one signed bundle owner",
                declaration.id
            );
            let connector = capture_declared_executable(
                root,
                node_trust_store,
                &verified.signer_fingerprint,
                &declaration.connector,
                "external provider connector",
            )?;
            let configuration_adapter = capture_declared_executable(
                root,
                node_trust_store,
                &verified.signer_fingerprint,
                &declaration.configuration_adapter,
                "external provider configuration adapter",
            )?;
            connectors.push(connector);
            configurations.push(InstalledExternalProviderConfiguration::new(
                declaration.clone(),
                verified.body_digest.clone(),
                verified.signer_fingerprint.clone(),
                configuration_adapter,
            )?);
        }

        for declaration in &verified.manifest.external_lifecycle_adapters {
            declaration.validate()?;
            if !declaration
                .targets
                .iter()
                .any(|candidate| candidate == target)
            {
                continue;
            }
            ensure!(
                lifecycle_owners
                    .insert(declaration.id.clone(), name.to_owned())
                    .is_none(),
                "external lifecycle adapter `{}` has more than one signed bundle owner",
                declaration.id
            );
            let signed_manifest =
                ryeos_engine::plan_builder::capture_signed_bundle_source_manifest(
                    root,
                    name,
                    node_trust_store,
                )
                .with_context(|| format!("capture signed lifecycle Bundle `{name}`"))?;
            ensure!(
                signed_manifest.identity.body_digest == verified.body_digest
                    && signed_manifest.identity.signer_fingerprint == verified.signer_fingerprint,
                "captured lifecycle Bundle source differs from admitted declaration"
            );
            let verifier = node_trust_store
                .get(&verified.signer_fingerprint)
                .context("lifecycle Bundle signer has no retained public verifier")?;
            ensure!(
                verifier.fingerprint == verified.signer_fingerprint
                    && lillux::crypto::fingerprint(&verifier.verifying_key)
                        == verified.signer_fingerprint,
                "lifecycle Bundle public verifier differs from admitted signer"
            );
            lifecycle_adapters.push(ResolvedExternalLifecycleArtifacts {
                declaration: declaration.clone(),
                bundle_name: name.to_owned(),
                bundle_manifest_digest: verified.body_digest.clone(),
                signer_fingerprint: verified.signer_fingerprint.clone(),
                signed_bundle_manifest: signed_manifest.signed_bytes,
                bundle_verifying_key: verifier.verifying_key.to_bytes(),
                adapter: capture_declared_executable(
                    root,
                    node_trust_store,
                    &verified.signer_fingerprint,
                    &declaration.adapter,
                    "external lifecycle adapter",
                )?,
                supervisor: capture_declared_executable(
                    root,
                    node_trust_store,
                    &verified.signer_fingerprint,
                    &declaration.supervisor,
                    "external candidate supervisor",
                )?,
                launcher: capture_declared_executable(
                    root,
                    node_trust_store,
                    &verified.signer_fingerprint,
                    &declaration.launcher,
                    "external candidate launcher",
                )?,
                restoration_verifier: declaration
                    .restoration_verifier
                    .as_ref()
                    .map(|name| {
                        capture_declared_executable(
                            root,
                            node_trust_store,
                            &verified.signer_fingerprint,
                            name,
                            "external restoration verifier",
                        )
                    })
                    .transpose()?,
                provider_spec: capture_declared_provider_spec(root, &declaration.provider_spec)?,
                snapshot_production_spec: declaration
                    .snapshot_production_spec
                    .as_ref()
                    .map(|identity| capture_declared_provider_spec(root, identity))
                    .transpose()?,
            });
        }
    }

    let backends = lifecycle_adapters
        .into_iter()
        .map(|artifacts| {
            Ok(Arc::new(
                crate::external_lifecycle_adapter::ExecutableExternalPlacementBackend::new(
                    artifacts,
                )?,
            ) as Arc<dyn ExternalPlacementBackend>)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ResolvedExternalExecutionArtifacts {
        connectors: ExternalCandidateConnectorRegistry::from_captured(connectors)?,
        provider_configurations: ExternalProviderConfigurationRegistry::from_artifacts(
            configurations,
        )?,
        placement_backends: ExternalPlacementBackendRegistry::from_backends(backends)?,
    })
}

fn capture_declared_provider_spec(
    root: &std::path::Path,
    identity: &LifecycleProviderSpecIdentity,
) -> Result<CapturedLifecycleProviderSpec> {
    // `validate` restricts this to a normalized bundle-relative path, and
    // Lillux opens each component without following symlinks.
    let path = root.join(&identity.path);
    let pinned = lillux::secure_fs::open_pinned_regular_file_no_follow(&path)
        .with_context(|| format!("open signed lifecycle provider spec `{}`", identity.path))?;
    let observation = pinned.observation()?;
    let bytes = observation.size();
    ensure!(
        (1..=MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).contains(&bytes),
        "signed lifecycle provider spec exceeds its byte bound"
    );
    ensure!(
        pinned.permission_mode()? & 0o111 == 0,
        "signed lifecycle provider spec must not be executable"
    );
    let captured =
        pinned.capture_sealed_bounded(&observation, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?;
    ensure!(
        captured.digest() == identity.sha256,
        "signed lifecycle provider spec digest does not match its declaration"
    );
    Ok(CapturedLifecycleProviderSpec {
        path: identity.path.clone(),
        sha256: identity.sha256.clone(),
        bytes,
        authority: captured.authority().clone(),
    })
}

fn capture_declared_executable(
    root: &std::path::Path,
    trust: &ryeos_engine::trust::TrustStore,
    bundle_signer: &str,
    executable: &str,
    role: &str,
) -> Result<CapturedExecutable> {
    let captured = capture_bundle_binary_ref(&format!("bin:{executable}"), root, trust)
        .with_context(|| format!("capture signed {role} `{executable}`"))?;
    ensure!(
        captured.identity.signer_fingerprint == bundle_signer,
        "{role} signer does not match its declaring bundle"
    );
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::{LIFECYCLE_ADAPTER_PROTOCOL, LifecycleCapability};
    use ryeos_state::objects::{
        RETAINED_LIFECYCLE_ARTIFACTS_KIND, RETAINED_LIFECYCLE_ARTIFACTS_SCHEMA,
        RetainedLifecycleExecutable, RetainedLifecycleExecutableRole, RetainedLifecycleSpec,
        RetainedLifecycleSpecRole,
    };

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    #[test]
    fn historical_lifecycle_roles_and_specs_must_match_signed_bundle() {
        let signing_key = lillux::crypto::SigningKey::from_bytes(&[31u8; 32]);
        let verifier = signing_key.verifying_key();
        let target = "x86_64-unknown-linux-gnu";
        let spec_bytes = b"{}";
        let spec_hash = lillux::sha256_hex(spec_bytes);
        let declaration = ExternalLifecycleAdapterDeclaration {
            id: "render-sandbox-early-access".to_owned(),
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.to_owned(),
            targets: vec![target.to_owned()],
            adapter: "adapter".to_owned(),
            supervisor: "supervisor".to_owned(),
            launcher: "launcher".to_owned(),
            restoration_verifier: None,
            provider_spec: LifecycleProviderSpecIdentity {
                path: "specs/provider.json".to_owned(),
                sha256: spec_hash.clone(),
            },
            snapshot_production_spec: None,
            settings_schema_digest: hash('a'),
            capabilities: [LifecycleCapability::ExactAllocationReconciliation]
                .into_iter()
                .collect(),
        };
        let manifest = ryeos_bundle::manifest::BundleManifest {
            name: "render-sandbox".to_owned(),
            version: "1.0.0".to_owned(),
            description: String::new(),
            provides_kinds: vec![],
            requires_kinds: vec![],
            uses_kinds: vec![],
            runtime_authority: Default::default(),
            smoke: vec![],
            shadows: vec![],
            isolation_backends: vec![],
            external_providers: vec![],
            external_lifecycle_adapters: vec![declaration.clone()],
        };
        let signed = lillux::signature::sign_content_at(
            &serde_yaml::to_string(&manifest).unwrap(),
            &signing_key,
            "#",
            None,
            "2026-09-30T00:00:00Z",
        );
        let verified = ryeos_bundle::manifest::verify_retained_manifest_bytes(
            signed.as_bytes(),
            "render-sandbox",
            &lillux::crypto::fingerprint(&verifier),
            &verifier,
        )
        .unwrap();
        let executable = |role, name: &str| RetainedLifecycleExecutable {
            role,
            item_ref: format!("bin/{target}/{name}"),
            target_triple: target.to_owned(),
            payload_blob_hash: hash('a'),
            payload_bytes: 1,
            mode: 0o755,
            signed_manifest_ref_blob_hash: hash('b'),
            manifest_object_blob_hash: hash('c'),
            item_source_object_hash: hash('d'),
            signed_sidecar_blob_hash: hash('e'),
        };
        let mut source = RetainedLifecycleArtifacts {
            schema: RETAINED_LIFECYCLE_ARTIFACTS_SCHEMA,
            kind: RETAINED_LIFECYCLE_ARTIFACTS_KIND.to_owned(),
            bundle_name: "render-sandbox".to_owned(),
            signed_bundle_manifest_blob_hash: lillux::sha256_hex(signed.as_bytes()),
            bundle_manifest_body_digest: verified.body_digest,
            signer_fingerprint: lillux::crypto::fingerprint(&verifier),
            signer_verifying_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifier.to_bytes())
            ),
            declaration_id: declaration.id.clone(),
            executables: vec![
                executable(RetainedLifecycleExecutableRole::Adapter, "adapter"),
                executable(RetainedLifecycleExecutableRole::Supervisor, "supervisor"),
                executable(RetainedLifecycleExecutableRole::Launcher, "launcher"),
            ],
            specs: vec![RetainedLifecycleSpec {
                role: RetainedLifecycleSpecRole::Provider,
                path: declaration.provider_spec.path.clone(),
                blob_hash: declaration.provider_spec.sha256.clone(),
                bytes: spec_bytes.len() as u64,
            }],
        };
        assert_eq!(
            verify_retained_lifecycle_declaration(&source, signed.as_bytes())
                .unwrap()
                .id,
            declaration.id
        );
        source.executables[1].item_ref = format!("bin/{target}/other");
        assert!(verify_retained_lifecycle_declaration(&source, signed.as_bytes()).is_err());
        source.executables[1].item_ref = format!("bin/{target}/supervisor");
        source.specs[0].blob_hash = hash('e');
        assert!(verify_retained_lifecycle_declaration(&source, signed.as_bytes()).is_err());
        source.specs[0].blob_hash = spec_hash;

        let tmp = tempfile::tempdir().unwrap();
        let cas = lillux::CasStore::new(tmp.path().to_path_buf());
        assert_eq!(
            cas.store_blob(signed.as_bytes()).unwrap(),
            source.signed_bundle_manifest_blob_hash
        );
        assert_eq!(
            cas.store_blob(spec_bytes).unwrap(),
            source.specs[0].blob_hash
        );
        let mut selected = serde_json::Map::new();
        for executable in &mut source.executables {
            let payload = format!("binary for {}", executable.item_ref);
            executable.payload_blob_hash = cas.store_blob(payload.as_bytes()).unwrap();
            executable.payload_bytes = payload.len() as u64;
            let item_source = serde_json::json!({
                "kind": "item_source",
                "item_ref": executable.item_ref.clone(),
                "content_blob_hash": executable.payload_blob_hash.clone(),
                "integrity": format!("sha256:{}", executable.payload_blob_hash),
                "signature_info": null,
                "mode": executable.mode,
            });
            executable.item_source_object_hash = cas.store_object(&item_source).unwrap();
            let sidecar = lillux::signature::sign_content_at(
                &lillux::canonical_json(&item_source).unwrap(),
                &signing_key,
                "#",
                None,
                "2026-09-30T00:00:00Z",
            );
            executable.signed_sidecar_blob_hash = cas.store_blob(sidecar.as_bytes()).unwrap();
            selected.insert(
                executable.item_ref.clone(),
                serde_json::Value::String(executable.item_source_object_hash.clone()),
            );
        }
        let manifest = serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": selected,
        });
        let manifest_hash = cas
            .store_blob(lillux::canonical_json(&manifest).unwrap().as_bytes())
            .unwrap();
        let signed_ref = lillux::signature::sign_content_at(
            &format!(
                "{}\n{}\n",
                ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN,
                manifest_hash
            ),
            &signing_key,
            "#",
            None,
            "2026-09-30T00:00:00Z",
        );
        let signed_ref_hash = cas.store_blob(signed_ref.as_bytes()).unwrap();
        for executable in &mut source.executables {
            executable.manifest_object_blob_hash = manifest_hash.clone();
            executable.signed_manifest_ref_blob_hash = signed_ref_hash.clone();
        }
        let root_hash = cas.store_object(&source.to_value().unwrap()).unwrap();
        assert_eq!(
            verify_retained_lifecycle_closure(&cas, &root_hash).unwrap(),
            source
        );
        source.executables[0].signed_sidecar_blob_hash = hash('f');
        let missing_root = cas.store_object(&source.to_value().unwrap()).unwrap();
        assert!(verify_retained_lifecycle_closure(&cas, &missing_root).is_err());
    }
}
