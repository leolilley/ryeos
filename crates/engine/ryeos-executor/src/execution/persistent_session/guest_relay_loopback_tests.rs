//! Provider-free loopback of an ordinary relay session's startup.
//!
//! The controller is this crate's real start loop over a composed node: the
//! production start owner reserves, allocates and activates (bootstrap,
//! guest package, signed import and node-signed assignment), and the real
//! relay cursor moves Attach and Exchange files until the start owner admits
//! Ready and authors Release. The guest is the real occurrence owner,
//! supervisor and launcher in a local root enclosure, reached through the
//! loopback placement backend: a relay download reads the guest's mailbox
//! file, an upload delivers it there owned by the guest account with mode
//! `0600`.
//!
//! `RYEOS_GUEST_RELAY_LOOPBACK_HOST` names the rehearsal host executable (the
//! `guest_start_rehearsal_native` test binary of ryeos-external-execution);
//! `RYEOS_GUEST_RELAY_LOOPBACK_BIN_DIR` the guest executables (default: the
//! guest-triple release directory of this target directory). Without them, or
//! on a host without namespaces, the cases skip; `RYEOS_GUEST_RELAY_LOOPBACK=1`
//! makes a skip a failure. `RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT` with
//! `RYEOS_GUEST_RELAY_LOOPBACK_BINDING` runs the node case on that node's
//! installed products (read-only), with `..._BASE_SNAPSHOT` and
//! `..._FILE_PRODUCT` as in the guest-start rehearsal.
//!
//! Three scenarios rehearse what live attempts have tripped on: a controller
//! whose wall clock disagrees with the guest's (the guest freshness check,
//! with and without its declared tolerance), and a supervisor that ends before
//! it attaches (the owner's report of that end must reach the start failure).
//! The clock offset is process-global, so run the cases one at a time
//! (`--test-threads=1`); `scripts/rehearsal/guest-relay-loopback.sh` does.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;

use anyhow::{Context as _, Result};
use ryeos_app::external_placement::guest_relay_loopback::{
    GuestRelayLoopbackContact, GuestRelayLoopbackFault, GuestRelayLoopbackSetup,
    accepted_relay_transfers, compose_guest_relay_loopback_session, journalled_frame_kinds,
};
use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;
use ryeos_external_execution::guest_start_rehearsal::{
    GuestStartRehearsalBase, GuestStartRehearsalFileProduct,
};
use ryeos_external_execution_contract::relay_file::{RelayFileEffect, RelayFileKind};
use ryeos_state::external_execution::admission::ExternalCandidateRequirement;
use ryeos_state::external_execution::transport::{
    ExternalControllerFileRelayContract, ExternalNetworkInputPolicy,
};

const OWNER: &str = "ryeos-external-guest-occurrence-owner";
const SUPERVISOR: &str = "ryeos-external-candidate-supervisor";
const LAUNCHER: &str = "ryeos-external-candidate-launcher";
/// Time the launched candidate has to report its applied launch after Ready.
const LAUNCH_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Startup steps after the supervisor publishes its attach request, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LoopbackStep {
    /// Node composition, reservation, allocation and activation.
    Activation,
    /// The guest's attach request, read by the controller.
    AttachRequest,
    /// The controller's attach response, delivered to the mailbox and its
    /// readback accepted by the controller's journal.
    AttachResponse,
    /// The guest's first exchange frame, read by the controller.
    ChannelBound,
    /// The start owner admitted Ready and authored Release.
    Ready,
    /// The supervisor reported the released candidate's applied launch.
    CandidateLaunch,
}

