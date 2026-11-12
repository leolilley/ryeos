//! Dedicated, one-shot owner below the existing external Worker workflow.
//!
//! A qualified guest snapshot must install this executable and the exact
//! controller-root/profile tree. Until a lifecycle adapter has qualified that
//! snapshot and proved this process survives a lost run stream, it must
//! remain fail-closed.
//!
//! Stdout carries only the contract's startup report lines: the phase as each
//! step begins (repeated while a long step makes progress), then held once
//! the supervisor is released, or the closed step word that refused. After
//! held the owner keeps supervising and reports the supervisor's end with a
//! bounded, sanitized tail of the supervisor's own stderr, both on stdout and
//! as one file in the occurrence's relay mailbox. Its own stderr
//! keeps the full error for local diagnosis and is never relayed.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution::guest_installation::prepare_authorized_held_guest_supervisor_once;
use ryeos_external_execution_contract::guest_import_authorization::{
    MAX_GUEST_IMPORT_AUTHORIZATION_BYTES, MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES,
};
use ryeos_external_execution_contract::guest_owner_report::{
    GuestClockReport, GuestClockTrust, GuestOwnerReport, GuestOwnerStep as Step,
    GuestRefusalDetail, GuestSupervisorExit, GuestSupervisorStatus,
    MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES, RefusedAt as _,
};

const RUNTIME_ROOT: &str = "/ryeos/guest-runtime";
const ACTIVATION_ROOT: &str = "/ryeos/activation";
const OCCURRENCES_ROOT: &str = ryeos_external_execution_contract::GUEST_OCCURRENCES_ROOT;
const IMPORT_NAME: &str = "signed-import.json";
const PACKAGE_NAME: &str = "guest-package";
const POLL_INTERVAL: lillux::time::Duration = lillux::time::Duration::from_millis(50);
const CLEANUP_TIMEOUT: lillux::time::Duration = lillux::time::Duration::from_secs(30);
/// A long step's progress points repeat its phase at most this often.
const PHASE_REPEAT_MS: u64 = 10_000;
/// How long to wait for a dead supervisor's last stderr bytes to arrive.
const STDERR_GRACE: lillux::time::Duration = lillux::time::Duration::from_secs(2);
const NO_PHASE: usize = usize::MAX;

static PHASE: AtomicUsize = AtomicUsize::new(NO_PHASE);
static LAST_REPORT_MS: AtomicU64 = AtomicU64::new(0);
static CLOCK: OnceLock<lillux::time::MonotonicTimer> = OnceLock::new();

