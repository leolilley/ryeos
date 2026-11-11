//! Local guest-start rehearsal.
//!
//! The installed guest occurrence owner executable is started exactly as a
//! sandbox host starts it: as the administrator of a `/ryeos` tree whose
//! roots are administrator-private and whose uploads are `0600`, with the
//! controller's real package producer, signed import and controller-signed
//! assignment. Locally there is no administrator, so the run happens inside a
//! Lillux root enclosure where the caller is root over a constructed root
//! and the profile's account is mapped. The run ends when the supervisor
//! publishes its relay attach request, or when the owner refuses.
//!
//! The runtime tree is the shipped product itself, which names no controller.
//! Every run generates a fresh controller key and owner key that exist only in
//! this process's memory; the assignment names that fresh controller key, and
//! the occurrence and placement identities are rehearsal-only. The documents
//! reach an owner only through a launch the rehearsal itself sets, the same
//! authority any controller holds over the sandboxes it launches, and no
//! controller ever records their occurrence.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use base64::{
    Engine as _, engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD,
};
use ryeos_external_execution_contract::guest_import_authorization::{
    GUEST_IMPORT_AUTHORIZATION_SCHEMA, GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
    GuestImportAuthorization, GuestOccurrenceAssignment, GuestOccurrenceAssignmentDocument,
};
use ryeos_external_execution_contract::staging_package::{
    GUEST_IMPORT_TICKET_SCHEMA, GuestImportTicket, GuestStagingExpected,
};
use ryeos_external_execution_contract::{
    EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection, GuestBaseSnapshotInput,
    GuestMountAccess, GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
    GuestProductManifestKind, GuestWorkspaceView,
};
use ryeos_state::external_execution::admission::{
    AdmittedExternalCandidateProgram, AdmittedExternalExecutionProgram, CONNECTOR_PROTOCOL,
    ExternalCandidateExecutionRoute, ExternalCandidateProcFilesystem,
    ExternalCandidateQualificationUse, ExternalCandidateRequirement,
    ExternalCandidateRuntimeAuthority, ExternalCandidateRuntimeRecipe,
    ExternalCandidateRuntimeSource, PROTOCOL,
};
use ryeos_state::external_execution::transport::{
    EXTERNAL_SUPERVISOR_ORDINARY_RELAY_BOOTSTRAP_SCHEMA, ExternalControllerFileRelayContract,
    ExternalControllerTransportSelection, ExternalNetworkInputPolicy, ExternalSupervisorBootstrap,
    ExternalSupervisorNativeEnvelope,
};
use ryeos_state::objects::{ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
use serde::{Deserialize, Serialize};

use crate::guest_import_authorization::{
    GuestOwnerRuntimeProfile, ObservedGuestRuntime, sign_guest_import_authorization,
    sign_guest_occurrence_assignment,
};
use crate::guest_inputs::ExternalGuestInputAuthority;
use crate::guest_package_producer::prepare_private_guest_package;
use crate::guest_runtime_product::{OWNER_NAME, produce_guest_owner_runtime};

/// Environment variable naming the prepared work directory. A process that
/// finds it set at startup is the rehearsal host and runs nothing else.
pub const REHEARSAL_HOST_ENV: &str = "RYEOS_GUEST_START_REHEARSAL_HOST";

/// Names an export directory: replay its `run.sh` in a local root enclosure,
/// as a stand-in for the sandbox host it is meant for. Handled first thing,
/// like [`REHEARSAL_HOST_ENV`].
pub const REHEARSAL_REPLAY_ENV: &str = "RYEOS_GUEST_START_REHEARSAL_REPLAY_EXPORT";

/// Names an export directory: run the rehearsal host's observation directly on
/// this machine, which already holds the activation's `/ryeos` layout.
pub const REHEARSAL_ON_HOST_ENV: &str = "RYEOS_GUEST_START_REHEARSAL_ON_HOST";

pub(crate) const PLAN_NAME: &str = "plan.json";
pub(crate) const RUNTIME_NAME: &str = "runtime";
pub(crate) const ACTIVATION_NAME: &str = "activation";
pub(crate) const IMPORT_NAME: &str = "signed-import.json";
pub(crate) const PACKAGE_NAME: &str = "guest-package";
const PLACEMENT_ID: &str = "T-guest-start-rehearsal";
const PRODUCT_MOUNT: &str = "runtime";
const PRODUCT_DESTINATION: &str = "/runtime/product";
const OWNER_RECORD_DIRECTORY: &str = "guest-import-owner";
const INSTALL_INTENT: &str = "guest-base-install-intent.json";
const LAUNCH_INTENT: &str = "guest-supervisor-launch-intent.json";
const SUPERVISOR_STATE: &str = "supervisor-state";
const ATTACH_REQUEST: &str = "attach.request.json";
pub(crate) const MAX_STDERR_BYTES: usize = 256 * 1024;
const MAX_PLAN_BYTES: u64 = 1024 * 1024;
pub(crate) const POLL_INTERVAL: lillux::time::Duration = lillux::time::Duration::from_millis(10);
/// The owner's own cancelled-occurrence settlement window, plus slack.
const OWNER_CLEANUP: lillux::time::Duration = lillux::time::Duration::from_secs(45);
/// Host directories the enclosure carries; `/ryeos` and `/tmp` are its own.
const HOST_ENTRIES: &[&str] = &[
    "usr", "bin", "sbin", "lib", "lib32", "lib64", "etc", "dev", "proc", "sys", "run",
];

/// Exact inputs a rehearsal starts from. Paths are read once and copied into
/// a private work directory before anything runs.
pub struct GuestStartRehearsalRequest<'a> {
    /// The owner executable of the selected guest runtime product.
    pub owner_executable: &'a Path,
    /// That product's installed owner profile.
    pub profile: GuestOwnerRuntimeProfile,
    pub supervisor_executable: &'a Path,
    pub launcher_executable: &'a Path,
    /// Directory delivered as the candidate runtime product mount.
    pub product: &'a Path,
    /// The product's retained manifest record, when the selection has one;
    /// otherwise a content manifest of `product` is derived.
    pub product_manifest_record: Option<&'a Path>,
    /// The project snapshot the activation delivers as its base, copied from
    /// this CAS exactly as live activation copies it; `None` delivers an
    /// empty project.
    pub base_snapshot: Option<GuestStartRehearsalBase<'a>>,
    /// Further single-file products the activation mounts beside the runtime
    /// product, each under its retained manifest record.
    pub file_products: &'a [GuestStartRehearsalFileProduct<'a>],
    /// The binding's controller relay, unchanged.
    pub relay: ExternalControllerFileRelayContract,
    /// The binding's guest network inputs, unchanged.
    pub network_inputs: ExternalNetworkInputPolicy,
    /// Time the controller allows between contact and the attach request.
    pub attach_budget: lillux::time::Duration,
    /// Least time the signed documents stay admissible after preparation,
    /// for documents delivered to a host by a slower route.
    pub document_lifetime: Option<lillux::time::Duration>,
    /// Private parent for the rehearsal work directory.
    pub work_parent: &'a Path,
    /// Executable that runs [`run_rehearsal_host_if_requested`] first thing.
    pub host_executable: &'a Path,
    /// A real candidate program and the mounts its inputs name. When set,
    /// `product` and `file_products` are not delivered.
    pub candidate: Option<RehearsalCandidate<'a>>,
    /// Answer the attach request as the controller would and observe the
    /// supervisor until its first exchange frame or its exit.
    pub answer_attach: bool,
}

/// A candidate program and the guest inputs a controller delivered with it.
pub struct RehearsalCandidate<'a> {
    pub program: AdmittedExternalExecutionProgram,
    pub bootstrap_schema: u32,
    pub mounts: &'a [RehearsalMount<'a>],
    pub environment: BTreeMap<String, String>,
    pub executable_search: Vec<String>,
    pub limits: RehearsalCandidateLimits,
}

/// The bootstrap bounds the controller sealed with the candidate.
#[derive(Debug, Clone, Deserialize)]
pub struct RehearsalCandidateLimits {
    pub execution_timeout_seconds: u32,
    pub post_execution_timeout_seconds: u32,
    pub candidate_export_max_bytes: u64,
    pub channel_max_bytes: u64,
    pub native_journal: ExternalSupervisorNativeEnvelope,
    pub report_capabilities: bool,
}

/// One guest input mount, rematerialized locally.
pub struct RehearsalMount<'a> {
    pub role: GuestMountRole,
    pub authority_id: &'a str,
    pub destination: &'a str,
    pub kind: GuestMountKind,
    pub access: GuestMountAccess,
    pub normalized_mode: Option<u32>,
    pub source: RehearsalMountSource<'a>,
}

pub enum RehearsalMountSource<'a> {
    /// A product's content (a directory or one file) and its manifest record.
    Product {
        content: &'a Path,
        manifest_record: &'a Path,
    },
    /// A fresh private scratch directory.
    PrivateScratch { binding_hash: &'a str },
}

/// A project snapshot held by a node's CAS.
#[derive(Clone, Copy)]
pub struct GuestStartRehearsalBase<'a> {
    pub cas: &'a lillux::CasStore,
    pub snapshot_hash: &'a str,
}

/// One regular-file product mount.
#[derive(Clone, Copy)]
pub struct GuestStartRehearsalFileProduct<'a> {
    pub authority_id: &'a str,
    pub destination: &'a str,
    pub file: &'a Path,
    pub manifest_record: &'a Path,
}

