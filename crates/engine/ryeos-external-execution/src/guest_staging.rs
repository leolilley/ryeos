//! Descriptor-relative, private staging of one external guest-input package.
//!
//! This is not a Render adapter, a qualification verdict, or a Ready claim.
//! It checks the wire and exact base CAS closure before returning a pinned
//! private tree. Product/source authority and fixed-FD supervisor installation
//! remain separate joined checks.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::ExternalGuestInputProjection;
use ryeos_external_execution_contract::staging_package::{
    GuestImportContext, GuestImportTicket, GuestStagingEntry, GuestStagingExpected,
    GuestStagingPackageManifest, GuestStagingStreamReader, MAX_GUEST_STAGING_ENTRIES,
    MAX_GUEST_STAGING_MANIFEST_BYTES,
};
use serde::{Deserialize, Serialize};

const STAGE_MANIFEST_FILE: &str = "stage-manifest.json";

/// Durable coordinate for one private import generation. A recovery owner
/// must resolve it under its separately retained private parent and compare
/// the exact pinned inode before inspecting or retiring the generation. This
/// record alone does not convey filesystem or cleanup authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestStageIdentity {
    schema: u32,
    name: String,
    directory_identity: lillux::PinnedDirectoryIdentity,
    manifest_sha256: String,
}

impl GuestStageIdentity {
    /// Validate retained stage coordinates without reconstructing an
    /// ephemeral source after its owner dies. This grants no stage access or
    /// recovery permission.
    pub fn validate_for_ticket(&self, manifest_sha256: &str) -> Result<()> {
        ensure!(
            self.schema == 1
                && canonical_stage_name(&self.name)
                && lillux::valid_hash(&self.manifest_sha256)
                && self.manifest_sha256 == manifest_sha256,
            "guest stage identity differs from retained import ticket"
        );
        Ok(())
    }

    /// Resolve only the exact named generation under the separately retained
    /// private parent. This is a point read, never a stage-directory scan.
    pub fn resolve_under(
        &self,
        parent: &lillux::PinnedDirectory,
    ) -> Result<lillux::PinnedDirectory> {
        ensure!(
            self.schema == 1
                && canonical_stage_name(&self.name)
                && lillux::valid_hash(&self.manifest_sha256),
            "guest stage identity is invalid"
        );
        parent.require_owner_private_directory()?;
        let root = parent
            .open_child_directory(OsStr::new(&self.name))?
            .context("retained guest stage generation is absent")?;
        root.require_owner_private_directory()?;
        ensure!(
            root.identity()? == self.directory_identity,
            "retained guest stage generation changed inode"
        );
        Ok(root)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn directory_identity(&self) -> lillux::PinnedDirectoryIdentity {
        self.directory_identity
    }

    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
}

fn canonical_stage_name(name: &str) -> bool {
    let mut parts = name.split('.');
    let (Some("guest-input"), Some(pid), Some(nonce), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    pid.parse::<u32>()
        .ok()
        .is_some_and(|value| value != 0 && value.to_string() == pid)
        && nonce
            .parse::<u64>()
            .ok()
            .is_some_and(|value| value.to_string() == nonce)
}

pub struct StagedGuestPackage {
    parent: lillux::PinnedDirectory,
    name: OsString,
    root: lillux::PinnedDirectory,
    // Serializes cooperating import/recovery owners for this exact inode.
    // It is not writer exclusion against an untrusted process in the guest.
    _owner_lock: lillux::PinnedDirectoryLock,
    manifest: GuestStagingPackageManifest,
    base: ryeos_project_capture::ProjectSnapshotTransferMeasurement,
}

/// A staged generation whose uploaded bytes, manifest, and realized content
/// were checked against the caller's occurrence import ticket and context.
/// The caller must have obtained those expectations from retained authority.
/// This is an import-time check, not immutable custody, supervisor adoption,
/// Ready, whole-scope settlement, or qualification evidence.
pub struct TicketedGuestImport {
    staged: StagedGuestPackage,
    ticket: GuestImportTicket,
}

impl TicketedGuestImport {
    pub(crate) fn staged(&self) -> &StagedGuestPackage {
        &self.staged
    }

    pub fn stage_identity(&self) -> Result<GuestStageIdentity> {
        let identity = GuestStageIdentity {
            schema: 1,
            name: self
                .staged
                .name
                .to_str()
                .context("guest staged generation name is not UTF-8")?
                .to_owned(),
            directory_identity: self.staged.root.identity()?,
            manifest_sha256: self.ticket.manifest_sha256.clone(),
        };
        identity.resolve_under(&self.staged.parent)?;
        Ok(identity)
    }

    pub(crate) fn root(&self) -> &lillux::PinnedDirectory {
        self.staged.root()
    }

    pub fn manifest(&self) -> &GuestStagingPackageManifest {
        self.staged.manifest()
    }

    pub fn base(&self) -> &ryeos_project_capture::ProjectSnapshotTransferMeasurement {
        self.staged.base()
    }

    pub fn ticket(&self) -> &GuestImportTicket {
        &self.ticket
    }

    /// Recheck the mutable private generation immediately before fixed-FD
    /// adoption. The caller still has to exclude concurrent writers while it
    /// binds the checked descriptors, and retain those descriptors through
    /// whole-scope settlement.
    pub fn recheck_for_adoption(
        &self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
    ) -> Result<()> {
        self.ticket.staging_expected(context, inputs)?;
        ensure!(
            self.staged.manifest().identity_digest()? == self.ticket.manifest_sha256,
            "guest import manifest changed before adoption"
        );
        let base_root = self
            .staged
            .root()
            .open_child_directory(OsStr::new("base"))?
            .context("guest base transfer disappeared before adoption")?;
        let base = ryeos_project_capture::inspect_project_snapshot_transfer(
            &base_root,
            &inputs.base_snapshot.snapshot_hash,
        )?;
        ensure!(
            base.snapshot_hash == self.staged.base().snapshot_hash
                && base.closure_digest == self.staged.base().closure_digest
                && base.object_count == self.staged.base().object_count
                && base.blob_count == self.staged.base().blob_count
                && base.total_bytes == self.staged.base().total_bytes,
            "guest base transfer changed before adoption"
        );
        for (name, expected) in [
            ("bootstrap", &self.ticket.bootstrap_sha256),
            ("supervisor", &self.ticket.supervisor_sha256),
            ("launcher", &self.ticket.launcher_sha256),
        ] {
            let (expected_bytes, expected_mode) = self
                .staged
                .manifest()
                .entries
                .iter()
                .find_map(|entry| match entry {
                    GuestStagingEntry::RegularFile {
                        path, bytes, mode, ..
                    } if path == name => Some((*bytes, *mode)),
                    _ => None,
                })
                .with_context(|| format!("guest {name} is absent from the import manifest"))?;
            let file = self
                .staged
                .root()
                .open_pinned_regular(OsStr::new(name), false)?
                .with_context(|| format!("guest {name} disappeared before adoption"))?;
            let observation = file.observation()?;
            ensure!(
                observation.size() == expected_bytes
                    && file.permission_mode()? == expected_mode
                    && file.digest_stable_exact(&observation)? == *expected,
                "guest {name} changed before adoption"
            );
        }
        crate::guest_content::recheck_staged_guest_content(&self.staged, inputs)
    }

    /// Install the verified base into an exact, empty private candidate
    /// runtime while retaining this import generation. The caller must have
    /// durably recorded the stage and runtime identities before this mutable
    /// installation. This operation does not grant supervisor launch or Ready:
    /// its caller still has to exclude untrusted writers, bind every checked
    /// descriptor, and retain the generation through scope settlement.
    pub(crate) fn install_base_into(
        &self,
        context: &GuestImportContext<'_>,
        inputs: &ExternalGuestInputProjection,
        candidate_runtime: &lillux::PinnedDirectory,
    ) -> Result<()> {
        candidate_runtime.require_owner_private_directory()?;
        candidate_runtime.require_disjoint_directory_tree(self.staged.root())?;
        self.recheck_for_adoption(context, inputs)?;
        let base = self
            .staged
            .root()
            .open_child_directory(OsStr::new("base"))?
            .context("ticketed guest base transfer disappeared before installation")?;
        ryeos_project_capture::install_project_snapshot_transfer(
            &base,
            candidate_runtime,
            self.staged.base(),
        )
    }

    /// Discard an unadopted import after failed preparation. The future guest
    /// owner must retain an adopted generation until separately proved scope
    /// and writer settlement; this method does not prove either condition.
    pub fn discard(self) -> Result<()> {
        self.staged.discard()
    }
}

impl StagedGuestPackage {
    pub fn root(&self) -> &lillux::PinnedDirectory {
        &self.root
    }

    pub fn manifest(&self) -> &GuestStagingPackageManifest {
        &self.manifest
    }

    pub fn base(&self) -> &ryeos_project_capture::ProjectSnapshotTransferMeasurement {
        &self.base
    }

    /// Explicitly discard the exact staged generation. The eventual fixed-FD
    /// adoption path must take ownership of this object and call this after
    /// whole-scope settlement; dropping it does not silently delete live input.
    pub fn discard(self) -> Result<()> {
        remove_staged_generation(&self.parent, &self.name, &self.root)
    }
}

/// Consume an immutable uploaded regular file into a newly created private
/// generation. The caller must provide an owner-private staging parent and
/// derive `expected` independently from the uploaded package. On any error,
/// this function attempts bounded exact-generation cleanup and never returns
/// the partial tree.
pub fn stage_guest_package<R: Read>(
    reader: R,
    parent: &lillux::PinnedDirectory,
    expected: &GuestStagingExpected<'_>,
) -> Result<StagedGuestPackage> {
    parent.require_owner_private_directory()?;
    let owner = parent.try_clone()?;
    let (name, stage) = parent.create_unique_child("guest-input", 0o700)?;
    let owner_lock = stage
        .try_lock_exclusive()?
        .context("new guest stage has a live owner; refusing to remove a contested generation")?;
    owner_lock.ensure_protects(&stage)?;
    let result = stage
        .require_owner_private_directory()
        .and_then(|()| stage_guest_package_in_private_root(reader, &stage, expected));
    match result {
        Ok((manifest, base)) => Ok(StagedGuestPackage {
            parent: owner,
            name,
            root: stage,
            _owner_lock: owner_lock,
            manifest,
            base,
        }),
        Err(error) => {
            let cleanup = remove_staged_generation(&owner, &name, &stage);
            if let Err(cleanup_error) = cleanup {
                return Err(
                    error.context(format!("guest staging cleanup failed: {cleanup_error:#}"))
                );
            }
            Err(error)
        }
    }
}

/// Import the exact uploaded inode named by an independently retained byte
/// count and digest. Lillux performs positional, mutation-checked reads; a
/// full-stream digest mismatch discards even an otherwise valid staged tree.
pub fn stage_uploaded_guest_package(
    upload: &lillux::PinnedRegularFile,
    upload_bytes: u64,
    upload_sha256: &str,
    parent: &lillux::PinnedDirectory,
    expected: &GuestStagingExpected<'_>,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<StagedGuestPackage> {
    ensure!(
        !deadline.has_elapsed(),
        "guest upload staging deadline expired"
    );
    let mut reader =
        upload.stable_reader_exact(upload_bytes, upload_sha256, expected.maximum_framed_bytes)?;
    let staged = stage_guest_package(
        &mut DeadlineReader {
            inner: &mut reader,
            deadline,
        },
        parent,
        expected,
    )?;
    if let Err(error) = reader.finish() {
        if let Err(cleanup_error) = staged.discard() {
            return Err(error.context(format!(
                "uploaded guest staging cleanup failed: {cleanup_error:#}"
            )));
        }
        return Err(error);
    }
    if deadline.has_elapsed() {
        if let Err(cleanup_error) = staged.discard() {
            anyhow::bail!(
                "guest upload staging deadline expired and cleanup failed: {cleanup_error:#}"
            );
        }
        anyhow::bail!("guest upload staging deadline expired");
    }
    Ok(staged)
}

/// Import one uploaded inode against a separately carried ticket and the
/// guest's independently retained placement. The stable reader measures every
/// uploaded byte before this returns; a matching provider acknowledgement is
/// never sufficient. This is still not supervisor adoption or Ready.
pub(crate) fn stage_ticketed_uploaded_guest_package(
    upload: &lillux::PinnedRegularFile,
    parent: &lillux::PinnedDirectory,
    ticket: &GuestImportTicket,
    context: &GuestImportContext<'_>,
    inputs: &ExternalGuestInputProjection,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<TicketedGuestImport> {
    let expected = ticket.staging_expected(context, inputs)?;
    let staged = stage_uploaded_guest_package(
        upload,
        ticket.framed_bytes,
        &ticket.payload_sha256,
        parent,
        &expected,
        deadline,
    )?;
    let check = ticket
        .validate_verified_manifest(
            context,
            staged.manifest(),
            &ticket.payload_sha256,
            ticket.framed_bytes,
        )
        .and_then(|()| crate::guest_content::recheck_staged_guest_content(&staged, inputs));
    if let Err(error) = check {
        if let Err(cleanup) = staged.discard() {
            return Err(error.context(format!("ticketed guest import cleanup failed: {cleanup:#}")));
        }
        return Err(error);
    }
    Ok(TicketedGuestImport {
        staged,
        ticket: ticket.clone(),
    })
}

/// Structural fixture for reopening a stage while its independently retained
/// parent still exists. Production recovery cannot reconstruct the ephemeral
/// process-private source after owner death and must not use this path to
/// restage, install, launch, or claim Ready.
#[cfg(test)]
pub(crate) fn recover_ticketed_guest_import(
    parent: &lillux::PinnedDirectory,
    identity: &GuestStageIdentity,
    ticket: &GuestImportTicket,
    context: &GuestImportContext<'_>,
    inputs: &ExternalGuestInputProjection,
) -> Result<TicketedGuestImport> {
    let expected = ticket.staging_expected(context, inputs)?;
    ensure!(
        identity.manifest_sha256() == ticket.manifest_sha256,
        "retained guest stage and import ticket disagree"
    );
    let root = identity.resolve_under(parent)?;
    let owner_lock = root
        .try_lock_exclusive()?
        .context("retained guest stage already has a live owner")?;
    owner_lock.ensure_protects(&root)?;
    let manifest = read_stage_manifest(&root, &ticket.manifest_sha256)?;
    manifest.validate_for(
        inputs,
        &ticket.activation_request_digest,
        &ticket.bootstrap_sha256,
        &ticket.supervisor_sha256,
        &ticket.launcher_sha256,
        expected.maximum_regular_bytes,
    )?;
    ensure!(
        manifest.framed_bytes()? == ticket.framed_bytes
            && manifest.total_regular_bytes == ticket.regular_bytes,
        "recovered guest manifest contradicts retained package bounds"
    );
    let base_root = root
        .open_child_directory(OsStr::new("base"))?
        .context("recovered guest base transfer is absent")?;
    let base = ryeos_project_capture::inspect_project_snapshot_transfer(
        &base_root,
        &inputs.base_snapshot.snapshot_hash,
    )?;
    ensure!(
        base.snapshot_hash == inputs.base_snapshot.snapshot_hash
            && base.closure_digest == inputs.base_snapshot.closure_digest
            && base.object_count == inputs.base_snapshot.object_count
            && base.blob_count == inputs.base_snapshot.blob_count
            && base.total_bytes == inputs.base_snapshot.total_bytes,
        "recovered guest base differs from retained snapshot"
    );
    let imported = TicketedGuestImport {
        staged: StagedGuestPackage {
            parent: parent.try_clone()?,
            name: OsString::from(identity.name()),
            root,
            _owner_lock: owner_lock,
            manifest,
            base,
        },
        ticket: ticket.clone(),
    };
    imported.recheck_for_adoption(context, inputs)?;
    Ok(imported)
}

#[cfg(test)]
fn read_stage_manifest(
    root: &lillux::PinnedDirectory,
    expected_digest: &str,
) -> Result<GuestStagingPackageManifest> {
    let file = root
        .open_pinned_regular(OsStr::new(STAGE_MANIFEST_FILE), false)?
        .context("retained guest stage manifest is absent")?;
    let observation = file.observation()?;
    ensure!(
        observation.size() <= MAX_GUEST_STAGING_MANIFEST_BYTES as u64,
        "retained guest stage manifest exceeds bound"
    );
    ensure!(
        file.permission_mode()? == 0o600,
        "retained guest stage manifest mode changed"
    );
    let bytes = file.read_stable_bounded(&observation, MAX_GUEST_STAGING_MANIFEST_BYTES as u64)?;
    ensure!(
        lillux::sha256_hex(&bytes) == expected_digest,
        "retained guest stage manifest changed bytes"
    );
    let manifest = GuestStagingPackageManifest::from_bounded_json(&bytes)?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&manifest)? == bytes,
        "retained guest stage manifest is noncanonical"
    );
    Ok(manifest)
}

struct DeadlineReader<'a, R> {
    inner: &'a mut R,
    deadline: lillux::time::MonotonicDeadline,
}