fn decode_assignment_argument(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Vec<u8>> {
    decode_assignment_words(args).refused_at(Step::AssignmentArgument)
}

fn decode_assignment_words(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Vec<u8>> {
    let mut args = args.into_iter();
    ensure!(
        args.next().as_deref() == Some(OsStr::new("--assignment-b64")),
        "guest owner requires only --assignment-b64"
    );
    let encoded = args.next().context("guest owner assignment is absent")?;
    ensure!(
        args.next().is_none(),
        "guest owner received extra arguments"
    );
    let encoded = encoded
        .to_str()
        .context("guest owner assignment is not UTF-8")?;
    ensure!(
        !encoded.is_empty()
            && encoded.len() <= (MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES * 4).div_ceil(3),
        "guest owner assignment exceeds command bound"
    );
    let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES
            && URL_SAFE_NO_PAD.encode(&bytes) == encoded,
        "guest owner assignment is not canonical base64url"
    );
    Ok(bytes)
}

fn open_private_root(path: &str) -> Result<lillux::PinnedDirectory> {
    let root = lillux::PinnedDirectory::open(Path::new(path))?
        .with_context(|| format!("guest owner has no installed {path} root"))?;
    root.require_owner_private_directory()?;
    Ok(root)
}

fn admissible_inbound_mode(mode: u32) -> bool {
    // A host's file delivery need not specify the resulting mode. The
    // enclosing activation directory is owner-private; group/other read
    // bits do not grant traversal, while any non-owner write bit would add
    // an unnecessary mutation path before the signed-byte import.
    mode & !0o777 == 0 && mode & 0o700 == 0o600 && mode & 0o077 == mode & 0o044
}

fn require_admissible_inbound_file(file: &lillux::PinnedRegularFile) -> Result<()> {
    lillux::require_effective_user_owned_regular(&file.try_clone_descriptor()?)?;
    ensure!(
        admissible_inbound_mode(file.observation()?.full_permission_mode()?),
        "guest owner inbound file has unexpected mode"
    );
    Ok(())
}

fn read_signed_import(activation: &lillux::PinnedDirectory) -> Result<Vec<u8>> {
    read_signed_import_bytes(activation).refused_at(Step::ImportRead)
}

fn read_signed_import_bytes(activation: &lillux::PinnedDirectory) -> Result<Vec<u8>> {
    let file = activation
        .open_pinned_regular(OsStr::new(IMPORT_NAME), false)?
        .context("guest owner has no separately delivered signed import")?;
    require_admissible_inbound_file(&file)?;
    let observation = file.observation()?;
    file.read_stable_bounded(
        &observation,
        (MAX_GUEST_IMPORT_AUTHORIZATION_BYTES + 256) as u64,
    )
    .map_err(anyhow::Error::msg)
}

/// The host starts the exact owner executable as its administrator. Before
/// any guest work, give only the two mutable private roots and their
/// already-uploaded exact files to the signed non-root account, then discard
/// administrator credentials irreversibly. Immutable runtime members remain
/// root-owned and readable but not writable by that account: `/ryeos` and the
/// runtime root may arrive administrator-private, so both are delegated
/// read-only traversal first, and root keeps every mutation.
fn prepare_non_root_owner() -> Result<()> {
    transition_to_non_root_owner().refused_at(Step::AccountTransition)
}

fn transition_to_non_root_owner() -> Result<()> {
    let runtime_root = lillux::PinnedDirectory::open(Path::new(RUNTIME_ROOT))?
        .context("guest owner runtime root is absent before account transition")?;
    let observed = ObservedGuestRuntime::observe(&runtime_root)?;
    let account = &observed.profile().account;
    let activation = lillux::PinnedDirectory::open(Path::new(ACTIVATION_ROOT))?
        .context("guest owner activation root is absent")?;
    for name in [IMPORT_NAME, PACKAGE_NAME] {
        let file = activation
            .open_pinned_regular(OsStr::new(name), false)?
            .with_context(|| format!("guest owner has no uploaded {name}"))?;
        account.grant_private_file(&file)?;
    }
    account.grant_private_directory(&activation)?;
    let ryeos_root = lillux::PinnedDirectory::open(Path::new("/ryeos"))?
        .context("guest owner namespace root is absent")?;
    account.grant_readonly_host_directory(&ryeos_root)?;
    account.grant_readonly_host_directory(&runtime_root)?;
    let occurrences = ryeos_root.open_or_create_child(OsStr::new("occurrences"), 0o700)?;
    account.grant_private_directory(&occurrences)?;
    account.drop_current_process()?;
    Ok(())
}

fn now_ms() -> u64 {
    CLOCK
        .get_or_init(lillux::time::MonotonicTimer::start)
        .elapsed_millis()
}

fn report(report: GuestOwnerReport) {
    #[cfg(test)]
    tests::REPORTS.with(|reports| reports.borrow_mut().push(report.clone()));
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(report.line().as_bytes())
        .and_then(|()| stdout.flush());
    LAST_REPORT_MS.store(now_ms(), Ordering::Relaxed);
}

/// Report the step being entered; it stays current until the next one.
fn enter(step: Step) {
    PHASE.store(step as usize, Ordering::Relaxed);
    report(GuestOwnerReport::Phase(step));
}

fn phase_repeat_due(last_report_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_report_ms) >= PHASE_REPEAT_MS
}

/// Lillux progress beacon: called synchronously at natural progress points
/// inside long steps (upload bytes, staged entries, copied and digested
/// files), so a silent step still repeats its phase on this one thread.
fn progress_beacon() {
    if phase_repeat_due(LAST_REPORT_MS.load(Ordering::Relaxed), now_ms())
        && let Some(step) = Step::ALL.get(PHASE.load(Ordering::Relaxed)).copied()
    {
        report(GuestOwnerReport::Phase(step));
    }
}