fn manifest_kind_of(record: &[u8]) -> Result<(GuestProductManifestKind, u64)> {
    let value: serde_json::Value = serde_json::from_slice(record)?;
    let kind = match value["kind"].as_str() {
        Some(ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND) => {
            GuestProductManifestKind::Content
        }
        Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
            GuestProductManifestKind::LargeContent
        }
        other => anyhow::bail!("rehearsal product manifest kind {other:?} is not admitted"),
    };
    let total = value["total_bytes"]
        .as_u64()
        .context("rehearsal product manifest has no total bytes")?;
    Ok((kind, total))
}

fn read_record(path: &Path) -> Result<Vec<u8>> {
    let record = open_source(path, "product manifest record")?;
    record
        .read_stable_bounded(&record.observation()?, 4 * 1024 * 1024)
        .map_err(anyhow::Error::msg)
}

/// Owner phases, in order. A refusal names the first phase that did not
/// complete; each completes at a durable record the owner writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestStartRehearsalStep {
    /// The host cannot run a rehearsal.
    Host,
    /// Package production, signing or enclosure layout.
    Prepare,
    /// Account transition and document verification, until the occurrence
    /// directory exists.
    Admit,
    /// Private source entry and upload import, until the base-install intent.
    Stage,
    /// Base install and launch preparation, until the supervisor launch intent.
    Install,
    /// Supervisor release, until the supervisor's first state record.
    Release,
    /// Supervisor startup, until the relay attach request.
    Attach,
}

impl GuestStartRehearsalStep {
    const OWNER_PHASES: [Self; 5] = [
        Self::Admit,
        Self::Stage,
        Self::Install,
        Self::Release,
        Self::Attach,
    ];
}

/// Elapsed milliseconds of each completed owner phase.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestStartRehearsalPhases {
    pub admit_ms: Option<u64>,
    pub stage_ms: Option<u64>,
    pub install_ms: Option<u64>,
    pub release_ms: Option<u64>,
    pub attach_ms: Option<u64>,
    /// Owner start to the last completed phase.
    pub total_ms: u64,
}

/// The supervisor after the rehearsal answered its attach request as the
/// controller would: either it published its first exchange frame, or it
/// stopped first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestStartRehearsalAfterAttach {
    /// Attach publication to the delivered response.
    pub response_ms: u64,
    /// Response delivery to the first exchange request, if one appeared.
    pub first_exchange_ms: Option<u64>,
    pub outcome: String,
}

/// What the package carried, for judging the owner's profile ceilings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestStartRehearsalPackage {
    pub framed_bytes: u64,
    pub regular_bytes: u64,
    pub entries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GuestStartRehearsalOutcome {
    AttachPublished {
        phases: GuestStartRehearsalPhases,
        attach_budget_ms: u64,
        within_budget: bool,
        package: GuestStartRehearsalPackage,
        /// What followed the rehearsal's own attach response, when it gave one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_attach: Option<GuestStartRehearsalAfterAttach>,
        /// How the owner ended after the rehearsal asked it to cancel.
        owner_cleanup: String,
        owner_stderr: String,
    },
    Refused {
        step: GuestStartRehearsalStep,
        message: String,
        phases: GuestStartRehearsalPhases,
        package: Option<GuestStartRehearsalPackage>,
        owner_stderr: String,
    },
}

impl GuestStartRehearsalOutcome {
    fn refused(step: GuestStartRehearsalStep, message: impl Into<String>) -> Self {
        Self::Refused {
            step,
            message: message.into(),
            phases: GuestStartRehearsalPhases::default(),
            package: None,
            owner_stderr: String::new(),
        }
    }
}

/// What the enclosed host does once the owner is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostMode {
    /// Observe the owner until the supervisor publishes its attach request.
    AttachProbe,
    /// Serve the controller's relay file moves until it asks to stop.
    RelayLoopback,
}

/// Everything the host needs besides the work directory's files.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostPlan {
    pub(crate) mode: HostMode,
    pub(crate) occurrence_id: String,
    pub(crate) mailbox_root: String,
    pub(crate) runtime_manifest_hash: String,
    pub(crate) assignment_b64: String,
    pub(crate) attach_budget_ms: u64,
    pub(crate) owner_deadline_ms: u64,
    pub(crate) package: GuestStartRehearsalPackage,
    /// Rehearsal-only controller authority for answering the attach request.
    pub(crate) answer_attach: Option<AttachAnswer>,
}

/// The rehearsal's own controller key and sealed bootstrap, never a node's.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachAnswer {
    controller_key_hex: String,
    bootstrap_b64: String,
    uid: u32,
    gid: u32,
}

fn digest_of(label: &str, run: &str) -> String {
    lillux::sha256_hex(format!("ryeos.guest-start-rehearsal.{run}.{label}").as_bytes())
}

pub(crate) fn write_file(
    directory: &lillux::PinnedDirectory,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<lillux::PinnedRegularFile> {
    let mut file = directory.open_regular_create(OsStr::new(name), true, true, 0o600)?;
    file.write_all(bytes)?;
    lillux::set_open_regular_file_mode(&file, mode)?;
    file.sync_all()?;
    drop(file);
    directory
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("rehearsal file {name} vanished"))
}

/// Stream an open source into a new exact-mode file of `directory`.
pub(crate) fn copy_file(
    directory: &lillux::PinnedDirectory,
    name: &str,
    source: &lillux::PinnedRegularFile,
    mode: u32,
) -> Result<lillux::PinnedRegularFile> {
    let observation = source.observation()?;
    let mut reader = source.try_clone_descriptor()?;
    let mut file = directory.open_regular_create(OsStr::new(name), true, true, 0o600)?;
    let copied = std::io::copy(&mut reader, &mut file)?;
    ensure!(
        copied == observation.size(),
        "rehearsal copy of {name} changed length"
    );
    lillux::set_open_regular_file_mode(&file, mode)?;
    file.sync_all()?;
    drop(file);
    directory
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("rehearsal file {name} vanished"))
}

fn open_source(path: &Path, what: &str) -> Result<lillux::PinnedRegularFile> {
    lillux::open_pinned_regular_file_no_follow(path)
        .with_context(|| format!("open rehearsal {what} {}", path.display()))
}

/// Copy a product directory tree with its exact modes.
pub(crate) fn copy_tree(
    source: &lillux::PinnedDirectory,
    target: &lillux::PinnedDirectory,
    budget: &mut u64,
) -> Result<()> {
    let mut names = source.entry_names()?;
    names.sort();
    for name in names {
        ensure!(*budget > 0, "rehearsal product has too many entries");
        *budget -= 1;
        let is_directory = std::fs::symlink_metadata(source.path().join(&name))?.is_dir();
        if is_directory {
            let child = source
                .open_child_directory(&name)?
                .with_context(|| format!("rehearsal product directory {name:?} vanished"))?;
            let copy = target.create_child(&name, 0o700)?;
            copy_tree(&child, &copy, budget)?;
            copy.set_mode(0o755)?;
        } else {
            let file = source
                .open_pinned_regular(&name, false)?
                .with_context(|| format!("rehearsal product member {name:?} is not regular"))?;
            let mode = file.permission_mode()?;
            copy_file(
                target,
                name.to_str()
                    .context("rehearsal product name is not UTF-8")?,
                &file,
                mode,
            )?;
        }
    }
    Ok(())
}

struct Prepared {
    plan: HostPlan,
    attachment_deadline_ms: i64,
}

fn base_snapshot(
    work: &lillux::PinnedDirectory,
    selected: Option<GuestStartRehearsalBase<'_>>,
) -> Result<(
    lillux::PinnedDirectory,
    ryeos_project_capture::ProjectSnapshotTransferMeasurement,
)> {
    let base = work.create_child(OsStr::new("base"), 0o700)?;
    if let Some(selected) = selected {
        let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
            selected.cas,
            [selected.snapshot_hash.to_owned()],
            ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
        )?;
        ensure!(
            closure.is_complete() && closure.large_object_hashes.is_empty(),
            "rehearsal base snapshot closure is incomplete or contains non-CAS content"
        );
        let measurement = ryeos_project_capture::copy_project_snapshot_closure(
            selected.cas,
            &closure,
            &base,
            selected.snapshot_hash,
        )?;
        return Ok((base, measurement));
    }
    let cas = lillux::CasStore::from_pinned_root(base.try_clone()?);
    let policy = ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        vec![],
        vec![],
        BTreeMap::new(),
    )?;
    let policy_hash = cas.store_object(&policy.to_value())?;
    let tree_hash = cas.store_object(
        &ProjectTree {
            files: BTreeMap::new(),
        }
        .to_value(),
    )?;
    let snapshot_hash = cas.store_object(
        &ProjectSnapshot {
            project_tree_hash: tree_hash,
            effective_policy_hash: policy_hash,
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-11-03T00:00:00Z".into(),
            source: "guest-start-rehearsal".into(),
        }
        .to_value(),
    )?;
    let measurement =
        ryeos_project_capture::inspect_project_snapshot_transfer(&base, &snapshot_hash)?;
    Ok((base, measurement))
}