impl<R: Read> Read for DeadlineReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.deadline.has_elapsed() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "guest upload staging deadline expired",
            ));
        }
        self.inner.read(buffer)
    }
}

fn remove_staged_generation(
    parent: &lillux::PinnedDirectory,
    name: &OsStr,
    root: &lillux::PinnedDirectory,
) -> Result<()> {
    root.remove_contents_recursive_bounded(staging_budget())?;
    ensure!(
        parent.remove_empty_child_if_same(name, root)?,
        "guest staging generation changed before cleanup"
    );
    Ok(())
}

fn stage_guest_package_in_private_root<R: Read>(
    reader: R,
    stage: &lillux::PinnedDirectory,
    expected: &GuestStagingExpected<'_>,
) -> Result<(
    GuestStagingPackageManifest,
    ryeos_project_capture::ProjectSnapshotTransferMeasurement,
)> {
    ensure!(
        stage.entries_no_follow_bounded(0)?.is_empty(),
        "guest staging generation is not empty"
    );
    let mut stream = GuestStagingStreamReader::new(reader, expected)?;
    let manifest = stream.manifest().clone();
    let mut product_symlinks = BTreeMap::<&str, Vec<(&str, &str)>>::new();
    for entry in &manifest.entries {
        if let GuestStagingEntry::Symlink { path, target } = entry {
            let (root, relative) = path
                .split_once('/')
                .context("guest product symlink has no relative path")?;
            product_symlinks
                .entry(root)
                .or_default()
                .push((relative, target));
        }
    }
    for links in product_symlinks.values() {
        ryeos_state::objects::validate_internal_symlink_graph(links.iter().copied())?;
    }
    for entry in &manifest.entries {
        let path = entry.path();
        let (parent_path, child_name) = match path.rsplit_once('/') {
            Some((parent_path, name)) => (Some(parent_path), name),
            None => (None, path),
        };
        let directory = open_staged_parent(stage, parent_path)?;
        match entry {
            GuestStagingEntry::Directory { mode, .. } => {
                let created = directory.create_child(OsStr::new(child_name), 0o700)?;
                created.set_mode(*mode)?;
            }
            GuestStagingEntry::RegularFile { mode, .. } => {
                ensure!(
                    stream.next_file().is_some_and(|next| next.path() == path),
                    "guest staging file order changed"
                );
                let mut file =
                    directory.open_regular_create(OsStr::new(child_name), true, true, 0o600)?;
                stream.copy_next_file(&mut file)?;
                lillux::set_open_regular_file_mode(&file, *mode)?;
                file.sync_all()?;
            }
            GuestStagingEntry::Symlink { target, .. } => {
                directory.create_symlink(OsStr::new(child_name), target.as_bytes())?;
            }
        }
    }
    stream.finish()?;

    let base_root = stage
        .open_child_directory(OsStr::new("base"))?
        .context("guest base transfer root is absent")?;
    let base = ryeos_project_capture::inspect_project_snapshot_transfer(
        &base_root,
        &expected.inputs.base_snapshot.snapshot_hash,
    )?;
    let admitted = &expected.inputs.base_snapshot;
    ensure!(
        base.snapshot_hash == admitted.snapshot_hash
            && base.closure_digest == admitted.closure_digest
            && base.object_count == admitted.object_count
            && base.blob_count == admitted.blob_count
            && base.total_bytes == admitted.total_bytes,
        "guest base transfer contradicts the retained snapshot measurement"
    );
    let manifest_bytes = ryeos_external_execution_contract::canonical_json(&manifest)?;
    ensure!(
        manifest_bytes.len() <= MAX_GUEST_STAGING_MANIFEST_BYTES,
        "guest stage manifest exceeds bound"
    );
    let mut sidecar =
        stage.open_regular_create(OsStr::new(STAGE_MANIFEST_FILE), true, true, 0o600)?;
    sidecar.write_all(&manifest_bytes)?;
    sidecar.sync_all()?;
    stage.sync_tree_with_symlinks_bounded(staging_budget(), 4096)?;
    Ok((manifest, base))
}

/// Reopen each component through the retained root descriptor. Holding one
/// directory per manifest entry would turn the entry bound into a file-
/// descriptor exhaustion path; the contract already limits depth to 32.
fn open_staged_parent(
    stage: &lillux::PinnedDirectory,
    parent_path: Option<&str>,
) -> Result<lillux::PinnedDirectory> {
    let mut directory = stage.try_clone()?;
    if let Some(path) = parent_path {
        for component in path.split('/') {
            directory = directory
                .open_child_directory(OsStr::new(component))?
                .context("guest staging parent directory is absent")?;
        }
    }
    Ok(directory)
}

