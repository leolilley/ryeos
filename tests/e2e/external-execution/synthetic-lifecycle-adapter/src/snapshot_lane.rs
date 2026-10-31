//! Provider-neutral snapshot production and restored-qualification lane.
//!
//! A "fake guest" is a directory tree per sandbox identifier under the
//! adapter's state root. The tree mirrors the guest filesystem layout: the
//! restored owner product lives at `fs/ryeos/guest-runtime` and delivered
//! verifier archives are unpacked under `fs/ryeos/qualification`. The adapter
//! contacts no provider; it only records what a provider would have observed
//! and runs the delivered verifier executable against that tree.
//!
//! Lillux offers no sandbox option that maps the invoking account to user id
//! zero: its mapped user namespace always uses a non-zero inner account, and
//! its root overlay primitive requires the invoking process to already be
//! root. This lane therefore enters the root overlay only when the adapter is
//! itself effective user zero and the guest runtime root already exists as a
//! directory; otherwise the verifier runs unmapped. An unmapped run records
//! its effective user in the retained run record, and the guest-wire verifier
//! refuses before producing evidence, so the adapter answers uncertain. The
//! synthetic lane cannot prove the owner account drop.

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use base64::Engine as _;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    CONSUMER_VERIFIER_REMOTE_NAME, ConsumerVerifierAdapterObservation,
    MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES, MAX_RESTORATION_VERIFIER_BYTES,
    MAX_RESTORED_OWNER_RESULT_BYTES, MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES,
    MAX_RESTORED_VERIFIER_ADAPTER_RESPONSE_BYTES, RESTORATION_VERIFIER_REMOTE_NAME,
    RemoteVerificationPurpose, RestoredOwnerMeasurement, RestoredVerifierAdapterObservation,
    RestoredVerifierAdapterRequest, RestoredVerifierAdapterResponse,
};
use ryeos_external_execution_contract::runtime_snapshot::{
    MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES, RUNTIME_SNAPSHOT_CREATE_RESULT_SCHEMA,
    RUNTIME_SNAPSHOT_RESULT_SCHEMA, RUNTIME_SNAPSHOT_UPLOAD_RECEIPT_SCHEMA,
    RuntimeSnapshotAdapterRequest, RuntimeSnapshotAdapterResponse,
    RuntimeSnapshotCreateAdapterRequest, RuntimeSnapshotCreateResult, RuntimeSnapshotIntent,
    RuntimeSnapshotLocator, RuntimeSnapshotQualificationAdapterRequest,
    RuntimeSnapshotQualificationAdapterResponse, RuntimeSnapshotQualificationOccurrence,
    RuntimeSnapshotQualificationTerminalObservation,
    RuntimeSnapshotQualificationTerminationAdapterRequest,
    RuntimeSnapshotQualificationTerminationAdapterResponse, RuntimeSnapshotReadinessObservation,
    RuntimeSnapshotReadinessRequest, RuntimeSnapshotUploadAdapterRequest,
    RuntimeSnapshotUploadReceipt,
};
use ryeos_external_execution_contract::{
    LIFECYCLE_BOOTSTRAP_FD_ENV, LIFECYCLE_CREDENTIAL_FD_ENV, LIFECYCLE_GUEST_PACKAGE_FD_ENV,
    LIFECYCLE_PROVIDER_SPEC_FD_ENV, LIFECYCLE_PROVIDER_SPEC_SHA256_ENV, LIFECYCLE_REQUEST_FD_ENV,
    LIFECYCLE_SETTINGS_FD_ENV, LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV, LIFECYCLE_SIGNED_IMPORT_FD_ENV,
    MAX_LIFECYCLE_PROVIDER_SPEC_BYTES, canonical_json, from_json_slice_strict,
};
use serde::{Deserialize, Serialize};

