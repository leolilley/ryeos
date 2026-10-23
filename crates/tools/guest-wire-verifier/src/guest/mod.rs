//! Guest role: runs inside a restored sandbox as the administrator. It
//! re-measures the subject, produces an equivalent owner product under an
//! ephemeral key, overlays it in a private mount namespace, serves a loopback
//! controller, runs the owner exactly as shipped, and reports what happened.

pub mod controller;
pub mod fixture;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution::guest_runtime_product::{OWNER_NAME, produce_guest_owner_runtime};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    CONSUMER_INPUT_RECORD_NAME, ConsumerRuntimeChallenge, MAX_CONSUMER_INPUT_RECORD_BYTES,
    MAX_RESTORED_OWNER_CHALLENGE_BYTES, RemoteVerificationPurpose,
};
use ryeos_external_execution_contract::{
    EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA, EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
};
use ryeos_state::external_content::qualification_consumer_record::RemoteProbeInputRecord;

use crate::evidence::{
    EVIDENCE_SCHEMA, GuestWireEvidence, OwnerExit, ProbeEvidence, SchemaDigest, SchemaPins,
    SubjectEvidence,
};
use crate::loopback_tls::{EphemeralPki, LoopbackHttpsServer};

pub const RUNTIME_ROOT: &str = "/ryeos/guest-runtime";
pub const ACTIVATION_ROOT: &str = "/ryeos/activation";
pub const OCCURRENCES_ROOT: &str = "/ryeos/occurrences";
pub const SUPERVISOR_MEMBER: &str = "ryeos-external-candidate-supervisor";
pub const LAUNCHER_MEMBER: &str = "ryeos-external-candidate-launcher";
const MAX_MEMBER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CONTROLLER_BODY: usize = 1024 * 1024;

pub fn parse_challenge(encoded: &str) -> Result<ConsumerRuntimeChallenge> {
    ensure!(
        !encoded.is_empty() && encoded.len() <= (MAX_RESTORED_OWNER_CHALLENGE_BYTES + 2048) * 2,
        "consumer challenge exceeds argument bound"
    );
    let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
    ensure!(
        URL_SAFE_NO_PAD.encode(&bytes) == encoded,
        "consumer challenge is not canonical base64url"
    );
    ConsumerRuntimeChallenge::parse(&bytes)
}

/// The record delivered beside the executable, joined to the challenge.
pub fn read_record(
    archive: &Path,
    challenge: &ConsumerRuntimeChallenge,
) -> Result<RemoteProbeInputRecord> {
    let bytes = std::fs::read(archive.join(CONSUMER_INPUT_RECORD_NAME))
        .context("probe input record is absent")?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
        "probe input record exceeds its bound"
    );
    let record = RemoteProbeInputRecord::from_value(&serde_json::from_slice(&bytes)?)?;
    let RemoteVerificationPurpose::ConsumerRuntime {
        coordinate,
        guest_runtime_manifest_hash,
        ..
    } = &challenge.intent.purpose
    else {
        bail!("challenge is not a consumer runtime challenge");
    };
    ensure!(
        coordinate == &record.coordinate
            && challenge.selection == record.selection
            && *guest_runtime_manifest_hash == record.purpose.subject_manifest_hash,
        "probe record differs from the challenge"
    );
    Ok(record)
}

/// Read each delivered member and require its registered digest.
pub fn read_members(
    archive: &Path,
    record: &RemoteProbeInputRecord,
) -> Result<(BTreeMap<String, String>, Vec<u8>, Vec<u8>)> {
    let digests = crate::origin::registered_member_digests(record)?;
    let mut supervisor = None;
    let mut launcher = None;
    for (name, expected) in &digests {
        let path = archive.join(name);
        let metadata =
            std::fs::metadata(&path).with_context(|| format!("member {name} is absent"))?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_MEMBER_BYTES,
            "member {name} is unusable"
        );
        let bytes = std::fs::read(&path)?;
        ensure!(
            lillux::sha256_hex(&bytes) == *expected,
            "delivered member {name} differs from its registered digest"
        );
        match name.as_str() {
            SUPERVISOR_MEMBER => supervisor = Some(bytes),
            LAUNCHER_MEMBER => launcher = Some(bytes),
            _ => bail!("unexpected archive member {name}"),
        }
    }
    Ok((
        digests,
        supervisor.context("supervisor member absent")?,
        launcher.context("launcher member absent")?,
    ))
}

fn encode_public_root(key: &lillux::crypto::VerifyingKey) -> String {
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(key.to_bytes())
    )
}

/// Everything the probe needs besides the restored filesystem.
pub struct ProbeRequest {
    pub challenge_digest: String,
    pub expected_subject: SubjectEvidence,
    pub member_digests: BTreeMap<String, String>,
    pub supervisor: Vec<u8>,
    pub launcher: Vec<u8>,
}