/// Milliseconds from the loopback backend's creation, just before the start
/// loop, to each completed step.
#[derive(Debug, Clone, Default, Serialize)]
struct LoopbackTimings {
    activation_ms: Option<u64>,
    attach_request_ms: Option<u64>,
    attach_response_ms: Option<u64>,
    channel_bound_ms: Option<u64>,
    ready_ms: Option<u64>,
    candidate_launch_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum LoopbackOutcome {
    Ready {
        timings: LoopbackTimings,
        transcript: Vec<String>,
        owner_stdout: String,
        owner_stderr: String,
        owner_cleanup: String,
    },
    Stopped {
        step: LoopbackStep,
        message: String,
        timings: LoopbackTimings,
        transcript: Vec<String>,
        owner_stdout: String,
        owner_stderr: String,
        owner_cleanup: String,
    },
}

fn contact_line(contact: &GuestRelayLoopbackContact) -> String {
    format!(
        "{:>7} ms relay {:?} #{} {:?}{}",
        contact.at_ms,
        contact.kind,
        contact.sequence,
        contact.effect,
        match contact.effect {
            RelayFileEffect::Download if !contact.found => " absent".to_owned(),
            _ => format!(" {} bytes", contact.bytes),
        }
    )
}

fn first(
    contacts: &[GuestRelayLoopbackContact],
    kind: RelayFileKind,
    effect: RelayFileEffect,
) -> Option<u64> {
    contacts
        .iter()
        .find(|contact| contact.kind == kind && contact.effect == effect && contact.found)
        .map(|contact| contact.at_ms)
}

/// Drive one composed loopback session from the first start advance to an
/// applied candidate launch, or to the first step that did not complete.
fn run_loopback(
    setup: &GuestRelayLoopbackSetup<'_>,
    root: &Path,
    controller_clock_offset_ms: i64,
) -> Result<LoopbackOutcome> {
    let (_store_owner, mut state) = ryeos_app::state::test_support::build_bound(root)?;
    // Everything the controller issues from here on carries its offset clock;
    // the guest, a separate process, keeps the host's.
    let _clock = ControllerClockOffset::install(controller_clock_offset_ms);
    let mut session = compose_guest_relay_loopback_session(&mut state, setup)?;
    let backend = std::sync::Arc::clone(&session.backend);
    let inputs = session
        .guest_inputs
        .take()
        .context("loopback session has no guest inputs")?;
    let ready = super::await_external_candidate_ready(&state, &session.placement, inputs);
    let ready_ms = ready.is_ok().then(|| backend.elapsed_ms());
    let mut message = None;
    let mut launched_ms = None;
    match ready {
        Ok(pump) => {
            let deadline = Instant::now() + LAUNCH_WINDOW;
            loop {
                let kinds = journalled_frame_kinds(&state)?;
                if kinds.iter().any(|(direction, kind)| {
                    direction == "supervisor_to_owner" && kind == "runtime_applied"
                }) {
                    launched_ms = Some(backend.elapsed_ms());
                    break;
                }
                if let Some(pump) = &pump
                    && let Err(error) = pump.ensure_running()
                {
                    message = Some(format!("{error:#}"));
                    break;
                }
                if Instant::now() > deadline {
                    message = Some(format!(
                        "the supervisor reported no applied launch within {}s of Ready",
                        LAUNCH_WINDOW.as_secs()
                    ));
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            drop(pump);
        }
        Err(error) => message = Some(format!("{error:#}")),
    }
    let contacts = backend.contacts();
    let mut transcript: Vec<String> = contacts.iter().map(contact_line).collect();
    transcript.extend(
        journalled_frame_kinds(&state)?
            .into_iter()
            .map(|(direction, kind)| format!("frame {direction} {kind}")),
    );
    let timings = LoopbackTimings {
        activation_ms: backend.activated_at_ms(),
        attach_request_ms: first(&contacts, RelayFileKind::Attach, RelayFileEffect::Download),
        // The response is complete when its readback is accepted, not when
        // its upload was sent.
        attach_response_ms: accepted_relay_transfers(&state)?
            .iter()
            .any(|(kind, _, direction)| kind == "attach" && direction == "controller_to_guest")
            .then(|| {
                contacts
                    .iter()
                    .rev()
                    .find(|contact| {
                        contact.kind == RelayFileKind::Attach
                            && contact.effect == RelayFileEffect::Download
                    })
                    .map(|contact| contact.at_ms)
            })
            .flatten(),
        channel_bound_ms: first(
            &contacts,
            RelayFileKind::Exchange,
            RelayFileEffect::Download,
        ),
        ready_ms,
        candidate_launch_ms: launched_ms,
    };
    let end = backend.stop_guest();
    drop(session);
    let (owner_stdout, owner_stderr, owner_cleanup) = end
        .map(|end| (end.owner_stdout, end.owner_stderr, end.owner_cleanup))
        .unwrap_or_default();
    let Some(message) = message else {
        return Ok(LoopbackOutcome::Ready {
            timings,
            transcript,
            owner_stdout,
            owner_stderr,
            owner_cleanup,
        });
    };
    let step = if timings.activation_ms.is_none() {
        LoopbackStep::Activation
    } else if timings.attach_request_ms.is_none() {
        LoopbackStep::AttachRequest
    } else if timings.attach_response_ms.is_none() {
        LoopbackStep::AttachResponse
    } else if timings.channel_bound_ms.is_none() {
        LoopbackStep::ChannelBound
    } else if timings.ready_ms.is_none() {
        LoopbackStep::Ready
    } else {
        LoopbackStep::CandidateLaunch
    };
    Ok(LoopbackOutcome::Stopped {
        step,
        message,
        timings,
        transcript,
        owner_stdout,
        owner_stderr,
        owner_cleanup,
    })
}

/// Shifts this process's wall clock for its lifetime, one holder at a time.
struct ControllerClockOffset(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl ControllerClockOffset {
    fn install(offset_ms: i64) -> Self {
        static HOLD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let hold = HOLD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lillux::time::set_wall_clock_test_offset_ms(offset_ms);
        Self(hold)
    }
}

impl Drop for ControllerClockOffset {
    fn drop(&mut self) {
        lillux::time::set_wall_clock_test_offset_ms(0);
    }
}

fn print(outcome: &LoopbackOutcome) {
    eprintln!("{}", serde_json::to_string_pretty(outcome).unwrap());
}

fn skip(reason: &str) -> bool {
    assert!(
        std::env::var("RYEOS_GUEST_RELAY_LOOPBACK").as_deref() != Ok("1"),
        "guest relay loopback required, but {reason}"
    );
    eprintln!("guest relay loopback skipped: {reason}");
    true
}

fn work_parent() -> PathBuf {
    std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
}

/// The host executable and guest executables, or why the case skips.
fn executables() -> std::result::Result<(PathBuf, PathBuf), String> {
    let host = std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_HOST")
        .map(PathBuf::from)
        .ok_or_else(|| "RYEOS_GUEST_RELAY_LOOPBACK_HOST is not set".to_owned())?;
    let bin = std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .ancestors()
                .nth(3)
                .unwrap()
                .join("x86_64-unknown-linux-gnu/release")
        });
    for path in [
        host.clone(),
        bin.join(OWNER),
        bin.join(SUPERVISOR),
        bin.join(LAUNCHER),
    ] {
        if !path.is_file() {
            return Err(format!("{} is not built", path.display()));
        }
    }
    let facts = lillux::process_control::HostFacts::observe(0, &work_parent());
    if !facts.unshare_user_pid_mount.allowed() {
        return Err("this host does not allow unprivileged namespaces".into());
    }
    Ok((host, bin))
}

fn synthetic_relay() -> ExternalControllerFileRelayContract {
    serde_json::from_value(serde_json::json!({
        "schema": 3,
        "route_contract": "ryeos.external-channel.controller-file-relay.v1",
        "owner_workspace_id": "loopback-workspace",
        "private_mailbox_parent": "/ryeos/occurrences",
        "maximum_file_bytes": 1048576,
        "maximum_frames": 1024,
        "poll_interval_ms": 250,
        "poll_deadline_ms": 30000
    }))
    .unwrap()
}

fn synthetic_network() -> ExternalNetworkInputPolicy {
    serde_json::from_value(serde_json::json!({
        "resolver": {"source": "/etc/resolv.conf", "max_bytes": 65536},
        "hosts": {"source": "/etc/hosts", "max_bytes": 65536}
    }))
    .unwrap()
}

/// A candidate recipe over the launcher executable: it starts and exits, which
/// is enough to prove the launch the supervisor applies after Release.
fn synthetic_requirement() -> ExternalCandidateRequirement {
    let mut requirement =
        ryeos_state::external_execution::admission::test_support::fixture_requirement();
    requirement.runtime_recipe.executable_relative_path = "bin/candidate".into();
    requirement
}

/// What a synthetic case varies from the baseline that reaches Ready.
#[derive(Default)]
struct Scenario {
    fault: Option<GuestRelayLoopbackFault>,
    /// Milliseconds the controller's wall clock is shifted from the guest's.
    controller_clock_offset_ms: i64,
    /// Run this executable as the supervisor instead of the real one.
    supervisor: Option<&'static str>,
}

fn run_synthetic(fault: Option<GuestRelayLoopbackFault>) -> Option<LoopbackOutcome> {
    run_scenario(Scenario {
        fault,
        ..Scenario::default()
    })
}

fn run_scenario(scenario: Scenario) -> Option<LoopbackOutcome> {
    let (host, bin) = match executables() {
        Ok(found) => found,
        Err(reason) => {
            skip(&reason);
            return None;
        }
    };
    let inputs = tempfile::Builder::new()
        .prefix("guest-relay-loopback-inputs-")
        .tempdir_in(work_parent())
        .unwrap();
    let product = inputs.path().join("product/bin");
    std::fs::create_dir_all(&product).unwrap();
    lillux::atomic_write_with_mode(
        &product.join("candidate"),
        &std::fs::read(bin.join(LAUNCHER)).unwrap(),
        0o755,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let outcome = run_loopback(
        &GuestRelayLoopbackSetup {
            // The relay binding admits only its signed relay backend; the
            // loopback stands in for it.
            backend_id: "test-backend",
            owner_executable: &bin.join(OWNER),
            profile: GuestOwnerRuntimeProfile {
                schema: 2,
                account: lillux::GuestRuntimeAccount::Unix {
                    uid: 65534,
                    gid: 65534,
                },
                private_source_max_bytes: 1024 * 1024 * 1024,
                private_source_max_inodes: 4096,
                owner_timeout_seconds: 120,
            },
            supervisor_executable: &bin.join(scenario.supervisor.unwrap_or(SUPERVISOR)),
            launcher_executable: &bin.join(LAUNCHER),
            requirement: synthetic_requirement(),
            product: &inputs.path().join("product"),
            product_manifest_record: None,
            file_products: &[],
            base_snapshot: None,
            relay: synthetic_relay(),
            network_inputs: synthetic_network(),
            work_parent: &work_parent(),
            host_executable: &host,
            fault: scenario.fault,
        },
        root.path(),
        scenario.controller_clock_offset_ms,
    )
    .unwrap();
    print(&outcome);
    Some(outcome)
}

#[test]
fn synthetic_loopback_reaches_ready_and_the_candidate_launch() {
    if std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT").is_some() {
        return;
    }
    let Some(outcome) = run_synthetic(None) else {
        return;
    };
    assert!(
        matches!(outcome, LoopbackOutcome::Ready { .. }),
        "synthetic loopback stopped: {outcome:?}"
    );
}

#[test]
fn a_corrupted_attach_response_stops_at_the_attach_response() {
    if std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT").is_some() {
        return;
    }
    let Some(outcome) = run_synthetic(Some(GuestRelayLoopbackFault::CorruptAttachResponse)) else {
        return;
    };
    let LoopbackOutcome::Stopped { step, .. } = outcome else {
        panic!("a corrupted attach response reached Ready");
    };
    assert_eq!(step, LoopbackStep::AttachResponse);
}

/// Inside the tolerance the guest declares for its freshness checks, a
/// controller clock that is ahead of or behind the guest's changes nothing.
#[test]
fn a_controller_clock_within_the_skew_tolerance_still_attaches() {
    if std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT").is_some() {
        return;
    }
    for offset_ms in [-30_000, 30_000] {
        let Some(outcome) = run_scenario(Scenario {
            controller_clock_offset_ms: offset_ms,
            ..Scenario::default()
        }) else {
            return;
        };
        assert!(
            matches!(outcome, LoopbackOutcome::Ready { .. }),
            "a controller clock {offset_ms} ms off the guest's stopped the loopback: {outcome:?}"
        );
    }
}