/// A structured-session program over the rehearsal's product. The relay
/// carrier is admitted only for session programs; the program's own
/// qualification identities are rehearsal-only.
fn rehearsal_program(
    manifest_kind: GuestProductManifestKind,
    product_manifest_hash: &str,
    run: &str,
) -> Result<AdmittedExternalCandidateProgram> {
    let recipe = ExternalCandidateRuntimeRecipe {
        schema: 2,
        runtime_mount_destination: PRODUCT_DESTINATION.into(),
        executable_relative_path: "bin/rehearsal".into(),
        argv0: "rehearsal".into(),
        arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
        cwd: "/workspace".into(),
        environment: BTreeMap::new(),
        max_stdout_bytes: 1024 * 1024,
        max_stderr_bytes: 1024 * 1024,
        proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
        contain_process_group: false,
        nested_sandbox: true,
    };
    let runtime_recipe_digest = recipe.digest()?;
    let requirement = ExternalCandidateRequirement {
        schema: 7,
        protocol: PROTOCOL.into(),
        connector_protocol: CONNECTOR_PROTOCOL.into(),
        execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
        required_lifecycle_capabilities: Default::default(),
        provider_declaration_id: "guest-start-rehearsal".into(),
        provider_configuration_destination: "rehearsal.toml".into(),
        runtime_product_declaration_id: PRODUCT_MOUNT.into(),
        runtime_authority: ExternalCandidateRuntimeAuthority::CapturedProduct,
        runtime_recipe: recipe,
    };
    let qualification_use = ExternalCandidateQualificationUse {
        schema: ryeos_state::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA.into(),
        requirement_digest: requirement.qualification_requirement_digest()?,
        profile_hash: digest_of("profile", run),
        source_binding_hash: digest_of("source-binding", run),
        source_content_manifest_hash: digest_of("source-content", run),
        provider_executable_manifest_hash: product_manifest_hash.into(),
        execution_environment_digest: digest_of("environment", run),
    };
    let program = AdmittedExternalCandidateProgram {
        requirement,
        qualification_use,
        runtime_manifest_kind: match manifest_kind {
            GuestProductManifestKind::Content => {
                ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
            }
            GuestProductManifestKind::LargeContent => {
                ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
            }
        }
        .into(),
        runtime_manifest_hash: product_manifest_hash.into(),
        runtime_source: ExternalCandidateRuntimeSource::CapturedProduct {
            witness_hash: digest_of("witness", run),
        },
        qualification_attestation_hash: digest_of("attestation", run),
        selection_identity_digest: digest_of("selection", run),
        runtime_recipe_digest,
    };
    program.validate()?;
    Ok(program)
}

/// What a rehearsal mounts: the candidate product, any single-file products
/// and the base snapshot.
pub struct RehearsalInputsRequest<'a> {
    pub product: &'a Path,
    pub product_manifest_record: Option<&'a Path>,
    pub product_mount: &'a str,
    pub product_destination: &'a str,
    pub base_snapshot: Option<GuestStartRehearsalBase<'a>>,
    pub file_products: &'a [GuestStartRehearsalFileProduct<'a>],
}

struct AssembledInputs {
    projection: ExternalGuestInputProjection,
    base_authority: lillux::InheritedDescriptorAuthority,
    input_authorities: Vec<lillux::InheritedDescriptorAuthority>,
    record_authorities: Vec<lillux::InheritedDescriptorAuthority>,
    measurement: ryeos_project_capture::ProjectSnapshotTransferMeasurement,
    product_manifest_hash: String,
    manifest_kind: GuestProductManifestKind,
    /// Product, record and file-product bytes the package carries.
    delivered_bytes: u64,
}

/// Rehearsal guest inputs copied into `work`, with their descriptor authority.
pub struct RehearsalGuestInputs {
    pub authority: ExternalGuestInputAuthority,
    pub product_manifest_hash: String,
    pub manifest_kind: GuestProductManifestKind,
    pub base_snapshot_hash: String,
}

/// Copy the inputs a rehearsal mounts into `work` and project them.
pub fn rehearsal_guest_inputs(
    work: &lillux::PinnedDirectory,
    request: &RehearsalInputsRequest<'_>,
) -> Result<RehearsalGuestInputs> {
    let assembled = assemble_inputs(work, request)?;
    Ok(RehearsalGuestInputs {
        base_snapshot_hash: assembled.measurement.snapshot_hash.clone(),
        product_manifest_hash: assembled.product_manifest_hash,
        manifest_kind: assembled.manifest_kind,
        authority: ExternalGuestInputAuthority::new(
            assembled.projection,
            assembled.base_authority,
            None,
            assembled.input_authorities,
            assembled.record_authorities,
            Vec::new(),
        )?,
    })
}

fn assemble_candidate_inputs(
    work: &lillux::PinnedDirectory,
    base: Option<GuestStartRehearsalBase<'_>>,
    candidate: &RehearsalCandidate<'_>,
) -> Result<AssembledInputs> {
    let (base_directory, measurement) = base_snapshot(work, base)?;
    let base_authority = base_directory.inherited_descriptor_authority()?;
    let mut inputs = Vec::new();
    let mut input_authorities = Vec::new();
    let mut record_authorities = Vec::new();
    let mut delivered_bytes = 0_u64;
    for (index, mount) in candidate.mounts.iter().enumerate() {
        let name = format!("mount-{index:02}");
        let (authority, content_authority, bytes) = match &mount.source {
            RehearsalMountSource::Product {
                content,
                manifest_record,
            } => {
                let record = read_record(manifest_record)?;
                let (kind, total) = manifest_kind_of(&record)?;
                let authority = match mount.kind {
                    GuestMountKind::Directory => {
                        let source =
                            lillux::PinnedDirectory::open(content)?.with_context(|| {
                                format!("rehearsal mount {} is absent", content.display())
                            })?;
                        let copy = work.create_child(OsStr::new(&name), 0o700)?;
                        copy_tree(&source, &copy, &mut 400_000)?;
                        copy.set_mode(0o755)?;
                        copy.inherited_descriptor_authority()?
                    }
                    GuestMountKind::RegularFile => copy_file(
                        work,
                        &name,
                        &open_source(content, "mounted file")?,
                        mount.normalized_mode.unwrap_or(0o444),
                    )?
                    .inherited_descriptor_authority()?,
                };
                let record_file = write_file(work, &format!("{name}.json"), &record, 0o644)?;
                let record_authority = record_file.inherited_descriptor_authority()?;
                let content_authority = GuestMountContentAuthority::ProductManifest {
                    manifest_kind: kind,
                    manifest_hash: lillux::sha256_hex(&record),
                    manifest_descriptor: record_authority
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?,
                    manifest_bytes: record.len() as u64,
                };
                record_authorities.push(record_authority);
                delivered_bytes += total + record.len() as u64;
                (authority, content_authority, total)
            }
            RehearsalMountSource::PrivateScratch { binding_hash } => (
                work.create_child(OsStr::new(&name), 0o700)?
                    .inherited_descriptor_authority()?,
                GuestMountContentAuthority::PrivateScratch {
                    binding_hash: (*binding_hash).to_owned(),
                },
                0,
            ),
        };
        inputs.push(GuestMountInput {
            role: mount.role,
            authority_id: mount.authority_id.into(),
            descriptor: authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            destination: mount.destination.into(),
            kind: mount.kind,
            access: mount.access,
            normalized_mode: mount.normalized_mode,
            content_authority,
            bytes,
        });
        input_authorities.push(authority);
    }
    let projection = ExternalGuestInputProjection {
        workspace_view: GuestWorkspaceView::FullSnapshot,
        schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        base_snapshot: GuestBaseSnapshotInput {
            descriptor: base_authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            snapshot_hash: measurement.snapshot_hash.clone(),
            closure_digest: measurement.closure_digest.clone(),
            object_count: measurement.object_count,
            blob_count: measurement.blob_count,
            total_bytes: measurement.total_bytes,
        },
        workspace_outputs: None,
        inputs,
        executable_search: candidate.executable_search.clone(),
        environment: candidate.environment.clone(),
    };
    projection.validate()?;
    candidate.program.validate_guest_inputs(&projection)?;
    Ok(AssembledInputs {
        projection,
        base_authority,
        input_authorities,
        record_authorities,
        measurement,
        product_manifest_hash: candidate.program.runtime_manifest_hash()?.to_owned(),
        manifest_kind: GuestProductManifestKind::LargeContent,
        delivered_bytes,
    })
}