/// Read the delivered record and members, then execute.
pub fn run_probe(challenge: &ConsumerRuntimeChallenge, archive: &Path) -> Result<Vec<u8>> {
    let record = read_record(archive, challenge)?;
    let (member_digests, supervisor, launcher) = read_members(archive, &record)?;
    execute(&ProbeRequest {
        challenge_digest: challenge.intent.consumer_challenge_digest()?,
        expected_subject: SubjectEvidence {
            owner_executable_sha256: record.prerequisite.owner_executable_sha256.clone(),
            manifest_hash: record.prerequisite.manifest_hash.clone(),
            controller_public_root: record.prerequisite.controller_public_root.clone(),
        },
        member_digests,
        supervisor,
        launcher,
    })
}

/// Execute the probe and return the canonical evidence bytes.
pub fn execute(request: &ProbeRequest) -> Result<Vec<u8>> {
    let supervisor = &request.supervisor;
    let launcher = &request.launcher;
    // 1. The subject must be exactly what the daemon measured.
    let runtime_root = lillux::PinnedDirectory::open(Path::new(RUNTIME_ROOT))?
        .context("restored guest has no owner-product root")?;
    let subject_runtime = ObservedGuestRuntime::observe(&runtime_root)?;
    let identity = subject_runtime.measure_exact_owner_product()?;
    let subject = SubjectEvidence {
        owner_executable_sha256: identity.owner_executable_sha256.clone(),
        manifest_hash: identity.manifest_hash.clone(),
        controller_public_root: identity.controller_public_root.clone(),
    };
    ensure!(
        subject == request.expected_subject,
        "restored subject differs from the daemon-retained owner measurement"
    );

    // 2. Private work area; its owner is the effective user.
    let ryeos = lillux::PinnedDirectory::open(Path::new("/ryeos"))?.context("/ryeos is absent")?;
    let (_, work) = ryeos.create_unique_child("guest-wire-probe", 0o700)?;
    let euid = std::fs::metadata(work.path())?.uid();
    ensure!(
        euid == 0,
        "the guest probe requires the administrator account"
    );

    let root_key = lillux::crypto::generate_signing_key();
    let controller_key = lillux::crypto::generate_signing_key();

    // 3. Same owner bytes and profile, ephemeral controller root.
    let bin = runtime_root
        .open_child_directory(OsStr::new("bin"))?
        .context("subject has no bin directory")?;
    let owner_file = bin
        .open_pinned_regular(OsStr::new(OWNER_NAME), false)?
        .context("subject has no owner executable")?;
    let owner_bytes = owner_file.observation()?.size();
    let owner_authority = owner_file.inherited_descriptor_authority()?;
    let parent = work.create_child(OsStr::new("runtime-parent"), 0o700)?;
    let profile = subject_runtime.profile().clone();
    let product = produce_guest_owner_runtime(
        &parent,
        OsStr::new("probe-runtime"),
        &owner_authority,
        owner_bytes,
        &subject.owner_executable_sha256,
        &root_key.verifying_key(),
        &profile,
    )?;
    let probe_identity =
        ObservedGuestRuntime::observe(product.root())?.measure_exact_owner_product()?;
    ensure!(
        probe_identity.owner_executable_sha256 == subject.owner_executable_sha256
            && probe_identity.controller_public_root
                == encode_public_root(&root_key.verifying_key())
            && probe_identity.manifest_hash == product.manifest_hash(),
        "probe product differs from the subject owner or its ephemeral root"
    );

    // 4. Loopback controller identity and the controller-side fixtures.
    let pki = EphemeralPki::generate()?;
    let server = LoopbackHttpsServer::bind(&pki)?;
    let prepared = fixture::prepare(&fixture::FixtureInputs {
        work: &work,
        root_key: &root_key,
        controller_key: &controller_key,
        probe_manifest_hash: product.manifest_hash(),
        supervisor,
        launcher,
        tls_root_der: pki.root_der(),
        port: server.port(),
        activation_root: ACTIVATION_ROOT,
    })?;
    let package_bytes = prepared.package.bytes();
    std::fs::create_dir_all(ACTIVATION_ROOT)?;
    std::fs::create_dir_all(OCCURRENCES_ROOT)?;

    // 5. Private mount namespace. Nothing before this point mutated the
    // subject's view, and no thread exists yet.
    let activation_limits = lillux::sandbox::TmpfsLimits {
        size_bytes: package_bytes * 2 + 16 * 1024 * 1024,
        inodes: 4096,
    };
    let occurrence_limits = lillux::sandbox::TmpfsLimits {
        size_bytes: package_bytes * 4 + profile.private_source_max_bytes + 64 * 1024 * 1024,
        inodes: 1 << 16,
    };
    lillux::sandbox::enter_root_overlay_namespace(&[
        lillux::sandbox::RootOverlay::Bind {
            source: product.root(),
            target: Path::new(RUNTIME_ROOT),
            read_only: true,
        },
        lillux::sandbox::RootOverlay::Tmpfs {
            target: Path::new(ACTIVATION_ROOT),
            limits: activation_limits,
        },
        lillux::sandbox::RootOverlay::Tmpfs {
            target: Path::new(OCCURRENCES_ROOT),
            limits: occurrence_limits,
        },
    ])
    .map_err(anyhow::Error::msg)?;

    // 6. Deliver the signed import, package and network inputs.
    let activation = lillux::PinnedDirectory::open(Path::new(ACTIVATION_ROOT))?
        .context("activation overlay is absent")?;
    fixture::write_file(
        &activation,
        fixture::IMPORT_NAME,
        &prepared.signed_import,
        0o600,
    )?;
    let payload = prepared.package.payload();
    let observation = payload.observation()?;
    let payload_bytes = payload
        .read_stable_bounded(&observation, package_bytes)
        .map_err(anyhow::Error::msg)?;
    fixture::write_file(&activation, fixture::PACKAGE_NAME, &payload_bytes, 0o600)?;
    drop(payload_bytes);
    for (name, bytes) in [
        (fixture::RESOLVER_NAME, &prepared.resolver_bytes),
        (fixture::HOSTS_NAME, &prepared.hosts_bytes),
    ] {
        let file = fixture::write_file(&activation, name, bytes, 0o600)?;
        profile.account.grant_private_file(&file)?;
    }

    // 7. Serve the controller while the owner runs as shipped.
    let controller = controller::Controller::new(
        &prepared.bootstrap_bytes,
        controller_key,
        prepared.expected_ready_inputs.clone(),
    )?;
    let assignment = URL_SAFE_NO_PAD.encode(&prepared.signed_assignment);
    let owner_timeout = f64::from(profile.owner_timeout_seconds).min(240.0);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs_f64(owner_timeout + 30.0);
    let mut controller = controller;
    let (owner_result, served) = std::thread::scope(|scope| {
        let served = scope.spawn(|| {
            server.serve(deadline, &stop, MAX_CONTROLLER_BODY, |request| {
                controller.handle(request)
            })
        });
        let result = lillux::run(lillux::SubprocessRequest {
            cmd: format!("{RUNTIME_ROOT}/bin/{OWNER_NAME}"),
            argv0: None,
            args: vec!["--assignment-b64".into(), assignment.clone()],
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: owner_timeout,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        });
        stop.store(true, std::sync::atomic::Ordering::Release);
        let served = served.join();
        (result, served)
    });
    match served {
        Ok(result) => result.context("loopback controller failed")?,
        Err(_) => bail!("loopback controller thread panicked"),
    }
    let outcome = controller.outcome()?;
    ensure!(
        outcome.command_terminated,
        "the exchange ended before the direct command's terminal acknowledgement"
    );
    let owner_exit = OwnerExit {
        success: owner_result.success && !owner_result.timed_out && owner_result.exit_code == 0,
        detail: format!(
            "exit_code {} timed_out {}",
            owner_result.exit_code, owner_result.timed_out
        ),
    };
    if !owner_exit.success {
        bail!(
            "guest owner failed: {} {}",
            owner_exit.detail,
            owner_result.stderr.trim()
        );
    }
    let evidence = GuestWireEvidence {
        schema: EVIDENCE_SCHEMA.into(),
        challenge_digest: request.challenge_digest.clone(),
        euid,
        subject,
        probe: ProbeEvidence {
            manifest_hash: product.manifest_hash().to_owned(),
            public_key: encode_public_root(&root_key.verifying_key()),
        },
        projection: SchemaDigest {
            schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            digest: prepared.projection.identity_digest()?,
        },
        launcher_spec: SchemaDigest {
            schema: EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA,
            digest: outcome
                .launcher_spec_digest
                .context("launcher specification digest absent")?,
        },
        exchange_transcript_digest: outcome.transcript_digest,
        exchange_frame_count: outcome.frame_count,
        applied_launch_receipt_digest: outcome
            .applied_launch_receipt_digest
            .context("no applied-launch receipt crossed the exchange")?,
        candidate_program_digest: outcome
            .candidate_program_digest
            .context("applied launch named no candidate program")?,
        owner_exit,
        member_digests: request.member_digests.clone(),
        schema_pins: SchemaPins {
            guest_inputs: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            launcher_spec: EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA,
        },
    };
    evidence.canonical_bytes()
}

/// Entry for `--consumer-challenge-b64`: evidence to fd 1, nothing else on stdout.
pub fn run(encoded: &str) -> Result<()> {
    let challenge = parse_challenge(encoded)?;
    let remaining = challenge.intent.attempt_deadline_ms - lillux::time::timestamp_millis();
    ensure!(remaining > 0, "consumer attempt deadline already elapsed");
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(
        u64::try_from(remaining)?,
    ));
    // SAFETY: exclusive synchronous startup, before any thread or other owner
    // of descriptor 1 exists; this process alone writes the evidence stream.
    let mut output = unsafe {
        lillux::inherited_pipes::InheritedPipeOutput::take_inherited_output(1, 4096, deadline)
    }
    .context("consumer evidence output is not an owned write pipe")?;
    let archive = std::env::current_exe()?
        .parent()
        .context("verifier executable has no directory")?
        .to_path_buf();
    let bytes = run_probe(&challenge, &archive)?;
    for chunk in bytes.chunks(4096) {
        output
            .write_all(chunk, deadline)
            .context("evidence delivery failed; no resend")?;
    }
    output
        .write_all(b"\n", deadline)
        .context("evidence terminator delivery failed; no resend")?;
    Ok(())
}
