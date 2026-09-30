//! Verifier-owned native Codex guest probe, not an admitted worker connector.
//!
//! Codex starts this dedicated process as its command environment. A trusted
//! verifier supplies an exact request and input trees in a private cwd. Only
//! the runtime, command tools and candidate enter the guest. Protocol stdout
//! carries exec-server bytes; observations remain in the pinned controller cwd.
//! No worker identities, qualification claims or completion fence are minted.

use anyhow::{ensure, Context as _, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use lillux::{
    time::{Duration, MonotonicDeadline},
    PinnedDirectory,
};
use ryeos_state::external_execution::admission::{
    ExternalCandidateProcFilesystem, ExternalCandidateRuntimeRecipe,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
};

const CHUNK: usize = 16 * 1024;
const TRANSCRIPT_LIMIT: usize = 1024 * 1024;

/// Convert the retained wall-clock contact bound once at native startup.
/// Cleanup is a separate finite settlement allowance, never more work time.
fn production_deadlines(
    attempt_deadline_ms: i64,
    now_ms: i64,
) -> Result<(MonotonicDeadline, MonotonicDeadline)> {
    let remaining_ms = attempt_deadline_ms.saturating_sub(now_ms);
    ensure!(
        attempt_deadline_ms > 0 && remaining_ms > 0,
        "consumer native attempt deadline expired"
    );
    let active_ms = u64::try_from(remaining_ms)?.min(90_000);
    Ok((
        MonotonicDeadline::after(Duration::from_millis(active_ms)),
        MonotonicDeadline::after(Duration::from_millis(active_ms + 15_000)),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    sha256: String,
    mode: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    recipe: ExternalCandidateRuntimeRecipe,
    effective_environment: BTreeMap<String, String>,
    runtime: BTreeMap<String, Member>,
    tools: BTreeMap<String, Member>,
    capture_limit: u64,
}

/// Independently compare the child-owned pre-exec receipt against the exact
/// request reconstructed by the verifier from signed admitted inputs. The
/// guest supervisor's `request_match` Boolean is not accepted as a substitute.
pub fn check_applied_receipt_against_signed_request(
    observation: &serde_json::Value,
    expected_request: &[u8],
) -> Result<()> {
    ensure!(
        expected_request.len() <= 64 * 1024,
        "native expected request exceeds bound"
    );
    let request: Request = serde_json::from_slice(expected_request)?;
    ensure!(
        request.schema == "test.routed_guest.v1",
        "wrong native request schema"
    );
    let receipt: lillux::LinuxSandboxAppliedLaunchReceipt = serde_json::from_value(
        observation
            .get("applied_receipt")
            .context("native applied receipt absent")?
            .clone(),
    )?;
    let executable = PathBuf::from(request.recipe.namespace_executable()?);
    let argv0 = OsString::from(&request.recipe.argv0);
    let arguments = request
        .recipe
        .arguments
        .iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    let cwd = PathBuf::from(&request.recipe.cwd);
    let environment = request
        .effective_environment
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        receipt
            .matches_target(lillux::LinuxSandboxAppliedLaunchTarget {
                executable: &executable,
                argv0: &argv0,
                arguments: &arguments,
                cwd: &cwd,
                environment: &environment,
            })
            .map_err(anyhow::Error::msg)?,
        "native applied receipt differs from signed expected target"
    );
    Ok(())
}

fn validate_effective_environment(environment: &BTreeMap<String, String>) -> Result<()> {
    ensure!(
        environment.len() <= 64
            && environment.iter().all(|(name, value)| {
                !name.is_empty()
                    && name.len() <= 128
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    && !value.contains('\0')
                    && value.len() <= 4096
            }),
        "routed guest effective environment is invalid"
    );
    Ok(())
}

/// Reconstruct the production target independently from retained admission,
/// not a fixture request or receipt-reported expected fields. This compares
/// target commitments only; mount/source custody and settlement still require
/// the native owner's evidence and the enclosing authenticated attempt join.
pub(crate) fn check_production_applied_receipt(
    observation: &serde_json::Value,
    record: &crate::consumer_record::ConsumerInputRecord,
) -> Result<()> {
    let selected = record.validate()?;
    let effective = selected.expected_guest_environment(&record.requirement)?;
    let recipe = &record.requirement.runtime_recipe;
    let receipt: lillux::LinuxSandboxAppliedLaunchReceipt = serde_json::from_value(
        observation
            .get("applied_receipt")
            .context("production applied receipt absent")?
            .clone(),
    )?;
    let executable = PathBuf::from(recipe.namespace_executable()?);
    let argv0 = OsString::from(&recipe.argv0);
    let arguments: Vec<_> = recipe.arguments.iter().map(OsString::from).collect();
    let cwd = PathBuf::from(&recipe.cwd);
    let environment = effective
        .environment
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect();
    ensure!(
        receipt
            .matches_target(lillux::LinuxSandboxAppliedLaunchTarget {
                executable: &executable,
                argv0: &argv0,
                arguments: &arguments,
                cwd: &cwd,
                environment: &environment,
            })
            .map_err(anyhow::Error::msg)?,
        "production applied receipt differs from admitted target"
    );
    ensure!(
        receipt.owned_child_pid > 0
            && receipt.namespace_pid == 1
            && receipt.effective_uid == 1
            && receipt.effective_gid == 1
            && receipt.no_new_privs
            && receipt.seccomp_mode == 2,
        "production applied receipt lacks native controls"
    );
    Ok(())
}

/// Shared native request construction for fixture and production probes.
/// The caller supplies already verified, retained mount descriptors. Lillux
/// remains responsible for applying and corroborating the sandbox; constructing
/// this request grants no execution or settlement evidence.
pub(crate) fn native_request(
    recipe: &ExternalCandidateRuntimeRecipe,
    environment: BTreeMap<String, String>,
    mounts: Vec<lillux::LinuxSandboxMount>,
) -> Result<lillux::LinuxSandboxRequest> {
    recipe.validate()?;
    validate_effective_environment(&environment)?;
    Ok(lillux::LinuxSandboxRequest {
        executable: recipe.namespace_executable()?.into(),
        argv0: recipe.argv0.clone().into(),
        arguments: recipe.arguments.iter().cloned().map(Into::into).collect(),
        cwd: recipe.cwd.clone().into(),
        environment: environment
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect(),
        mounts,
        fixed_parent_views: vec![],
        overlay: None,
        network: lillux::LinuxSandboxNetwork::Isolated,
        private_tmp: true,
        proc_filesystem: match recipe.proc_filesystem {
            ExternalCandidateProcFilesystem::Empty => lillux::LinuxSandboxProcFilesystem::Empty,
            ExternalCandidateProcFilesystem::PidNamespace => {
                lillux::LinuxSandboxProcFilesystem::PidNamespace
            }
            ExternalCandidateProcFilesystem::PidNamespaceNested => {
                lillux::LinuxSandboxProcFilesystem::PidNamespaceNested
            }
        },
        minimal_devices: true,
        character_devices: vec![],
        target_channels: vec![],
        lifecycle: lillux::LinuxSandboxLifecycle::Run,
        contain_process_group: recipe.contain_process_group,
        nested_sandbox: recipe.nested_sandbox,
        aggregate_limits: None,
    })
}

pub fn run_probe_entrypoint() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // No private paths, canaries or provider bytes in ambient diagnostics.
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

/// Dedicated product-native command environment. The trusted enclosing
/// verifier supplies the challenge over its authenticated run handoff and
/// owns this private cwd plus finish delivery. No daemon callback, provider
/// credentials, Worker identity or qualification claims are manufactured.
pub fn run_production_probe(encoded_challenge: &str) -> Result<()> {
    use crate::consumer_record::{
        ConsumerInputRecord, CONSUMER_INPUT_RECORD_NAME, MAX_CONSUMER_INPUT_RECORD_BYTES,
    };
    use ryeos_external_execution_contract::restored_runtime_measurement::{
        ConsumerRuntimeChallenge, MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES,
    };
    ensure!(
        !encoded_challenge.is_empty() && encoded_challenge.len() <= 8192,
        "consumer challenge encoding exceeds bound"
    );
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded_challenge)?;
    let challenge = ConsumerRuntimeChallenge::parse(&bytes)?;
    let (active, cleanup) = production_deadlines(
        challenge.intent.attempt_deadline_ms,
        lillux::time::timestamp_millis(),
    )?;
    // SAFETY: dedicated synchronous executable startup before any stdio
    // wrappers or helper threads; opposite protocol pipe ends are inherited.
    let pair = unsafe {
        lillux::inherited_pipes::InheritedPipePair::take_inherited_pipes(0, 1, CHUNK, active)
    }?;
    let (input, mut output, interrupt) = pair.split();
    let root = PinnedDirectory::open(Path::new("."))?.context("consumer private cwd absent")?;
    let record_bytes = read_fixed(
        &root,
        CONSUMER_INPUT_RECORD_NAME,
        MAX_CONSUMER_INPUT_RECORD_BYTES as u64,
    )?;
    let record = ConsumerInputRecord::parse(&record_bytes)?;
    let imported = record.open_imported_products(&root, &challenge)?;
    let content = record
        .purpose
        .consumer_content
        .as_ref()
        .context("consumer content absent")?;
    let context = record
        .purpose
        .policy_source
        .policy
        .consumer_execution_context
        .as_ref()
        .context("consumer context absent")?;
    let private =
        crate::production_inputs::ProductionPrivateStaging::create(&root, content, context)?;
    let observed = imported.run_native_protocol(
        &record,
        &private,
        input,
        &mut output,
        interrupt,
        active,
        cleanup,
    )?;
    // The native owner has settled the whole namespace before any capture.
    // This is verifier-private evidence, not Worker C retention/publication.
    let captured = private.capture_candidate_after_settlement(cleanup)?;
    let observation = serde_json::json!({
        "schema": "ryeos.consumer-native-observation.v1",
        "operation_id": challenge.intent.operation_id,
        "challenge_digest": challenge.intent.consumer_challenge_digest()?,
        "input_record_sha256": lillux::sha256_hex(&record_bytes),
        "applied_receipt": observed.applied_receipt,
        "guest_input_base64": STANDARD.encode(observed.sent),
        "guest_output_base64": STANDARD.encode(observed.received),
        "forwarded_output_bytes": observed.forwarded_output_bytes,
        "guest_stderr_sha256": lillux::sha256_hex(&observed.diagnostics),
        "guest_stderr_bytes": observed.diagnostics.len(),
        "namespace_exit": observed.namespace_exit,
        "captured_files": captured.files,
    });
    let bytes = lillux::canonical_json(&observation)?.into_bytes();
    ensure!(
        bytes.len() as u64 <= MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES,
        "consumer native observation exceeds evidence budget"
    );
    root.atomic_create_pinned_regular(OsStr::new("guest-observation.json"), &bytes, 0o400)?
        .context("consumer native observation already exists")?;
    Ok(())
}

fn read_fixed(root: &PinnedDirectory, name: &str, limit: u64) -> Result<Vec<u8>> {
    let file = root
        .open_pinned_regular(OsStr::new(name), false)?
        .context("missing input")?;
    file.read_stable_bounded(&file.observation()?, limit)
}

fn child(root: &PinnedDirectory, name: &str) -> Result<PinnedDirectory> {
    root.open_child_directory(OsStr::new(name))?
        .context("missing directory")
}

fn stage(
    input: &PinnedDirectory,
    output: &PinnedDirectory,
    members: &BTreeMap<String, Member>,
) -> Result<()> {
    ensure!(
        !members.is_empty() && members.len() <= 64,
        "runtime member bound"
    );
    let mut actual = Vec::new();
    input.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(128, 8),
        |_, _| Ok(false),
        |path, _| {
            actual.push(path.to_str().context("non-UTF8 member")?.to_owned());
            Ok(())
        },
    )?;
    actual.sort();
    ensure!(
        actual == members.keys().cloned().collect::<Vec<_>>(),
        "runtime closure mismatch"
    );
    let mut total = 0usize;
    for (name, member) in members {
        ensure!(
            !name.is_empty()
                && name.len() <= 512
                && !name.contains('\\')
                && !name.chars().any(char::is_control)
                && name
                    .split('/')
                    .all(|s| !s.is_empty() && s != "." && s != ".."),
            "invalid runtime member"
        );
        ensure!(
            matches!(member.mode, 0o644 | 0o755) && lillux::valid_hash(&member.sha256),
            "invalid runtime identity"
        );
        let file = input
            .open_pinned_regular_descendant(Path::new(name), false)?
            .context("missing runtime member")?;
        let bytes = file.read_stable_bounded(&file.observation()?, 384 * 1024 * 1024)?;
        total = total
            .checked_add(bytes.len())
            .context("runtime size overflow")?;
        ensure!(
            total <= 512 * 1024 * 1024 && lillux::sha256_hex(&bytes) == member.sha256,
            "runtime identity or size mismatch"
        );
        let path = Path::new(name);
        let mut parent = output.try_clone()?;
        for part in path.parent().context("member parent")?.components() {
            parent = parent.open_or_create_child(part.as_os_str(), 0o755)?;
        }
        parent
            .atomic_create_pinned_regular(
                path.file_name().context("member name")?,
                &bytes,
                member.mode,
            )?
            .context("duplicate staged member")?;
    }
    Ok(())
}