/// Far outside the tolerance the guest refuses, and the refusal reaches the
/// controller as the guest owner's own report of the step it refused at.
#[test]
fn a_controller_clock_far_outside_the_skew_tolerance_is_refused_by_name() {
    if std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT").is_some() {
        return;
    }
    let Some(outcome) = run_scenario(Scenario {
        controller_clock_offset_ms: -3_600_000,
        ..Scenario::default()
    }) else {
        return;
    };
    let LoopbackOutcome::Stopped {
        message,
        owner_stderr,
        ..
    } = outcome
    else {
        panic!("a controller clock an hour off the guest's reached Ready");
    };
    // The step the owner refused at, and its own bounded account of why: the
    // observed difference, the tolerance and what the guest knows of its clock.
    assert!(
        message.contains("guest owner refused at admission_expired: guest clock is")
            && message.contains("declared skew tolerance 60 s")
            && message.contains("guest clock trust:"),
        "the start failure does not carry the owner's refusal detail: {message}"
    );
    assert!(
        owner_stderr.contains("guest clock is") && owner_stderr.contains("skew tolerance"),
        "the owner's diagnostics do not name the clock: {owner_stderr}"
    );
}

/// A supervisor that ends before it attaches: the guest owner reports the end
/// through its mailbox, and the start failure carries that report.
#[test]
fn an_early_supervisor_exit_reaches_the_start_failure_through_the_owner_report() {
    if std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT").is_some() {
        return;
    }
    // The launcher is a real guest executable that is not a supervisor: it
    // starts and refuses its arguments.
    let Some(outcome) = run_scenario(Scenario {
        supervisor: Some(LAUNCHER),
        ..Scenario::default()
    }) else {
        return;
    };
    let LoopbackOutcome::Stopped {
        message,
        owner_stdout,
        ..
    } = outcome
    else {
        panic!("a supervisor that is not a supervisor reached Ready");
    };
    assert!(
        owner_stdout.contains("supervisor_exit"),
        "the owner never reported the supervisor's end: {owner_stdout}"
    );
    assert!(
        message.contains("supervisor exited"),
        "the start failure does not carry the owner's report of the supervisor's end: {message}"
    );
}