const SNAPSHOTS_DIR: &str = "snapshots";
const UPLOADS_DIR: &str = "uploads";
const SANDBOXES_DIR: &str = "sandboxes";
const TERMINATIONS_DIR: &str = "terminations";
const VERIFIER_RUNS_DIR: &str = "verifier-runs";
const SANDBOX_FILE: &str = "sandbox.json";
const GUEST_FS_DIR: &str = "fs";
const GUEST_ROOT_DIR: &str = "ryeos";
const GUEST_RUNTIME_DIR: &str = "guest-runtime";
/// Where the restoration verifiers look for the owner product.
const GUEST_RUNTIME_ROOT: &str = "/ryeos/guest-runtime";
const UPLOAD_PATH: &str = "/synthetic/snapshot-upload";
const UPLOAD_CONTENT_TYPE: &str = "application/x-tar";
const MAX_STAGED_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_VERIFIER_SECONDS: i64 = 600;
const MAX_OWNER_COMMAND_BYTES: usize = 8192;
const MAX_CONSUMER_COMMAND_BYTES: usize = 9 * 1024;
const MAX_STDERR_RECORD_CHARS: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRecord {
    schema: u32,
    snapshot_id: String,
    operation_id: String,
    intent_digest: String,
    upload_sha256: String,
    upload_bytes: u64,
    source_occurrence_id: String,
    provider_group_id: String,
    created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxRecord {
    schema: u32,
    sandbox_id: String,
    qualification_operation_id: String,
    snapshot_id: String,
    created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminationRecord {
    schema: u32,
    sandbox_id: String,
    operation_id: String,
    terminated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunPhase {
    Claimed,
    Finished,
}

/// Retained testimony of the one verifier execution for an attempt. The
/// effective user and overlay bit are how a test observes that the synthetic
/// lane could not reproduce the guest's administrator account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifierRunRecord {
    schema: u32,
    operation_id: String,
    purpose: String,
    phase: RunPhase,
    exit_code: Option<i32>,
    complete: bool,
    euid: Option<u32>,
    overlay_applied: bool,
    stdout_bytes: u64,
    stderr: String,
}

/// One fully resolved verifier execution inside the fake guest.
#[derive(Debug, Clone)]
pub(crate) struct GuestInvocation {
    pub(crate) executable: PathBuf,
    pub(crate) arguments: Vec<String>,
    pub(crate) runtime_root: PathBuf,
    pub(crate) timeout_seconds: f64,
    pub(crate) stdout_limit: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct GuestRun {
    pub(crate) complete: bool,
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) euid: u32,
    pub(crate) overlay_applied: bool,
}

pub(crate) trait GuestRunner {
    fn run(&self, invocation: &GuestInvocation) -> Result<GuestRun>;
}

/// Runs the delivered executable as a child of this adapter invocation. The
/// adapter is a dedicated single-threaded process, so entering a private
/// mount namespace here changes nothing for any other actor.
pub(crate) struct ProcessGuestRunner;

impl GuestRunner for ProcessGuestRunner {
    fn run(&self, invocation: &GuestInvocation) -> Result<GuestRun> {
        let euid = lillux::effective_uid()?;
        let overlay_applied = euid == 0 && Path::new(GUEST_RUNTIME_ROOT).is_dir();
        if overlay_applied {
            let source = lillux::PinnedDirectory::open(&invocation.runtime_root)?
                .context("fake guest has no restored owner product")?;
            lillux::sandbox::enter_root_overlay_namespace(&[lillux::sandbox::RootOverlay::Bind {
                source: &source,
                target: Path::new(GUEST_RUNTIME_ROOT),
                read_only: true,
            }])
            .map_err(anyhow::Error::msg)?;
        }
        let result = lillux::run(lillux::SubprocessRequest {
            cmd: invocation
                .executable
                .to_str()
                .context("verifier path is not UTF-8")?
                .to_owned(),
            argv0: None,
            args: invocation.arguments.clone(),
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: invocation.timeout_seconds,
            limits: Some(lillux::SubprocessLimits {
                max_stdout_bytes: Some(invocation.stdout_limit),
                max_stderr_bytes: Some(64 * 1024),
                ..lillux::SubprocessLimits::default()
            }),
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        });
        Ok(GuestRun {
            complete: result.success
                && !result.timed_out
                && result.output_limit_exceeded.is_none()
                && !result.stdout_truncated
                && !result.stderr_truncated,
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
            euid,
            overlay_applied,
        })
    }
}

struct SnapshotContext {
    settings: crate::SyntheticSettings,
    root: lillux::PinnedDirectory,
}

fn load_context(settings_digest: &str, provider_spec_digest: &str) -> Result<SnapshotContext> {
    ensure!(
        [
            LIFECYCLE_BOOTSTRAP_FD_ENV,
            LIFECYCLE_SIGNED_IMPORT_FD_ENV,
            LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
            LIFECYCLE_GUEST_PACKAGE_FD_ENV,
        ]
        .into_iter()
        .all(|name| std::env::var_os(name).is_none()),
        "snapshot operation received worker activation authority"
    );
    let spec = crate::read_sealed_env(
        LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let captured = std::env::var(LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("snapshot operation lacks the captured provider spec digest")?;
    ensure!(
        captured == provider_spec_digest && lillux::sha256_hex(&spec) == provider_spec_digest,
        "snapshot provider spec changed its signed handoff"
    );
    let settings_bytes =
        crate::read_sealed_env(LIFECYCLE_SETTINGS_FD_ENV, crate::MAX_SETTINGS_BYTES)?;
    ensure!(
        lillux::sha256_hex(&settings_bytes) == settings_digest,
        "snapshot settings changed their signed identity"
    );
    let settings: crate::SyntheticSettings = serde_json::from_slice(&settings_bytes)?;
    settings.validate()?;
    ensure!(
        crate::canonical_bytes(&settings)? == settings_bytes,
        "snapshot settings are noncanonical"
    );
    let credential =
        crate::read_sealed_env(LIFECYCLE_CREDENTIAL_FD_ENV, crate::MAX_CREDENTIAL_BYTES)?;
    ensure!(
        lillux::sha256_hex(&credential) == settings.expected_credential_sha256,
        "snapshot credential is not the admitted generation"
    );
    let root = crate::open_state_root(&settings)?;
    Ok(SnapshotContext { settings, root })
}

fn read_request<T>(maximum: usize) -> Result<T>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let bytes = crate::read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, maximum)?;
    let request: T = from_json_slice_strict(&bytes, maximum)?;
    ensure!(
        canonical_json(&request)? == bytes,
        "snapshot request is noncanonical"
    );
    Ok(request)
}

fn take_upload(descriptor: u32) -> Result<lillux::InheritedDescriptorAuthority> {
    // SAFETY: the admitted lifecycle runner transferred this exact sealed
    // descriptor once into this single-threaded invocation.
    unsafe { lillux::take_inherited_descriptor_authority(descriptor) }.map_err(anyhow::Error::msg)
}

fn read_upload(
    upload: &lillux::InheritedDescriptorAuthority,
    bytes: u64,
    sha256: &str,
    mode: u32,
) -> Result<Vec<u8>> {
    upload.require_owned_regular()?;
    let observation = upload.regular_file_observation()?;
    ensure!(
        observation.full_permission_mode()? == mode
            && observation.size() == bytes
            && bytes <= MAX_STAGED_ARCHIVE_BYTES
            && upload.digest_regular_file_stable_exact(&observation)? == sha256,
        "sealed upload differs from its retained attempt"
    );
    let (data, _) = upload.read_regular_file_stable_bounded(MAX_STAGED_ARCHIVE_BYTES)?;
    ensure!(
        data.len() as u64 == bytes && lillux::sha256_hex(&data) == sha256,
        "sealed upload changed while it was read"
    );
    Ok(data)
}

fn write_bounded_response<T: Serialize>(response: &T, maximum: usize) -> Result<()> {
    let bytes = canonical_json(response)?;
    ensure!(
        bytes.len() <= maximum,
        "snapshot lane response exceeds its bound"
    );
    println!("{}", String::from_utf8(bytes)?);
    Ok(())
}

fn open_or_create(
    parent: &lillux::PinnedDirectory,
    name: &str,
    mode: u32,
) -> Result<lillux::PinnedDirectory> {
    match parent.open_child_directory(OsStr::new(name))? {
        Some(existing) => Ok(existing),
        None => parent.create_child(OsStr::new(name), mode),
    }
}

fn descend(
    base: &lillux::PinnedDirectory,
    parts: &[OsString],
    mode: u32,
) -> Result<lillux::PinnedDirectory> {
    let mut current = base.try_clone()?;
    for part in parts {
        let next = match current.open_child_directory(part)? {
            Some(existing) => existing,
            None => current.create_child(part, mode)?,
        };
        current = next;
    }
    Ok(current)
}

fn ensure_record<T>(
    directory: &lillux::PinnedDirectory,
    name: &str,
    make: impl FnOnce() -> Result<T>,
) -> Result<T>
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    if directory
        .open_pinned_regular(OsStr::new(name), false)?
        .is_some()
    {
        return crate::read_document(directory, name);
    }
    let value = make()?;
    crate::ensure_document(directory, name, &value)?;
    Ok(value)
}

fn put_blob(directory: &lillux::PinnedDirectory, name: &str, bytes: &[u8]) -> Result<()> {
    if let Some(existing) = directory.open_pinned_regular(OsStr::new(name), false)? {
        ensure!(
            lillux::sha256_hex(&existing.read_bounded(MAX_STAGED_ARCHIVE_BYTES)?)
                == lillux::sha256_hex(bytes),
            "synthetic retained blob changed"
        );
        return Ok(());
    }
    directory.atomic_write_pinned_if_same(
        OsStr::new(name),
        None,
        bytes,
        0o600,
        crate::SYNTHETIC_TEMP_PURPOSE,
    )
}

fn get_blob(directory: &lillux::PinnedDirectory, name: &str) -> Result<Vec<u8>> {
    directory
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("synthetic retained blob {name} is absent"))?
        .read_bounded(MAX_STAGED_ARCHIVE_BYTES)
}

fn snapshot_id(intent_digest: &str) -> String {
    format!("snp-{}", &intent_digest[..40])
}

fn sandbox_id(qualification_operation_id: &str) -> String {
    format!("sbx-{}", &qualification_operation_id[..40])
}

fn execution_id(kind: &str, operation_id: &str) -> String {
    format!(
        "exe-{kind}-{}",
        &lillux::sha256_hex(operation_id.as_bytes())[..24]
    )
}

/// Unpack a plain tar of regular files and directories below `destination`.
/// Every name is a normalized relative path; links, devices and anything
/// that could address outside the destination are refused.
fn unpack_archive(destination: &lillux::PinnedDirectory, archive: &[u8]) -> Result<()> {
    let mut reader = tar::Archive::new(archive);
    let mut count = 0usize;
    for entry in reader.entries()? {
        let mut entry = entry?;
        count += 1;
        ensure!(count <= MAX_ARCHIVE_ENTRIES, "archive has too many entries");
        let kind = entry.header().entry_type();
        let mode = entry.header().mode()? & 0o777;
        let path = entry.path()?.into_owned();
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(name) => parts.push(name.to_os_string()),
                Component::CurDir => {}
                _ => bail!("archive entry escapes its destination"),
            }
        }
        ensure!(!parts.is_empty(), "archive entry has no name");
        match kind {
            tar::EntryType::Directory => {
                descend(destination, &parts, 0o755)?;
            }
            tar::EntryType::Regular => {
                let name = parts.pop().context("archive file has no name")?;
                let parent = descend(destination, &parts, 0o755)?;
                let mut data = Vec::new();
                entry
                    .by_ref()
                    .take(MAX_STAGED_ARCHIVE_BYTES + 1)
                    .read_to_end(&mut data)?;
                ensure!(
                    data.len() as u64 <= MAX_STAGED_ARCHIVE_BYTES,
                    "archive file exceeds its bound"
                );
                parent.atomic_write_pinned_if_same(
                    &name,
                    None,
                    &data,
                    mode,
                    crate::SYNTHETIC_TEMP_PURPOSE,
                )?;
            }
            _ => bail!("archive entry kind is unsupported"),
        }
    }
    Ok(())
}

fn creation_observation(record: &SnapshotRecord) -> serde_json::Value {
    serde_json::json!({
        "created_at": record.created_at,
        "intent_digest": record.intent_digest,
        "kind": "synthetic_snapshot_creation",
        "operation_id": record.operation_id,
        "snapshot_id": record.snapshot_id,
        "upload_sha256": record.upload_sha256,
    })
}