fn run(assignment_bytes: &[u8]) -> Result<()> {
    enter(Step::RuntimeRoot);
    let mut runtime = (|| -> Result<_> {
        let runtime_root = lillux::PinnedDirectory::open(Path::new(RUNTIME_ROOT))?
            .context("qualified guest runtime root is absent")?;
        let runtime = ObservedGuestRuntime::observe(&runtime_root)?;
        runtime.profile().account.require_current_process()?;
        Ok(runtime)
    })()
    .refused_at(Step::RuntimeRoot)?;
    enter(Step::PrivateRoots);
    let (activation, occurrences) = (|| -> Result<_> {
        let activation = open_private_root(ACTIVATION_ROOT)?;
        let occurrences = open_private_root(OCCURRENCES_ROOT)?;
        runtime.require_disjoint_directory_tree(&activation)?;
        runtime.require_disjoint_directory_tree(&occurrences)?;
        activation.require_disjoint_directory_tree(&occurrences)?;
        Ok((activation, occurrences))
    })()
    .refused_at(Step::PrivateRoots)?;
    enter(Step::ImportRead);
    let import_bytes = read_signed_import(&activation)?;
    enter(Step::ImportVerify);
    let admitted = runtime
        .verify_import_documents(&import_bytes, assignment_bytes)
        .refused_at(Step::ImportVerify)?;
    admitted
        .require_fresh_admission()
        .refused_with_detail(Step::AdmissionExpired)?;
    enter(Step::PackageIdentity);
    let upload = (|| -> Result<_> {
        let upload = activation
            .open_pinned_regular(OsStr::new(PACKAGE_NAME), false)?
            .context("guest owner has no exact uploaded package")?;
        require_admissible_inbound_file(&upload)?;
        ensure!(
            upload.observation()?.size() == admitted.authorization().ticket.framed_bytes,
            "guest owner upload changed its exact signed size"
        );
        Ok(upload)
    })()
    .refused_at(Step::PackageIdentity)?;
    enter(Step::OccurrenceRoot);
    let mut occurrence = (|| -> Result<_> {
        let occurrence =
            occurrences.create_child(OsStr::new(&admitted.authorization().occurrence_id), 0o700)?;
        occurrence.require_owner_private_directory()?;
        Ok(occurrence)
    })()
    .refused_at(Step::OccurrenceRoot)?;
    let limits = runtime.profile().private_source_limits();
    let lifetime = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(runtime.profile().owner_timeout_seconds),
    ));
    enter(Step::PrivateSourceEntry);
    let source = lillux::sandbox::enter_linux_private_source_filesystem(limits)
        .map_err(anyhow::Error::msg)
        .refused_at(Step::PrivateSourceEntry)?;
    // Every mount has a new identity in the private namespace just entered.
    // Re-anchor the two roots carried across it before any recheck.
    enter(Step::PathRebind);
    runtime
        .rebind_in_current_mount_namespace()
        .and_then(|()| occurrence.rebind_in_current_mount_namespace())
        .refused_at(Step::PathRebind)?;
    let ready_boot_inputs = admitted.ready_boot_inputs(&import_bytes);
    let (mut supervisor_stderr, supervisor_stderr_writer) =
        lillux::stderr_tail::StderrTail::open(MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES)
            .map_err(anyhow::Error::msg)
            .refused_at(Step::HeldPrepare)?;
    let mut held = prepare_authorized_held_guest_supervisor_once(
        &occurrence,
        &upload,
        &runtime,
        &import_bytes,
        assignment_bytes,
        source,
        lifetime,
        Some(ready_boot_inputs),
        Some(supervisor_stderr_writer),
        &mut enter,
    )?;
    enter(Step::MountPreparation);
    held.mount_preparation_receipt()
        .refused_at(Step::MountPreparation)?;
    enter(Step::SignalRegistration);
    let cancelling = Arc::new(AtomicBool::new(false));
    // Provider stream loss is not a cancellation. Retain this occurrence on
    // SIGHUP and wait for the original supervisor/channel outcome.
    let _stream_hangup = Arc::new(AtomicBool::new(false));
    (|| -> Result<_> {
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            signal_hook::flag::register(signal, Arc::clone(&cancelling))?;
        }
        signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&_stream_hangup))?;
        Ok(())
    })()
    .refused_at(Step::SignalRegistration)?;
    enter(Step::Release);
    let mut released = held.release_once().refused_at(Step::Release)?;
    report(GuestOwnerReport::Held);
    loop {
        supervisor_stderr.drain();
        let natural = match released.try_observe_natural_exit() {
            Ok(exit) => exit,
            Err(error) => {
                let cleanup = released.terminate_namespace_for_export_until(
                    lillux::time::MonotonicDeadline::after(CLEANUP_TIMEOUT),
                );
                return Err(match cleanup {
                    Ok(_) => error.context("guest owner settled the refused native target"),
                    Err(cleanup) => error.context(format!(
                        "guest owner could not prove refused target death: {cleanup:#}"
                    )),
                })
                .refused_at(Step::SupervisorExit);
            }
        };
        if let Some(exit) = natural {
            let tail = supervisor_stderr.finish(STDERR_GRACE);
            let ended = supervisor_exit_report(exit.termination(), &tail);
            // The mailbox file is the controller's in-band copy; failing to
            // write it must not change how this owner settles.
            if let Some(sink) = released.report_sink()
                && let Err(error) = sink.publish(ended.clone())
            {
                eprintln!("guest owner could not publish its supervisor report: {error:#}");
            }
            report(GuestOwnerReport::SupervisorExit(ended));
            ensure!(
                exit.target_reported_success(),
                anyhow::anyhow!(
                    "guest supervisor exited without successful applied native launch: {:?}{}",
                    exit.termination().exit(),
                    exit.termination()
                        .launch_failure()
                        .map(|failure| format!("; launch failure: {}", failure.diagnostic()))
                        .unwrap_or_default()
                )
                .context(Step::SupervisorExit)
            );
            // The controller must independently join its authenticated
            // supervisor transcript and frozen export. This exit is only
            // native namespace writer exclusion.
            return Ok(());
        }
        if lifetime.has_elapsed() || cancelling.load(Ordering::Acquire) {
            let _death = released
                .terminate_namespace_for_export_until(lillux::time::MonotonicDeadline::after(
                    CLEANUP_TIMEOUT,
                ))
                .refused_at(Step::Lifetime)?;
            return Err(anyhow::anyhow!(
                "guest owner cancelled or exceeded installed lifetime"
            ))
            .refused_at(Step::Lifetime);
        }
        lillux::time::sleep(POLL_INTERVAL);
    }
}