fn append_bounded(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<()> {
    ensure!(
        bytes
            .len()
            .checked_add(chunk.len())
            .is_some_and(|n| n <= TRANSCRIPT_LIMIT),
        "transcript limit exceeded"
    );
    bytes.extend_from_slice(chunk);
    Ok(())
}

fn drain_settled(
    pipe: &mut impl std::io::Read,
    bytes: &mut Vec<u8>,
    deadline: MonotonicDeadline,
) -> Result<()> {
    let mut chunk = [0; CHUNK];
    loop {
        ensure!(!deadline.has_elapsed(), "settled pipe drain expired");
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => append_bounded(bytes, &chunk[..n])?,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                lillux::time::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn run() -> Result<()> {
    // Both ceilings are fixed at startup. Active work cannot consume the
    // settlement allowance by repeatedly renewing an idle timeout.
    let active = MonotonicDeadline::after(Duration::from_secs(90));
    let cleanup = MonotonicDeadline::after(Duration::from_secs(105));
    // SAFETY: exclusive executable startup, uniquely inherited opposite pipe
    // ends, no stdio wrappers or other host threads have been created.
    let pair = unsafe {
        lillux::inherited_pipes::InheritedPipePair::take_inherited_pipes(0, 1, CHUNK, active)
    }?;
    let (input, mut output, interrupt) = pair.split();
    let root = PinnedDirectory::open(Path::new("."))?.context("verifier cwd missing")?;
    let result = run_in_directory(&root, input, &mut output, interrupt, active, cleanup);
    if let Err(error) = &result {
        // Private synthetic-fixture diagnostics, never qualification evidence.
        // Retain the first failing boundary even when no guest was released;
        // absence of the success observation still proves no settlement claim.
        let reason: String = format!("{error:#}").chars().take(4096).collect();
        let diagnostic = serde_json::json!({
            "schema":"test.routed_guest_failure.v1", "reason":reason,
            "settlement":"not_attested",
        });
        let _ = root.atomic_create_pinned_regular(
            OsStr::new("guest-failure.json"),
            &serde_json::to_vec(&diagnostic)?,
            0o600,
        );
    }
    result
}

fn run_in_directory(
    root: &PinnedDirectory,
    input: lillux::inherited_pipes::InheritedPipeInput,
    output: &mut lillux::inherited_pipes::InheritedPipeOutput,
    interrupt: lillux::inherited_pipes::PipeInterrupt,
    active: MonotonicDeadline,
    cleanup: MonotonicDeadline,
) -> Result<()> {
    let request_bytes = read_fixed(&root, "guest-request.json", 64 * 1024)?;
    let request: Request = serde_json::from_slice(&request_bytes)?;
    ensure!(
        request.schema == "test.routed_guest.v1"
            && (1..=1024 * 1024).contains(&request.capture_limit),
        "unsupported routed guest request"
    );
    ensure!(
        request.runtime.contains_key("bin/codex")
            && request.tools.contains_key("bin/zsh")
            && request.tools.contains_key("bin/rg"),
        "required executable absent"
    );
    request.recipe.validate()?;
    ensure!(
        request.recipe.runtime_mount_destination == "/runtime"
            && request.recipe.executable_relative_path == "bin/codex"
            && request.recipe.argv0 == "codex"
            && request.recipe.arguments == ["exec-server", "--listen", "stdio"]
            && request.recipe.cwd == "/workspace"
            && request.recipe.environment.is_empty()
            && request.recipe.proc_filesystem
                == ExternalCandidateProcFilesystem::PidNamespaceNested
            && !request.recipe.contain_process_group
            && request.recipe.nested_sandbox,
        "routed guest request changed the selected signed Codex recipe"
    );
    validate_effective_environment(&request.effective_environment)?;
    ensure!(
        root.open_pinned_regular(OsStr::new("finish"), false)?
            .is_none()
            && root
                .open_pinned_regular(OsStr::new("guest-observation.json"), false)?
                .is_none(),
        "reused verifier occurrence"
    );
    // Codex may attempt transport reconnection. A second environment process
    // must not start a second writer against this verifier's candidate.
    root.atomic_create_pinned_regular(
        OsStr::new("guest-started"),
        lillux::sha256_hex(&request_bytes).as_bytes(),
        0o600,
    )?
    .context("verifier guest already attempted")?;
    let (_, scratch) = root.create_unique_child("routed-guest", 0o700)?;
    let runtime = scratch.create_child(OsStr::new("runtime"), 0o700)?;
    let tools = scratch.create_child(OsStr::new("tools"), 0o700)?;
    let runtime_views = scratch.create_child(OsStr::new("runtime-views"), 0o700)?;
    let tmpdir = runtime_views.create_child(OsStr::new("TMPDIR"), 0o700)?;
    stage(&child(&root, "runtime")?, &runtime, &request.runtime)?;
    stage(&child(&root, "tools")?, &tools, &request.tools)?;
    let candidate = child(&root, "candidate")?;
    let state_root = scratch.create_child(OsStr::new("cas-state"), 0o700)?;
    let state =
        ryeos_state::StateDb::open(state_root.path(), Arc::new(ryeos_state::TrustStore::new()))
            .context("initialize private verifier CAS")?;
    let authority = state
        .pinned_authority()
        .context("pin private verifier CAS")?;
    let guard = authority
        .acquire_shared_guard()
        .context("guard private verifier CAS")?;
    drop(state);
    let runtime_fd = runtime
        .inherited_descriptor_authority()
        .context("retain runtime mount")?;
    let tools_fd = tools
        .inherited_descriptor_authority()
        .context("retain tools mount")?;
    let candidate_fd = candidate
        .inherited_descriptor_authority()
        .context("retain candidate mount")?;
    let tmpdir_fd = tmpdir
        .inherited_descriptor_authority()
        .context("retain private TMPDIR mount")?;
    let mounts = [
        (
            &runtime_fd,
            "/runtime",
            lillux::LinuxSandboxMountAccess::ReadOnly,
        ),
        (
            &tools_fd,
            "/ryeos/realizations/authoring-tools",
            lillux::LinuxSandboxMountAccess::ReadOnly,
        ),
        (
            &candidate_fd,
            "/workspace",
            lillux::LinuxSandboxMountAccess::Writable,
        ),
        (
            &tmpdir_fd,
            "/ryeos/runtime-views/TMPDIR",
            lillux::LinuxSandboxMountAccess::Writable,
        ),
    ]
    .into_iter()
    .map(|(fd, path, access)| {
        Ok(lillux::LinuxSandboxMount {
            source_fd: fd.inherited_descriptor().map_err(anyhow::Error::msg)?,
            destination: path.into(),
            access,
            layer: 0,
        })
    })
    .collect::<Result<Vec<_>>>()?;
    // The enclosing verifier derives this request from signed admitted inputs
    // and checks its exact hash. This still proves only what was requested;
    // actual guest argv, environment and containment require independent
    // applied-launch evidence before qualification claims are possible.
    let native = native_request(&request.recipe, request.effective_environment, mounts)?;
    let observed = run_prepared_protocol(root, native, input, output, interrupt, active, cleanup)?;
    let policy = ryeos_state::objects::ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        vec![],
        vec![],
        BTreeMap::new(),
    )?;
    let captured = ryeos_project_capture::ingest_project_tree_bounded(
        &authority,
        &guard,
        &candidate,
        &policy,
        ryeos_project_capture::ProjectCaptureBudget {
            max_bytes: request.capture_limit,
            deadline: cleanup,
        },
    )?;
    let observation = serde_json::json!({
        "schema": "test.routed_guest_observation.v1",
        "request_sha256": lillux::sha256_hex(&request_bytes),
        "applied_receipt": observed.applied_receipt,
        "guest_input_base64": STANDARD.encode(observed.sent), "guest_output_base64": STANDARD.encode(observed.received),
        "forwarded_output_bytes": observed.forwarded_output_bytes,
        "guest_stderr_sha256": lillux::sha256_hex(&observed.diagnostics), "guest_stderr_bytes": observed.diagnostics.len(),
        "namespace_exit": observed.namespace_exit, "captured_files": captured.files,
    });
    root.atomic_create_pinned_regular(
        OsStr::new("guest-observation.json"),
        &serde_json::to_vec(&observation)?,
        0o600,
    )?
    .context("observation already exists")?;
    Ok(())
}

/// Raw execution facts shared by fixture and production verifier probes.
/// Not a Worker terminal envelope or semantic qualification testimony.
pub(crate) struct NativeProtocolObservation {
    pub applied_receipt: lillux::LinuxSandboxAppliedLaunchReceipt,
    pub sent: Vec<u8>,
    pub received: Vec<u8>,
    pub diagnostics: Vec<u8>,
    pub forwarded_output_bytes: usize,
    pub namespace_exit: String,
}

/// Drive an already constructed native request in this dedicated, single-
/// threaded verifier process. Namespace preparation and whole-namespace
/// settlement remain Lillux-owned. Caller retains mount descriptors and writer
/// authority through this call and independently checks the raw observation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_prepared_protocol(
    root: &PinnedDirectory,
    native: lillux::LinuxSandboxRequest,
    mut input: lillux::inherited_pipes::InheritedPipeInput,
    output: &mut lillux::inherited_pipes::InheritedPipeOutput,
    interrupt: lillux::inherited_pipes::PipeInterrupt,
    active: MonotonicDeadline,
    cleanup: MonotonicDeadline,
) -> Result<NativeProtocolObservation> {
    ensure!(
        !active.has_elapsed(),
        "staging exhausted execution allowance"
    );
    let expected_native = native.clone();
    // Native preparation changes this dedicated process's namespaces, including
    // the PID namespace for future children. Remain single-threaded throughout;
    // starting a reader thread here fails after that namespace transition.
    let (mut held, pipes) = lillux::prepare_linux_sandbox_piped(native)
        .map_err(anyhow::Error::msg)
        .context("prepare native routed guest")?;
    let mut guest_input = Some(pipes.stdin);
    let mut guest_output = pipes.stdout;
    let mut guest_error = pipes.stderr;
    let mut sent = Vec::new();
    let mut received = Vec::new();
    let mut diagnostics = Vec::new();
    let mut applied_launch = None;
    let observed = (|| -> Result<()> {
        held.release_once().map_err(anyhow::Error::msg)?;
        let mut pending = Vec::new();
        let mut offset = 0;
        let mut input_eof = false;
        loop {
            ensure!(!active.has_elapsed(), "guest scenario expired");
            if applied_launch.is_none() {
                if let Some(receipt) = held
                    .try_observe_applied_launch()
                    .map_err(anyhow::Error::msg)?
                {
                    ensure!(
                        receipt
                            .matches_request(&expected_native)
                            .map_err(anyhow::Error::msg)?,
                        "native guest applied launch differs from signed request"
                    );
                    applied_launch = Some(receipt);
                }
            }
            if pending.is_empty() {
                let mut bytes = [0; CHUNK];
                match input.try_read_chunk(&mut bytes, Some(active)) {
                    Ok(0) => input_eof = true,
                    Ok(n) => {
                        pending.extend_from_slice(&bytes[..n]);
                        offset = 0;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        ()
                    }
                    Err(e) => return Err(e).context("read controller protocol"),
                }
            }
            if !pending.is_empty() {
                // Lillux created these pipes nonblocking. A failed prefix is
                // terminal. Rust executable startup ignores SIGPIPE; no claim
                // about arbitrary/default-SIGPIPE callers is made here.
                match guest_input
                    .as_mut()
                    .context("closed guest input")?
                    .write(&pending[offset..])
                {
                    Ok(0) => anyhow::bail!("guest input closed"),
                    Ok(n) => {
                        append_bounded(&mut sent, &pending[offset..offset + n])?;
                        offset += n;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        ()
                    }
                    Err(e) => return Err(e.into()),
                }
                if offset == pending.len() {
                    pending.clear();
                }
            }
            if input_eof && pending.is_empty() {
                guest_input = None;
            }
            let mut bytes = [0; CHUNK];
            match guest_output.read(&mut bytes) {
                Ok(0) => anyhow::bail!("guest output ended before verifier finish"),
                Ok(n) => {
                    append_bounded(&mut received, &bytes[..n])?;
                    output.write_all(&bytes[..n], active)?;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    ()
                }
                Err(e) => return Err(e.into()),
            }
            match guest_error.read(&mut bytes) {
                Ok(n) => append_bounded(&mut diagnostics, &bytes[..n])?,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    ()
                }
                Err(e) => return Err(e.into()),
            }
            ensure!(
                held.try_observe_target_exit()
                    .map_err(anyhow::Error::msg)?
                    .is_none(),
                "guest exited before verifier finish"
            );
            if let Some(finish) = root.open_pinned_regular(OsStr::new("finish"), false)? {
                ensure!(
                    finish.read_stable_bounded(&finish.observation()?, 7)? == b"capture",
                    "invalid finish"
                );
                ensure!(
                    pending.is_empty()
                        && !sent.is_empty()
                        && !received.is_empty()
                        && applied_launch.is_some(),
                    "unfinished protocol"
                );
                return Ok(());
            }
            ensure!(!input_eof, "controller input ended without finish");
            lillux::time::sleep(Duration::from_millis(1));
        }
    })();
    // No early return can skip native settlement. There are no reader tasks,
    // queues or prefetched chunks whose lifecycle could outlast this owner.
    guest_input = None;
    drop(guest_input);
    let interrupted = interrupt.interrupt();
    let settled = held
        .terminate_namespace_for_export_until(cleanup)
        .map_err(anyhow::Error::msg);
    interrupted.context("interrupt controller input reader")?;
    let termination = settled.context("guest namespace settlement uncertain")?;
    ensure!(
        termination.launch_failure().is_none(),
        "guest launch failed"
    );
    observed.context("relay guest protocol")?;
    // Reaping the namespace proves settlement, not that the last buffered
    // diagnostic/output bytes have already been observed.
    let forwarded_output_bytes = received.len();
    drain_settled(&mut guest_output, &mut received, cleanup)?;
    drain_settled(&mut guest_error, &mut diagnostics, cleanup)?;
    let applied_launch = applied_launch.context("native guest has no applied-launch receipt")?;
    Ok(NativeProtocolObservation {
        applied_receipt: applied_launch,
        sent,
        received,
        diagnostics,
        forwarded_output_bytes,
        namespace_exit: format!("{:?}", termination.exit()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_deadlines_refuse_expiry_and_never_renew_work_time() {
        for (deadline, now) in [(0, 0), (-1, -2), (1000, 1000), (999, 1000)] {
            assert!(production_deadlines(deadline, now).is_err());
        }
        let (active, cleanup) = production_deadlines(1250, 1000).unwrap();
        assert!(active.remaining() <= Duration::from_millis(250));
        assert!(cleanup.remaining() <= Duration::from_millis(15_250));
        let (active, cleanup) = production_deadlines(i64::MAX, 1000).unwrap();
        assert!(active.remaining() <= Duration::from_secs(90));
        assert!(cleanup.remaining() <= Duration::from_secs(105));
    }

    #[test]
    fn signed_expected_target_rejects_applied_command_environment_and_identity_drift() {
        fn digest(bytes: &[u8]) -> [u8; 32] {
            let encoded = lillux::sha256_hex(bytes);
            std::array::from_fn(|index| {
                u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16).unwrap()
            })
        }
        fn c_string(value: &str) -> Vec<u8> {
            let mut bytes = value.as_bytes().to_vec();
            bytes.push(0);
            bytes
        }
        fn list(values: &[String]) -> [u8; 32] {
            let mut bytes = (values.len() as u64).to_le_bytes().to_vec();
            for value in values {
                let encoded = c_string(value);
                bytes.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
                bytes.extend_from_slice(&encoded);
            }
            digest(&bytes)
        }
        let recipe =
            ryeos_state::external_execution::admission::test_support::fixture_requirement()
                .runtime_recipe;
        let expected = serde_json::to_vec(&serde_json::json!({
            "schema":"test.routed_guest.v1", "recipe":recipe,
            "effective_environment":{"TZ":"UTC"}, "runtime":{}, "tools":{},
            "capture_limit":4096
        }))
        .unwrap();
        let executable = recipe.namespace_executable().unwrap();
        let mut argv = vec![recipe.argv0.clone()];
        argv.extend(recipe.arguments.clone());
        let receipt = lillux::LinuxSandboxAppliedLaunchReceipt {
            owned_child_pid: 42,
            namespace_pid: 1,
            effective_uid: 1,
            effective_gid: 1,
            no_new_privs: true,
            seccomp_mode: 2,
            executable_sha256: digest(&c_string(&executable)),
            argv_sha256: list(&argv),
            environment_sha256: list(&["TZ=UTC".to_owned()]),
            cwd_sha256: digest(&c_string(&recipe.cwd)),
            post_release_mount_view: lillux::LinuxSandboxMountPreparationCommitments {
                schema: 1,
                mount_count: 0,
                destination_access_sha256: [0; 32],
            },
        };
        let observation = serde_json::json!({"applied_receipt":receipt});
        assert!(check_applied_receipt_against_signed_request(&observation, &expected).is_ok());
        let mut changed_request: serde_json::Value = serde_json::from_slice(&expected).unwrap();
        changed_request["effective_environment"]["TZ"] = serde_json::json!("Pacific/Auckland");
        assert!(check_applied_receipt_against_signed_request(
            &observation,
            &serde_json::to_vec(&changed_request).unwrap()
        )
        .is_err());
        for field in [
            "executable_sha256",
            "argv_sha256",
            "environment_sha256",
            "cwd_sha256",
        ] {
            let mut drifted = observation.clone();
            drifted["applied_receipt"][field][0] = serde_json::json!(255);
            assert!(check_applied_receipt_against_signed_request(&drifted, &expected).is_err());
        }
        let mut wrong_uid = observation.clone();
        wrong_uid["applied_receipt"]["effective_uid"] = serde_json::json!(0);
        assert!(check_applied_receipt_against_signed_request(&wrong_uid, &expected).is_err());
    }

    #[test]
    fn effective_environment_is_explicit_and_bounded() {
        let mut values = BTreeMap::from([("PATH".into(), "/tools/bin".into())]);
        validate_effective_environment(&values).unwrap();
        values.insert("BAD=NAME".into(), "value".into());
        assert!(validate_effective_environment(&values).is_err());
        values.remove("BAD=NAME");
        values.insert("TMPDIR".into(), "contains\0nul".into());
        assert!(validate_effective_environment(&values).is_err());
        values.remove("TMPDIR");
        values.insert("OVERSIZED".into(), "x".repeat(4097));
        assert!(validate_effective_environment(&values).is_err());
    }

    // Harness setup deliberately creates invalid source trees. Product reads,
    // copying and observations still go through pinned Lillux capabilities.
    fn staged_fixture() -> (
        tempfile::TempDir,
        PinnedDirectory,
        PinnedDirectory,
        BTreeMap<String, Member>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let root = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let input = root.create_child(OsStr::new("input"), 0o700).unwrap();
        let output = root.create_child(OsStr::new("output"), 0o700).unwrap();
        let bin = input.create_child(OsStr::new("bin"), 0o755).unwrap();
        bin.atomic_create_pinned_regular(OsStr::new("fixture"), b"exact", 0o755)
            .unwrap()
            .unwrap();
        let members = BTreeMap::from([(
            "bin/fixture".to_owned(),
            Member {
                sha256: lillux::sha256_hex(b"exact"),
                mode: 0o755,
            },
        )]);
        (temp, input, output, members)
    }

    #[test]
    fn exact_runtime_staging_rejects_changed_bytes_and_extra_members() {
        let (_temp, input, output, mut members) = staged_fixture();
        members.get_mut("bin/fixture").unwrap().sha256 = lillux::sha256_hex(b"other");
        assert!(stage(&input, &output, &members).is_err());
        members.get_mut("bin/fixture").unwrap().sha256 = lillux::sha256_hex(b"exact");
        input
            .atomic_create_pinned_regular(OsStr::new("ambient"), b"x", 0o644)
            .unwrap();
        assert!(stage(&input, &output, &members).is_err());
    }

    #[test]
    fn exact_runtime_staging_copies_only_verified_bytes() {
        let (_temp, input, output, members) = staged_fixture();
        stage(&input, &output, &members).unwrap();
        let bin = child(&output, "bin").unwrap();
        assert_eq!(read_fixed(&bin, "fixture", 5).unwrap(), b"exact");
        assert!(
            stage(&input, &output, &members).is_err(),
            "cannot overwrite staged member"
        );
    }

    #[test]
    fn exact_runtime_staging_refuses_symlink_and_noncanonical_member() {
        let (temp, input, output, mut members) = staged_fixture();
        std::os::unix::fs::symlink("bin/fixture", temp.path().join("input/alias")).unwrap();
        assert!(stage(&input, &output, &members).is_err());
        members.insert(
            "../escape".into(),
            Member {
                sha256: lillux::sha256_hex(b"x"),
                mode: 0o755,
            },
        );
        assert!(stage(&input, &output, &members).is_err());
    }

    #[test]
    fn transcript_limit_is_cumulative_and_does_not_keep_overflow() {
        let mut bytes = vec![0; TRANSCRIPT_LIMIT - 1];
        append_bounded(&mut bytes, b"x").unwrap();
        assert!(append_bounded(&mut bytes, b"y").is_err());
        assert_eq!(bytes.len(), TRANSCRIPT_LIMIT);
        assert_eq!(bytes.last(), Some(&b'x'));
    }
}