fn bind_locator(
    intent: &RuntimeSnapshotIntent,
    record: &SnapshotRecord,
) -> Result<RuntimeSnapshotLocator> {
    let observation = creation_observation(record);
    let locator = RuntimeSnapshotLocator {
        schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
        operation_id: intent.operation_id.clone(),
        intent_digest: intent.digest()?,
        source_occurrence_id: intent.source_occurrence_id.clone(),
        provider_group_id: intent.provider_group_id.clone(),
        snapshot_id: record.snapshot_id.clone(),
        provider_response_sha256: crate::evidence_digest("snapshot_creation", &observation)?,
        adapter_observation_sha256: lillux::sha256_hex(&canonical_json(&observation)?),
        provider_creation_observation: observation,
    };
    locator.validate_for(intent)?;
    Ok(locator)
}

fn stage_upload(
    root: &lillux::PinnedDirectory,
    intent: &RuntimeSnapshotIntent,
    bytes: &[u8],
) -> Result<()> {
    ensure!(
        bytes.len() as u64 == intent.upload_bytes
            && lillux::sha256_hex(bytes) == intent.upload_sha256,
        "snapshot upload differs from its retained intent"
    );
    put_blob(
        &open_or_create(root, UPLOADS_DIR, 0o700)?,
        &format!("{}.tar", intent.operation_id),
        bytes,
    )
}

fn create_snapshot(
    root: &lillux::PinnedDirectory,
    intent: &RuntimeSnapshotIntent,
) -> Result<SnapshotRecord> {
    let intent_digest = intent.digest()?;
    let uploads = open_or_create(root, UPLOADS_DIR, 0o700)?;
    let staged = get_blob(&uploads, &format!("{}.tar", intent.operation_id))?;
    ensure!(
        staged.len() as u64 == intent.upload_bytes
            && lillux::sha256_hex(&staged) == intent.upload_sha256,
        "staged snapshot upload differs from its retained intent"
    );
    let snapshots = open_or_create(root, SNAPSHOTS_DIR, 0o700)?;
    let id = snapshot_id(&intent_digest);
    ensure_record(&snapshots, &format!("{id}.json"), || {
        Ok(SnapshotRecord {
            schema: 1,
            snapshot_id: id.clone(),
            operation_id: intent.operation_id.clone(),
            intent_digest: intent_digest.clone(),
            upload_sha256: intent.upload_sha256.clone(),
            upload_bytes: intent.upload_bytes,
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            created_at: lillux::time::iso8601_now(),
        })
    })
}

fn load_snapshot(root: &lillux::PinnedDirectory, id: &str) -> Result<SnapshotRecord> {
    ensure!(
        id.starts_with("snp-")
            && id.len() <= 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "snapshot identity is invalid"
    );
    let snapshots = open_or_create(root, SNAPSHOTS_DIR, 0o700)?;
    let record: SnapshotRecord = crate::read_document(&snapshots, &format!("{id}.json"))?;
    ensure!(record.snapshot_id == id, "snapshot record changed identity");
    Ok(record)
}

// ---------------------------------------------------------------------------
// Snapshot production.

/// One-shot production: stage the upload and create the snapshot together.
pub(crate) fn produce_core(
    root: &lillux::PinnedDirectory,
    request: &RuntimeSnapshotAdapterRequest,
    bytes: &[u8],
) -> Result<RuntimeSnapshotAdapterResponse> {
    request.validate()?;
    stage_upload(root, &request.intent, bytes)?;
    let record = create_snapshot(root, &request.intent)?;
    let response = RuntimeSnapshotAdapterResponse::Bound {
        locator: bind_locator(&request.intent, &record)?,
    };
    response.validate_for(request)?;
    Ok(response)
}

pub(crate) fn upload_core(
    root: &lillux::PinnedDirectory,
    request: &RuntimeSnapshotUploadAdapterRequest,
    bytes: &[u8],
) -> Result<RuntimeSnapshotUploadReceipt> {
    request.validate()?;
    stage_upload(root, &request.intent, bytes)?;
    let receipt = RuntimeSnapshotUploadReceipt {
        schema: RUNTIME_SNAPSHOT_UPLOAD_RECEIPT_SCHEMA,
        upload_operation_id: request.stage.operation_id.clone(),
        upload_intent_digest: request.stage.digest()?,
        parent_operation_id: request.intent.operation_id.clone(),
        parent_intent_digest: request.intent.digest()?,
        source_occurrence_id: request.intent.source_occurrence_id.clone(),
        provider_group_id: request.intent.provider_group_id.clone(),
        upload_path: UPLOAD_PATH.into(),
        content_type: UPLOAD_CONTENT_TYPE.into(),
        upload_sha256: request.intent.upload_sha256.clone(),
        upload_bytes: request.intent.upload_bytes,
        source_response_sha256: crate::evidence_digest(
            "upload_source",
            &request.intent.upload_sha256,
        )?,
        provider_response_sha256: crate::evidence_digest(
            "upload_provider",
            &request.intent.upload_sha256,
        )?,
        provider_status: 200,
        completed_at_ms: lillux::time::timestamp_millis(),
    };
    receipt.validate_observation_for_stage(&request.intent, &request.stage)?;
    Ok(receipt)
}

pub(crate) fn create_core(
    root: &lillux::PinnedDirectory,
    request: &RuntimeSnapshotCreateAdapterRequest,
) -> Result<RuntimeSnapshotCreateResult> {
    request.validate()?;
    ensure!(
        request.accepted_upload.upload_path == UPLOAD_PATH
            && request.accepted_upload.content_type == UPLOAD_CONTENT_TYPE,
        "snapshot create upload receipt differs from the synthetic route"
    );
    let record = create_snapshot(root, &request.intent)?;
    let result = RuntimeSnapshotCreateResult {
        schema: RUNTIME_SNAPSHOT_CREATE_RESULT_SCHEMA,
        create_operation_id: request.create_stage.operation_id.clone(),
        create_intent_digest: request.create_stage.digest()?,
        accepted_upload_receipt_digest: request.accepted_upload.digest()?,
        locator: bind_locator(&request.intent, &record)?,
    };
    result.validate_for(request)?;
    Ok(result)
}

pub(crate) fn readiness_core(
    root: &lillux::PinnedDirectory,
    request: &RuntimeSnapshotReadinessRequest,
) -> Result<RuntimeSnapshotReadinessObservation> {
    request.validate()?;
    let record = load_snapshot(root, &request.locator.snapshot_id)?;
    ensure!(
        record.intent_digest == request.intent.digest()?
            && record.operation_id == request.intent.operation_id,
        "snapshot readiness differs from the retained snapshot"
    );
    let observation = RuntimeSnapshotReadinessObservation {
        schema: 1,
        operation_id: request.intent.operation_id.clone(),
        intent_digest: request.intent.digest()?,
        snapshot_id: record.snapshot_id.clone(),
        source_occurrence_id: request.intent.source_occurrence_id.clone(),
        provider_group_id: request.intent.provider_group_id.clone(),
        creation_response_sha256: request.locator.provider_response_sha256.clone(),
        readiness_response_sha256: crate::evidence_digest("snapshot_readiness", &record)?,
        captured_at: record.created_at.clone(),
        size_bytes: i64::try_from(record.upload_bytes)?,
    };
    observation.validate_for(request)?;
    Ok(observation)
}