/// What the owner relays of a supervisor's end: its status, then whatever
/// Lillux recorded of a failed launch, then the supervisor's stderr tail.
fn supervisor_exit_report(
    termination: &lillux::LinuxSandboxTermination,
    stderr: &[u8],
) -> GuestSupervisorExit {
    supervisor_exit_from_parts(
        termination.exit(),
        termination
            .launch_failure()
            .map(|failure| failure.diagnostic()),
        stderr,
    )
}

fn supervisor_exit_from_parts(
    exit: lillux::LinuxSandboxExit,
    launch_failure: Option<&str>,
    stderr: &[u8],
) -> GuestSupervisorExit {
    let status = match exit {
        lillux::LinuxSandboxExit::Code(code) => GuestSupervisorStatus::Code(code),
        lillux::LinuxSandboxExit::Signal(signal) => GuestSupervisorStatus::Signal(signal),
    };
    let mut text = Vec::new();
    if let Some(diagnostic) = launch_failure {
        text.extend_from_slice(b"launch failure: ");
        text.extend_from_slice(diagnostic.as_bytes());
        text.push(b'\n');
    }
    text.extend_from_slice(stderr);
    GuestSupervisorExit::from_raw_tail(status, &text).with_clock(GuestClockReport {
        trust: match lillux::time::wall_clock_trust() {
            lillux::time::WallClockTrust::Synced {
                maximum_error_us,
                estimated_error_us,
            } => GuestClockTrust::Synced {
                maximum_error_us,
                estimated_error_us,
            },
            lillux::time::WallClockTrust::Unsynchronized => GuestClockTrust::Unsynchronized,
            lillux::time::WallClockTrust::Unknown => GuestClockTrust::Unknown,
        },
        observed_issue_lag_ms: GuestClockReport::observed_issue_lag_from_stderr(stderr),
    })
}