fn assemble_inputs(
    work: &lillux::PinnedDirectory,
    request: &RehearsalInputsRequest<'_>,
) -> Result<AssembledInputs> {
    // Candidate product as delivered, and its manifest record.
    let product_source = lillux::PinnedDirectory::open(request.product)?
        .with_context(|| format!("rehearsal product {} is absent", request.product.display()))?;
    let product = work.create_child(OsStr::new("product"), 0o700)?;
    copy_tree(&product_source, &product, &mut 400_000)?;
    product.set_mode(0o755)?;
    let (manifest_kind, manifest_record, product_bytes) = match request.product_manifest_record {
        Some(path) => {
            let bytes = read_record(path)?;
            let (kind, total) = manifest_kind_of(&bytes)?;
            (kind, bytes, total)
        }
        None => {
            let manifest = ryeos_state::observe_external_content_tree_exact(&product)?;
            let record = lillux::canonical_json(&serde_json::to_value(&manifest)?)?.into_bytes();
            ensure!(
                ryeos_state::external_content_manifest_digest(&manifest)?
                    == lillux::sha256_hex(&record),
                "rehearsal product manifest record differs from its digest"
            );
            (
                GuestProductManifestKind::Content,
                record,
                manifest.total_bytes,
            )
        }
    };
    let product_manifest_hash = lillux::sha256_hex(&manifest_record);
    let record_file = write_file(work, "product-manifest.json", &manifest_record, 0o644)?;

    let (base, measurement) = base_snapshot(work, request.base_snapshot)?;
    let base_authority = base.inherited_descriptor_authority()?;
    let product_authority = product.inherited_descriptor_authority()?;
    let record_authority = record_file.inherited_descriptor_authority()?;
    let mut inputs = vec![GuestMountInput {
        role: GuestMountRole::Product,
        authority_id: request.product_mount.into(),
        descriptor: product_authority
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?,
        destination: request.product_destination.into(),
        kind: GuestMountKind::Directory,
        access: GuestMountAccess::ReadOnly,
        normalized_mode: None,
        content_authority: GuestMountContentAuthority::ProductManifest {
            manifest_kind,
            manifest_hash: product_manifest_hash.clone(),
            manifest_descriptor: record_authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            manifest_bytes: manifest_record.len() as u64,
        },
        bytes: product_bytes,
    }];
    let mut input_authorities = vec![product_authority];
    let mut record_authorities = vec![record_authority];
    let mut extra_bytes = 0_u64;
    for (index, extra) in request.file_products.iter().enumerate() {
        let record = read_record(extra.manifest_record)?;
        let (kind, bytes) = manifest_kind_of(&record)?;
        // A file product's single entry carries the mode it is mounted with.
        let mode = serde_json::from_slice::<serde_json::Value>(&record)?["entries"][0]["mode"]
            .as_u64()
            .and_then(|mode| u32::try_from(mode).ok())
            .context("rehearsal file product manifest has no file mode")?;
        let file = copy_file(
            work,
            &format!("file-product-{index:02}"),
            &open_source(extra.file, "file product")?,
            mode,
        )?;
        let record_file = write_file(
            work,
            &format!("file-product-{index:02}.json"),
            &record,
            0o644,
        )?;
        let file_authority = file.inherited_descriptor_authority()?;
        let record_authority = record_file.inherited_descriptor_authority()?;
        inputs.push(GuestMountInput {
            role: GuestMountRole::Product,
            authority_id: extra.authority_id.into(),
            descriptor: file_authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            destination: extra.destination.into(),
            kind: GuestMountKind::RegularFile,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: Some(mode),
            content_authority: GuestMountContentAuthority::ProductManifest {
                manifest_kind: kind,
                manifest_hash: lillux::sha256_hex(&record),
                manifest_descriptor: record_authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
                manifest_bytes: record.len() as u64,
            },
            bytes,
        });
        extra_bytes += bytes + record.len() as u64;
        input_authorities.push(file_authority);
        record_authorities.push(record_authority);
    }
    let projection = ExternalGuestInputProjection {
        workspace_view: GuestWorkspaceView::FullSnapshot,
        schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        base_snapshot: GuestBaseSnapshotInput {
            descriptor: base_authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            snapshot_hash: measurement.snapshot_hash.clone(),
            closure_digest: measurement.closure_digest.clone(),
            object_count: measurement.object_count,
            blob_count: measurement.blob_count,
            total_bytes: measurement.total_bytes,
        },
        workspace_outputs: None,
        inputs,
        executable_search: vec![],
        environment: BTreeMap::new(),
    };
    projection.validate()?;
    Ok(AssembledInputs {
        projection,
        base_authority,
        input_authorities,
        record_authorities,
        measurement,
        product_manifest_hash,
        manifest_kind,
        delivered_bytes: product_bytes + manifest_record.len() as u64 + extra_bytes,
    })
}