/// What this machine's enforced isolation qualifies as descendant containment:
/// the lane a Q Worker would take here. Prints the readiness; asserts only that
/// the generation composes, since a host without the mechanism is a finding,
/// not a failure.
#[test]
fn this_nodes_enforced_isolation_readiness_is_reported() {
    if std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT").is_some() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut policy = ryeos_engine::isolation::IsolationPolicy::disabled_for_authoring();
    policy.mode = ryeos_engine::isolation::IsolationMode::Enforce;
    policy.backend = Some(ryeos_isolation_protocol::IsolationBackendSelection {
        bundle: "core".into(),
        implementation: "linux-lillux".into(),
    });
    policy.filesystem.proc_filesystem =
        ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespace;
    let roots = vec![ryeos_engine::test_support::core_bundle_root()];
    let trust = ryeos_engine::test_support::live_trust_store();
    let isolation = match ryeos_node_runtime::engine_init::load_test_execution_isolation(
        root.path(),
        &roots,
        &trust,
        policy,
    ) {
        Ok(isolation) => isolation,
        Err(error) => {
            eprintln!("enforced isolation does not compose here: {error:#}");
            assert!(
                std::env::var("RYEOS_GUEST_RELAY_LOOPBACK").as_deref() != Ok("1"),
                "isolation required, but it does not compose: {error:#}"
            );
            return;
        }
    };
    eprintln!(
        "enforced: {}\nprocess scope readiness: {}",
        isolation.is_enforced(),
        serde_json::to_string_pretty(&isolation.inspection().process_scope_readiness).unwrap()
    );
}