fn owner() -> Result<()> {
    lillux::progress::install_beacon(progress_beacon);
    enter(Step::AssignmentArgument);
    let assignment = decode_assignment_argument(std::env::args_os().skip(1))?;
    enter(Step::AccountTransition);
    prepare_non_root_owner()?;
    run(&assignment)
}

fn main() {
    if let Err(error) = owner() {
        // The chain's text goes out on the closed-word line as a bounded,
        // sanitized tail; the controller shows it and never acts on it.
        // Only a refusal site that vouched for its text sends any: the line
        // otherwise names the closed step word and nothing else.
        report(GuestOwnerReport::refused(
            Step::of(&error),
            GuestRefusalDetail::of(&error).unwrap_or_default(),
        ));
        eprintln!("Error: {error:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    thread_local! {
        pub(super) static REPORTS: std::cell::RefCell<Vec<GuestOwnerReport>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    fn take_reports() -> Vec<GuestOwnerReport> {
        REPORTS.with(|reports| std::mem::take(&mut *reports.borrow_mut()))
    }

    #[test]
    fn phases_are_reported_on_entry_and_repeat_only_after_the_interval() {
        take_reports();
        if !Path::new(RUNTIME_ROOT).exists() {
            assert_eq!(
                Step::of(&run(b"assignment").unwrap_err()),
                Step::RuntimeRoot
            );
            assert_eq!(
                take_reports(),
                vec![GuestOwnerReport::Phase(Step::RuntimeRoot)]
            );
        }
        assert!(!phase_repeat_due(5_000, 5_000));
        assert!(!phase_repeat_due(5_000, 5_000 + PHASE_REPEAT_MS - 1));
        assert!(phase_repeat_due(5_000, 5_000 + PHASE_REPEAT_MS));
        assert!(!phase_repeat_due(9_000, 1_000));
    }

    #[test]
    fn owner_phases_follow_startup_order() {
        // `owner` enters the first two before `run`, which enters the rest in
        // source order; the library enters stage through held_prepare
        // between path_rebind and mount_preparation.
        let entered = [
            Step::AssignmentArgument,
            Step::AccountTransition,
            Step::RuntimeRoot,
            Step::PrivateRoots,
            Step::ImportRead,
            Step::ImportVerify,
            Step::PackageIdentity,
            Step::OccurrenceRoot,
            Step::PrivateSourceEntry,
            Step::PathRebind,
            Step::Stage,
            Step::Install,
            Step::LaunchPreparation,
            Step::HeldPrepare,
            Step::MountPreparation,
            Step::SignalRegistration,
            Step::Release,
        ];
        assert!(
            entered
                .windows(2)
                .all(|pair| (pair[0] as usize) < (pair[1] as usize))
        );
        let source = include_str!("main.rs");
        let positions: Vec<usize> = entered
            .iter()
            .filter(|step| {
                !matches!(
                    step,
                    Step::AssignmentArgument
                        | Step::AccountTransition
                        | Step::Stage
                        | Step::Install
                        | Step::LaunchPreparation
                        | Step::HeldPrepare
                )
            })
            .map(|step| {
                let needle = format!("enter(Step::{step:?});");
                source.find(&needle).unwrap_or_else(|| panic!("{needle}"))
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn inbound_file_mode_allows_only_owner_writes_inside_private_activation_root() {
        for mode in [0o600, 0o640, 0o644] {
            assert!(admissible_inbound_mode(mode));
        }
        for mode in [
            0o000, 0o400, 0o500, 0o606, 0o620, 0o622, 0o666, 0o755, 0o777, 0o1600, 0o2600, 0o4600,
        ] {
            assert!(!admissible_inbound_mode(mode));
        }
    }

    #[test]
    fn private_activation_import_accepts_readable_upload_but_refuses_other_writers() {
        let activation = tempfile::tempdir().unwrap();
        std::fs::set_permissions(activation.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let upload = activation.path().join(IMPORT_NAME);
        std::fs::write(&upload, b"signed-import-fixture").unwrap();
        let root = lillux::PinnedDirectory::open(activation.path())
            .unwrap()
            .unwrap();
        root.require_owner_private_directory().unwrap();
        std::fs::set_permissions(&upload, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_signed_import(&root).unwrap(), b"signed-import-fixture");
        std::fs::set_permissions(&upload, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_signed_import(&root).is_err());
        std::fs::set_permissions(activation.path(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(root.require_owner_private_directory().is_err());
    }

    #[test]
    fn assignment_argument_is_canonical_bounded_and_single_use() {
        let exact = vec![b'a'; MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES];
        let encoded = URL_SAFE_NO_PAD.encode(&exact);
        assert_eq!(
            decode_assignment_argument(["--assignment-b64".into(), encoded.clone().into()])
                .unwrap(),
            exact
        );
        assert!(
            decode_assignment_argument(["--assignment-b64".into(), format!("{encoded}=").into()])
                .is_err()
        );
        assert!(
            decode_assignment_argument(["--assignment-b64".into(), encoded.into(), "extra".into()])
                .is_err()
        );
        assert!(
            decode_assignment_argument(["--assignment-b64".into(), "a".repeat(10_000).into()])
                .is_err()
        );
    }

    #[test]
    fn each_deterministic_early_refusal_names_its_step() {
        fn refused<T>(result: Result<T>) -> Step {
            Step::of(&result.err().expect("step refuses"))
        }
        assert_eq!(
            refused(decode_assignment_argument(["--other".into()])),
            Step::AssignmentArgument
        );
        let activation = tempfile::tempdir().unwrap();
        std::fs::set_permissions(activation.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let root = lillux::PinnedDirectory::open(activation.path())
            .unwrap()
            .unwrap();
        assert_eq!(refused(read_signed_import(&root)), Step::ImportRead);
        // These steps need the installed guest image; on any other host the
        // runtime root is absent and the refusal must still name its step.
        if !Path::new(RUNTIME_ROOT).exists() {
            assert_eq!(refused(prepare_non_root_owner()), Step::AccountTransition);
            assert_eq!(refused(run(b"assignment")), Step::RuntimeRoot);
        }
    }

    #[test]
    fn a_nonzero_supervisor_exit_reports_its_status_and_stderr_tail() {
        let (mut tail, writer) =
            lillux::stderr_tail::StderrTail::open(MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES).unwrap();
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                "echo 'ryeos-external-candidate-supervisor: launcher refused: cause' >&2; exit 126",
            ])
            .stderr(writer)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(126));
        let raw = tail.finish(STDERR_GRACE);
        let mut exit = supervisor_exit_from_parts(lillux::LinuxSandboxExit::Code(126), None, &raw);
        // The owner always attaches its account of the guest clock; the rest
        // of the line is what this test pins.
        assert!(exit.clock.take().is_some());
        let line = GuestOwnerReport::SupervisorExit(exit.clone()).line();
        assert_eq!(
            line,
            "{\"guest_owner\":\"supervisor_exit\",\"status\":{\"code\":126},\"stderr_tail\":\"ryeos-external-candidate-supervisor: launcher refused: cause\"}\n"
        );
        assert_eq!(
            GuestOwnerReport::parse_line(line.trim_end()).unwrap(),
            GuestOwnerReport::SupervisorExit(exit)
        );
    }

    #[test]
    fn a_failed_launch_diagnostic_precedes_the_stderr_tail() {
        let exit = supervisor_exit_from_parts(
            lillux::LinuxSandboxExit::Signal(9),
            Some("exec failed"),
            b"last words\n",
        );
        assert_eq!(exit.status, GuestSupervisorStatus::Signal(9));
        assert_eq!(exit.stderr_tail, "launch failure: exec failed | last words");
    }

    #[test]
    fn refusal_report_carries_only_the_step_word() {
        let error = Err::<(), _>(anyhow::anyhow!("/private/path secret"))
            .refused_at(Step::PackageIdentity)
            .unwrap_err();
        assert_eq!(
            GuestOwnerReport::Refused(Step::of(&error)).line(),
            "{\"guest_owner\":\"refused\",\"step\":\"package_identity\"}\n"
        );
        assert_eq!(
            GuestOwnerReport::Refused(Step::of(&anyhow::anyhow!("unclassified"))).line(),
            "{\"guest_owner\":\"refused\",\"step\":\"other\"}\n"
        );
    }
}