fn prepare(
    request: &GuestStartRehearsalRequest<'_>,
    work: &lillux::PinnedDirectory,
) -> Result<Prepared> {
    let run = hex::encode(lillux::crypto::generate_random_bytes::<8>());
    let occurrence_id = format!("rehearsal-{run}");
    let root_key = lillux::crypto::generate_signing_key();
    let controller_key = lillux::crypto::generate_signing_key();

    // Same owner bytes and profile as the shipped product; a fresh rehearsal
    // controller key signs the assignment.
    let owner_source = open_source(request.owner_executable, "owner executable")?;
    let owner = copy_file(work, OWNER_NAME, &owner_source, 0o755)?;
    let owner_bytes = owner.observation()?.size();
    let owner_sha256 = owner.digest_stable_exact(&owner.observation()?)?;
    let runtime = produce_guest_owner_runtime(
        work,
        OsStr::new(RUNTIME_NAME),
        &owner.inherited_descriptor_authority()?,
        owner_bytes,
        &owner_sha256,
        &request.profile,
    )?;
    let runtime_manifest_hash = runtime.manifest_hash().to_owned();

    let supervisor = copy_file(
        work,
        "supervisor",
        &open_source(request.supervisor_executable, "supervisor executable")?,
        0o755,
    )?;
    let launcher = copy_file(
        work,
        "launcher",
        &open_source(request.launcher_executable, "launcher executable")?,
        0o755,
    )?;
    let supervisor_sha256 = supervisor.digest_stable_exact(&supervisor.observation()?)?;
    let launcher_sha256 = launcher.digest_stable_exact(&launcher.observation()?)?;
    let member_bytes = supervisor.observation()?.size() + launcher.observation()?.size();

    let AssembledInputs {
        projection,
        base_authority,
        input_authorities,
        record_authorities,
        measurement,
        product_manifest_hash,
        manifest_kind,
        delivered_bytes,
    } = match &request.candidate {
        Some(candidate) => assemble_candidate_inputs(work, request.base_snapshot, candidate)?,
        None => assemble_inputs(
            work,
            &RehearsalInputsRequest {
                product: request.product,
                product_manifest_record: request.product_manifest_record,
                product_mount: PRODUCT_MOUNT,
                product_destination: PRODUCT_DESTINATION,
                base_snapshot: request.base_snapshot,
                file_products: request.file_products,
            },
        )?,
    };
    let guest_input_identity = projection.identity_digest()?;
    let (program, bootstrap_schema) = match &request.candidate {
        Some(candidate) => (candidate.program.clone(), candidate.bootstrap_schema),
        None => (
            rehearsal_program(manifest_kind, &product_manifest_hash, &run)?.into(),
            EXTERNAL_SUPERVISOR_ORDINARY_RELAY_BOOTSTRAP_SCHEMA,
        ),
    };
    let supervisor_runtime_hash = program.runtime_manifest_hash()?.to_owned();

    let attach_budget_ms = u64::try_from(request.attach_budget.as_millis())
        .context("rehearsal attach budget overflow")?;
    let owner_deadline_ms = u64::from(request.profile.owner_timeout_seconds) * 1_000;
    let document_lifetime_ms = request.document_lifetime.map_or(0, |lifetime| {
        u64::try_from(lifetime.as_millis()).unwrap_or(u64::MAX)
    });
    let attachment_deadline_ms = lillux::time::timestamp_millis()
        + i64::try_from(
            owner_deadline_ms
                .max(attach_budget_ms)
                .max(document_lifetime_ms),
        )?
        + 120_000;
    let allocation_request_digest = digest_of("allocation", &run);
    let admitted_capsule_hash = digest_of("capsule", &run);
    let activation_request_digest = digest_of("activation", &run);
    let execution_binding_hash = digest_of("binding", &run);
    let bootstrap = ExternalSupervisorBootstrap {
        schema: bootstrap_schema,
        controller: ExternalControllerTransportSelection::ControllerFileRelay(
            request.relay.clone(),
        ),
        tls_root_certificates_der_base64: Vec::new(),
        placement_thread_id: PLACEMENT_ID.into(),
        occurrence_id: occurrence_id.clone(),
        allocation_request_digest: allocation_request_digest.clone(),
        admitted_capsule_hash: admitted_capsule_hash.clone(),
        base_snapshot_hash: measurement.snapshot_hash.clone(),
        execution_binding_hash: execution_binding_hash.clone(),
        supervisor_runtime_hash: supervisor_runtime_hash.clone(),
        launcher_artifact_hash: launcher_sha256.clone(),
        candidate_program: program,
        guest_input_identity: guest_input_identity.clone(),
        guest_inputs: projection.clone(),
        owner_public_key: ryeos_state::external_execution::encode_channel_public_key(
            &controller_key.verifying_key(),
        )?,
        bootstrap_capability: STANDARD.encode(lillux::crypto::generate_random_bytes::<32>()),
        attachment_deadline_ms,
        execution_timeout_seconds: 60,
        post_execution_timeout_seconds: 120,
        candidate_export_max_bytes: 512 * 1024,
        channel_max_bytes: 1024 * 1024,
        native_journal: ExternalSupervisorNativeEnvelope {
            storage_bytes: 67_108_864,
            allocator_steps: 2_000_000_000,
            connections: 16,
            operation_millis: 4_860_000,
            cleanup_millis: 120_000,
            cleanup_steps: 3_000_000,
        },
        report_capabilities: false,
    };
    let mut bootstrap = bootstrap;
    if let Some(limits) = request
        .candidate
        .as_ref()
        .map(|candidate| &candidate.limits)
    {
        bootstrap.execution_timeout_seconds = limits.execution_timeout_seconds;
        bootstrap.post_execution_timeout_seconds = limits.post_execution_timeout_seconds;
        bootstrap.candidate_export_max_bytes = limits.candidate_export_max_bytes;
        bootstrap.channel_max_bytes = limits.channel_max_bytes;
        bootstrap.native_journal = limits.native_journal;
        bootstrap.report_capabilities = limits.report_capabilities;
    }
    bootstrap.validate()?;
    let bootstrap_bytes = bootstrap.canonical_bytes()?;
    let bootstrap_sha256 = lillux::sha256_hex(&bootstrap_bytes);
    let mailbox_root = bootstrap.relay_mailbox_root()?;
    let bootstrap_file = write_file(work, "bootstrap", &bootstrap_bytes, 0o600)?;

    let known_regular =
        measurement.total_bytes + member_bytes + bootstrap_bytes.len() as u64 + delivered_bytes;
    let maximum_regular_bytes = known_regular + 64 * 1024;
    let maximum_framed_bytes = maximum_regular_bytes * 2 + 16 * 1024 * 1024;
    let expected = GuestStagingExpected {
        inputs: &projection,
        activation_request_digest: &activation_request_digest,
        bootstrap_sha256: &bootstrap_sha256,
        supervisor_sha256: &supervisor_sha256,
        launcher_sha256: &launcher_sha256,
        maximum_regular_bytes,
        maximum_framed_bytes,
    };
    let authority = ExternalGuestInputAuthority::new(
        projection.clone(),
        base_authority,
        None,
        input_authorities,
        record_authorities,
        Vec::new(),
    )?;
    let package = prepare_private_guest_package(
        work,
        &authority,
        &bootstrap_file.inherited_descriptor_authority()?,
        &supervisor.inherited_descriptor_authority()?,
        &launcher.inherited_descriptor_authority()?,
        &expected,
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(600)),
    )?;

    let ticket = GuestImportTicket {
        schema: GUEST_IMPORT_TICKET_SCHEMA,
        binding_hash: execution_binding_hash.clone(),
        allocation_request_digest: allocation_request_digest.clone(),
        occurrence_id: occurrence_id.clone(),
        activation_request_digest: activation_request_digest.clone(),
        guest_input_identity,
        payload_sha256: package.sha256().to_owned(),
        manifest_sha256: package.manifest_sha256().to_owned(),
        framed_bytes: package.bytes(),
        regular_bytes: package.manifest().total_regular_bytes,
        bootstrap_sha256,
        supervisor_sha256,
        launcher_sha256,
        maximum_regular_bytes,
        maximum_framed_bytes,
    };
    let assignment = GuestOccurrenceAssignment {
        placement_thread_id: PLACEMENT_ID,
        admitted_capsule_hash: &admitted_capsule_hash,
        base_snapshot_hash: &measurement.snapshot_hash,
        execution_binding_hash: &execution_binding_hash,
        allocation_request_digest: &allocation_request_digest,
        occurrence_id: &occurrence_id,
        activation_request_digest: &activation_request_digest,
        supervisor_runtime_hash: &supervisor_runtime_hash,
        guest_runtime_manifest_hash: &runtime_manifest_hash,
        attachment_deadline_ms,
    };
    let document = GuestOccurrenceAssignmentDocument {
        schema: GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
        placement_thread_id: PLACEMENT_ID.into(),
        admitted_capsule_hash: admitted_capsule_hash.clone(),
        base_snapshot_hash: measurement.snapshot_hash.clone(),
        execution_binding_hash: execution_binding_hash.clone(),
        allocation_request_digest: allocation_request_digest.clone(),
        occurrence_id: occurrence_id.clone(),
        activation_request_digest: activation_request_digest.clone(),
        supervisor_runtime_hash: supervisor_runtime_hash.clone(),
        guest_runtime_manifest_hash: runtime_manifest_hash.clone(),
        owner_public_key_hex: hex::encode(controller_key.verifying_key().to_bytes()),
        controller_public_key_hex: hex::encode(root_key.verifying_key().to_bytes()),
        attachment_deadline_ms,
    };
    let signed_assignment = ryeos_external_execution_contract::canonical_json(
        &sign_guest_occurrence_assignment(document, &root_key)?,
    )?;
    let authorization = GuestImportAuthorization {
        schema: GUEST_IMPORT_AUTHORIZATION_SCHEMA,
        placement_thread_id: PLACEMENT_ID.into(),
        admitted_capsule_hash: admitted_capsule_hash.clone(),
        base_snapshot_hash: measurement.snapshot_hash.clone(),
        execution_binding_hash: execution_binding_hash.clone(),
        allocation_request_digest: allocation_request_digest.clone(),
        occurrence_id: occurrence_id.clone(),
        activation_request_digest: activation_request_digest.clone(),
        supervisor_runtime_hash: supervisor_runtime_hash.clone(),
        guest_runtime_manifest_hash: runtime_manifest_hash.clone(),
        attachment_deadline_ms,
        admission_deadline_ms: attachment_deadline_ms - 1_000,
        max_clock_skew_ms: None,
        nonce_sha256: lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>()),
        ticket,
        guest_inputs: projection.clone(),
    };
    let signed_import = ryeos_external_execution_contract::canonical_json(
        &sign_guest_import_authorization(authorization, &controller_key, &assignment)?,
    )?;
    let package_extent = GuestStartRehearsalPackage {
        framed_bytes: package.bytes(),
        regular_bytes: package.manifest().total_regular_bytes,
        entries: package.manifest().entries.len() as u64,
    };

    let activation = work.create_child(OsStr::new(ACTIVATION_NAME), 0o700)?;
    write_file(&activation, IMPORT_NAME, &signed_import, 0o600)?;
    copy_file(&activation, PACKAGE_NAME, package.payload(), 0o600)?;
    package.discard()?;
    let plan = HostPlan {
        mode: HostMode::AttachProbe,
        occurrence_id,
        mailbox_root,
        runtime_manifest_hash,
        assignment_b64: URL_SAFE_NO_PAD.encode(&signed_assignment),
        attach_budget_ms,
        owner_deadline_ms,
        package: package_extent,
        answer_attach: if request.answer_attach {
            let lillux::GuestRuntimeAccount::Unix { uid, gid } = request.profile.account;
            Some(AttachAnswer {
                controller_key_hex: hex::encode(controller_key.to_bytes()),
                bootstrap_b64: STANDARD.encode(&bootstrap_bytes),
                uid,
                gid,
            })
        } else {
            None
        },
    };
    write_file(work, PLAN_NAME, &serde_json::to_vec(&plan)?, 0o600)?;
    Ok(Prepared {
        plan,
        attachment_deadline_ms,
    })
}

/// Refuse where the host cannot create the enclosure the owner needs.
pub(crate) fn host_refusal(
    profile: &GuestOwnerRuntimeProfile,
    work_parent: &Path,
) -> Option<String> {
    let facts = lillux::process_control::HostFacts::observe(0, work_parent);
    if !facts.unshare_user_pid_mount.allowed() {
        return Some(format!(
            "this host does not allow unprivileged user, PID and mount namespaces: {:?}",
            facts.unshare_user_pid_mount
        ));
    }
    let lillux::GuestRuntimeAccount::Unix { uid, gid } = profile.account;
    lillux::root_enclosure::root_enclosure_unavailable_reason(&enclosure_spec(uid, gid, 1))
}

fn enclosure_spec(
    uid: u32,
    gid: u32,
    package_bytes: u64,
) -> lillux::root_enclosure::RootEnclosureSpec<'static> {
    lillux::root_enclosure::RootEnclosureSpec {
        account_uid: uid,
        account_gid: gid,
        root_limits: lillux::sandbox::TmpfsLimits {
            size_bytes: package_bytes
                .saturating_mul(2)
                .saturating_add(1024 * 1024 * 1024),
            inodes: 1 << 18,
        },
        host_entries: HOST_ENTRIES,
    }
}

/// Files of an exported rehearsal, for a real sandbox host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestStartRehearsalExport {
    pub directory: String,
    pub occurrence_id: String,
    pub expected_mailbox: String,
    pub runtime_manifest_hash: String,
    pub attachment_deadline_ms: i64,
    /// Whether the exported host answers the attach request itself.
    pub answers_attach: bool,
    pub package: GuestStartRehearsalPackage,
    /// Each exported file and its size in bytes.
    pub files: BTreeMap<String, u64>,
}

/// Name of the host executable beside an export's `run.sh`.
const EXPORT_HOST_NAME: &str = "rehearsal-host";

const EXPORT_RUN_SCRIPT: &str = include_str!("guest_start_rehearsal_export_run.sh");