/// One regular-file entry of an installed content manifest, either tier.
struct ManifestFile {
    path: String,
    mode: Option<u32>,
    blob_hash: Option<String>,
    file_sha256: Option<String>,
    size: Option<u64>,
}

fn manifest_files(value: &serde_json::Value) -> Result<Vec<ManifestFile>> {
    use ryeos_state::objects::{
        EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, ExternalContentManifestEntryKind,
        ExternalContentManifestObject, ExternalLargeContentManifestObject,
    };
    if value["kind"] == EXTERNAL_LARGE_CONTENT_MANIFEST_KIND {
        Ok(ExternalLargeContentManifestObject::from_value(value)?
            .entries
            .into_iter()
            .filter(|entry| entry.kind == ExternalContentManifestEntryKind::File)
            .map(|entry| ManifestFile {
                path: entry.path,
                mode: entry.mode,
                blob_hash: entry.blob_hash,
                file_sha256: entry.file_sha256,
                size: entry.size,
            })
            .collect())
    } else {
        Ok(ExternalContentManifestObject::from_value(value)?
            .entries
            .into_iter()
            .filter(|entry| entry.kind == ExternalContentManifestEntryKind::File)
            .map(|entry| ManifestFile {
                path: entry.path,
                mode: entry.mode,
                blob_hash: entry.blob_hash,
                file_sha256: None,
                size: entry.size,
            })
            .collect())
    }
}