pub(crate) fn produce_snapshot(adapter: &lillux::InheritedDescriptorAuthority) -> Result<()> {
    let request: RuntimeSnapshotAdapterRequest =
        read_request(MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    crate::verify_artifact(
        adapter,
        &request.intent.adapter_artifact_hash,
        None,
        "adapter",
    )?;
    let context = load_context(
        &request.intent.settings_digest,
        &request.provider_spec_digest,
    )?;
    let upload = take_upload(request.upload_descriptor)?;
    let bytes = read_upload(&upload, request.upload_bytes, &request.upload_sha256, 0o400)?;
    crate::write_response(&produce_core(&context.root, &request, &bytes)?)
}

pub(crate) fn produce_snapshot_upload(
    adapter: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    let request: RuntimeSnapshotUploadAdapterRequest =
        read_request(MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    crate::verify_artifact(
        adapter,
        &request.intent.adapter_artifact_hash,
        None,
        "adapter",
    )?;
    let context = load_context(
        &request.intent.settings_digest,
        &request.provider_spec_digest,
    )?;
    let upload = take_upload(request.upload_descriptor)?;
    let bytes = read_upload(&upload, request.upload_bytes, &request.upload_sha256, 0o400)?;
    crate::write_response(&upload_core(&context.root, &request, &bytes)?)
}

pub(crate) fn produce_snapshot_create(
    adapter: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    let request: RuntimeSnapshotCreateAdapterRequest =
        read_request(MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    crate::verify_artifact(
        adapter,
        &request.intent.adapter_artifact_hash,
        None,
        "adapter",
    )?;
    let context = load_context(
        &request.intent.settings_digest,
        &request.provider_spec_digest,
    )?;
    crate::write_response(&create_core(&context.root, &request)?)
}

pub(crate) fn observe_snapshot_readiness(
    adapter: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    let request: RuntimeSnapshotReadinessRequest =
        read_request(MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    crate::verify_artifact(
        adapter,
        &request.intent.adapter_artifact_hash,
        None,
        "adapter",
    )?;
    let context = load_context(
        &request.intent.settings_digest,
        &request.provider_spec_digest,
    )?;
    crate::write_response(&readiness_core(&context.root, &request)?)
}

// ---------------------------------------------------------------------------
// Restored sandbox qualification.

/// Restore the retained snapshot into a fresh fake-guest tree.
pub(crate) fn qualify_create_core(
    root: &lillux::PinnedDirectory,
    request: &RuntimeSnapshotQualificationAdapterRequest,
) -> Result<RuntimeSnapshotQualificationAdapterResponse> {
    request.validate()?;
    let snapshot = load_snapshot(root, &request.locator.snapshot_id)?;
    ensure!(
        snapshot.intent_digest == request.locator.intent_digest
            && snapshot.operation_id == request.locator.operation_id
            && request.intent.snapshot_id == snapshot.snapshot_id,
        "qualification create differs from the retained snapshot"
    );
    let id = sandbox_id(&request.intent.operation_id);
    let sandboxes = open_or_create(root, SANDBOXES_DIR, 0o700)?;
    let record = match sandboxes.open_child_directory(OsStr::new(&id))? {
        Some(existing) => crate::read_document(&existing, SANDBOX_FILE)?,
        None => {
            let staged = get_blob(
                &open_or_create(root, UPLOADS_DIR, 0o700)?,
                &format!("{}.tar", snapshot.operation_id),
            )?;
            ensure!(
                lillux::sha256_hex(&staged) == snapshot.upload_sha256,
                "retained snapshot content changed"
            );
            let sandbox = sandboxes.create_child(OsStr::new(&id), 0o700)?;
            let guest_root = descend(
                &sandbox,
                &[
                    GUEST_FS_DIR.into(),
                    GUEST_ROOT_DIR.into(),
                    GUEST_RUNTIME_DIR.into(),
                ],
                0o755,
            )?;
            unpack_archive(&guest_root, &staged)?;
            let record = SandboxRecord {
                schema: 1,
                sandbox_id: id.clone(),
                qualification_operation_id: request.intent.operation_id.clone(),
                snapshot_id: snapshot.snapshot_id.clone(),
                created_at: lillux::time::iso8601_now(),
            };
            crate::ensure_document(&sandbox, SANDBOX_FILE, &record)?;
            record
        }
    };
    let response = RuntimeSnapshotQualificationAdapterResponse::OccurrenceBound {
        occurrence: RuntimeSnapshotQualificationOccurrence {
            schema: 1,
            operation_id: request.intent.operation_id.clone(),
            occurrence_id: record.sandbox_id.clone(),
            provider_response_sha256: crate::evidence_digest("qualification_sandbox", &record)?,
            contact_deadline_exceeded: false,
        },
    };
    response.validate_for(request)?;
    Ok(response)
}

fn require_running_sandbox(
    root: &lillux::PinnedDirectory,
    id: &str,
) -> Result<lillux::PinnedDirectory> {
    let terminated = open_or_create(root, TERMINATIONS_DIR, 0o700)?
        .open_pinned_regular(OsStr::new(&format!("{id}.json")), false)?
        .is_some();
    ensure!(!terminated, "fake guest sandbox is terminated");
    let sandbox = open_or_create(root, SANDBOXES_DIR, 0o700)?
        .open_child_directory(OsStr::new(id))?
        .context("fake guest sandbox is absent")?;
    ensure!(
        sandbox
            .open_pinned_regular(OsStr::new(SANDBOX_FILE), false)?
            .is_some(),
        "fake guest sandbox is not running"
    );
    Ok(sandbox)
}

/// Deliver the archive into the fake guest, run the delivered verifier once
/// and bind its stdout to the attempt. Any refusal is an error; the caller
/// reports it as uncertain, as the provider adapter does.
pub(crate) fn verify_core(
    root: &lillux::PinnedDirectory,
    state_root: &Path,
    request: &RestoredVerifierAdapterRequest,
    upload: &[u8],
    runner: &dyn GuestRunner,
) -> Result<RestoredVerifierAdapterResponse> {
    request.validate()?;
    let id = request.occurrence.occurrence_id.clone();
    let sandbox = require_running_sandbox(root, &id)?;
    let operation_id = request.intent.operation_id.clone();
    let purpose = match &request.intent.purpose {
        RemoteVerificationPurpose::OwnerMeasurement { .. } => "owner_measurement",
        RemoteVerificationPurpose::ConsumerRuntime { .. } => "consumer_runtime",
    };
    // First contact only: the claim exists before anything runs, so a replay
    // can never execute the verifier a second time.
    let runs = open_or_create(root, VERIFIER_RUNS_DIR, 0o700)?;
    let run_file = format!("{operation_id}.json");
    let mut run_record = VerifierRunRecord {
        schema: 1,
        operation_id: operation_id.clone(),
        purpose: purpose.into(),
        phase: RunPhase::Claimed,
        exit_code: None,
        complete: false,
        euid: None,
        overlay_applied: false,
        stdout_bytes: 0,
        stderr: String::new(),
    };
    ensure!(
        runs.open_pinned_regular(OsStr::new(&run_file), false)?
            .is_none(),
        "fake guest verifier attempt was already claimed"
    );
    crate::ensure_document(&runs, &run_file, &run_record)?;

    let remote = request.intent.remote_upload_directory()?;
    let relative: Vec<OsString> = remote
        .trim_start_matches('/')
        .split('/')
        .map(OsString::from)
        .collect();
    let guest_fs = descend(&sandbox, &[GUEST_FS_DIR.into()], 0o755)?;
    let destination = descend(&guest_fs, &relative, 0o755)?;
    unpack_archive(&destination, upload)?;

    let (member, flag, challenge, command_bound, stdout_limit) = match &request.intent.purpose {
        RemoteVerificationPurpose::OwnerMeasurement { challenge } => (
            RESTORATION_VERIFIER_REMOTE_NAME,
            "--challenge-b64",
            canonical_json(challenge)?,
            MAX_OWNER_COMMAND_BYTES,
            MAX_RESTORED_OWNER_RESULT_BYTES as u64 + 2,
        ),
        RemoteVerificationPurpose::ConsumerRuntime { .. } => (
            CONSUMER_VERIFIER_REMOTE_NAME,
            "--consumer-challenge-b64",
            request.consumer_challenge()?.canonical_bytes()?,
            MAX_CONSUMER_COMMAND_BYTES,
            MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES + 2,
        ),
    };
    let delivered = destination
        .open_pinned_regular(OsStr::new(member), false)?
        .context("delivered archive has no verifier executable")?;
    ensure!(
        lillux::sha256_hex(&delivered.read_bounded(MAX_RESTORATION_VERIFIER_BYTES)?)
            == request.intent.verifier_artifact_hash,
        "delivered verifier differs from the selected artifact"
    );
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&challenge);
    ensure!(
        remote.len() + member.len() + flag.len() + encoded.len() + 2 <= command_bound,
        "verifier command exceeds its bound"
    );
    let guest_path = state_root.join(SANDBOXES_DIR).join(&id).join(GUEST_FS_DIR);
    let remaining_ms = request
        .intent
        .attempt_deadline_ms
        .saturating_sub(lillux::time::timestamp_millis());
    ensure!(remaining_ms > 0, "verifier attempt expired before it ran");
    let invocation = GuestInvocation {
        executable: guest_path.join(remote.trim_start_matches('/')).join(member),
        arguments: vec![flag.into(), encoded],
        runtime_root: guest_path.join(GUEST_ROOT_DIR).join(GUEST_RUNTIME_DIR),
        timeout_seconds: (remaining_ms / 1000).clamp(1, MAX_VERIFIER_SECONDS) as f64,
        stdout_limit,
    };
    let run = runner.run(&invocation)?;
    run_record.phase = RunPhase::Finished;
    run_record.exit_code = Some(run.exit_code);
    run_record.complete = run.complete;
    run_record.euid = Some(run.euid);
    run_record.overlay_applied = run.overlay_applied;
    run_record.stdout_bytes = run.stdout.len() as u64;
    run_record.stderr = run.stderr.chars().take(MAX_STDERR_RECORD_CHARS).collect();
    crate::replace_document(&runs, &run_file, &run_record)?;
    ensure!(
        run.complete && run.exit_code == 0,
        "fake guest verifier refused (exit {}, euid {})",
        run.exit_code,
        run.euid
    );
    let output = run
        .stdout
        .strip_suffix('\n')
        .context("verifier output lacks its single terminator")?
        .as_bytes();
    let upload_response = crate::evidence_digest("verifier_upload", &lillux::sha256_hex(upload))?;
    let run_stream = lillux::sha256_hex(run.stdout.as_bytes());

    let response = match &request.intent.purpose {
        RemoteVerificationPurpose::OwnerMeasurement { challenge } => {
            ensure!(
                !output.is_empty() && output.len() <= MAX_RESTORED_OWNER_RESULT_BYTES,
                "owner measurement is empty or exceeds its bound"
            );
            let measurement: RestoredOwnerMeasurement =
                from_json_slice_strict(output, MAX_RESTORED_OWNER_RESULT_BYTES)?;
            ensure!(
                canonical_json(&measurement)? == output,
                "owner measurement is noncanonical"
            );
            measurement.validate_content_for_bound_snapshot(
                challenge,
                &request.source_intent,
                &request.locator,
                &request.readiness,
            )?;
            RestoredVerifierAdapterResponse::Observed {
                observation: Box::new(RestoredVerifierAdapterObservation {
                    schema: 1,
                    operation_id: operation_id.clone(),
                    occurrence_id: id.clone(),
                    upload_token_execution_id: execution_id("upload", &operation_id),
                    run_token_execution_id: execution_id("run", &operation_id),
                    upload_response_sha256: upload_response,
                    run_stream_sha256: run_stream,
                    measurement,
                    contact_deadline_exceeded: false,
                }),
            }
        }
        RemoteVerificationPurpose::ConsumerRuntime { .. } => {
            let maximum = usize::try_from(MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES)?;
            ensure!(
                !output.is_empty() && output.len() <= maximum,
                "consumer evidence is empty or exceeds its bound"
            );
            let evidence: serde_json::Value = from_json_slice_strict(output, maximum)?;
            ensure!(
                evidence.is_object() && canonical_json(&evidence)? == output,
                "consumer evidence is not an exact canonical object"
            );
            RestoredVerifierAdapterResponse::ConsumerObserved {
                observation: Box::new(ConsumerVerifierAdapterObservation {
                    schema: 1,
                    operation_id: operation_id.clone(),
                    occurrence_id: id.clone(),
                    verifier_artifact_hash: request.intent.verifier_artifact_hash.clone(),
                    challenge_digest: request.intent.consumer_challenge_digest()?,
                    upload_token_execution_id: execution_id("upload", &operation_id),
                    run_token_execution_id: execution_id("run", &operation_id),
                    upload_response_sha256: upload_response,
                    run_stream_sha256: run_stream,
                    evidence_sha256: lillux::sha256_hex(output),
                    evidence_bytes: output.len() as u64,
                    contact_deadline_exceeded: false,
                }),
                evidence,
            }
        }
    };
    response.validate_for(request)?;
    Ok(response)
}

/// Remove the fake guest on first contact; an observation never mutates.
pub(crate) fn terminate_core(
    root: &lillux::PinnedDirectory,
    request: &RuntimeSnapshotQualificationTerminationAdapterRequest,
    first_contact: bool,
) -> Result<RuntimeSnapshotQualificationTerminationAdapterResponse> {
    request.validate()?;
    let id = request.intent.occurrence_id.clone();
    ensure!(
        id.starts_with("sbx-")
            && id.len() <= 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "fake guest sandbox identity is invalid"
    );
    let pending = || RuntimeSnapshotQualificationTerminationAdapterResponse::Pending {
        operation_id: request.intent.operation_id.clone(),
    };
    let terminations = open_or_create(root, TERMINATIONS_DIR, 0o700)?;
    let file = format!("{id}.json");
    let record = if terminations
        .open_pinned_regular(OsStr::new(&file), false)?
        .is_some()
    {
        crate::read_document::<TerminationRecord>(&terminations, &file)?
    } else {
        let Some(sandbox) =
            open_or_create(root, SANDBOXES_DIR, 0o700)?.open_child_directory(OsStr::new(&id))?
        else {
            return Ok(pending());
        };
        if !first_contact {
            return Ok(pending());
        }
        sandbox.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
            MAX_ARCHIVE_ENTRIES * 8,
            64,
        ))?;
        let record = TerminationRecord {
            schema: 1,
            sandbox_id: id.clone(),
            operation_id: request.intent.operation_id.clone(),
            terminated_at: lillux::time::iso8601_now(),
        };
        crate::ensure_document(&terminations, &file, &record)?;
        record
    };
    let response = RuntimeSnapshotQualificationTerminationAdapterResponse::Terminal {
        observation: RuntimeSnapshotQualificationTerminalObservation {
            schema: 1,
            operation_id: request.intent.operation_id.clone(),
            occurrence_id: id,
            provider_response_sha256: crate::evidence_digest("qualification_termination", &record)?,
            terminated_at: record.terminated_at,
            contact_deadline_exceeded: false,
        },
    };
    response.validate_for(request)?;
    Ok(response)
}

pub(crate) fn qualify_snapshot_create(
    adapter: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    let request: RuntimeSnapshotQualificationAdapterRequest =
        read_request(MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    crate::verify_artifact(
        adapter,
        &request.intent.adapter_artifact_hash,
        None,
        "adapter",
    )?;
    let context = load_context(
        &request.intent.settings_digest,
        &request.provider_spec_digest,
    )?;
    crate::write_response(&qualify_create_core(&context.root, &request)?)
}

pub(crate) fn qualify_snapshot_verify(
    adapter: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    let request: RestoredVerifierAdapterRequest =
        read_request(MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    crate::verify_artifact(
        adapter,
        &request.qualification_intent.adapter_artifact_hash,
        None,
        "adapter",
    )?;
    let context = load_context(
        &request.qualification_intent.settings_digest,
        &request.provider_spec_digest,
    )?;
    let upload = take_upload(request.upload_descriptor)?;
    let bytes = read_upload(
        &upload,
        request.upload_bytes,
        &request.upload_sha256,
        request.required_upload_mode(),
    )?;
    let response = match verify_core(
        &context.root,
        &context.settings.state_root,
        &request,
        &bytes,
        &ProcessGuestRunner,
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("synthetic restored verifier refused: {error:#}");
            RestoredVerifierAdapterResponse::Uncertain {
                operation_id: request.intent.operation_id.clone(),
                reason: None,
            }
        }
    };
    response.validate_for(&request)?;
    write_bounded_response(&response, MAX_RESTORED_VERIFIER_ADAPTER_RESPONSE_BYTES)
}

pub(crate) fn terminate_snapshot_qualification(
    adapter: &lillux::InheritedDescriptorAuthority,
    first_contact: bool,
) -> Result<()> {
    let request: RuntimeSnapshotQualificationTerminationAdapterRequest =
        read_request(MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES)?;
    request.validate()?;
    crate::verify_artifact(
        adapter,
        &request.qualification_intent.adapter_artifact_hash,
        None,
        "adapter",
    )?;
    let context = load_context(
        &request.qualification_intent.settings_digest,
        &request.provider_spec_digest,
    )?;
    crate::write_response(&terminate_core(&context.root, &request, first_contact)?)
}

/// Retained run records by operation, for tests and diagnostics.
#[cfg(test)]
fn read_run_records(
    root: &lillux::PinnedDirectory,
) -> Result<std::collections::BTreeMap<String, VerifierRunRecord>> {
    let mut records = std::collections::BTreeMap::new();
    let runs = open_or_create(root, VERIFIER_RUNS_DIR, 0o700)?;
    for entry in runs.entries_no_follow()? {
        let name = entry.name.to_string_lossy().into_owned();
        let record: VerifierRunRecord = crate::read_document(&runs, &name)?;
        records.insert(record.operation_id.clone(), record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::restored_runtime_measurement::{
        ConsumerArchiveBudget, ConsumerRuntimeVerificationCoordinate,
        ConsumerRuntimeVerifierSelection, RESTORED_OWNER_MEASUREMENT_PROTOCOL,
        RESTORED_VERIFIER_ADAPTER_PROTOCOL, RestoredOwnerChallenge, RestoredVerifierAttemptIntent,
    };
    use ryeos_external_execution_contract::runtime_snapshot::{
        RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL, RUNTIME_SNAPSHOT_INTENT_SCHEMA,
        RUNTIME_SNAPSHOT_QUALIFICATION_ADAPTER_PROTOCOL, RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA,
        RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL, RUNTIME_SNAPSHOT_READINESS_PROTOCOL,
        RUNTIME_SNAPSHOT_STAGE_SCHEMA, RUNTIME_SNAPSHOT_UPLOAD_ADAPTER_PROTOCOL,
        RuntimeSnapshotQualificationIntent, RuntimeSnapshotQualificationTerminationIntent,
        RuntimeSnapshotSource, RuntimeSnapshotStage, RuntimeSnapshotStageIntent,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Lane {
        _temp: tempfile::TempDir,
        state: PathBuf,
        root: lillux::PinnedDirectory,
    }

    fn lane() -> Lane {
        let temp = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        Lane {
            state: temp.path().to_path_buf(),
            root,
            _temp: temp,
        }
    }

    fn append_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, bytes: &[u8], mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append_data(&mut header, path, bytes).unwrap();
    }

    fn runtime_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "bin/", std::io::empty())
            .unwrap();
        append_file(
            &mut builder,
            "bin/ryeos-external-guest-occurrence-owner",
            b"owner",
            0o755,
        );
        append_file(&mut builder, "controller-root.hex", &[b'a'; 64], 0o644);
        append_file(&mut builder, "guest-owner-profile.json", b"{}", 0o644);
        builder.into_inner().unwrap()
    }

    fn verifier_tar(name: &str, verifier: &[u8], extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        append_file(&mut builder, name, verifier, 0o500);
        for (path, bytes) in extra {
            append_file(&mut builder, path, bytes, 0o400);
        }
        builder.into_inner().unwrap()
    }

    struct Staged {
        source: RuntimeSnapshotIntent,
        locator: RuntimeSnapshotLocator,
        readiness: RuntimeSnapshotReadinessObservation,
        qualification: RuntimeSnapshotQualificationIntent,
        occurrence: RuntimeSnapshotQualificationOccurrence,
    }

    fn source_intent(archive: &[u8]) -> RuntimeSnapshotIntent {
        let mut intent = RuntimeSnapshotIntent {
            schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            provider_id: "synthetic-lifecycle".into(),
            source_occurrence_id: "src-1".into(),
            source_bootstrap_operation_id: None,
            source_created_at: None,
            source_timeout_seconds: None,
            provider_group_id: "group-1".into(),
            production_profile_digest: "2".repeat(64),
            adapter_artifact_hash: "3".repeat(64),
            provider_spec_digest: "4".repeat(64),
            settings_digest: "5".repeat(64),
            source: RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: "6".repeat(64),
            },
            guest_runtime_manifest_hash: "7".repeat(64),
            owner_executable_sha256: "8".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode([9u8; 32])
            ),
            upload_sha256: lillux::sha256_hex(archive),
            upload_bytes: archive.len() as u64,
            attempt_deadline_ms: lillux::time::timestamp_millis() + 600_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        intent
    }

    fn upload_request(
        intent: &RuntimeSnapshotIntent,
    ) -> (
        RuntimeSnapshotStageIntent,
        RuntimeSnapshotUploadAdapterRequest,
    ) {
        let mut stage = RuntimeSnapshotStageIntent {
            schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
            operation_id: String::new(),
            parent_operation_id: intent.operation_id.clone(),
            parent_intent_digest: intent.digest().unwrap(),
            stage: RuntimeSnapshotStage::Upload,
            accepted_upload_receipt_digest: None,
            attempt_deadline_ms: intent.attempt_deadline_ms - 1000,
        };
        stage.operation_id = stage.derived_operation_id().unwrap();
        let request = RuntimeSnapshotUploadAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_UPLOAD_ADAPTER_PROTOCOL.into(),
            intent: intent.clone(),
            stage: stage.clone(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            upload_descriptor: 9,
            upload_bytes: intent.upload_bytes,
            upload_sha256: intent.upload_sha256.clone(),
        };
        (stage, request)
    }

    /// Production through restoration: upload, create, readiness, sandbox.
    fn stage_lane(lane: &Lane, verifier_sha256: &str) -> Staged {
        let archive = runtime_tar();
        let source = source_intent(&archive);
        let (upload_stage, upload) = upload_request(&source);
        let receipt = upload_core(&lane.root, &upload, &archive).unwrap();
        let mut create_stage = RuntimeSnapshotStageIntent {
            schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
            operation_id: String::new(),
            parent_operation_id: source.operation_id.clone(),
            parent_intent_digest: source.digest().unwrap(),
            stage: RuntimeSnapshotStage::Create,
            accepted_upload_receipt_digest: Some(receipt.digest().unwrap()),
            attempt_deadline_ms: source.attempt_deadline_ms - 500,
        };
        create_stage.operation_id = create_stage.derived_operation_id().unwrap();
        let created = create_core(
            &lane.root,
            &RuntimeSnapshotCreateAdapterRequest {
                protocol: RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL.into(),
                intent: source.clone(),
                upload_stage,
                accepted_upload: receipt,
                create_stage,
                provider_spec_digest: source.provider_spec_digest.clone(),
            },
        )
        .unwrap();
        let locator = created.locator;
        let readiness = readiness_core(
            &lane.root,
            &RuntimeSnapshotReadinessRequest {
                protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
                intent: source.clone(),
                locator: locator.clone(),
                provider_spec_digest: source.provider_spec_digest.clone(),
            },
        )
        .unwrap();
        let mut qualification = RuntimeSnapshotQualificationIntent {
            schema: RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA,
            operation_id: String::new(),
            owner_principal: source.owner_principal.clone(),
            snapshot_operation_id: source.operation_id.clone(),
            snapshot_intent_digest: source.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            provider_id: source.provider_id.clone(),
            provider_group_id: source.provider_group_id.clone(),
            qualification_profile_digest: "a".repeat(64),
            adapter_artifact_hash: "b".repeat(64),
            provider_spec_digest: "c".repeat(64),
            settings_digest: "d".repeat(64),
            verifier_artifact_hash: verifier_sha256.into(),
            maximum_lifetime_seconds: 900,
            attempt_deadline_ms: source.attempt_deadline_ms - 100,
        };
        qualification.operation_id = qualification.derived_operation_id().unwrap();
        let bound = qualify_create_core(
            &lane.root,
            &RuntimeSnapshotQualificationAdapterRequest {
                protocol: RUNTIME_SNAPSHOT_QUALIFICATION_ADAPTER_PROTOCOL.into(),
                intent: qualification.clone(),
                source_intent: source.clone(),
                locator: locator.clone(),
                readiness: readiness.clone(),
                provider_spec_digest: qualification.provider_spec_digest.clone(),
            },
        )
        .unwrap();
        let RuntimeSnapshotQualificationAdapterResponse::OccurrenceBound { occurrence } = bound
        else {
            panic!("qualification create did not bind a sandbox");
        };
        Staged {
            source,
            locator,
            readiness,
            qualification,
            occurrence,
        }
    }

    fn owner_request(staged: &Staged, archive: &[u8]) -> RestoredVerifierAdapterRequest {
        let mut intent = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: staged.qualification.operation_id.clone(),
            restored_occurrence_id: staged.occurrence.occurrence_id.clone(),
            verifier_artifact_hash: staged.qualification.verifier_artifact_hash.clone(),
            upload_sha256: lillux::sha256_hex(archive),
            upload_bytes: archive.len() as u64,
            purpose: RemoteVerificationPurpose::OwnerMeasurement {
                challenge: RestoredOwnerChallenge {
                    schema: 1,
                    protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
                    operation_id: staged.source.operation_id.clone(),
                    snapshot_id: staged.locator.snapshot_id.clone(),
                    restored_occurrence_id: staged.occurrence.occurrence_id.clone(),
                    nonce_hex: "9".repeat(64),
                },
            },
            attempt_deadline_ms: staged.source.attempt_deadline_ms - 200,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        verifier_request(staged, intent, None)
    }

    fn consumer_request(
        staged: &Staged,
        archive: &[u8],
        verifier_sha256: &str,
    ) -> RestoredVerifierAdapterRequest {
        let coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-fixture-root".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: "b".repeat(64),
            scenario_id: "guest_wire".into(),
            scenario_source_digest: "c".repeat(64),
            subject_digest: "d".repeat(64),
            use_digest: "e".repeat(64),
            prerequisite_measurement_attempt_id: "f".repeat(64),
            prerequisite_measurement_observation_digest: "1".repeat(64),
        };
        let selection = ConsumerRuntimeVerifierSelection {
            scenario_source_digest: coordinate.scenario_source_digest.clone(),
            verifier_artifact_hash: verifier_sha256.into(),
            archive_budget: ConsumerArchiveBudget::new(16, 4096, 8192).unwrap(),
        };
        let mut intent = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: staged.qualification.operation_id.clone(),
            restored_occurrence_id: staged.occurrence.occurrence_id.clone(),
            verifier_artifact_hash: verifier_sha256.into(),
            upload_sha256: lillux::sha256_hex(archive),
            upload_bytes: archive.len() as u64,
            purpose: RemoteVerificationPurpose::ConsumerRuntime {
                coordinate,
                nonce_hex: "9".repeat(64),
                guest_runtime_manifest_hash: staged.source.guest_runtime_manifest_hash.clone(),
            },
            attempt_deadline_ms: staged.source.attempt_deadline_ms - 200,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        verifier_request(staged, intent, Some(selection))
    }

    fn verifier_request(
        staged: &Staged,
        intent: RestoredVerifierAttemptIntent,
        consumer_selection: Option<ConsumerRuntimeVerifierSelection>,
    ) -> RestoredVerifierAdapterRequest {
        RestoredVerifierAdapterRequest {
            protocol: RESTORED_VERIFIER_ADAPTER_PROTOCOL.into(),
            upload_bytes: intent.upload_bytes,
            upload_sha256: intent.upload_sha256.clone(),
            intent,
            consumer_selection,
            source_intent: staged.source.clone(),
            locator: staged.locator.clone(),
            readiness: staged.readiness.clone(),
            qualification_intent: staged.qualification.clone(),
            occurrence: staged.occurrence.clone(),
            provider_spec_digest: staged.qualification.provider_spec_digest.clone(),
            upload_descriptor: 9,
        }
    }

    struct CannedRunner {
        run: GuestRun,
        calls: AtomicUsize,
        last: Mutex<Option<GuestInvocation>>,
    }

    impl CannedRunner {
        fn new(stdout: String, euid: u32, exit_code: i32) -> Self {
            Self {
                run: GuestRun {
                    complete: exit_code == 0,
                    exit_code,
                    stdout,
                    stderr: "refused".into(),
                    euid,
                    overlay_applied: false,
                },
                calls: AtomicUsize::new(0),
                last: Mutex::new(None),
            }
        }
    }

    impl GuestRunner for CannedRunner {
        fn run(&self, invocation: &GuestInvocation) -> Result<GuestRun> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(invocation.clone());
            Ok(self.run.clone())
        }
    }

    #[test]
    fn production_lane_binds_one_snapshot_and_restores_the_exact_product() {
        let lane = lane();
        let staged = stage_lane(&lane, &"e".repeat(64));
        assert!(staged.locator.snapshot_id.starts_with("snp-"));
        assert_eq!(staged.readiness.snapshot_id, staged.locator.snapshot_id);
        assert!(staged.occurrence.occurrence_id.starts_with("sbx-"));
        assert!(!staged.occurrence.contact_deadline_exceeded);
        // The restored tree mirrors the guest layout and carries the product.
        let owner = lane
            .state
            .join("sandboxes")
            .join(&staged.occurrence.occurrence_id)
            .join("fs/ryeos/guest-runtime/bin/ryeos-external-guest-occurrence-owner");
        assert_eq!(std::fs::read(owner).unwrap(), b"owner");
        // One-shot production binds a snapshot of the same kind.
        let archive = runtime_tar();
        assert!(lane_single_shot(&archive).snapshot_id.starts_with("snp-"));
    }

    fn lane_single_shot(archive: &[u8]) -> RuntimeSnapshotLocator {
        let lane = lane();
        let intent = source_intent(archive);
        let request = RuntimeSnapshotAdapterRequest {
            protocol: ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL
                .into(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            upload_descriptor: 9,
            upload_bytes: intent.upload_bytes,
            upload_sha256: intent.upload_sha256.clone(),
            intent,
        };
        match produce_core(&lane.root, &request, archive).unwrap() {
            RuntimeSnapshotAdapterResponse::Bound { locator } => locator,
            RuntimeSnapshotAdapterResponse::Uncertain { .. } => panic!("uncertain"),
        }
    }

    #[test]
    fn production_refuses_changed_upload_bytes() {
        let lane = lane();
        let archive = runtime_tar();
        let intent = source_intent(&archive);
        let (_, request) = upload_request(&intent);
        let mut changed = archive.clone();
        changed[0] ^= 1;
        assert!(upload_core(&lane.root, &request, &changed).is_err());
    }

    #[test]
    fn owner_measurement_is_bound_to_its_challenge_and_stays_within_bounds() {
        let lane = lane();
        let verifier = b"restoration-verifier".to_vec();
        let verifier_sha = lillux::sha256_hex(&verifier);
        let staged = stage_lane(&lane, &verifier_sha);
        let archive = verifier_tar(RESTORATION_VERIFIER_REMOTE_NAME, &verifier, &[]);
        let request = owner_request(&staged, &archive);
        let RemoteVerificationPurpose::OwnerMeasurement { challenge } = &request.intent.purpose
        else {
            panic!("owner purpose expected");
        };
        let measurement = RestoredOwnerMeasurement {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            challenge_digest: challenge.digest().unwrap(),
            manifest_hash: staged.source.guest_runtime_manifest_hash.clone(),
            owner_executable_sha256: staged.source.owner_executable_sha256.clone(),
            controller_public_root: staged.source.controller_public_root.clone(),
        };
        let stdout = format!(
            "{}\n",
            String::from_utf8(canonical_json(&measurement).unwrap()).unwrap()
        );
        let runner = CannedRunner::new(stdout, 0, 0);
        let response = verify_core(&lane.root, &lane.state, &request, &archive, &runner).unwrap();
        response.validate_for(&request).unwrap();
        assert!(matches!(
            response,
            RestoredVerifierAdapterResponse::Observed { .. }
        ));
        let invocation = runner.last.lock().unwrap().clone().unwrap();
        assert_eq!(invocation.arguments[0], "--challenge-b64");
        assert!(invocation.executable.is_file());
        assert!(invocation.runtime_root.is_dir());
        assert!(
            canonical_json(&response).unwrap().len()
                <= ryeos_external_execution_contract::MAX_LIFECYCLE_RESPONSE_BYTES
        );

        // A replayed attempt never executes the verifier again.
        assert!(verify_core(&lane.root, &lane.state, &request, &archive, &runner).is_err());
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn owner_measurement_of_another_product_is_refused() {
        let lane = lane();
        let verifier = b"restoration-verifier".to_vec();
        let staged = stage_lane(&lane, &lillux::sha256_hex(&verifier));
        let archive = verifier_tar(RESTORATION_VERIFIER_REMOTE_NAME, &verifier, &[]);
        let request = owner_request(&staged, &archive);
        let RemoteVerificationPurpose::OwnerMeasurement { challenge } = &request.intent.purpose
        else {
            panic!("owner purpose expected");
        };
        let measurement = RestoredOwnerMeasurement {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            challenge_digest: challenge.digest().unwrap(),
            manifest_hash: "0".repeat(64),
            owner_executable_sha256: staged.source.owner_executable_sha256.clone(),
            controller_public_root: staged.source.controller_public_root.clone(),
        };
        let stdout = format!(
            "{}\n",
            String::from_utf8(canonical_json(&measurement).unwrap()).unwrap()
        );
        let runner = CannedRunner::new(stdout, 0, 0);
        assert!(verify_core(&lane.root, &lane.state, &request, &archive, &runner).is_err());
    }

    #[test]
    fn consumer_evidence_is_bound_to_the_challenge_digest() {
        let lane = lane();
        let verifier = b"consumer-verifier".to_vec();
        let verifier_sha = lillux::sha256_hex(&verifier);
        let staged = stage_lane(&lane, &"e".repeat(64));
        let archive = verifier_tar(
            CONSUMER_VERIFIER_REMOTE_NAME,
            &verifier,
            &[("consumer-input.json", b"{}")],
        );
        let request = consumer_request(&staged, &archive, &verifier_sha);
        let digest = request.intent.consumer_challenge_digest().unwrap();
        let evidence = serde_json::json!({"challenge_digest": digest, "euid": 0});
        let stdout = format!(
            "{}\n",
            String::from_utf8(canonical_json(&evidence).unwrap()).unwrap()
        );
        let runner = CannedRunner::new(stdout, 0, 0);
        let response = verify_core(&lane.root, &lane.state, &request, &archive, &runner).unwrap();
        response.validate_for(&request).unwrap();
        let RestoredVerifierAdapterResponse::ConsumerObserved {
            observation,
            evidence: returned,
        } = response
        else {
            panic!("consumer observation expected");
        };
        assert_eq!(observation.challenge_digest, digest);
        assert_eq!(returned, evidence);
        assert_eq!(
            observation.evidence_bytes,
            canonical_json(&evidence).unwrap().len() as u64
        );
        let invocation = runner.last.lock().unwrap().clone().unwrap();
        assert_eq!(invocation.arguments[0], "--consumer-challenge-b64");
        assert!(invocation.executable.ends_with(format!(
            "consumer-{}/{CONSUMER_VERIFIER_REMOTE_NAME}",
            request.intent.operation_id
        )));
    }

    #[test]
    fn unmapped_guest_refusal_is_retained_and_reported_uncertain() {
        let lane = lane();
        let verifier = b"consumer-verifier".to_vec();
        let verifier_sha = lillux::sha256_hex(&verifier);
        let staged = stage_lane(&lane, &"e".repeat(64));
        let archive = verifier_tar(
            CONSUMER_VERIFIER_REMOTE_NAME,
            &verifier,
            &[("consumer-input.json", b"{}")],
        );
        let request = consumer_request(&staged, &archive, &verifier_sha);
        // The guest-wire verifier exits nonzero when it is not administrator.
        let runner = CannedRunner::new(String::new(), 1000, 1);
        assert!(verify_core(&lane.root, &lane.state, &request, &archive, &runner).is_err());
        let records = read_run_records(&lane.root).unwrap();
        let record = &records[&request.intent.operation_id];
        assert_eq!(record.phase, RunPhase::Finished);
        assert_eq!(record.euid, Some(1000));
        assert!(!record.overlay_applied);
        assert!(!record.complete);
    }

    #[test]
    fn evidence_beyond_its_bound_is_refused() {
        let lane = lane();
        let verifier = b"consumer-verifier".to_vec();
        let verifier_sha = lillux::sha256_hex(&verifier);
        let staged = stage_lane(&lane, &"e".repeat(64));
        let archive = verifier_tar(
            CONSUMER_VERIFIER_REMOTE_NAME,
            &verifier,
            &[("consumer-input.json", b"{}")],
        );
        let request = consumer_request(&staged, &archive, &verifier_sha);
        let filler = "x".repeat(usize::try_from(MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES).unwrap());
        let evidence = serde_json::json!({"filler": filler});
        let stdout = format!(
            "{}\n",
            String::from_utf8(canonical_json(&evidence).unwrap()).unwrap()
        );
        let runner = CannedRunner::new(stdout, 0, 0);
        assert!(verify_core(&lane.root, &lane.state, &request, &archive, &runner).is_err());
    }

    #[test]
    fn delivered_verifier_must_be_the_selected_artifact() {
        let lane = lane();
        let staged = stage_lane(&lane, &"e".repeat(64));
        let archive = verifier_tar(
            CONSUMER_VERIFIER_REMOTE_NAME,
            b"substituted",
            &[("consumer-input.json", b"{}")],
        );
        let request = consumer_request(&staged, &archive, &lillux::sha256_hex(b"selected"));
        let runner = CannedRunner::new(String::new(), 0, 0);
        assert!(verify_core(&lane.root, &lane.state, &request, &archive, &runner).is_err());
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn archive_unpack_refuses_escape_absolute_and_link_entries() {
        let lane = lane();
        let destination = open_or_create(&lane.root, "unpack", 0o755).unwrap();
        let mut escaping = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..7].copy_from_slice(b"../evil");
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        escaping.append(&header, &b"x"[..]).unwrap();
        assert!(unpack_archive(&destination, &escaping.into_inner().unwrap()).is_err());

        let mut absolute = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..5].copy_from_slice(b"/evil");
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        absolute.append(&header, &b"x"[..]).unwrap();
        assert!(unpack_archive(&destination, &absolute.into_inner().unwrap()).is_err());

        let mut link = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        link.append_link(&mut header, "link", "target").unwrap();
        assert!(unpack_archive(&destination, &link.into_inner().unwrap()).is_err());
    }

    #[test]
    fn termination_removes_the_guest_and_observation_never_mutates() {
        let lane = lane();
        let staged = stage_lane(&lane, &lillux::sha256_hex(b"v"));
        let mut intent = RuntimeSnapshotQualificationTerminationIntent {
            schema: 1,
            operation_id: String::new(),
            qualification_operation_id: staged.qualification.operation_id.clone(),
            occurrence_id: staged.occurrence.occurrence_id.clone(),
            owner_principal: staged.qualification.owner_principal.clone(),
            provider_id: staged.qualification.provider_id.clone(),
            provider_spec_digest: staged.qualification.provider_spec_digest.clone(),
            attempt_deadline_ms: lillux::time::timestamp_millis() + 60_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let request = RuntimeSnapshotQualificationTerminationAdapterRequest {
            protocol: RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL.into(),
            provider_spec_digest: intent.provider_spec_digest.clone(),
            intent,
            qualification_intent: staged.qualification.clone(),
            occurrence: staged.occurrence.clone(),
        };
        let guest = lane
            .state
            .join("sandboxes")
            .join(&staged.occurrence.occurrence_id)
            .join("fs");
        assert!(guest.is_dir());
        // Recovery observation before first contact finds nothing terminal.
        assert!(matches!(
            terminate_core(&lane.root, &request, false).unwrap(),
            RuntimeSnapshotQualificationTerminationAdapterResponse::Pending { .. }
        ));
        assert!(guest.is_dir());
        let first = terminate_core(&lane.root, &request, true).unwrap();
        assert!(matches!(
            first,
            RuntimeSnapshotQualificationTerminationAdapterResponse::Terminal { .. }
        ));
        assert!(!guest.exists());
        // The observation entry now reports the same terminal evidence.
        assert_eq!(terminate_core(&lane.root, &request, false).unwrap(), first);
        // A terminated guest accepts no verifier contact.
        let verifier = b"v".to_vec();
        let archive = verifier_tar(RESTORATION_VERIFIER_REMOTE_NAME, &verifier, &[]);
        let owner = owner_request(&staged, &archive);
        let runner = CannedRunner::new(String::new(), 0, 0);
        assert!(verify_core(&lane.root, &lane.state, &owner, &archive, &runner).is_err());
    }

    #[test]
    fn identities_are_bounded_and_stable() {
        let digest = "ab".repeat(32);
        assert_eq!(snapshot_id(&digest), snapshot_id(&digest));
        assert_eq!(sandbox_id(&digest).len(), 44);
        let execution = execution_id("run", &digest);
        assert!(execution.starts_with("exe-") && execution.len() <= 128);
        assert!(
            execution
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        );
    }
}