/// Prepare one rehearsal exactly as [`run_guest_start_rehearsal`] would, but
/// write what a real sandbox host needs to `directory` (which must not exist)
/// instead of running it: the runtime as the same deterministic tar a real
/// runtime upload carries, the signed import, the framed package, the
/// assignment, the host plan (with the answer-attach authority when the
/// request asks for it), the host executable and `run.sh`. `run.sh` lays out
/// `/ryeos` as an activation does, then runs the host executable, which
/// starts the owner and observes it exactly as the local rehearsal does.
pub fn export_guest_start_rehearsal(
    request: &GuestStartRehearsalRequest<'_>,
    directory: &Path,
) -> Result<GuestStartRehearsalExport> {
    let parent = directory
        .parent()
        .context("rehearsal export directory has no parent")?;
    let name = directory
        .file_name()
        .context("rehearsal export directory has no name")?;
    let parent = lillux::PinnedDirectory::open(parent)?
        .with_context(|| format!("rehearsal export parent {} is absent", parent.display()))?;
    let export = parent.create_child(name, 0o700)?;
    let (work_name, work) = export.create_unique_child(".prepare", 0o700)?;
    let result = (|| -> Result<GuestStartRehearsalExport> {
        let prepared = prepare(request, &work)?;
        let plan = &prepared.plan;
        let runtime = work
            .open_child_directory(OsStr::new(RUNTIME_NAME))?
            .context("rehearsal runtime is absent")?;
        let upload = crate::guest_runtime_product::seal_guest_owner_snapshot_upload(
            &runtime,
            &plan.runtime_manifest_hash,
            64 * 1024 * 1024,
        )?;
        let (tar, _) = upload
            .descriptor()
            .read_regular_file_stable_bounded(upload.bytes())?;
        write_file(&export, "guest-runtime.tar", &tar, 0o644)?;
        let activation = work
            .open_child_directory(OsStr::new(ACTIVATION_NAME))?
            .context("rehearsal activation is absent")?;
        for name in [IMPORT_NAME, PACKAGE_NAME] {
            copy_file(
                &export,
                name,
                &activation
                    .open_pinned_regular(OsStr::new(name), false)?
                    .with_context(|| format!("rehearsal {name} is absent"))?,
                0o644,
            )?;
        }
        write_file(&export, PLAN_NAME, &serde_json::to_vec(plan)?, 0o600)?;
        write_file(
            &export,
            "assignment.b64",
            plan.assignment_b64.as_bytes(),
            0o644,
        )?;
        write_file(
            &export,
            "expected-mailbox",
            plan.mailbox_root.as_bytes(),
            0o644,
        )?;
        copy_file(
            &export,
            EXPORT_HOST_NAME,
            &open_source(request.host_executable, "rehearsal host executable")?,
            0o755,
        )?;
        write_file(&export, "run.sh", EXPORT_RUN_SCRIPT.as_bytes(), 0o755)?;
        let mut files = BTreeMap::new();
        for name in [
            "guest-runtime.tar",
            IMPORT_NAME,
            PACKAGE_NAME,
            PLAN_NAME,
            "assignment.b64",
            "expected-mailbox",
            EXPORT_HOST_NAME,
            "run.sh",
        ] {
            let file = export
                .open_pinned_regular(OsStr::new(name), false)?
                .with_context(|| format!("exported {name} vanished"))?;
            files.insert(name.to_owned(), file.observation()?.size());
        }
        Ok(GuestStartRehearsalExport {
            directory: export.path().to_string_lossy().into_owned(),
            occurrence_id: plan.occurrence_id.clone(),
            expected_mailbox: plan.mailbox_root.clone(),
            runtime_manifest_hash: plan.runtime_manifest_hash.clone(),
            attachment_deadline_ms: prepared.attachment_deadline_ms,
            answers_attach: plan.answer_attach.is_some(),
            package: plan.package.clone(),
            files,
        })
    })();
    work.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(1_000_000, 64))?;
    export.remove_empty_child_if_same(&work_name, &work)?;
    result
}

/// Run one rehearsal. Every refusal, including one before the owner starts,
/// is an outcome; an error means the rehearsal itself could not report.
pub fn run_guest_start_rehearsal(
    request: &GuestStartRehearsalRequest<'_>,
) -> Result<GuestStartRehearsalOutcome> {
    if let Some(reason) = host_refusal(&request.profile, request.work_parent) {
        return Ok(GuestStartRehearsalOutcome::refused(
            GuestStartRehearsalStep::Host,
            reason,
        ));
    }
    let parent = lillux::PinnedDirectory::open(request.work_parent)?.with_context(|| {
        format!(
            "rehearsal work parent {} is absent",
            request.work_parent.display()
        )
    })?;
    let (name, work) = parent.create_unique_child("guest-start-rehearsal", 0o700)?;
    let result = (|| -> Result<GuestStartRehearsalOutcome> {
        let prepared = match prepare(request, &work) {
            Ok(prepared) => prepared,
            Err(error) => {
                return Ok(GuestStartRehearsalOutcome::refused(
                    GuestStartRehearsalStep::Prepare,
                    format!("{error:#}"),
                ));
            }
        };
        let timeout = (prepared
            .plan
            .owner_deadline_ms
            .max(prepared.plan.attach_budget_ms)
            + OWNER_CLEANUP.as_millis() as u64
            + 120_000) as f64
            / 1_000.0;
        let output = lillux::run(lillux::SubprocessRequest {
            cmd: request.host_executable.to_string_lossy().into_owned(),
            argv0: None,
            args: Vec::new(),
            cwd: None,
            envs: vec![(
                REHEARSAL_HOST_ENV.into(),
                work.path().to_string_lossy().into_owned(),
            )],
            stdin_data: None,
            timeout,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        });
        let line = output
            .stdout
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty());
        match line.map(serde_json::from_str::<GuestStartRehearsalOutcome>) {
            Some(Ok(outcome)) => Ok(outcome),
            _ => Ok(GuestStartRehearsalOutcome::Refused {
                step: GuestStartRehearsalStep::Prepare,
                message: format!(
                    "the rehearsal host reported no outcome (exit {}, timed out {}): {}",
                    output.exit_code,
                    output.timed_out,
                    output.stderr.trim()
                ),
                phases: GuestStartRehearsalPhases::default(),
                package: Some(prepared.plan.package),
                owner_stderr: String::new(),
            }),
        }
    })();
    let cleanup = work
        .remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(1_000_000, 64))
        .and_then(|_| parent.remove_empty_child_if_same(&name, &work).map(|_| ()));
    match (result, cleanup) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error.context("rehearsal work directory cleanup failed")),
        (Err(error), _) => Err(error),
    }
}

/// Rehearsal host entry. Returns at once unless [`REHEARSAL_HOST_ENV`] is
/// set; otherwise it runs the rehearsal, prints its outcome as one JSON line
/// and exits. Call it before the process starts any thread.
pub fn run_rehearsal_host_if_requested() {
    if let Some(export) = std::env::var_os(REHEARSAL_REPLAY_ENV) {
        let code = replay_export(Path::new(&export)).unwrap_or_else(|error| {
            eprintln!("the export could not be replayed: {error:#}");
            2
        });
        std::process::exit(code);
    }
    if let Some(export) = std::env::var_os(REHEARSAL_ON_HOST_ENV) {
        let outcome = on_host(Path::new(&export)).unwrap_or_else(|error| {
            GuestStartRehearsalOutcome::refused(
                GuestStartRehearsalStep::Prepare,
                format!("{error:#}"),
            )
        });
        report(&outcome);
        std::process::exit(0);
    }
    let Some(work) = std::env::var_os(REHEARSAL_HOST_ENV) else {
        return;
    };
    let outcome = match host(Path::new(&work)) {
        Ok(outcome) => outcome,
        Err(error) => GuestStartRehearsalOutcome::refused(
            GuestStartRehearsalStep::Prepare,
            format!("{error:#}"),
        ),
    };
    report(&outcome);
    std::process::exit(0);
}

/// Run an export's `run.sh` inside a root enclosure built for its runtime's
/// account, with the export copied in beside `/ryeos`. Returns the script's
/// exit code, after printing its output.
fn replay_export(export: &Path) -> Result<i32> {
    let directory = lillux::PinnedDirectory::open(export)?.context("rehearsal export is absent")?;
    let tar_file = directory
        .open_pinned_regular(OsStr::new("guest-runtime.tar"), false)?
        .context("rehearsal export has no runtime tar")?;
    let tar_bytes = tar_file
        .read_stable_bounded(&tar_file.observation()?, 64 * 1024 * 1024)
        .map_err(anyhow::Error::msg)?;
    let mut profile = None;
    for entry in tar::Archive::new(tar_bytes.as_slice()).entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name() == Some(OsStr::new("guest-owner-profile.json")) {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes)?;
            profile = Some(serde_json::from_slice::<GuestOwnerRuntimeProfile>(&bytes)?);
        }
    }
    let lillux::GuestRuntimeAccount::Unix { uid, gid } = profile
        .context("exported runtime has no owner profile")?
        .account;
    let mut files = Vec::new();
    for entry in std::fs::read_dir(export)? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|name| anyhow::anyhow!("export file name {name:?} is not text"))?;
        let file = directory
            .open_pinned_regular(OsStr::new(&name), false)?
            .with_context(|| format!("export file {name:?} vanished"))?;
        let mode = if name == "run.sh" || name == "rehearsal-host" {
            0o755
        } else {
            0o644
        };
        files.push((name, file, mode));
    }
    let package_bytes = files
        .iter()
        .map(|(_, file, _)| file.observation().map(|seen| seen.size()))
        .sum::<Result<u64, _>>()?;
    let spec = enclosure_spec(uid, gid, package_bytes);
    let code = lillux::root_enclosure::run_in_root_enclosure(&spec, || {
        let copied = (|| -> Result<()> {
            let root = lillux::PinnedDirectory::open(Path::new("/"))?
                .context("enclosure root is absent")?;
            let here = root.create_child(OsStr::new("export"), 0o755)?;
            for (name, file, mode) in &files {
                copy_file(&here, name, file, *mode)?;
            }
            Ok(())
        })();
        if let Err(error) = copied {
            eprintln!("the export could not be copied into the enclosure: {error:#}");
            return 2;
        }
        let output = lillux::run(lillux::SubprocessRequest {
            cmd: "/bin/sh".into(),
            argv0: None,
            args: vec!["/export/run.sh".into()],
            cwd: Some("/export".into()),
            envs: Vec::new(),
            stdin_data: None,
            timeout: 900.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        });
        print!("{}", output.stdout);
        eprint!("{}", output.stderr);
        let _ = std::io::stdout().flush();
        output.exit_code
    })
    .map_err(anyhow::Error::msg)?;
    Ok(code)
}