fn staging_budget() -> lillux::DirectoryTraversalBudget {
    lillux::DirectoryTraversalBudget::new(MAX_GUEST_STAGING_ENTRIES + 1, 32)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::Arc;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use super::*;
    use ryeos_external_execution_contract::staging_package::{
        GUEST_STAGING_PACKAGE_SCHEMA, GuestStagingStreamWriter,
    };
    use ryeos_external_execution_contract::{
        EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection,
        GuestBaseSnapshotInput, GuestMountAccess, GuestMountContentAuthority, GuestMountInput,
        GuestMountKind, GuestMountRole, GuestProductManifestKind,
    };
    use ryeos_state::objects::*;

    fn valid_supervisor_bootstrap(
        inputs: &ExternalGuestInputProjection,
        launcher_hash: &str,
    ) -> Vec<u8> {
        use ryeos_state::external_execution::admission::{
            AdmittedExternalCandidateProgram, ExternalCandidateProcFilesystem,
            ExternalCandidateRequirement, ExternalCandidateRuntimeRecipe, PROTOCOL,
        };
        use ryeos_state::external_execution::transport::{
            EXTERNAL_CHANNEL_ROUTE_CONTRACT, ExternalControllerTransportContract,
            ExternalNetworkInputPolicy, ExternalNetworkInputSelection,
            ExternalSupervisorBootstrap, external_tls_root_bundle_digest,
        };

        let roots = vec![STANDARD.encode(b"fixture DER root")];
        let recipe = ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime/product".into(),
            executable_relative_path: "bin/tool".into(),
            argv0: "tool".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        };
        let recipe_digest = recipe.digest().unwrap();
        let requirement = ExternalCandidateRequirement {
            schema: 6,
            protocol: PROTOCOL.into(),
            connector_protocol: ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
            execution_route: ryeos_state::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
            required_lifecycle_capabilities: Default::default(),
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "product".into(),
            runtime_recipe: recipe,
        };
        let qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        let network_inputs = ExternalNetworkInputPolicy {
            resolver: ExternalNetworkInputSelection {
                source: "/etc/resolv.conf".into(),
                max_bytes: 65_536,
            },
            hosts: ExternalNetworkInputSelection {
                source: "/etc/hosts".into(),
                max_bytes: 65_536,
            },
        };
        let bootstrap = ExternalSupervisorBootstrap {
            schema: 7,
            controller: ExternalControllerTransportContract {
                schema: 2,
                network_inputs,
                https_origin: "https://controller.example:7443".into(),
                route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
                tls_root_bundle_digest: external_tls_root_bundle_digest(&roots).unwrap(),
                connect_timeout_ms: 5_000,
                request_timeout_ms: 10_000,
                maximum_response_bytes: 1024 * 1024,
            },
            tls_root_certificates_der_base64: roots,
            placement_thread_id: "T-staging-test".into(),
            occurrence_id: "occ-staging-test".into(),
            allocation_request_digest: "2".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: inputs.base_snapshot.snapshot_hash.clone(),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            launcher_artifact_hash: launcher_hash.into(),
            candidate_program: AdmittedExternalCandidateProgram {
                requirement,
                qualification_use,
                runtime_manifest_kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
                runtime_manifest_hash: "e".repeat(64),
                runtime_witness_hash: "1".repeat(64),
                qualification_attestation_hash: "2".repeat(64),
                selection_identity_digest: "3".repeat(64),
                runtime_recipe_digest: recipe_digest,
            }
            .into(),
            guest_input_identity: inputs.identity_digest().unwrap(),
            guest_inputs: inputs.clone(),
            owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[41; 32]).verifying_key(),
            )
            .unwrap(),
            bootstrap_capability: STANDARD.encode([42_u8; 32]),
            attachment_deadline_ms: 2_000_000,
            execution_timeout_seconds: 60,
            post_execution_timeout_seconds: 120,
            candidate_export_max_bytes: 512 * 1024,
            channel_max_bytes: 1024 * 1024,
        };
        bootstrap.canonical_bytes().unwrap()
    }

    #[test]
    fn malformed_base_closure_never_escapes_private_staging() {
        let bootstrap = b"boot".as_slice();
        let configuration = b"cfg".as_slice();
        let launcher = b"start".as_slice();
        let supervisor = b"runrun".as_slice();
        let inputs = ExternalGuestInputProjection {
            schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 56,
                snapshot_hash: "a".repeat(64),
                closure_digest: "b".repeat(64),
                object_count: 3,
                blob_count: 0,
                total_bytes: 0,
            },
            workspace_outputs: None,
            inputs: vec![GuestMountInput {
                role: GuestMountRole::Configuration,
                authority_id: "config".into(),
                descriptor: 64,
                destination: "/runtime/config".into(),
                kind: GuestMountKind::RegularFile,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: Some(0o644),
                content_authority: GuestMountContentAuthority::RawFile {
                    sha256: lillux::sha256_hex(configuration),
                },
                bytes: configuration.len() as u64,
            }],
            executable_search: Vec::new(),
            environment: BTreeMap::new(),
        };
        let manifest = GuestStagingPackageManifest {
            schema: GUEST_STAGING_PACKAGE_SCHEMA,
            activation_request_digest: "c".repeat(64),
            guest_input_identity: inputs.identity_digest().unwrap(),
            bootstrap_sha256: lillux::sha256_hex(bootstrap),
            supervisor_sha256: lillux::sha256_hex(supervisor),
            launcher_sha256: lillux::sha256_hex(launcher),
            total_regular_bytes: (bootstrap.len()
                + configuration.len()
                + launcher.len()
                + supervisor.len()) as u64,
            entries: vec![
                GuestStagingEntry::Directory {
                    path: "base".into(),
                    mode: 0o700,
                },
                GuestStagingEntry::RegularFile {
                    path: "bootstrap".into(),
                    mode: 0o600,
                    bytes: bootstrap.len() as u64,
                    sha256: lillux::sha256_hex(bootstrap),
                },
                GuestStagingEntry::RegularFile {
                    path: "input-00".into(),
                    mode: 0o644,
                    bytes: configuration.len() as u64,
                    sha256: lillux::sha256_hex(configuration),
                },
                GuestStagingEntry::RegularFile {
                    path: "launcher".into(),
                    mode: 0o755,
                    bytes: launcher.len() as u64,
                    sha256: lillux::sha256_hex(launcher),
                },
                GuestStagingEntry::RegularFile {
                    path: "supervisor".into(),
                    mode: 0o755,
                    bytes: supervisor.len() as u64,
                    sha256: lillux::sha256_hex(supervisor),
                },
            ],
        };
        let expected = GuestStagingExpected {
            inputs: &inputs,
            activation_request_digest: &manifest.activation_request_digest,
            bootstrap_sha256: &manifest.bootstrap_sha256,
            supervisor_sha256: &manifest.supervisor_sha256,
            launcher_sha256: &manifest.launcher_sha256,
            maximum_regular_bytes: manifest.total_regular_bytes,
            maximum_framed_bytes: manifest.framed_bytes().unwrap(),
        };
        let mut stream =
            GuestStagingStreamWriter::new(Vec::new(), manifest.clone(), &expected).unwrap();
        for payload in [bootstrap, configuration, launcher, supervisor] {
            stream.copy_next_file(&mut payload.as_ref()).unwrap();
        }
        let bytes = stream.finish().unwrap();
        let private = tempfile::tempdir().unwrap();
        let parent = lillux::PinnedDirectory::open(private.path())
            .unwrap()
            .unwrap();
        parent.tighten_owner_private_directory().unwrap();
        let error = stage_guest_package(bytes.as_slice(), &parent, &expected)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("snapshot"));
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
    }

    #[test]
    fn exact_base_closure_can_be_staged_without_publishing_it() {
        let state = tempfile::tempdir().unwrap();
        let db = ryeos_state::StateDb::open(state.path(), Arc::new(ryeos_state::TrustStore::new()))
            .unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();
        let policy = ryeos_state::objects::ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            BTreeMap::new(),
        )
        .unwrap();
        let policy_hash = cas.store_object(&policy.to_value()).unwrap();
        let tree = ryeos_state::objects::ProjectTree {
            files: BTreeMap::new(),
        };
        let tree_hash = cas.store_object(&tree.to_value()).unwrap();
        let snapshot = ryeos_state::objects::ProjectSnapshot {
            project_tree_hash: tree_hash,
            effective_policy_hash: policy_hash,
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-09-26T00:00:00Z".into(),
            source: "guest-staging-test".into(),
        };
        let snapshot_hash = cas.store_object(&snapshot.to_value()).unwrap();
        let transfer = ryeos_project_capture::prepare_project_snapshot_transfer(
            &authority,
            &guard,
            &snapshot_hash,
        )
        .unwrap();
        let measurement = transfer.measurement().clone();
        let transfer_root = transfer
            .descriptor()
            .try_clone_pinned_directory("<guest-staging-test-base>".into())
            .unwrap();
        let mut files = BTreeMap::<String, Vec<u8>>::new();
        let mut directory_entries = vec![GuestStagingEntry::Directory {
            path: "base".into(),
            mode: 0o700,
        }];
        let mut file_entries = Vec::new();
        transfer_root
            .visit_regular_files_bounded(
                lillux::DirectoryTraversalBudget::new(400_010, 4),
                |relative, is_directory| {
                    if is_directory {
                        directory_entries.push(GuestStagingEntry::Directory {
                            path: format!("base/{}", relative.to_str().unwrap()),
                            mode: 0o700,
                        });
                    }
                    Ok(false)
                },
                |relative, file| {
                    let bytes = lillux::read_open_regular_file_bounded(file, 1024 * 1024).unwrap();
                    let path = format!("base/{}", relative.to_str().unwrap());
                    file_entries.push(GuestStagingEntry::RegularFile {
                        path: path.clone(),
                        mode: 0o600,
                        bytes: bytes.len() as u64,
                        sha256: lillux::sha256_hex(&bytes),
                    });
                    files.insert(path, bytes);
                    Ok(())
                },
            )
            .unwrap();
        let mut entries = directory_entries;
        entries.extend(file_entries);
        let mut bootstrap = b"boot".to_vec();
        let config = b"cfg".to_vec();
        let launcher = b"start".to_vec();
        let supervisor = b"runrun".to_vec();
        let product_manifest_object = ryeos_state::objects::ExternalContentManifestObject {
            schema: ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA.into(),
            kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            entries: vec![
                ryeos_state::objects::ExternalContentManifestEntry {
                    path: "bin".into(),
                    kind: ryeos_state::objects::ExternalContentManifestEntryKind::Dir,
                    mode: None,
                    blob_hash: None,
                    size: None,
                    target: None,
                },
                ryeos_state::objects::ExternalContentManifestEntry {
                    path: "bin/tool".into(),
                    kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
                    mode: Some(0o755),
                    blob_hash: Some(lillux::sha256_hex(b"tool")),
                    size: Some(4),
                    target: None,
                },
                ryeos_state::objects::ExternalContentManifestEntry {
                    path: "current".into(),
                    kind: ryeos_state::objects::ExternalContentManifestEntryKind::Symlink,
                    mode: None,
                    blob_hash: None,
                    size: None,
                    target: Some("bin/tool".into()),
                },
            ],
            entry_count: 3,
            total_bytes: 4,
        };
        product_manifest_object.validate().unwrap();
        let product_manifest =
            lillux::canonical_json(&serde_json::to_value(&product_manifest_object).unwrap())
                .unwrap()
                .into_bytes();
        let source_manifest = SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".into(),
            }],
            vec![SourceClosureFile {
                root: "source".into(),
                path: "run.py".into(),
                blob_hash: lillux::sha256_hex(b"run"),
                size: 3,
                mode: SourceFileMode::ReadOnly,
            }],
        )
        .unwrap();
        let schema_body = "kind: kind\n".to_owned();
        let source_binding = EffectiveSourceBinding {
            schema: EFFECTIVE_SOURCE_BINDING_SCHEMA,
            kind: EFFECTIVE_SOURCE_BINDING_KIND.into(),
            owner: SourceOwnerIdentity {
                canonical_ref: "tool:test/run".into(),
                item_kind: "tool".into(),
                source_space: SourceSpaceIdentity::Project,
                source_root: SourceRootIdentity::Project,
                root_source_content_digest: "a".repeat(64),
                root_raw_content_digest: "b".repeat(64),
                signer_fingerprint: "c".repeat(64),
                logical_item_key: "test/run".into(),
            },
            kind_ceiling: SignedKindSourceCeiling {
                schema_ref: "kind:tool".into(),
                source_content_digest: "d".repeat(64),
                raw_content_digest: lillux::signature::content_hash(&schema_body),
                signer_fingerprint: "f".repeat(64),
                signature_header: "signed".into(),
                schema_body,
                schema_document: serde_json::json!({"kind": "kind", "location": {"directory": "tools"}}),
                normalized_declaration: serde_json::json!({
                    "derived": SOURCE_CLOSURE_DERIVED_KEY,
                    "location": {"type": "item_namespace"}, "testimony": "owner_signed_files",
                    "max_files": 8, "max_total_bytes": 1024, "max_file_bytes": 512, "max_depth": 8,
                }),
                root_kind_format: serde_json::json!({"extensions": ["yaml"]}),
                root_signature_envelope: serde_json::json!({"style": "header"}),
            },
            content_manifest_hash: source_manifest.digest().unwrap(),
            testimony: SourceTestimonyProof::OwnerSignedFiles {
                signer_fingerprint: "c".repeat(64),
                file_count: 1,
                entries_digest: "2".repeat(64),
            },
            execution_policy: SourceExecutionPolicyIdentity::Executor {
                declarer_ref: "tool:ryeos/core/runtimes/python/function".into(),
                signer_fingerprint: "3".repeat(64),
                source_content_digest: "4".repeat(64),
                raw_content_digest: "5".repeat(64),
                policy_digest: "6".repeat(64),
                chain_digest: "7".repeat(64),
            },
            logical_binding: SourceLogicalBinding::Tool {
                loader_roots: vec![SourceLoaderRoot::ItemDirectory],
                root_entry: "run.py".into(),
            },
        };
        let source_binding_bytes =
            lillux::canonical_json(&serde_json::to_value(&source_binding).unwrap())
                .unwrap()
                .into_bytes();
        let source_manifest_bytes =
            lillux::canonical_json(&serde_json::to_value(&source_manifest).unwrap())
                .unwrap()
                .into_bytes();
        let verified_source =
            ryeos_state::source_verification::VerifiedAdmittedSourceRecords::from_canonical_bytes(
                &lillux::sha256_hex(&source_binding_bytes),
                &lillux::sha256_hex(&source_manifest_bytes),
                &source_binding_bytes,
                &source_manifest_bytes,
            )
            .unwrap();
        for (path, mode, bytes) in [
            ("bootstrap", 0o600, &bootstrap),
            ("input-00", 0o644, &config),
            ("launcher", 0o755, &launcher),
            ("supervisor", 0o755, &supervisor),
        ] {
            entries.push(GuestStagingEntry::RegularFile {
                path: path.into(),
                mode,
                bytes: bytes.len() as u64,
                sha256: lillux::sha256_hex(bytes),
            });
            files.insert(path.into(), bytes.clone());
        }
        entries.extend([
            GuestStagingEntry::Directory {
                path: "input-01".into(),
                mode: 0o700,
            },
            GuestStagingEntry::Directory {
                path: "input-01/bin".into(),
                mode: 0o755,
            },
            GuestStagingEntry::RegularFile {
                path: "input-01/bin/tool".into(),
                mode: 0o755,
                bytes: 4,
                sha256: lillux::sha256_hex(b"tool"),
            },
            GuestStagingEntry::Symlink {
                path: "input-01/current".into(),
                target: "bin/tool".into(),
            },
            GuestStagingEntry::RegularFile {
                path: "record-00".into(),
                mode: 0o600,
                bytes: product_manifest.len() as u64,
                sha256: lillux::sha256_hex(&product_manifest),
            },
            GuestStagingEntry::Directory {
                path: "input-02".into(),
                mode: 0o700,
            },
            GuestStagingEntry::RegularFile {
                path: "input-02/run.py".into(),
                mode: 0o644,
                bytes: 3,
                sha256: lillux::sha256_hex(b"run"),
            },
        ]);
        files.insert("input-01/bin/tool".into(), b"tool".to_vec());
        files.insert("record-00".into(), product_manifest.clone());
        files.insert("input-02/run.py".into(), b"run".to_vec());
        for (path, bytes) in [
            ("record-01", &source_binding_bytes),
            ("record-02", &source_manifest_bytes),
        ] {
            entries.push(GuestStagingEntry::RegularFile {
                path: path.into(),
                mode: 0o600,
                bytes: bytes.len() as u64,
                sha256: lillux::sha256_hex(bytes),
            });
            files.insert(path.into(), bytes.clone());
        }
        entries.sort_by(|left, right| left.path().cmp(right.path()));
        let inputs = ExternalGuestInputProjection {
            schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 56,
                snapshot_hash: measurement.snapshot_hash.clone(),
                closure_digest: measurement.closure_digest.clone(),
                object_count: measurement.object_count,
                blob_count: measurement.blob_count,
                total_bytes: measurement.total_bytes,
            },
            workspace_outputs: None,
            inputs: vec![
                GuestMountInput {
                    role: GuestMountRole::Configuration,
                    authority_id: "config".into(),
                    descriptor: 64,
                    destination: "/runtime/config".into(),
                    kind: GuestMountKind::RegularFile,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: Some(0o644),
                    content_authority: GuestMountContentAuthority::RawFile {
                        sha256: lillux::sha256_hex(&config),
                    },
                    bytes: config.len() as u64,
                },
                GuestMountInput {
                    role: GuestMountRole::Product,
                    authority_id: "product".into(),
                    descriptor: 65,
                    destination: "/runtime/product".into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: None,
                    content_authority: GuestMountContentAuthority::ProductManifest {
                        manifest_kind: GuestProductManifestKind::Content,
                        manifest_hash: lillux::sha256_hex(&product_manifest),
                        manifest_descriptor: 66,
                        manifest_bytes: product_manifest.len() as u64,
                    },
                    bytes: 4,
                },
                GuestMountInput {
                    role: GuestMountRole::Source,
                    authority_id: verified_source.binding_hash().into(),
                    descriptor: 67,
                    destination: verified_source
                        .runtime_destination()
                        .to_str()
                        .unwrap()
                        .into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: None,
                    content_authority: GuestMountContentAuthority::SourceClosure {
                        binding_descriptor: 68,
                        binding_hash: lillux::sha256_hex(&source_binding_bytes),
                        binding_bytes: source_binding_bytes.len() as u64,
                        manifest_descriptor: 69,
                        manifest_hash: lillux::sha256_hex(&source_manifest_bytes),
                        manifest_bytes: source_manifest_bytes.len() as u64,
                    },
                    bytes: source_manifest.totals.total_bytes,
                },
                GuestMountInput {
                    role: GuestMountRole::PrivateScratch,
                    authority_id: "scratch-TMPDIR".into(),
                    descriptor: 70,
                    destination: "/ryeos/runtime-views/TMPDIR".into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::PrivateWritable,
                    normalized_mode: None,
                    content_authority: GuestMountContentAuthority::PrivateScratch {
                        binding_hash: "a".repeat(64),
                    },
                    bytes: 0,
                },
            ],
            executable_search: Vec::new(),
            environment: BTreeMap::new(),
        };
        bootstrap = valid_supervisor_bootstrap(&inputs, &lillux::sha256_hex(&launcher));
        files.insert("bootstrap".into(), bootstrap.clone());
        let bootstrap_entry = entries
            .iter_mut()
            .find(|entry| entry.path() == "bootstrap")
            .unwrap();
        let GuestStagingEntry::RegularFile {
            bytes, sha256, ..
        } = bootstrap_entry else {
            panic!("bootstrap fixture lost regular-file inventory");
        };
        *bytes = bootstrap.len() as u64;
        *sha256 = lillux::sha256_hex(&bootstrap);
        let manifest = GuestStagingPackageManifest {
            schema: GUEST_STAGING_PACKAGE_SCHEMA,
            activation_request_digest: "c".repeat(64),
            guest_input_identity: inputs.identity_digest().unwrap(),
            bootstrap_sha256: lillux::sha256_hex(&bootstrap),
            supervisor_sha256: lillux::sha256_hex(&supervisor),
            launcher_sha256: lillux::sha256_hex(&launcher),
            total_regular_bytes: files.values().map(|bytes| bytes.len() as u64).sum(),
            entries,
        };
        let expected = GuestStagingExpected {
            inputs: &inputs,
            activation_request_digest: &manifest.activation_request_digest,
            bootstrap_sha256: &manifest.bootstrap_sha256,
            supervisor_sha256: &manifest.supervisor_sha256,
            launcher_sha256: &manifest.launcher_sha256,
            maximum_regular_bytes: manifest.total_regular_bytes,
            maximum_framed_bytes: manifest.framed_bytes().unwrap() + 128,
        };
        let mut stream =
            GuestStagingStreamWriter::new(Vec::new(), manifest.clone(), &expected).unwrap();
        while let Some(entry) = stream.next_file() {
            let mut source = files.get(entry.path()).unwrap().as_slice();
            stream.copy_next_file(&mut source).unwrap();
        }
        let bytes = stream.finish().unwrap();
        let private = tempfile::tempdir().unwrap();
        let parent = lillux::PinnedDirectory::open(private.path())
            .unwrap()
            .unwrap();
        parent.tighten_owner_private_directory().unwrap();
        for unsafe_target in ["../../escape", "/etc/passwd", "current"] {
            let mut invalid = manifest.clone();
            for entry in &mut invalid.entries {
                if let GuestStagingEntry::Symlink { target, .. } = entry {
                    *target = unsafe_target.into();
                }
            }
            let mut invalid_stream =
                GuestStagingStreamWriter::new(Vec::new(), invalid, &expected).unwrap();
            while let Some(entry) = invalid_stream.next_file() {
                let mut source = files.get(entry.path()).unwrap().as_slice();
                invalid_stream.copy_next_file(&mut source).unwrap();
            }
            let invalid_bytes = invalid_stream.finish().unwrap();
            let error = stage_guest_package(invalid_bytes.as_slice(), &parent, &expected)
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("symlink"));
            assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
        }
        let mut truncated = bytes.clone();
        truncated.pop();
        let mut tampered = bytes.clone();
        *tampered.last_mut().unwrap() ^= 1;
        let mut trailing = bytes.clone();
        trailing.push(0);
        for invalid in [truncated, tampered, trailing] {
            assert!(stage_guest_package(invalid.as_slice(), &parent, &expected).is_err());
            assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
        }
        let upload_dir = tempfile::tempdir().unwrap();
        let upload_parent = lillux::PinnedDirectory::open(upload_dir.path())
            .unwrap()
            .unwrap();
        upload_parent.tighten_owner_private_directory().unwrap();
        let mut upload_file = upload_parent
            .open_regular_create(OsStr::new("payload"), true, true, 0o600)
            .unwrap();
        upload_file.write_all(&bytes).unwrap();
        upload_file.sync_all().unwrap();
        drop(upload_file);
        let upload = upload_parent
            .open_pinned_regular(OsStr::new("payload"), false)
            .unwrap()
            .unwrap();
        assert!(
            stage_uploaded_guest_package(
                &upload,
                bytes.len() as u64,
                &"0".repeat(64),
                &parent,
                &expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
        let ticket = GuestImportTicket {
            schema: ryeos_external_execution_contract::staging_package::GUEST_IMPORT_TICKET_SCHEMA,
            binding_hash: "1".repeat(64),
            allocation_request_digest: "2".repeat(64),
            occurrence_id: "occ-staging-test".into(),
            activation_request_digest: manifest.activation_request_digest.clone(),
            guest_input_identity: manifest.guest_input_identity.clone(),
            payload_sha256: lillux::sha256_hex(&bytes),
            manifest_sha256: manifest.identity_digest().unwrap(),
            framed_bytes: bytes.len() as u64,
            regular_bytes: manifest.total_regular_bytes,
            bootstrap_sha256: manifest.bootstrap_sha256.clone(),
            supervisor_sha256: manifest.supervisor_sha256.clone(),
            launcher_sha256: manifest.launcher_sha256.clone(),
            maximum_regular_bytes: expected.maximum_regular_bytes,
            maximum_framed_bytes: expected.maximum_framed_bytes,
        };
        let context = GuestImportContext {
            binding_hash: &ticket.binding_hash,
            allocation_request_digest: &ticket.allocation_request_digest,
            occurrence_id: &ticket.occurrence_id,
            activation_request_digest: &ticket.activation_request_digest,
        };
        let imported = stage_ticketed_uploaded_guest_package(
            &upload,
            &parent,
            &ticket,
            &context,
            &inputs,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
        )
        .unwrap();
        assert_eq!(imported.manifest(), &manifest);
        assert_eq!(imported.ticket(), &ticket);
        let stage_identity = imported.stage_identity().unwrap();
        assert_eq!(
            stage_identity.directory_identity(),
            imported.root().identity().unwrap()
        );
        assert_eq!(stage_identity.manifest_sha256(), ticket.manifest_sha256);
        assert!(stage_identity.name().starts_with("guest-input"));
        let retained_stage: GuestStageIdentity =
            serde_json::from_slice(&serde_json::to_vec(&stage_identity).unwrap()).unwrap();
        assert_eq!(retained_stage, stage_identity);
        assert_eq!(
            retained_stage
                .resolve_under(&parent)
                .unwrap()
                .identity()
                .unwrap(),
            imported.root().identity().unwrap()
        );
        let mut wrong_stage_identity = stage_identity.clone();
        wrong_stage_identity.name = "guest-input.other".into();
        assert!(wrong_stage_identity.resolve_under(&parent).is_err());
        wrong_stage_identity = stage_identity.clone();
        wrong_stage_identity.directory_identity = parent.identity().unwrap();
        assert!(wrong_stage_identity.resolve_under(&parent).is_err());
        wrong_stage_identity = stage_identity.clone();
        wrong_stage_identity.name = "guest-input./escape".into();
        assert!(wrong_stage_identity.resolve_under(&parent).is_err());
        wrong_stage_identity = stage_identity.clone();
        wrong_stage_identity.schema = 2;
        assert!(wrong_stage_identity.resolve_under(&parent).is_err());
        wrong_stage_identity = stage_identity.clone();
        wrong_stage_identity.manifest_sha256 = "not-a-digest".into();
        assert!(wrong_stage_identity.resolve_under(&parent).is_err());
        let contested =
            recover_ticketed_guest_import(&parent, &stage_identity, &ticket, &context, &inputs)
                .err()
                .expect("recovery must not acquire a generation with a live importer");
        assert!(format!("{contested:#}").contains("already has a live owner"));
        drop(imported);
        let imported =
            recover_ticketed_guest_import(&parent, &stage_identity, &ticket, &context, &inputs)
                .unwrap();
        assert_eq!(imported.stage_identity().unwrap(), stage_identity);
        assert_eq!(imported.manifest(), &manifest);
        let runtime_dir = tempfile::tempdir().unwrap();
        let runtime = lillux::PinnedDirectory::open(runtime_dir.path())
            .unwrap()
            .unwrap();
        runtime.tighten_owner_private_directory().unwrap();
        imported
            .install_base_into(&context, &inputs, &runtime)
            .unwrap();
        let installed_objects = runtime
            .open_child_directory(OsStr::new("objects"))
            .unwrap()
            .unwrap();
        assert_eq!(
            ryeos_project_capture::inspect_project_snapshot_transfer(
                &installed_objects,
                &inputs.base_snapshot.snapshot_hash,
            )
            .unwrap(),
            measurement
        );
        assert!(
            imported
                .install_base_into(&context, &inputs, &runtime)
                .is_err(),
            "an installed runtime cannot be overwritten"
        );
        let mut wrong_ticket = ticket.clone();
        wrong_ticket.manifest_sha256 = "0".repeat(64);
        assert!(
            recover_ticketed_guest_import(
                &parent,
                &stage_identity,
                &wrong_ticket,
                &context,
                &inputs,
            )
            .is_err()
        );
        imported.recheck_for_adoption(&context, &inputs).unwrap();
        assert!(
            imported
                .recheck_for_adoption(
                    &GuestImportContext {
                        binding_hash: context.binding_hash,
                        allocation_request_digest: context.allocation_request_digest,
                        occurrence_id: "occ-other",
                        activation_request_digest: context.activation_request_digest,
                    },
                    &inputs,
                )
                .is_err()
        );
        let mut changed = imported
            .root()
            .open_regular_create(OsStr::new("input-00"), true, false, 0o600)
            .unwrap();
        changed.write_all(b"bad").unwrap();
        changed.sync_all().unwrap();
        drop(changed);
        assert!(imported.recheck_for_adoption(&context, &inputs).is_err());
        let rejected_runtime_dir = tempfile::tempdir().unwrap();
        let rejected_runtime = lillux::PinnedDirectory::open(rejected_runtime_dir.path())
            .unwrap()
            .unwrap();
        rejected_runtime.tighten_owner_private_directory().unwrap();
        assert!(
            imported
                .install_base_into(&context, &inputs, &rejected_runtime)
                .is_err(),
            "changed admitted input must refuse base installation"
        );
        assert!(
            rejected_runtime
                .entries_no_follow_bounded(0)
                .unwrap()
                .is_empty()
        );
        let mut restored = imported
            .root()
            .open_regular_create(OsStr::new("input-00"), true, false, 0o600)
            .unwrap();
        restored.write_all(&config).unwrap();
        restored.sync_all().unwrap();
        drop(restored);
        imported.recheck_for_adoption(&context, &inputs).unwrap();
        let mut changed_bootstrap = imported
            .root()
            .open_regular_create(OsStr::new("bootstrap"), true, false, 0o600)
            .unwrap();
        changed_bootstrap.write_all(b"evil").unwrap();
        changed_bootstrap.sync_all().unwrap();
        drop(changed_bootstrap);
        assert!(imported.recheck_for_adoption(&context, &inputs).is_err());
        let mut restored_bootstrap = imported
            .root()
            .open_regular_create(OsStr::new("bootstrap"), true, false, 0o600)
            .unwrap();
        restored_bootstrap.write_all(&bootstrap).unwrap();
        restored_bootstrap.sync_all().unwrap();
        drop(restored_bootstrap);
        imported.recheck_for_adoption(&context, &inputs).unwrap();
        imported
            .root()
            .open_pinned_regular(OsStr::new("supervisor"), false)
            .unwrap()
            .unwrap()
            .set_mode(0o600)
            .unwrap();
        assert!(imported.recheck_for_adoption(&context, &inputs).is_err());
        imported
            .root()
            .open_pinned_regular(OsStr::new("supervisor"), false)
            .unwrap()
            .unwrap()
            .set_mode(0o755)
            .unwrap();
        imported.recheck_for_adoption(&context, &inputs).unwrap();
        let sidecar = imported
            .root()
            .open_pinned_regular(OsStr::new(STAGE_MANIFEST_FILE), false)
            .unwrap()
            .unwrap();
        sidecar.set_mode(0o644).unwrap();
        drop(imported);
        assert!(
            recover_ticketed_guest_import(&parent, &stage_identity, &ticket, &context, &inputs)
                .is_err()
        );
        sidecar.set_mode(0o600).unwrap();
        drop(sidecar);
        let imported =
            recover_ticketed_guest_import(&parent, &stage_identity, &ticket, &context, &inputs)
                .unwrap();
        let mut changed_sidecar = imported
            .root()
            .open_regular_create(OsStr::new(STAGE_MANIFEST_FILE), true, false, 0o600)
            .unwrap();
        changed_sidecar.write_all(b"bad!").unwrap();
        changed_sidecar.sync_all().unwrap();
        drop(changed_sidecar);
        drop(imported);
        assert!(
            recover_ticketed_guest_import(&parent, &stage_identity, &ticket, &context, &inputs,)
                .is_err()
        );
        let root = stage_identity.resolve_under(&parent).unwrap();
        let mut restored_sidecar = root
            .open_regular_create(OsStr::new(STAGE_MANIFEST_FILE), true, false, 0o600)
            .unwrap();
        restored_sidecar
            .write_all(&ryeos_external_execution_contract::canonical_json(&manifest).unwrap())
            .unwrap();
        restored_sidecar.sync_all().unwrap();
        drop(restored_sidecar);
        let imported =
            recover_ticketed_guest_import(&parent, &stage_identity, &ticket, &context, &inputs)
                .unwrap();
        imported.discard().unwrap();
        let overlapping_dir = tempfile::tempdir().unwrap();
        let overlapping_occurrence = lillux::PinnedDirectory::open(overlapping_dir.path())
            .unwrap()
            .unwrap();
        overlapping_occurrence.tighten_owner_private_directory().unwrap();
        let overlapping_owner = crate::guest_installation::GuestOccurrenceOwner::begin(
            &overlapping_occurrence,
            &ticket,
            &context,
            &inputs,
        )
        .unwrap();
        assert!(
            overlapping_owner
                .stage_uploaded_with_source_root_for_test(
                    &upload,
                    &overlapping_occurrence,
                    &context,
                    &inputs,
                    lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
                )
                .is_err(),
            "durable occurrence root cannot double as private source"
        );
        assert_eq!(
            crate::guest_installation::recover_guest_occurrence(
                &overlapping_occurrence,
                &ticket,
                &context,
                &inputs,
            )
            .unwrap()
            .phase(),
            &crate::guest_installation::GuestOccurrenceRecoveryPhase::ImportUncertain
        );
        let occurrence_dir = tempfile::tempdir().unwrap();
        let occurrence = lillux::PinnedDirectory::open(occurrence_dir.path())
            .unwrap()
            .unwrap();
        occurrence.tighten_owner_private_directory().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let source_root = lillux::PinnedDirectory::open(source_dir.path())
            .unwrap()
            .unwrap();
        source_root.tighten_owner_private_directory().unwrap();
        let owner = crate::guest_installation::GuestOccurrenceOwner::begin(
            &occurrence,
            &ticket,
            &context,
            &inputs,
        )
        .unwrap();
        assert!(
            crate::guest_installation::GuestOccurrenceOwner::begin(
                &occurrence,
                &ticket,
                &context,
                &inputs,
            )
            .is_err(),
            "the exact occurrence cannot reserve a second import owner"
        );
        let staged_occurrence = owner
            .stage_uploaded_with_source_root_for_test(
                &upload,
                &source_root,
                &context,
                &inputs,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .unwrap();
        let installed_occurrence = staged_occurrence
            .install_base_once(&context, &inputs)
            .unwrap();
        let owned_runtime = occurrence
            .open_child_directory(OsStr::new("candidate-runtime"))
            .unwrap()
            .unwrap();
        let adoption = installed_occurrence
            .recheck_for_adoption(&context, &inputs)
            .unwrap();
        assert_eq!(adoption.occurrence_id, context.occurrence_id);
        assert_eq!(
            adoption.base_snapshot_hash,
            inputs.base_snapshot.snapshot_hash
        );
        assert_eq!(adoption.base_closure_digest, measurement.closure_digest);
        assert_eq!(
            adoption.candidate_runtime,
            owned_runtime.identity().unwrap()
        );
        assert_eq!(
            adoption.children.objects,
            owned_runtime
                .open_child_directory(OsStr::new("objects"))
                .unwrap()
                .unwrap()
                .identity()
                .unwrap()
        );
        owned_runtime.set_mode(0o777).unwrap();
        assert!(
            installed_occurrence
                .recheck_for_adoption(&context, &inputs)
                .is_err(),
            "installed runtime mode drift must refuse pre-adoption"
        );
        owned_runtime.set_mode(0o700).unwrap();
        let refs = owned_runtime
            .open_child_directory(OsStr::new("refs"))
            .unwrap()
            .unwrap();
        let ambient_name = OsStr::new("ambient-before-adoption");
        let ambient = refs
            .open_regular_create(ambient_name, true, true, 0o600)
            .unwrap();
        assert!(
            installed_occurrence
                .recheck_for_adoption(&context, &inputs)
                .is_err(),
            "ambient refs must refuse pre-adoption"
        );
        refs.remove_if_same(ambient_name, &ambient).unwrap();
        installed_occurrence
            .recheck_for_adoption(&context, &inputs)
            .unwrap();
        let recovery = owned_runtime
            .open_child_directory(OsStr::new("recovery"))
            .unwrap()
            .unwrap();
        let unexpected_name = OsStr::new("unexpected-before-adoption");
        let unexpected = recovery
            .open_regular_create(unexpected_name, true, true, 0o600)
            .unwrap();
        assert!(
            installed_occurrence
                .recheck_for_adoption(&context, &inputs)
                .is_err(),
            "ambient recovery content must refuse pre-adoption"
        );
        recovery
            .remove_if_same(unexpected_name, &unexpected)
            .unwrap();
        let mut lock = recovery
            .open_regular(OsStr::new("cas-mutation.lock"), true)
            .unwrap()
            .unwrap();
        lock.write_all(b"changed").unwrap();
        lock.sync_all().unwrap();
        assert!(
            installed_occurrence
                .recheck_for_adoption(&context, &inputs)
                .is_err(),
            "changed mutation lock must refuse pre-adoption"
        );
        lock.set_len(0).unwrap();
        lock.sync_all().unwrap();
        installed_occurrence
            .recheck_for_adoption(&context, &inputs)
            .unwrap();
        assert_eq!(
            ryeos_project_capture::inspect_project_snapshot_transfer(
                &owned_runtime
                    .open_child_directory(OsStr::new("objects"))
                    .unwrap()
                    .unwrap(),
                &inputs.base_snapshot.snapshot_hash,
            )
            .unwrap(),
            measurement
        );
        let prepared = installed_occurrence
            .prepare_content_for_adoption(&context, &inputs)
            .unwrap();
        assert_eq!(prepared.observation.occurrence_id, context.occurrence_id);
        assert_eq!(prepared.handles.runtime_mounts.len(), inputs.inputs.len());
        assert_eq!(
            prepared.handles.content_records.len(),
            inputs.record_descriptors().count()
        );
        assert!(prepared.handles.runtime_mounts[..3].iter().all(Option::is_some));
        assert!(prepared.handles.runtime_mounts[3].is_none());
        assert!(prepared.handles.workspace_outputs.is_none());
        let private = prepared.create_private_scratch_once(&context, &inputs).unwrap();
        assert_eq!(private.observation.scratch.len(), 1);
        assert_eq!(private.observation.scratch[0].input_index, 3);
        assert!(private.content.handles.runtime_mounts.iter().all(Option::is_some));
        let scratch = private
            .private_parent
            .open_child_directory(OsStr::new("guest-scratch-03"))
            .unwrap()
            .unwrap();
        assert_eq!(scratch.identity().unwrap(), private.observation.scratch[0].directory);
        assert!(scratch.entries_no_follow_bounded(0).unwrap().is_empty());
        assert!(
            private
                .private_parent
                .create_child(OsStr::new("guest-scratch-03"), 0o700)
                .is_err(),
            "a second scratch directory must not replace the first identity"
        );
        let request = lillux::SubprocessRequest {
            cmd: "/bin/true".into(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 1.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        let (bound, plan) = private
            .bind_content_to_supervisor_request(&context, &inputs, request)
            .unwrap();
        assert_eq!(plan.execution_inputs.identity_digest().unwrap(), inputs.identity_digest().unwrap());
        assert_eq!(bound.inherited_fd_mappings.len(), 2 + inputs.inputs.len() + inputs.record_descriptors().count());
        let mut expected_targets = vec![
            ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_CANDIDATE_RUNTIME_FD,
            ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_PRIVATE_PARENT_FD,
        ];
        expected_targets.extend(&plan.runtime_mount_descriptors);
        expected_targets.extend(&plan.content_record_descriptors);
        assert_eq!(
            bound.inherited_fd_mappings.iter().map(lillux::InheritedDescriptorMapping::target_descriptor).collect::<Vec<_>>(),
            expected_targets
        );
        for (mapping, handle) in bound.inherited_fd_mappings[2..2 + inputs.inputs.len()]
            .iter()
            .zip(&private.content.handles.runtime_mounts)
        {
            assert_eq!(mapping.source_descriptor().unwrap(), handle.as_ref().unwrap().inherited_descriptor().unwrap());
        }
        for (mapping, handle) in bound.inherited_fd_mappings[2 + inputs.inputs.len()..]
            .iter()
            .zip(&private.content.handles.content_records)
        {
            assert_eq!(mapping.source_descriptor().unwrap(), handle.inherited_descriptor().unwrap());
        }
        assert!(!expected_targets.contains(&ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_CONSUMED_BASE_SNAPSHOT_FD));
        assert!(
            private
                .bind_content_to_supervisor_request(&context, &inputs, bound)
                .is_err(),
            "prebound descriptors must not enter guest content binding"
        );
        let ambient = scratch.create_child(OsStr::new("ambient"), 0o700).unwrap();
        let second_request = lillux::SubprocessRequest {
            cmd: "/bin/true".into(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 1.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        };
        assert!(
            private
                .bind_content_to_supervisor_request(&context, &inputs, second_request)
                .is_err(),
            "ambient private scratch content must refuse descriptor binding"
        );
        assert!(scratch.remove_empty_child_if_same(OsStr::new("ambient"), &ambient).unwrap());
        let artifacts = private.prepare_launch_artifacts_once(&context, &inputs).unwrap();
        assert_eq!(artifacts.observation.schema, 1);
        assert_eq!(artifacts.observation.state_root, artifacts.state_root.identity().unwrap());
        assert_eq!(artifacts.bootstrap_source.regular_file_observation().unwrap().size(), bootstrap.len() as u64);
        assert_eq!(artifacts.supervisor.regular_file_observation().unwrap().size(), supervisor.len() as u64);
        assert_eq!(artifacts.launcher.regular_file_observation().unwrap().size(), launcher.len() as u64);
        assert!(artifacts.state_root.entries_no_follow_bounded(0).unwrap().is_empty());
        assert!(artifacts.private.private_parent.entries_no_follow_bounded(1).is_ok());
        let wrong_context = GuestImportContext {
            binding_hash: context.binding_hash,
            allocation_request_digest: context.allocation_request_digest,
            occurrence_id: "different-occurrence",
            activation_request_digest: context.activation_request_digest,
        };
        assert!(artifacts.seal_supervisor_bootstrap(&wrong_context, &inputs).is_err());
        let sealed = artifacts.seal_supervisor_bootstrap(&context, &inputs).unwrap();
        assert_eq!(
            lillux::read_sealed_inherited_descriptor(
                sealed.inherited_descriptor().unwrap(),
                ryeos_state::external_execution::transport::MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
            )
            .unwrap(),
            bootstrap
        );
        let prepared = artifacts
            .prepare_supervisor_request(&context, &inputs, 10.0)
            .unwrap();
        let (request, plan, bootstrap_sha256, supervisor_source) = prepared.inspect_for_test();
        let mut actual_targets = request
            .inherited_fd_mappings
            .iter()
            .map(lillux::InheritedDescriptorMapping::target_descriptor)
            .collect::<Vec<_>>();
        actual_targets.sort_unstable();
        let mut expected_targets = plan.inherited_descriptors.clone();
        expected_targets.sort_unstable();
        assert_eq!(actual_targets, expected_targets);
        assert!(!actual_targets.contains(&ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_CONSUMED_BASE_SNAPSHOT_FD));
        assert_eq!(request.cmd, "/proc/self/fd/57");
        assert_eq!(bootstrap_sha256, lillux::sha256_hex(&bootstrap));
        let sealed_mapping = request.inherited_fd_mappings.iter().find(|mapping| {
            mapping.target_descriptor()
                == ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_BOOTSTRAP_FD
        }).unwrap();
        assert_eq!(
            lillux::read_sealed_inherited_descriptor(
                sealed_mapping.source_descriptor().unwrap(),
                ryeos_state::external_execution::transport::MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
            ).unwrap(),
            bootstrap
        );
        let executable = request.inherited_fd_mappings.iter().find(|mapping| {
            mapping.target_descriptor()
                == ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_EXECUTABLE_FD
        }).unwrap();
        assert_eq!(
            executable.source_descriptor().unwrap(),
            supervisor_source.inherited_descriptor().unwrap()
        );
        let synthetic_launch_intent = prepared
            .planned_outer_launch_intent_bytes_for_test(&inputs)
            .unwrap();
        let launch_value: serde_json::Value =
            serde_json::from_slice(&synthetic_launch_intent).unwrap();
        assert_eq!(
            launch_value["request_timeout_bits"].as_u64(),
            Some(10.0_f64.to_bits())
        );
        drop(prepared);
        let recovered_occurrence = crate::guest_installation::recover_guest_occurrence(
            &occurrence,
            &ticket,
            &context,
            &inputs,
        )
        .unwrap();
        assert_eq!(
            recovered_occurrence.phase(),
            &crate::guest_installation::GuestOccurrenceRecoveryPhase::InstallationUncertain
        );
        drop(recovered_occurrence);
        let owner_root = occurrence
            .open_child_directory(OsStr::new("guest-import-owner"))
            .unwrap()
            .unwrap();
        let intent_value: serde_json::Value = serde_json::from_slice(
            &owner_root
                .open_pinned_regular(OsStr::new("guest-base-install-intent.json"), false)
                .unwrap()
                .unwrap()
                .read_bounded(8 * 1024)
                .unwrap(),
        )
        .unwrap();
        assert!(
            owner_root
                .open_child_directory(OsStr::new(intent_value["stage"]["name"].as_str().unwrap()))
                .unwrap()
                .is_none(),
            "private source stage must not live under the durable owner journal"
        );
        let installed_stage = source_root
            .open_child_directory(OsStr::new(intent_value["stage"]["name"].as_str().unwrap()))
            .unwrap()
            .unwrap();
        let install_marker = installed_stage
            .open_pinned_regular(OsStr::new("guest-base-install-owner.json"), false)
            .unwrap()
            .unwrap();
        install_marker.set_mode(0o644).unwrap();
        assert_eq!(
            crate::guest_installation::recover_guest_occurrence(
                &occurrence, &ticket, &context, &inputs,
            )
            .unwrap()
            .phase(),
            &crate::guest_installation::GuestOccurrenceRecoveryPhase::InstallationUncertain,
            "recovery cannot reopen ephemeral source or grant adoption"
        );
        assert!(
            crate::guest_installation::GuestOccurrenceOwner::begin(
                &occurrence, &ticket, &context, &inputs,
            )
            .is_err(),
            "weakened ephemeral marker cannot permit a second import owner"
        );
        install_marker.set_mode(0o600).unwrap();
        owned_runtime.set_mode(0o777).unwrap();
        assert!(
            crate::guest_installation::recover_guest_occurrence(
                &occurrence,
                &ticket,
                &context,
                &inputs,
            )
            .is_err(),
            "recovery must reject a mode-weakened installed runtime"
        );
        owned_runtime.set_mode(0o700).unwrap();
        assert!(
            crate::guest_installation::GuestOccurrenceOwner::begin(
                &occurrence,
                &ticket,
                &context,
                &inputs,
            )
            .is_err(),
            "an installed occurrence cannot be restaged after owner exit"
        );
        let (launch_file, launch_sha256) =
            crate::guest_installation::create_launch_intent_record_for_test(
                &owner_root,
                &synthetic_launch_intent,
            )
            .unwrap();
        assert_eq!(launch_sha256, lillux::sha256_hex(&synthetic_launch_intent));
        assert!(crate::guest_installation::create_launch_intent_record_for_test(
            &owner_root,
            &synthetic_launch_intent,
        )
        .is_err());
        assert_eq!(
            launch_file,
            lillux::pinned_regular_file_identity(
                &owner_root
                    .open_pinned_regular(OsStr::new("guest-supervisor-launch-intent.json"), false)
                    .unwrap()
                    .unwrap()
                    .try_clone_descriptor()
                    .unwrap(),
            )
            .unwrap()
        );
        // A launched supervisor may legitimately have changed its runtime.
        // Recovery must quarantine the launch rather than demand pristine
        // prelaunch state or make another launch possible.
        owned_runtime
            .open_child_directory(OsStr::new("refs"))
            .unwrap()
            .unwrap()
            .create_child(OsStr::new("post-launch-mutation"), 0o700)
            .unwrap();
        let recovered_launch = crate::guest_installation::recover_guest_occurrence(
            &occurrence,
            &ticket,
            &context,
            &inputs,
        )
        .unwrap();
        assert_eq!(
            recovered_launch.phase(),
            &crate::guest_installation::GuestOccurrenceRecoveryPhase::LaunchUncertain
        );
        drop(recovered_launch);
        let launch_file = owner_root
            .open_pinned_regular(OsStr::new("guest-supervisor-launch-intent.json"), false)
            .unwrap()
            .unwrap();
        launch_file.set_mode(0o644).unwrap();
        assert!(
            crate::guest_installation::recover_guest_occurrence(
                &occurrence,
                &ticket,
                &context,
                &inputs,
            )
            .is_err(),
            "recovery must reject a mode-weakened outer launch intent"
        );
        launch_file.set_mode(0o600).unwrap();
        for child in ["candidate-private", "supervisor-state"] {
            let original = occurrence_dir.path().join(child);
            let detached = occurrence_dir.path().join(format!("{child}-detached"));
            std::fs::rename(&original, &detached).unwrap();
            assert!(
                crate::guest_installation::recover_guest_occurrence(
                    &occurrence,
                    &ticket,
                    &context,
                    &inputs,
                )
                .is_err(),
                "recovery must reject a missing fixed {child} child"
            );
            occurrence.create_child(OsStr::new(child), 0o700).unwrap();
            assert!(
                crate::guest_installation::recover_guest_occurrence(
                    &occurrence,
                    &ticket,
                    &context,
                    &inputs,
                )
                .is_err(),
                "recovery must reject a replaced fixed {child} child"
            );
            std::fs::remove_dir(&original).unwrap();
            std::fs::rename(&detached, &original).unwrap();
        }
        let scratch_path = occurrence_dir
            .path()
            .join("candidate-private/guest-scratch-03");
        let detached_scratch = occurrence_dir
            .path()
            .join("candidate-private/guest-scratch-detached");
        std::fs::rename(&scratch_path, &detached_scratch).unwrap();
        assert!(
            crate::guest_installation::recover_guest_occurrence(
                &occurrence, &ticket, &context, &inputs,
            )
            .is_err(),
            "recovery must reject detached private scratch"
        );
        std::fs::create_dir(&scratch_path).unwrap();
        assert!(
            crate::guest_installation::recover_guest_occurrence(
                &occurrence, &ticket, &context, &inputs,
            )
            .is_err(),
            "recovery must reject replacement private scratch"
        );
        std::fs::remove_dir(&scratch_path).unwrap();
        std::fs::rename(&detached_scratch, &scratch_path).unwrap();
        let crash_occurrence_dir = tempfile::tempdir().unwrap();
        let crash_occurrence = lillux::PinnedDirectory::open(crash_occurrence_dir.path())
            .unwrap()
            .unwrap();
        crash_occurrence.tighten_owner_private_directory().unwrap();
        let crash_owner = crate::guest_installation::GuestOccurrenceOwner::begin(
            &crash_occurrence,
            &ticket,
            &context,
            &inputs,
        )
        .unwrap();
        drop(crash_owner);
        let recovered_crash = crate::guest_installation::recover_guest_occurrence(
            &crash_occurrence,
            &ticket,
            &context,
            &inputs,
        )
        .unwrap();
        assert_eq!(
            recovered_crash.phase(),
            &crate::guest_installation::GuestOccurrenceRecoveryPhase::ImportUncertain
        );
        drop(recovered_crash);
        assert!(
            crate::guest_installation::GuestOccurrenceOwner::begin(
                &crash_occurrence,
                &ticket,
                &context,
                &inputs,
            )
            .is_err(),
            "an ambiguous import cannot restage the same occurrence"
        );
        let staged_occurrence_dir = tempfile::tempdir().unwrap();
        let staged_root = lillux::PinnedDirectory::open(staged_occurrence_dir.path())
            .unwrap()
            .unwrap();
        staged_root.tighten_owner_private_directory().unwrap();
        let staged_source_dir = tempfile::tempdir().unwrap();
        let staged_source_root = lillux::PinnedDirectory::open(staged_source_dir.path())
            .unwrap()
            .unwrap();
        staged_source_root.tighten_owner_private_directory().unwrap();
        let staged_owner = crate::guest_installation::GuestOccurrenceOwner::begin(
            &staged_root,
            &ticket,
            &context,
            &inputs,
        )
        .unwrap();
        let staged_without_install = staged_owner
            .stage_uploaded_with_source_root_for_test(
                &upload,
                &staged_source_root,
                &context,
                &inputs,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .unwrap();
        drop(staged_without_install);
        let recovered_stage = crate::guest_installation::recover_guest_occurrence(
            &staged_root,
            &ticket,
            &context,
            &inputs,
        )
        .unwrap();
        assert_eq!(
            recovered_stage.phase(),
            &crate::guest_installation::GuestOccurrenceRecoveryPhase::ImportUncertain,
            "a finished stage does not grant a post-crash install attempt"
        );
        drop(recovered_stage);
        assert!(
            crate::guest_installation::GuestOccurrenceOwner::begin(
                &staged_root,
                &ticket,
                &context,
                &inputs,
            )
            .is_err(),
            "a completed pre-install stage cannot reserve a second owner"
        );
        // Separate exact occurrences exercise the production one-way cut and
        // its pre-record name-binding refusals, not just the record helper.
        let prepare_launch_occurrence = || {
            let occurrence_dir = tempfile::tempdir().unwrap();
            let occurrence = lillux::PinnedDirectory::open(occurrence_dir.path())
                .unwrap()
                .unwrap();
            occurrence.tighten_owner_private_directory().unwrap();
            let source_dir = tempfile::tempdir().unwrap();
            let source = lillux::PinnedDirectory::open(source_dir.path())
                .unwrap()
                .unwrap();
            source.tighten_owner_private_directory().unwrap();
            let prepared = crate::guest_installation::GuestOccurrenceOwner::begin(
                &occurrence, &ticket, &context, &inputs,
            )
            .unwrap()
            .stage_uploaded_with_source_root_for_test(
                &upload,
                &source,
                &context,
                &inputs,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .unwrap()
            .install_base_once(&context, &inputs)
            .unwrap()
            .prepare_content_for_adoption(&context, &inputs)
            .unwrap()
            .create_private_scratch_once(&context, &inputs)
            .unwrap()
            .prepare_launch_artifacts_once(&context, &inputs)
            .unwrap()
            .prepare_supervisor_request(&context, &inputs, 10.0)
            .unwrap();
            (occurrence_dir, source_dir, occurrence, prepared)
        };
        let (_committed_occurrence_dir, _committed_source_dir, committed_occurrence, prepared) =
            prepare_launch_occurrence();
        let committed = prepared.commit_outer_launch_intent(&context, &inputs).unwrap();
        let committed_owner = committed_occurrence
            .open_child_directory(OsStr::new("guest-import-owner"))
            .unwrap()
            .unwrap();
        let committed_file = committed_owner
            .open_pinned_regular(OsStr::new("guest-supervisor-launch-intent.json"), false)
            .unwrap()
            .unwrap();
        let committed_bytes = committed_file.read_bounded(8 * 1024).unwrap();
        assert_eq!(committed.record_sha256(), lillux::sha256_hex(&committed_bytes));
        assert_eq!(
            committed.record_file(),
            &lillux::pinned_regular_file_identity(
                &committed_file.try_clone_descriptor().unwrap()
            )
            .unwrap()
        );
        assert!(crate::guest_installation::create_launch_intent_record_for_test(
            &committed_owner,
            &committed_bytes,
        )
        .is_err(), "committed launch intent cannot be replaced");
        drop(committed);
        assert_eq!(
            crate::guest_installation::recover_guest_occurrence(
                &committed_occurrence,
                &ticket,
                &context,
                &inputs,
            )
            .unwrap()
            .phase(),
            &crate::guest_installation::GuestOccurrenceRecoveryPhase::LaunchUncertain,
        );
        for child in ["candidate-private", "supervisor-state"] {
            let (occurrence_dir, _source_dir, occurrence, prepared) =
                prepare_launch_occurrence();
            let original = occurrence_dir.path().join(child);
            std::fs::rename(&original, occurrence_dir.path().join(format!("{child}-detached")))
                .unwrap();
            assert!(
                prepared.commit_outer_launch_intent(&context, &inputs).is_err(),
                "detached {child} must refuse before the one-way launch record"
            );
            assert!(occurrence
                .open_child_directory(OsStr::new("guest-import-owner"))
                .unwrap()
                .unwrap()
                .open_pinned_regular(OsStr::new("guest-supervisor-launch-intent.json"), false)
                .unwrap()
                .is_none());
        }
        let (scratch_occurrence_dir, _scratch_source_dir, scratch_occurrence, prepared) =
            prepare_launch_occurrence();
        let scratch_path = scratch_occurrence_dir
            .path()
            .join("candidate-private/guest-scratch-03");
        std::fs::rename(&scratch_path, scratch_occurrence_dir.path().join("detached-scratch"))
            .unwrap();
        assert!(prepared.commit_outer_launch_intent(&context, &inputs).is_err());
        assert!(scratch_occurrence
            .open_child_directory(OsStr::new("guest-import-owner"))
            .unwrap()
            .unwrap()
            .open_pinned_regular(OsStr::new("guest-supervisor-launch-intent.json"), false)
            .unwrap()
            .is_none());
        let (_ambient_occurrence_dir, _ambient_source_dir, ambient_occurrence, prepared) =
            prepare_launch_occurrence();
        ambient_occurrence
            .open_child_directory(OsStr::new("supervisor-state"))
            .unwrap()
            .unwrap()
            .create_child(OsStr::new("ambient"), 0o700)
            .unwrap();
        assert!(
            prepared.commit_outer_launch_intent(&context, &inputs).is_err(),
            "nonempty supervisor state must refuse before the one-way record"
        );
        assert!(ambient_occurrence
            .open_child_directory(OsStr::new("guest-import-owner"))
            .unwrap()
            .unwrap()
            .open_pinned_regular(OsStr::new("guest-supervisor-launch-intent.json"), false)
            .unwrap()
            .is_none());
        let wrong_context = GuestImportContext {
            occurrence_id: "occ-other",
            ..context
        };
        assert!(
            stage_ticketed_uploaded_guest_package(
                &upload,
                &parent,
                &ticket,
                &wrong_context,
                &inputs,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
        let mut wrong_ticket = ticket.clone();
        wrong_ticket.payload_sha256 = "0".repeat(64);
        assert!(
            stage_ticketed_uploaded_guest_package(
                &upload,
                &parent,
                &wrong_ticket,
                &context,
                &inputs,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        wrong_ticket = ticket.clone();
        wrong_ticket.manifest_sha256 = "0".repeat(64);
        assert!(
            stage_ticketed_uploaded_guest_package(
                &upload,
                &parent,
                &wrong_ticket,
                &context,
                &inputs,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
        let staged = stage_uploaded_guest_package(
            &upload,
            bytes.len() as u64,
            &lillux::sha256_hex(&bytes),
            &parent,
            &expected,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
        )
        .unwrap();
        assert_eq!(staged.base(), &measurement);
        assert_eq!(staged.manifest(), &manifest);
        crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        crate::guest_content::recheck_mounted_guest_content(staged.root(), &inputs).unwrap();
        let opened = crate::guest_content::open_verified_staged_guest_content(&staged, &inputs)
            .unwrap();
        assert_eq!(opened.runtime_mounts.len(), inputs.inputs.len());
        assert_eq!(opened.content_records.len(), inputs.record_descriptors().count());
        drop(opened);
        let mounted =
            crate::guest_content::open_verified_mounted_guest_content(staged.root(), &inputs)
                .unwrap();
        assert_eq!(mounted.runtime_mounts.len(), inputs.inputs.len());
        assert_eq!(mounted.content_records.len(), inputs.record_descriptors().count());
        for (opened, (_, hash, bytes)) in mounted
            .content_records
            .iter()
            .zip(inputs.record_descriptors())
        {
            let (actual, _) = opened.read_regular_file_stable_bounded(bytes).unwrap();
            assert_eq!(actual.len() as u64, bytes);
            assert_eq!(lillux::sha256_hex(&actual), hash);
        }
        for (opened, input) in mounted.runtime_mounts.iter().zip(&inputs.inputs) {
            assert_eq!(
                opened.is_none(),
                matches!(
                    input.content_authority,
                    ryeos_external_execution_contract::GuestMountContentAuthority::PrivateScratch { .. }
                )
            );
        }
        drop(mounted);
        let config_source = staged
            .root()
            .open_pinned_regular(OsStr::new("input-00"), false)
            .unwrap()
            .unwrap();
        config_source.set_mode(0o600).unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        assert!(
            crate::guest_content::recheck_mounted_guest_content(staged.root(), &inputs).is_err()
        );
        assert!(
            crate::guest_content::open_verified_mounted_guest_content(staged.root(), &inputs)
                .is_err()
        );
        assert!(crate::guest_content::open_verified_staged_guest_content(&staged, &inputs).is_err());
        config_source.set_mode(0o644).unwrap();
        let mut writable_config = staged
            .root()
            .open_regular(OsStr::new("input-00"), true)
            .unwrap()
            .unwrap();
        writable_config.write_all(b"bad").unwrap();
        writable_config.sync_all().unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        assert!(crate::guest_content::open_verified_staged_guest_content(&staged, &inputs).is_err());
        use std::io::Seek as _;
        writable_config.rewind().unwrap();
        writable_config.write_all(&config).unwrap();
        writable_config.sync_all().unwrap();
        crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        let mut writable_record = staged
            .root()
            .open_regular(OsStr::new("record-00"), true)
            .unwrap()
            .unwrap();
        writable_record.write_all(b"x").unwrap();
        writable_record.sync_all().unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        assert!(crate::guest_content::open_verified_staged_guest_content(&staged, &inputs).is_err());
        assert!(
            crate::guest_content::open_verified_mounted_guest_content(staged.root(), &inputs)
                .is_err()
        );
        writable_record.rewind().unwrap();
        writable_record.write_all(&product_manifest).unwrap();
        writable_record.sync_all().unwrap();
        crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        let mut writable_source = staged
            .root()
            .open_child_directory(OsStr::new("input-02"))
            .unwrap()
            .unwrap()
            .open_regular(OsStr::new("run.py"), true)
            .unwrap()
            .unwrap();
        writable_source.write_all(b"bad").unwrap();
        writable_source.sync_all().unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        writable_source.rewind().unwrap();
        writable_source.write_all(b"run").unwrap();
        writable_source.sync_all().unwrap();
        crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        for (name, original) in [
            ("record-01", &source_binding_bytes),
            ("record-02", &source_manifest_bytes),
        ] {
            let mut writable = staged
                .root()
                .open_regular(OsStr::new(name), true)
                .unwrap()
                .unwrap();
            writable.write_all(b"x").unwrap();
            writable.sync_all().unwrap();
            assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
            writable.rewind().unwrap();
            writable.write_all(original).unwrap();
            writable.sync_all().unwrap();
            crate::guest_content::recheck_staged_guest_content(&staged, &inputs).unwrap();
        }

        // A controller package is produced from retained descriptors, never
        // from the diagnostic paths used to construct this test fixture.
        let source_fixture = tempfile::tempdir().unwrap();
        for entry in &manifest.entries {
            let path = entry.path();
            if path == "base" || path.starts_with("base/") {
                continue;
            }
            let destination = source_fixture.path().join(path);
            match entry {
                GuestStagingEntry::Directory { mode, .. } => {
                    std::fs::create_dir_all(&destination).unwrap();
                    std::fs::set_permissions(
                        &destination,
                        std::os::unix::fs::PermissionsExt::from_mode(*mode),
                    )
                    .unwrap();
                }
                GuestStagingEntry::RegularFile { mode, .. } => {
                    std::fs::write(&destination, files.get(path).unwrap()).unwrap();
                    std::fs::set_permissions(
                        &destination,
                        std::os::unix::fs::PermissionsExt::from_mode(*mode),
                    )
                    .unwrap();
                }
                GuestStagingEntry::Symlink { target, .. } => {
                    std::os::unix::fs::symlink(target, &destination).unwrap();
                }
            }
        }
        let source_root = lillux::PinnedDirectory::open(source_fixture.path())
            .unwrap()
            .unwrap();
        let inherited_file = |name: &str| {
            source_root
                .open_inherited_regular(OsStr::new(name), false)
                .unwrap()
                .unwrap()
        };
        // The installed signed-bundle resolver passes sealed executable
        // captures (0500), not the mode of the original bundle files.
        let supervisor_capture =
            lillux::sealed_executable_memfd(c"test-supervisor", files.get("supervisor").unwrap())
                .unwrap();
        let launcher_capture =
            lillux::sealed_executable_memfd(c"test-launcher", files.get("launcher").unwrap())
                .unwrap();
        let config_authority = inherited_file("input-00");
        let product_authority = source_root
            .open_child_directory(OsStr::new("input-01"))
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let source_authority = source_root
            .open_child_directory(OsStr::new("input-02"))
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let scratch_authority = source_root
            .create_child(OsStr::new("scratch-03"), 0o700)
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let records = ["record-00", "record-01", "record-02"]
            .into_iter()
            .map(inherited_file)
            .collect::<Vec<_>>();
        let mut retained_inputs = inputs.clone();
        retained_inputs.base_snapshot.descriptor =
            transfer.descriptor().inherited_descriptor().unwrap();
        for (input, authority) in retained_inputs.inputs.iter_mut().zip([
            &config_authority,
            &product_authority,
            &source_authority,
            &scratch_authority,
        ]) {
            input.descriptor = authority.inherited_descriptor().unwrap();
        }
        if let GuestMountContentAuthority::ProductManifest {
            manifest_descriptor,
            ..
        } = &mut retained_inputs.inputs[1].content_authority
        {
            *manifest_descriptor = records[0].inherited_descriptor().unwrap();
        }
        if let GuestMountContentAuthority::SourceClosure {
            binding_descriptor,
            manifest_descriptor,
            ..
        } = &mut retained_inputs.inputs[2].content_authority
        {
            *binding_descriptor = records[1].inherited_descriptor().unwrap();
            *manifest_descriptor = records[2].inherited_descriptor().unwrap();
        }
        let retained = crate::guest_inputs::ExternalGuestInputAuthority::new(
            retained_inputs.clone(),
            transfer.descriptor().clone(),
            None,
            vec![config_authority, product_authority, source_authority, scratch_authority],
            records,
            vec![],
        )
        .unwrap();
        let produced_expected = GuestStagingExpected {
            inputs: &retained_inputs,
            activation_request_digest: &manifest.activation_request_digest,
            bootstrap_sha256: &manifest.bootstrap_sha256,
            supervisor_sha256: &manifest.supervisor_sha256,
            launcher_sha256: &manifest.launcher_sha256,
            maximum_regular_bytes: 16 * 1024 * 1024,
            maximum_framed_bytes: 16 * 1024 * 1024,
        };
        let (produced_bytes, produced_manifest) =
            crate::guest_package_producer::write_guest_package(
                Vec::new(),
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &produced_expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .unwrap();
        assert_eq!(
            produced_manifest.guest_input_identity,
            manifest.guest_input_identity
        );
        for executable in ["supervisor", "launcher"] {
            assert!(produced_manifest.entries.iter().any(|entry| {
                matches!(entry, GuestStagingEntry::RegularFile { path, mode: 0o500, .. }
                    if path == executable)
            }));
        }
        let produced_stage =
            stage_guest_package(produced_bytes.as_slice(), &parent, &produced_expected).unwrap();
        for executable in ["supervisor", "launcher"] {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                produced_stage
                    .root()
                    .open_regular(OsStr::new(executable), false)
                    .unwrap()
                    .unwrap()
                    .metadata()
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o500
            );
        }
        crate::guest_content::recheck_staged_guest_content(&produced_stage, &retained_inputs)
            .unwrap();
        produced_stage.discard().unwrap();
        let prepare = || {
            crate::guest_package_producer::prepare_private_guest_package(
                &parent,
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &produced_expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
        };
        let prepared = prepare().unwrap();
        assert_eq!(prepared.manifest(), &produced_manifest);
        assert_eq!(
            prepared.manifest_sha256(),
            produced_manifest.identity_digest().unwrap()
        );
        assert_eq!(prepared.bytes(), produced_bytes.len() as u64);
        assert_eq!(prepared.sha256(), lillux::sha256_hex(&produced_bytes));
        let delivery = prepared.delivery_descriptor().unwrap();
        let mut delivery_reader = delivery
            .stable_regular_reader_exact(prepared.bytes(), prepared.sha256(), prepared.bytes())
            .unwrap();
        let mut delivered_bytes = Vec::new();
        use std::io::Read as _;
        delivery_reader.read_to_end(&mut delivered_bytes).unwrap();
        delivery_reader.finish().unwrap();
        assert_eq!(delivered_bytes, produced_bytes);
        drop(delivery);
        prepared.discard().unwrap();
        assert!(
            crate::guest_package_producer::prepare_private_guest_package(
                &parent,
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &produced_expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
            )
            .is_err()
        );
        let undersized = GuestStagingExpected {
            maximum_regular_bytes: 1,
            ..produced_expected
        };
        let size_error = crate::guest_package_producer::write_guest_package(
            Vec::new(),
            &retained,
            &inherited_file("bootstrap"),
            &supervisor_capture,
            &launcher_capture,
            &undersized,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
        )
        .err()
        .unwrap();
        assert!(format!("{size_error:#}").contains("budget"));
        let short_frame = GuestStagingExpected {
            maximum_framed_bytes: produced_manifest.framed_bytes().unwrap() - 1,
            ..produced_expected
        };
        assert!(
            crate::guest_package_producer::prepare_private_guest_package(
                &parent,
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &short_frame,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        assert_eq!(parent.entries_no_follow_bounded(1).unwrap().len(), 1);
        struct StopWriter {
            remaining: usize,
        }
        impl Write for StopWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("injected package writer failure"));
                }
                let count = bytes.len().min(self.remaining);
                self.remaining -= count;
                Ok(count)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(
            crate::guest_package_producer::write_guest_package(
                StopWriter { remaining: 128 },
                &retained,
                &inherited_file("bootstrap"),
                &supervisor_capture,
                &launcher_capture,
                &produced_expected,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30)),
            )
            .is_err()
        );
        std::fs::write(
            source_fixture.path().join("input-01/ambient-secret"),
            b"secret",
        )
        .unwrap();
        assert!(prepare().is_err());
        std::fs::remove_file(source_fixture.path().join("input-01/ambient-secret")).unwrap();
        assert_eq!(parent.entries_no_follow_bounded(1).unwrap().len(), 1);
        assert!(
            staged
                .root()
                .open_child_directory(OsStr::new("base"))
                .unwrap()
                .is_some()
        );
        let product = staged
            .root()
            .open_child_directory(OsStr::new("input-01"))
            .unwrap()
            .unwrap();
        assert_eq!(
            product
                .read_symlink_target(OsStr::new("current"), 4096)
                .unwrap()
                .unwrap(),
            b"bin/tool"
        );
        // Harness-only mutation: the observation cannot become a durable
        // Ready claim while a same-UID writer still owns the staged tree.
        std::fs::remove_file(product.path().join("current")).unwrap();
        std::os::unix::fs::symlink("bin/other", product.path().join("current")).unwrap();
        assert!(crate::guest_content::recheck_staged_guest_content(&staged, &inputs).is_err());
        staged.discard().unwrap();
        assert!(parent.entries_no_follow_bounded(0).unwrap().is_empty());
    }
}