/// Copy one manifest file out of the node's CAS or large-object store.
fn copy_member(app: &Path, cas: &lillux::CasStore, file: &ManifestFile, to: &Path) -> Result<()> {
    let bytes = match (file.blob_hash.as_deref(), file.file_sha256.as_deref()) {
        (Some(blob), _) => cas
            .get_blob_bounded(blob, 512 * 1024 * 1024)?
            .with_context(|| format!("blob {blob} is not in the node's CAS"))?,
        (None, Some(large)) => {
            let state = lillux::PinnedDirectory::open(&app.join(".ai/state"))?
                .context("node has no runtime state directory")?;
            let store = ryeos_state::LargeObjectStore::open_under(&state)?
                .context("node has no large-object store")?;
            let lease = store.lease_object(large, file.size.context("large file has no size")?)?;
            std::fs::read(lease.path())?
        }
        _ => anyhow::bail!("manifest file {} has no content", file.path),
    };
    lillux::atomic_write_with_mode(to, &bytes, file.mode.unwrap_or(0o755))?;
    Ok(())
}

fn find_bundle_member(app: &Path, name: &str, sha256: &str) -> Result<PathBuf> {
    for bundle in std::fs::read_dir(app.join(".ai/bundles"))? {
        let bin = bundle?.path().join(".ai/bin");
        let Ok(targets) = std::fs::read_dir(&bin) else {
            continue;
        };
        for target in targets {
            let candidate = target?.path().join(name);
            if candidate.is_file() && lillux::sha256_hex(&std::fs::read(&candidate)?) == sha256 {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!("no installed bundle carries {name} with sha256 {sha256}")
}

/// The node's selected products, as the binding and its bundles name them.
fn run_node(app: &Path, binding_id: &str, host: &Path) -> Result<LoopbackOutcome> {
    let text = std::fs::read_to_string(
        app.join(".ai/node/external_execution")
            .join(format!("{binding_id}.yaml")),
    )?;
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("# ryeos:signed:"))
        .collect::<Vec<_>>()
        .join("\n");
    let binding: serde_json::Value = serde_yaml::from_str(&body)?;
    let field = |name: &str| -> Result<String> {
        binding[name]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("binding has no {name}"))
    };
    let cas = lillux::CasStore::new(app.join(".ai/state/objects"));
    let inputs = tempfile::Builder::new()
        .prefix("guest-relay-loopback-node-")
        .tempdir_in(work_parent())?;

    let runtime_hash = field("guest_runtime_manifest_hash")?;
    let runtime =
        manifest_files(&cas.get_object(&runtime_hash)?.with_context(|| {
            format!("runtime manifest {runtime_hash} is not in the node's CAS")
        })?)?;
    let owner = inputs.path().join("owner");
    copy_member(
        app,
        &cas,
        runtime
            .iter()
            .find(|entry| entry.path == format!("bin/{OWNER}"))
            .context("runtime manifest has no owner")?,
        &owner,
    )?;
    let profile = GuestOwnerRuntimeProfile::from_cas(&cas, &runtime_hash)?
        .context("runtime manifest has no owner profile")?;

    // The binding's requirement as its signed worker profile declares it.
    let profile_document: serde_json::Value = serde_json::from_slice(&std::fs::read(
        app.join(".ai/bundles/codex/.ai/workers/codex/lib/hosted/external-authoring.profile.json"),
    )?)?;
    let mut requirement: ExternalCandidateRequirement =
        serde_json::from_value(profile_document["external_candidate"].clone())?;
    let requirement_digest = requirement.qualification_requirement_digest()?;
    eprintln!(
        "node requirement digest {requirement_digest} (binding {})",
        binding["workload"]["requirement_digest"]
    );
    // The fixture qualification selection is keyed by this declaration.
    requirement.runtime_product_declaration_id = "auxiliary".into();

    // The candidate product, delivered under a content manifest of its files.
    let product_hash = binding["workload"]["runtime_manifest_hash"]
        .as_str()
        .context("binding workload has no runtime manifest")?;
    let product = inputs.path().join("product");
    for file in
        manifest_files(&cas.get_object(product_hash)?.with_context(|| {
            format!("product manifest {product_hash} is not in the node's CAS")
        })?)?
    {
        let target = product.join(&file.path);
        std::fs::create_dir_all(target.parent().context("product member has no parent")?)?;
        copy_member(app, &cas, &file, &target)?;
    }

    let file_product = match std::env::var("RYEOS_GUEST_RELAY_LOOPBACK_FILE_PRODUCT") {
        Ok(hash) => {
            let record = cas
                .get_object_bytes_bounded(&hash, 4 * 1024 * 1024)?
                .with_context(|| format!("file product {hash} is not in the node's CAS"))?;
            let file = manifest_files(&serde_json::from_slice(&record)?)?
                .into_iter()
                .next()
                .context("file product manifest has no file")?;
            let content = inputs.path().join("file-product");
            copy_member(app, &cas, &file, &content)?;
            let record_path = inputs.path().join("file-product-record.json");
            std::fs::write(&record_path, record)?;
            Some((record_path, content))
        }
        Err(_) => None,
    };
    let file_products: Vec<GuestStartRehearsalFileProduct<'_>> = file_product
        .iter()
        .map(|(record, content)| GuestStartRehearsalFileProduct {
            authority_id: "provider-executable",
            destination: "/ryeos/realizations/provider-executable",
            file: content,
            manifest_record: record,
        })
        .collect();
    let base_hash = std::env::var("RYEOS_GUEST_RELAY_LOOPBACK_BASE_SNAPSHOT").ok();
    let root = tempfile::tempdir()?;
    run_loopback(
        &GuestRelayLoopbackSetup {
            backend_id: &field("backend")?,
            owner_executable: &owner,
            profile,
            supervisor_executable: &find_bundle_member(
                app,
                SUPERVISOR,
                &field("supervisor_artifact_hash")?,
            )?,
            launcher_executable: &find_bundle_member(
                app,
                LAUNCHER,
                &field("launcher_artifact_hash")?,
            )?,
            requirement,
            product: &product,
            product_manifest_record: None,
            file_products: &file_products,
            base_snapshot: base_hash
                .as_deref()
                .map(|snapshot_hash| GuestStartRehearsalBase {
                    cas: &cas,
                    snapshot_hash,
                }),
            relay: serde_json::from_value(binding["controller_transport"].clone())?,
            network_inputs: serde_json::from_value(binding["provider_network_inputs"].clone())?,
            work_parent: &work_parent(),
            host_executable: host,
            fault: None,
        },
        root.path(),
        0,
    )
}

#[test]
fn node_loopback_reports_its_outcome() {
    let Some(app) = std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT") else {
        return;
    };
    let binding = std::env::var("RYEOS_GUEST_RELAY_LOOPBACK_BINDING")
        .expect("RYEOS_GUEST_RELAY_LOOPBACK_BINDING names the binding");
    let (host, _) = match executables() {
        Ok(found) => found,
        Err(reason) => {
            skip(&reason);
            return;
        }
    };
    let outcome = run_node(Path::new(&app), &binding, &host).expect("run the node loopback");
    print(&outcome);
}

/// Write a node placement's original relay bootstrap and signed import to
/// `RYEOS_GUEST_RELAY_LOOPBACK_DUMP` for rehearsal.
#[test]
fn node_relay_bootstrap_dump() {
    let (Some(app), Ok(placement), Ok(digest), Some(out)) = (
        std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_APP_ROOT"),
        std::env::var("RYEOS_GUEST_RELAY_LOOPBACK_PLACEMENT"),
        std::env::var("RYEOS_GUEST_RELAY_LOOPBACK_REQUEST_DIGEST"),
        std::env::var_os("RYEOS_GUEST_RELAY_LOOPBACK_DUMP"),
    ) else {
        return;
    };
    let (bootstrap, signed_import) =
        ryeos_app::external_placement::guest_relay_loopback::read_retained_relay_bootstrap(
            Path::new(&app),
            &placement,
            &digest,
        )
        .unwrap();
    std::fs::write(
        out,
        serde_json::to_vec_pretty(&serde_json::json!({
            "bootstrap": bootstrap,
            "signed_import": signed_import,
        }))
        .unwrap(),
    )
    .unwrap();
}