/// Observe the owner on the machine this runs on, which must already hold the
/// `/ryeos` layout an activation leaves (an export's `run.sh` lays it out):
/// there is no enclosure, because the machine is the enclosure.
fn on_host(export: &Path) -> Result<GuestStartRehearsalOutcome> {
    let directory = lillux::PinnedDirectory::open(export)?.context("rehearsal export is absent")?;
    let plan_file = directory
        .open_pinned_regular(OsStr::new(PLAN_NAME), false)?
        .context("rehearsal export has no plan")?;
    let plan: HostPlan = serde_json::from_slice(
        &plan_file
            .read_stable_bounded(&plan_file.observation()?, MAX_PLAN_BYTES)
            .map_err(anyhow::Error::msg)?,
    )?;
    ensure!(
        plan.mode == HostMode::AttachProbe,
        "an exported plan observes the owner until its attach request"
    );
    observe_owner(&plan)
}

pub(crate) fn report(outcome: &GuestStartRehearsalOutcome) {
    match serde_json::to_string(outcome) {
        Ok(line) => println!("{line}"),
        Err(error) => eprintln!("rehearsal outcome is not serializable: {error}"),
    }
    let _ = std::io::stdout().flush();
}

fn host(work_path: &Path) -> Result<GuestStartRehearsalOutcome> {
    let work = lillux::PinnedDirectory::open(work_path)?.context("rehearsal work is absent")?;
    let plan_file = work
        .open_pinned_regular(OsStr::new(PLAN_NAME), false)?
        .context("rehearsal plan is absent")?;
    let plan: HostPlan = serde_json::from_slice(
        &plan_file
            .read_stable_bounded(&plan_file.observation()?, MAX_PLAN_BYTES)
            .map_err(anyhow::Error::msg)?,
    )?;
    let runtime = work
        .open_child_directory(OsStr::new(RUNTIME_NAME))?
        .context("rehearsal runtime is absent")?;
    let observed = ObservedGuestRuntime::observe(&runtime)?;
    ensure!(
        observed.manifest_hash() == plan.runtime_manifest_hash,
        "rehearsal runtime changed before the host started"
    );
    let lillux::GuestRuntimeAccount::Unix { uid, gid } = observed.profile().account;
    let activation = work
        .open_child_directory(OsStr::new(ACTIVATION_NAME))?
        .context("rehearsal activation is absent")?;
    let import = activation
        .open_pinned_regular(OsStr::new(IMPORT_NAME), false)?
        .context("rehearsal import is absent")?;
    let package = activation
        .open_pinned_regular(OsStr::new(PACKAGE_NAME), false)?
        .context("rehearsal package is absent")?;
    let spec = enclosure_spec(uid, gid, plan.package.framed_bytes);
    let mut outcome = None;
    let entered = lillux::root_enclosure::run_in_root_enclosure(&spec, || {
        let laid_out = lay_out(&plan, &runtime, &import, &package);
        match plan.mode {
            HostMode::AttachProbe => {
                let result = laid_out.and_then(|()| observe_owner(&plan));
                report(&result.unwrap_or_else(|error| {
                    GuestStartRehearsalOutcome::refused(
                        GuestStartRehearsalStep::Prepare,
                        format!("{error:#}"),
                    )
                }));
            }
            HostMode::RelayLoopback => {
                crate::guest_relay_loopback::serve_enclosed(&plan, laid_out);
            }
        }
        0
    });
    match entered {
        // The enclosed init printed the outcome itself.
        Ok(_) => std::process::exit(0),
        Err(error) => outcome.get_or_insert(GuestStartRehearsalOutcome::refused(
            GuestStartRehearsalStep::Host,
            error,
        )),
    };
    outcome.context("rehearsal host has no outcome")
}

/// Lay out `/ryeos` as a sandbox host would.
fn lay_out(
    plan: &HostPlan,
    runtime: &lillux::PinnedDirectory,
    import: &lillux::PinnedRegularFile,
    package: &lillux::PinnedRegularFile,
) -> Result<()> {
    let root =
        lillux::PinnedDirectory::open(Path::new("/"))?.context("enclosure root is absent")?;
    let ryeos = root.create_child(OsStr::new("ryeos"), 0o700)?;
    let installed = ryeos.create_child(OsStr::new("guest-runtime"), 0o700)?;
    let bin = installed.create_child(OsStr::new("bin"), 0o700)?;
    let source_bin = runtime
        .open_child_directory(OsStr::new("bin"))?
        .context("rehearsal runtime has no bin")?;
    copy_file(
        &bin,
        OWNER_NAME,
        &source_bin
            .open_pinned_regular(OsStr::new(OWNER_NAME), false)?
            .context("rehearsal runtime has no owner")?,
        0o755,
    )?;
    bin.set_mode(0o755)?;
    copy_file(
        &installed,
        "guest-owner-profile.json",
        &runtime
            .open_pinned_regular(OsStr::new("guest-owner-profile.json"), false)?
            .context("rehearsal runtime has no guest-owner-profile.json")?,
        0o644,
    )?;
    // A host delivers the namespace and runtime roots administrator-private.
    installed.set_mode(0o750)?;
    ensure!(
        ObservedGuestRuntime::observe(&installed)?.manifest_hash() == plan.runtime_manifest_hash,
        "installed rehearsal runtime differs from the prepared runtime"
    );
    let activation = ryeos.create_child(OsStr::new("activation"), 0o700)?;
    copy_file(&activation, IMPORT_NAME, import, 0o600)?;
    copy_file(&activation, PACKAGE_NAME, package, 0o600)?;
    ryeos.set_mode(0o750)?;
    Ok(())
}

fn present(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn non_empty_directory(path: &str) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

fn elapsed_ms(start: std::time::Instant, at: std::time::Instant) -> u64 {
    u64::try_from(at.saturating_duration_since(start).as_millis()).unwrap_or(u64::MAX)
}

/// Start the installed owner exactly as a sandbox host starts it.
pub(crate) fn spawn_owner(
    plan: &HostPlan,
) -> Result<lillux::subordinate_process::SubordinateProcess> {
    lillux::subordinate_process::SubordinateProcess::spawn(
        lillux::subordinate_process::SubordinateProcessRequest {
            cmd: format!("/ryeos/guest-runtime/bin/{OWNER_NAME}"),
            argv0: None,
            args: vec!["--assignment-b64".into(), plan.assignment_b64.clone()],
            cwd: "/".into(),
            envs: Vec::new(),
            limits: None,
            inherited_fds: Vec::new(),
        },
    )
    .map_err(anyhow::Error::msg)
}

fn observe_owner(plan: &HostPlan) -> Result<GuestStartRehearsalOutcome> {
    let occurrence = format!("/ryeos/occurrences/{}", plan.occurrence_id);
    let records = format!("{occurrence}/{OWNER_RECORD_DIRECTORY}");
    let markers = [
        occurrence.clone(),
        format!("{records}/{INSTALL_INTENT}"),
        format!("{records}/{LAUNCH_INTENT}"),
        format!("{occurrence}/{SUPERVISOR_STATE}"),
        format!("{}/{ATTACH_REQUEST}", plan.mailbox_root),
    ];
    let reached = |index: usize| -> bool {
        if index == 3 {
            non_empty_directory(&markers[3])
        } else {
            present(&markers[index])
        }
    };
    let start = std::time::Instant::now();
    let mut owner = spawn_owner(plan)?;
    let _stdout = owner.take_output().map_err(anyhow::Error::msg)?;
    let mut stderr = owner.take_error().map_err(anyhow::Error::msg)?;
    let mut captured = Vec::new();
    let mut drain = |captured: &mut Vec<u8>, slice: lillux::time::Duration| {
        if let Ok(diagnostics) = stderr.drain_until(
            MAX_STDERR_BYTES.saturating_sub(captured.len()),
            lillux::time::MonotonicDeadline::after(slice),
        ) {
            captured.extend_from_slice(&diagnostics.bytes);
        }
    };
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(
        plan.owner_deadline_ms.max(plan.attach_budget_ms) + 60_000,
    ));
    let mut times: [Option<std::time::Instant>; 5] = [None; 5];
    let exited = loop {
        for (index, slot) in times.iter_mut().enumerate() {
            if slot.is_none() && (index == 0 || reached(index)) && reached(index) {
                *slot = Some(std::time::Instant::now());
            }
        }
        if times[4].is_some() {
            break None;
        }
        if let Some(exit) = owner.try_exit().map_err(anyhow::Error::msg)? {
            break Some(exit);
        }
        if deadline.has_elapsed() {
            break None;
        }
        drain(&mut captured, POLL_INTERVAL);
    };
    let mut phases = GuestStartRehearsalPhases::default();
    let mut previous = start;
    for (index, slot) in times.iter().enumerate() {
        let Some(at) = *slot else { break };
        let value = Some(elapsed_ms(previous, at));
        match index {
            0 => phases.admit_ms = value,
            1 => phases.stage_ms = value,
            2 => phases.install_ms = value,
            3 => phases.release_ms = value,
            _ => phases.attach_ms = value,
        }
        phases.total_ms = elapsed_ms(start, at);
        previous = at;
    }
    let reached_count = times.iter().take_while(|slot| slot.is_some()).count();
    if reached_count == times.len() {
        let after_attach = plan.answer_attach.as_ref().map(|answer| {
            answer_and_observe(plan, answer, &mut owner, &mut |slice| {
                drain(&mut captured, slice)
            })
        });
        let cleanup = settle_owner(&mut owner, &mut |slice| drain(&mut captured, slice));
        let attach_budget_ms = plan.attach_budget_ms;
        let attach_window = phases.release_ms.unwrap_or(0) + phases.attach_ms.unwrap_or(0);
        return Ok(GuestStartRehearsalOutcome::AttachPublished {
            within_budget: attach_window <= attach_budget_ms
                && phases.total_ms <= plan.owner_deadline_ms,
            phases,
            attach_budget_ms,
            package: plan.package.clone(),
            after_attach,
            owner_cleanup: cleanup,
            owner_stderr: String::from_utf8_lossy(&captured).into_owned(),
        });
    }
    let step = GuestStartRehearsalStep::OWNER_PHASES[reached_count];
    let message = match exited {
        Some(exit) => {
            drain(&mut captured, lillux::time::Duration::from_millis(500));
            let stderr = String::from_utf8_lossy(&captured);
            let last = stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("");
            format!(
                "the owner exited with {:?} during {step:?}: {last}",
                exit.code
            )
        }
        None => {
            let cleanup = settle_owner(&mut owner, &mut |slice| drain(&mut captured, slice));
            format!("the owner reached no {step:?} record within its deadline; {cleanup}")
        }
    };
    Ok(GuestStartRehearsalOutcome::Refused {
        step,
        message,
        phases,
        package: Some(plan.package.clone()),
        owner_stderr: String::from_utf8_lossy(&captured).into_owned(),
    })
}

/// Time the supervisor has after the attach response to publish its first
/// exchange frame.
const AFTER_ATTACH_WINDOW: lillux::time::Duration = lillux::time::Duration::from_secs(180);

fn answer_and_observe(
    plan: &HostPlan,
    answer: &AttachAnswer,
    owner: &mut lillux::subordinate_process::SubordinateProcess,
    drain: &mut dyn FnMut(lillux::time::Duration),
) -> GuestStartRehearsalAfterAttach {
    let started = std::time::Instant::now();
    if let Err(error) = answer_attach(plan, answer) {
        return GuestStartRehearsalAfterAttach {
            response_ms: elapsed_ms(started, std::time::Instant::now()),
            first_exchange_ms: None,
            outcome: format!("the rehearsal could not answer the attach request: {error:#}"),
        };
    }
    let answered = std::time::Instant::now();
    let exchange = format!("{}/exchange-00001.request.json", plan.mailbox_root);
    let deadline = lillux::time::MonotonicDeadline::after(AFTER_ATTACH_WINDOW);
    let outcome = loop {
        if present(&exchange) {
            break (
                Some(elapsed_ms(answered, std::time::Instant::now())),
                "the supervisor published its first exchange frame".to_owned(),
            );
        }
        match owner.try_exit() {
            Ok(Some(exit)) => {
                drain(lillux::time::Duration::from_millis(500));
                break (
                    None,
                    format!(
                        "the owner exited with {:?} before any exchange frame",
                        exit.code
                    ),
                );
            }
            Ok(None) if deadline.has_elapsed() => {
                break (
                    None,
                    "no exchange frame within the observation window".to_owned(),
                );
            }
            Ok(None) => drain(POLL_INTERVAL),
            Err(error) => break (None, format!("owner exit observation failed: {error}")),
        }
    };
    GuestStartRehearsalAfterAttach {
        response_ms: elapsed_ms(started, answered),
        first_exchange_ms: outcome.0,
        outcome: outcome.1,
    }
}

/// Build and deliver the attach response a controller would: the channel
/// binding over the sealed bootstrap, signed by the rehearsal's own key.
fn answer_attach(plan: &HostPlan, answer: &AttachAnswer) -> Result<()> {
    use ryeos_state::external_execution::transport::{
        EXTERNAL_CHANNEL_TRANSPORT_SCHEMA, ExternalChannelAttachRequest,
        ExternalChannelAttachResponse, ExternalRelayResponseEnvelope, ExternalRelayResponseKind,
    };
    let key_bytes: [u8; 32] = hex::decode(&answer.controller_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("rehearsal controller key has the wrong length"))?;
    let controller_key = lillux::crypto::SigningKey::from_bytes(&key_bytes);
    let bootstrap: ExternalSupervisorBootstrap =
        serde_json::from_slice(&STANDARD.decode(&answer.bootstrap_b64)?)?;
    let request_bytes = std::fs::read(format!("{}/{ATTACH_REQUEST}", plan.mailbox_root))?;
    let request: ExternalChannelAttachRequest = serde_json::from_slice(&request_bytes)?;
    request.validate_for_bootstrap(&bootstrap)?;
    let issued_at_ms = lillux::time::timestamp_millis();
    let execution_deadline_ms =
        issued_at_ms + i64::from(bootstrap.execution_timeout_seconds) * 1_000;
    let max_bytes = bootstrap.channel_max_bytes;
    let binding = ryeos_state::external_execution::ExecutionChannelBinding {
        schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
        execution_mode: bootstrap.candidate_program.execution_mode(),
        placement_thread_id: bootstrap.placement_thread_id.clone(),
        allocation_request_digest: bootstrap.allocation_request_digest.clone(),
        occurrence_id: bootstrap.occurrence_id.clone(),
        admitted_capsule_hash: bootstrap.admitted_capsule_hash.clone(),
        base_snapshot_hash: bootstrap.base_snapshot_hash.clone(),
        execution_binding_hash: bootstrap.execution_binding_hash.clone(),
        supervisor_runtime_hash: bootstrap.supervisor_runtime_hash.clone(),
        candidate_program_digest: bootstrap.candidate_program.digest()?,
        channel_nonce: lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>()),
        owner_public_key: bootstrap.owner_public_key.clone(),
        supervisor_public_key: request.supervisor_public_key.clone(),
        issued_at_ms,
        execution_deadline_ms,
        expires_at_ms: execution_deadline_ms
            + i64::from(bootstrap.post_execution_timeout_seconds) * 1_000,
        candidate_export_max_bytes: bootstrap.candidate_export_max_bytes,
        max_frames: u32::try_from(
            max_bytes
                .div_ceil(ryeos_state::external_execution::MAX_CHUNK_BYTES as u64)
                .saturating_mul(4)
                .clamp(64, 65_536),
        )?,
        max_bytes,
    };
    binding.validate()?;
    let response = ExternalChannelAttachResponse {
        schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
        binding_digest: binding.digest()?,
        binding,
    };
    let envelope = ExternalRelayResponseEnvelope::sign(
        &bootstrap,
        ExternalRelayResponseKind::Attach,
        &lillux::sha256_hex(&request_bytes),
        None,
        &response,
        &controller_key,
    )?;
    crate::guest_relay_loopback::deliver_owned(
        &format!("{}/attach.response.json", plan.mailbox_root),
        &envelope.canonical_bytes()?,
        answer.uid,
        answer.gid,
    )
}

/// Ask the owner to cancel, as a lost controller would, then force it.
pub(crate) fn settle_owner(
    owner: &mut lillux::subordinate_process::SubordinateProcess,
    drain: &mut dyn FnMut(lillux::time::Duration),
) -> String {
    let requested = owner
        .cooperative_termination()
        .and_then(|termination| termination.request().map(|_| ()));
    if let Err(error) = requested {
        return format!("cancellation request failed: {error}");
    }
    let deadline = lillux::time::MonotonicDeadline::after(OWNER_CLEANUP);
    loop {
        match owner.try_exit() {
            Ok(Some(exit)) => {
                drain(lillux::time::Duration::from_millis(200));
                return format!("cancelled; owner exited with {:?}", exit.code);
            }
            Ok(None) if deadline.has_elapsed() => break,
            Ok(None) => drain(POLL_INTERVAL),
            Err(error) => return format!("owner exit observation failed: {error}"),
        }
    }
    match owner.kill_exact_child_until(lillux::time::MonotonicDeadline::after(
        lillux::time::Duration::from_secs(5),
    )) {
        Ok(Some(_)) => "owner ignored cancellation and was killed".into(),
        Ok(None) => "owner survived a kill; the enclosure ends it".into(),
        Err(error) => format!("owner kill failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_wire_is_tagged_and_round_trips() {
        let outcome = GuestStartRehearsalOutcome::refused(GuestStartRehearsalStep::Stage, "full");
        let text = serde_json::to_string(&outcome).unwrap();
        assert!(text.contains(r#""outcome":"refused""#) && text.contains(r#""step":"stage""#));
        assert_eq!(
            serde_json::from_str::<GuestStartRehearsalOutcome>(&text).unwrap(),
            outcome
        );
    }

    #[test]
    fn rehearsal_program_is_a_valid_session_program() {
        let program =
            rehearsal_program(GuestProductManifestKind::Content, &"a".repeat(64), "test").unwrap();
        assert_eq!(program.runtime_manifest_hash, "a".repeat(64));
    }

    #[test]
    fn rehearsal_digests_are_per_run() {
        assert_ne!(digest_of("binding", "one"), digest_of("binding", "two"));
    }
}
