//! Restart acceptance for the scheduler participant of the offline
//! execution-history reset, driven through the real `ryeos node reset
//! execution-history` command.
//!
//! The CLI under test is built with the scheduler participant selected and
//! the crash-qualification cut compiled in:
//!
//! ```text
//! cargo build -p ryeos-cli --features scheduling,ryeos-scheduler-service/crash-qualification-test-support
//! ```
//!
//! Each case parks the reset at one armed stage transition of one scheduler
//! role, SIGKILLs the CLI there, checks exactly where the original role bytes
//! survive, reruns the ordinary reset to settlement, and requires the daemon
//! to reach readiness on a freshly constructed scheduler projection. Those
//! cases need the crash-qualification cut and are compiled only with it.
//!
//! The Core cases need no cut: on a node whose installed composition does not
//! select the scheduler, the reset runs without the scheduler participant,
//! never creates scheduler state, and refuses stale scheduler state with the
//! sanctioned remedy instead of touching it.

#![cfg(all(unix, feature = "scheduling"))]
#![recursion_limit = "256"]

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
#[cfg(feature = "crash-qualification-test-support")]
use lillux::time::{Duration, MonotonicTimer};
#[cfg(feature = "crash-qualification-test-support")]
use serde_json::json;
#[cfg(feature = "crash-qualification-test-support")]
use tokio::io::{AsyncBufReadExt, BufReader};

use common::{DaemonHarness, ryeos_binary};

const MAIN: &str = "scheduler.sqlite3";
#[cfg(feature = "crash-qualification-test-support")]
const WAL: &str = "scheduler.sqlite3-wal";
#[cfg(feature = "crash-qualification-test-support")]
const SHM: &str = "scheduler.sqlite3-shm";

/// Inode identity of one directory entry, observed without following links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryIdentity {
    device: u64,
    inode: u64,
}

fn runtime_dir(h: &DaemonHarness) -> PathBuf {
    ryeos_engine::roots::RuntimeRoot::new(h.state_path.clone()).state()
}

fn observe(runtime: &Path, name: &str) -> Result<Option<EntryIdentity>> {
    let directory = lillux::PinnedDirectory::open(runtime)?
        .with_context(|| format!("scheduler runtime {} is absent", runtime.display()))?;
    Ok(directory
        .entry_no_follow(OsStr::new(name))?
        .map(|entry| EntryIdentity {
            device: entry.containing_device,
            inode: entry.inode,
        }))
}

#[cfg(feature = "crash-qualification-test-support")]
/// Every entry in the runtime directory naming a scheduler role at its
/// canonical (P), quarantine (Q), purge (U) or resolution (R) coordinate.
fn role_coordinates(runtime: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(runtime)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.starts_with("scheduler.sqlite3")
            || name.starts_with("scheduler-retirement.role.")
            || (name.starts_with("scheduler-retirement.resolve.") && name.ends_with(".quarantine"))
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(feature = "crash-qualification-test-support")]
/// Every role coordinate currently holding `identity`.
fn locate(runtime: &Path, identity: EntryIdentity) -> Result<Vec<String>> {
    let mut holders = Vec::new();
    for name in role_coordinates(runtime)? {
        if observe(runtime, &name)? == Some(identity) {
            holders.push(name);
        }
    }
    Ok(holders)
}

#[cfg(feature = "crash-qualification-test-support")]
fn quarantine_coordinate(name: &str, role: &str) -> bool {
    name.starts_with("scheduler-retirement.role.") && name.ends_with(&format!(".{role}.quarantine"))
}

fn reset_command(h: &DaemonHarness) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(ryeos_binary());
    command
        .args([
            "node",
            "reset",
            "execution-history",
            "--confirm",
            "--json",
            "--app-root",
        ])
        .arg(&h.state_path)
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("HOME", h.user_space.path())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

#[cfg(feature = "crash-qualification-test-support")]
/// Run the reset with one armed cut selected, wait for the CLI to announce it,
/// and SIGKILL it there.
async fn reset_until_cut(h: &DaemonHarness, cut: &str) -> Result<()> {
    let marker = format!(
        "{}scheduler_reset:{cut}",
        ryeos_app::env_contract::TEST_CRASH_CUT_REACHED_PREFIX
    );
    let mut child = reset_command(h)
        .env(ryeos_app::env_contract::SCHEDULER_RESET_TEST_CUT_ENV, cut)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn ryeos node reset execution-history")?;
    let stderr = child.stderr.take().context("reset stderr pipe")?;
    let mut lines = BufReader::new(stderr).lines();
    let mut seen = String::new();
    let timer = MonotonicTimer::start();
    let reached = loop {
        let remaining = Duration::from_secs(300).saturating_sub(timer.elapsed());
        anyhow::ensure!(
            !remaining.is_zero(),
            "reset did not reach {cut} within 300s; stderr:\n{seen}"
        );
        match tokio::time::timeout(remaining, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if line == marker {
                    break true;
                }
                seen.push_str(&line);
                seen.push('\n');
            }
            Ok(Ok(None)) => break false,
            Ok(Err(error)) => return Err(error).context("read reset stderr"),
            Err(_) => continue,
        }
    };
    if !reached {
        let status = child.wait().await?;
        anyhow::bail!(
            "reset exited {status} without reaching cut {cut}; the CLI must be built with \
             ryeos-scheduler-service/crash-qualification-test-support; stderr:\n{seen}"
        );
    }
    child.start_kill().context("SIGKILL parked reset")?;
    let status = child.wait().await?;
    anyhow::ensure!(!status.success(), "a SIGKILLed reset cannot report success");
    Ok(())
}

/// Run the ordinary reset to completion and return its JSON report.
async fn reset_to_completion(h: &DaemonHarness) -> Result<serde_json::Value> {
    let output = reset_command(h)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("spawn ryeos node reset execution-history")?;
    anyhow::ensure!(
        output.status.success(),
        "reset exited {}; stdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("decode reset JSON report")
}

/// Run the ordinary reset and require a refusal, returning its stderr.
async fn reset_refused(h: &DaemonHarness) -> Result<String> {
    let output = reset_command(h)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("spawn ryeos node reset execution-history")?;
    anyhow::ensure!(
        !output.status.success(),
        "reset must hold rather than settle; stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(String::from_utf8_lossy(&output.stderr).into_owned())
}

#[cfg(feature = "crash-qualification-test-support")]
/// After settlement no scheduler role remains at any coordinate, the daemon
/// reaches readiness, and its scheduler service answers on a fresh projection.
async fn assert_settled_and_ready(h: &mut DaemonHarness, runtime: &Path) -> Result<()> {
    // Every canonical, quarantine, purge and resolution name is gone, whether
    // or not the case took the resolution path.
    let left = role_coordinates(runtime)?;
    assert!(
        left.is_empty(),
        "settled reset left scheduler role coordinates: {left:?}"
    );
    // Once acknowledged, the reset retires its own journals and the empty
    // fires root too: a completed scheduler reset leaves no scheduler state.
    let present = scheduler_state_entries(h)?;
    assert!(
        present.is_empty(),
        "a completed scheduler reset must leave no scheduler state: {present:?}"
    );
    h.respawn_with(|_| {}).await?;
    assert!(
        observe(runtime, MAIN)?.is_some(),
        "ready daemon must construct a fresh scheduler projection"
    );
    let (status, body) = h
        .post_execute("service:scheduler/list", ".", json!({}))
        .await?;
    assert!(
        status.is_success(),
        "scheduler must answer after reset; got {status}: {body}"
    );
    Ok(())
}

#[cfg(feature = "crash-qualification-test-support")]
/// Start a Core+Scheduling node, stop it in order, and return the runtime
/// directory with the pinned identity of the scheduler main database.
async fn stopped_scheduling_node() -> Result<(DaemonHarness, PathBuf, EntryIdentity)> {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling().await?;
    h.stop_gracefully().await?;
    let runtime = runtime_dir(&h);
    let main =
        observe(&runtime, MAIN)?.context("started scheduling node has no scheduler.sqlite3")?;
    Ok((h, runtime, main))
}

#[cfg(feature = "crash-qualification-test-support")]
/// One armed cut on the main role, the coordinate the original must occupy
/// after the kill, then settlement and readiness.
async fn main_role_cut(cut: &str, survivor: Survivor) -> Result<()> {
    let (mut h, runtime, original) = stopped_scheduling_node().await?;
    reset_until_cut(&h, cut).await?;

    let holders = locate(&runtime, original)?;
    match survivor {
        Survivor::Canonical => assert_eq!(
            holders,
            vec![MAIN.to_owned()],
            "after {cut} the original must remain only at its canonical coordinate"
        ),
        Survivor::Quarantine => {
            assert_eq!(
                holders.len(),
                1,
                "after {cut} the original must survive at exactly one coordinate: {holders:?}"
            );
            assert!(
                quarantine_coordinate(&holders[0], "main"),
                "after {cut} the original must survive only at Q, found {holders:?}"
            );
            assert_eq!(observe(&runtime, MAIN)?, None, "P must be vacated at {cut}");
        }
        Survivor::Removed => {
            assert!(
                holders.is_empty(),
                "after {cut} the original must not survive at any coordinate: {holders:?}"
            );
            assert_eq!(observe(&runtime, MAIN)?, None, "P must be vacated at {cut}");
        }
    }

    reset_to_completion(&h).await?;
    assert!(
        locate(&runtime, original)?.is_empty(),
        "settlement must retire the original from every coordinate"
    );
    assert_settled_and_ready(&mut h, &runtime).await
}

#[cfg(feature = "crash-qualification-test-support")]
#[derive(Clone, Copy)]
enum Survivor {
    Canonical,
    Quarantine,
    Removed,
}

#[cfg(feature = "crash-qualification-test-support")]
#[tokio::test(flavor = "multi_thread")]
async fn crash_at_armed_p_to_q_keeps_the_original_at_p_then_settles() -> Result<()> {
    main_role_cut("quarantine_armed:main", Survivor::Canonical).await
}

#[cfg(feature = "crash-qualification-test-support")]
#[tokio::test(flavor = "multi_thread")]
async fn crash_after_p_to_q_before_ack_survives_only_at_q_then_settles() -> Result<()> {
    main_role_cut("quarantined_before_ack:main", Survivor::Quarantine).await
}

#[cfg(feature = "crash-qualification-test-support")]
#[tokio::test(flavor = "multi_thread")]
async fn crash_at_armed_q_to_u_survives_only_at_q_then_settles() -> Result<()> {
    main_role_cut("purge_armed:main", Survivor::Quarantine).await
}

#[cfg(feature = "crash-qualification-test-support")]
#[tokio::test(flavor = "multi_thread")]
async fn crash_after_unlink_before_ack_leaves_no_survivor_then_settles() -> Result<()> {
    main_role_cut("unlinked_before_ack:main", Survivor::Removed).await
}

#[cfg(feature = "crash-qualification-test-support")]
/// SIGKILL the daemon so the scheduler's WAL and shared-memory sidecars stay
/// on disk, retire main, and cut while both sidecars are still canonical:
/// they are orphans of an already-removed main when the reset is rerun.
#[tokio::test(flavor = "multi_thread")]
async fn orphan_sidecars_after_main_removal_settle() -> Result<()> {
    let (mut h, runtime, [(_, main), (_, wal), (_, shm)]) = killed_scheduling_node().await?;

    reset_until_cut(&h, "quarantine_armed:wal").await?;
    assert!(
        locate(&runtime, main)?.is_empty(),
        "main must already be removed when the WAL quarantine is armed"
    );
    assert_eq!(
        observe(&runtime, WAL)?,
        Some(wal),
        "orphan WAL must stay at P"
    );
    assert_eq!(
        observe(&runtime, SHM)?,
        Some(shm),
        "orphan SHM must stay at P"
    );

    reset_to_completion(&h).await?;
    for (role, identity) in [("main", main), ("wal", wal), ("shm", shm)] {
        assert!(
            locate(&runtime, identity)?.is_empty(),
            "settlement must retire the original {role} from every coordinate"
        );
    }
    assert_settled_and_ready(&mut h, &runtime).await
}

#[cfg(feature = "crash-qualification-test-support")]
/// A foreign file swapped into the canonical coordinate after the reset
/// pinned and armed the original is never moved, rewritten or removed: the
/// rerun refuses on the pinned identity mismatch instead of settling over it,
/// repeatably.
#[tokio::test(flavor = "multi_thread")]
async fn foreign_file_swapped_in_after_the_pin_is_kept() -> Result<()> {
    let (h, runtime, original) = stopped_scheduling_node().await?;
    reset_until_cut(&h, "quarantine_armed:main").await?;

    let foreign_bytes = b"foreign occupant, not a scheduler database".to_vec();
    let staged = runtime.join("foreign-occupant.tmp");
    std::fs::write(&staged, &foreign_bytes)?;
    std::fs::rename(&staged, runtime.join(MAIN))?;
    let foreign = observe(&runtime, MAIN)?.context("foreign occupant must be at P")?;
    assert_ne!(
        foreign, original,
        "swap must replace the pinned original inode"
    );

    for attempt in 0..2 {
        let refusal = reset_refused(&h).await?;
        assert!(
            refusal.contains("original SQLite source identity or extent changed"),
            "attempt {attempt}: the rerun must refuse on the pinned identity mismatch; reset said:\n{refusal}"
        );
        assert_eq!(
            observe(&runtime, MAIN)?,
            Some(foreign),
            "attempt {attempt}: foreign occupant must stay at P; reset said:\n{refusal}"
        );
        assert_eq!(
            std::fs::read(runtime.join(MAIN))?,
            foreign_bytes,
            "attempt {attempt}: foreign bytes must be untouched"
        );
        assert_eq!(
            locate(&runtime, foreign)?,
            vec![MAIN.to_owned()],
            "attempt {attempt}: foreign occupant must not be copied or moved"
        );
    }
    Ok(())
}

#[cfg(feature = "crash-qualification-test-support")]
/// The Q to U move and the unlink of U run inside one native purge call; the
/// cut inside it parks after the move, so the original survives only at the
/// purge coordinate U until the rerun settles it.
#[tokio::test(flavor = "multi_thread")]
async fn crash_after_q_to_u_before_unlink_survives_only_at_u_then_settles() -> Result<()> {
    let (mut h, runtime, original) = stopped_scheduling_node().await?;
    reset_until_cut(&h, "moved_to_purge_before_unlink:main").await?;

    let holders = locate(&runtime, original)?;
    assert_eq!(
        holders.len(),
        1,
        "the original must survive at exactly one coordinate: {holders:?}"
    );
    assert!(
        holders[0].starts_with("scheduler-retirement.role.") && holders[0].ends_with(".main.purge"),
        "the original must survive only at U, found {holders:?}"
    );
    assert_eq!(observe(&runtime, MAIN)?, None, "P must be vacated at U");

    reset_to_completion(&h).await?;
    assert!(
        locate(&runtime, original)?.is_empty(),
        "settlement must retire the original from every coordinate"
    );
    assert_settled_and_ready(&mut h, &runtime).await
}

#[cfg(feature = "crash-qualification-test-support")]
/// The U to R move happens only inside a native resolution after a failed
/// purge. The cut makes the ordinary purge fail its unlink at U, lets the
/// resolution move the original from U to the resolution quarantine R, and
/// parks there before its unlink: the original survives only at R until the
/// rerun settles it.
#[tokio::test(flavor = "multi_thread")]
async fn crash_after_u_to_r_survives_only_at_r_then_settles() -> Result<()> {
    let (mut h, runtime, original) = stopped_scheduling_node().await?;
    reset_until_cut(&h, "moved_to_resolution_before_unlink:main").await?;

    let holders = locate(&runtime, original)?;
    assert_eq!(
        holders.len(),
        1,
        "the original must survive at exactly one coordinate: {holders:?}"
    );
    assert!(
        holders[0].starts_with("scheduler-retirement.resolve.")
            && holders[0].ends_with(".quarantine"),
        "the original must survive only at R, found {holders:?}"
    );
    assert_eq!(observe(&runtime, MAIN)?, None, "P must be vacated at R");

    reset_to_completion(&h).await?;
    assert!(
        locate(&runtime, original)?.is_empty(),
        "settlement must retire the original from every coordinate"
    );
    assert_settled_and_ready(&mut h, &runtime).await
}

#[cfg(feature = "crash-qualification-test-support")]
/// A SIGKILLed scheduling node: main plus the WAL and shared-memory sidecars
/// the kill left on disk, each with its pinned identity.
async fn killed_scheduling_node()
-> Result<(DaemonHarness, PathBuf, [(&'static str, EntryIdentity); 3])> {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling().await?;
    let runtime = runtime_dir(&h);
    h.kill_daemon().await?;
    let main = observe(&runtime, MAIN)?.context("scheduling node has no scheduler.sqlite3")?;
    let wal = observe(&runtime, WAL)?
        .context("a killed scheduling node must retain scheduler.sqlite3-wal for this fixture")?;
    let shm = observe(&runtime, SHM)?
        .context("a killed scheduling node must retain scheduler.sqlite3-shm for this fixture")?;
    Ok((h, runtime, [("main", main), ("wal", wal), ("shm", shm)]))
}

#[cfg(feature = "crash-qualification-test-support")]
/// The unlink of the shared-memory sidecar completes and the reset dies
/// before acknowledging it: main and the WAL are already retired, and the
/// sidecar survives at no coordinate.
#[tokio::test(flavor = "multi_thread")]
async fn crash_after_unlink_before_ack_on_shm_leaves_no_survivor_then_settles() -> Result<()> {
    let (mut h, runtime, roles) = killed_scheduling_node().await?;
    reset_until_cut(&h, "unlinked_before_ack:shm").await?;

    for (role, identity) in roles {
        assert!(
            locate(&runtime, identity)?.is_empty(),
            "after unlinked_before_ack:shm the original {role} must not survive at any coordinate"
        );
    }
    assert_eq!(observe(&runtime, SHM)?, None, "P must be vacated for shm");

    reset_to_completion(&h).await?;
    assert_settled_and_ready(&mut h, &runtime).await
}

#[cfg(feature = "crash-qualification-test-support")]
/// The resolution move of the WAL sidecar from U to R parks before its
/// unlink: main is already retired and the WAL survives only at R.
#[tokio::test(flavor = "multi_thread")]
async fn crash_after_u_to_r_on_wal_survives_only_at_r_then_settles() -> Result<()> {
    let (mut h, runtime, [(_, main), (_, wal), (_, _shm)]) = killed_scheduling_node().await?;
    reset_until_cut(&h, "moved_to_resolution_before_unlink:wal").await?;

    assert!(
        locate(&runtime, main)?.is_empty(),
        "main must already be retired when the WAL reaches R"
    );
    let holders = locate(&runtime, wal)?;
    assert_eq!(
        holders.len(),
        1,
        "the original WAL must survive at exactly one coordinate: {holders:?}"
    );
    assert!(
        holders[0].starts_with("scheduler-retirement.resolve.")
            && holders[0].ends_with(".quarantine"),
        "the original WAL must survive only at R, found {holders:?}"
    );
    assert_eq!(
        observe(&runtime, WAL)?,
        None,
        "P must be vacated for the WAL"
    );

    reset_to_completion(&h).await?;
    assert!(
        locate(&runtime, wal)?.is_empty(),
        "settlement must retire the original WAL from every coordinate"
    );
    assert_settled_and_ready(&mut h, &runtime).await
}

#[cfg(feature = "crash-qualification-test-support")]
/// A reset killed once at Q and killed again while resuming at the Q to U
/// arming still holds the original only at Q, and the next rerun settles it.
#[tokio::test(flavor = "multi_thread")]
async fn second_kill_during_settlement_still_settles() -> Result<()> {
    let (mut h, runtime, original) = stopped_scheduling_node().await?;

    for cut in ["quarantined_before_ack:main", "purge_armed:main"] {
        reset_until_cut(&h, cut).await?;
        let holders = locate(&runtime, original)?;
        assert_eq!(
            holders.len(),
            1,
            "after {cut} the original must survive at exactly one coordinate: {holders:?}"
        );
        assert!(
            quarantine_coordinate(&holders[0], "main"),
            "after {cut} the original must survive only at Q, found {holders:?}"
        );
    }

    reset_to_completion(&h).await?;
    assert!(
        locate(&runtime, original)?.is_empty(),
        "settlement must retire the original from every coordinate"
    );
    assert_settled_and_ready(&mut h, &runtime).await
}

/// The node-configuration directory signed schedule sources live in.
fn node_schedules_dir(h: &DaemonHarness) -> PathBuf {
    h.state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("schedules")
}

/// Every runtime or node-configuration entry that holds scheduler state. The
/// `.ai/node/schedules` name counts whatever it is, even an empty directory:
/// a Core node never creates one, so only a case that plants it may expect
/// it, and then only alone.
fn scheduler_state_entries(h: &DaemonHarness) -> Result<Vec<PathBuf>> {
    let root = ryeos_engine::roots::RuntimeRoot::new(h.state_path.clone());
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root.state())? {
        let path = entry?.path();
        let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if name.starts_with("scheduler") || name == "schedules" {
            found.push(path);
        }
    }
    let schedules = node_schedules_dir(h);
    match std::fs::symlink_metadata(&schedules) {
        Ok(metadata) => {
            if metadata.file_type().is_dir() {
                for entry in std::fs::read_dir(&schedules)? {
                    found.push(entry?.path());
                }
            }
            found.push(schedules);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    found.sort();
    Ok(found)
}

/// Start a Core-only node and stop it in order, requiring that it holds no
/// scheduler state.
async fn stopped_core_only_node() -> Result<DaemonHarness> {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_only_prepared().await?;
    h.stop_gracefully().await?;
    let present = scheduler_state_entries(&h)?;
    anyhow::ensure!(
        present.is_empty(),
        "a Core-only node must hold no scheduler state before the reset: {present:?}"
    );
    Ok(h)
}

/// A stopped Core-only node resets through the real CLI without the scheduler
/// participant: the reset completes, creates no scheduler state, and the
/// daemon reaches readiness afterwards, still without scheduler state.
#[tokio::test(flavor = "multi_thread")]
async fn core_only_reset_completes_without_scheduler_state() -> Result<()> {
    let mut h = stopped_core_only_node().await?;

    let report = reset_to_completion(&h).await?;
    anyhow::ensure!(
        report.get("dry_run") == Some(&serde_json::Value::Bool(false)),
        "the confirmed reset must report a real run: {report}"
    );
    let present = scheduler_state_entries(&h)?;
    assert!(
        present.is_empty(),
        "the Core reset must not create scheduler state: {present:?}"
    );

    h.respawn_with(|_| {}).await?;
    let present = scheduler_state_entries(&h)?;
    assert!(
        present.is_empty(),
        "a ready Core daemon after the reset must hold no scheduler state: {present:?}"
    );
    assert_eq!(
        observe(&runtime_dir(&h), MAIN)?,
        None,
        "a ready Core daemon must not construct a scheduler database"
    );
    Ok(())
}

/// A refusal names its cause and the one shared remedy, whose steps are the
/// real CLI shapes an operator follows to retire scheduler state.
fn assert_names_remedy(refusal: &str, cause: &str) {
    assert!(
        refusal.contains(cause),
        "the refusal must name its cause {cause:?}; reset said:\n{refusal}"
    );
    assert!(
        refusal.contains(ryeos_app::execution_history_reset::SCHEDULER_STATE_REMEDY),
        "the refusal must name the shared remedy; reset said:\n{refusal}"
    );
    for step in [
        "from a scheduling-enabled build whose --source holds the scheduling bundle",
        ".ai/node/substrate-identity.json",
        "`ryeos node reset policy-generation --node-profile core-scheduling --host-class <host-class> --source <source> --confirm`",
        "A hosted container follows the same steps from a shell",
        "`ryeos scheduler deregister <schedule_id>`",
        "`ryeos node reset execution-history`",
        "the node's own scheduler-free profile",
        "`ryeos node reset policy-generation --node-profile core --host-class <host-class> --source <source> --confirm`",
        "replaces the whole node policy generation",
    ] {
        assert!(
            refusal.contains(step),
            "the refusal must name the remedy step {step:?}; reset said:\n{refusal}"
        );
    }
}

/// Stale scheduler state on a Core node, as left by an earlier build that ran
/// the scheduler regardless of composition, refuses the reset with the
/// sanctioned remedy and stays byte-for-byte and inode-for-inode untouched.
#[tokio::test(flavor = "multi_thread")]
async fn core_only_reset_refuses_stale_scheduler_state_with_remedy() -> Result<()> {
    let h = stopped_core_only_node().await?;
    let runtime = runtime_dir(&h);
    let stale_bytes = b"stale scheduler database from a scheduling build".to_vec();
    std::fs::write(runtime.join(MAIN), &stale_bytes)?;
    let stale = observe(&runtime, MAIN)?.context("planted scheduler.sqlite3 must exist")?;

    let refusal = reset_refused(&h).await?;
    assert_names_remedy(
        &refusal,
        "scheduler history requires its selected reset participant",
    );
    assert_eq!(
        observe(&runtime, MAIN)?,
        Some(stale),
        "the stale scheduler database must keep its inode"
    );
    assert_eq!(
        std::fs::read(runtime.join(MAIN))?,
        stale_bytes,
        "the stale scheduler database bytes must be untouched"
    );
    let present = scheduler_state_entries(&h)?;
    assert_eq!(
        present,
        vec![runtime.join(MAIN)],
        "the refused reset must create no other scheduler state"
    );
    Ok(())
}

/// Earlier scheduling builds created `.ai/node/schedules` even with no
/// declarations, and deregistering every schedule leaves it behind empty. An
/// empty directory holds no schedule source: the Core reset completes, leaves
/// the directory as it was, and the daemon reaches readiness afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn core_only_reset_accepts_an_empty_node_schedules_directory() -> Result<()> {
    let mut h = stopped_core_only_node().await?;
    let schedules = node_schedules_dir(&h);
    std::fs::create_dir(&schedules)?;

    let report = reset_to_completion(&h).await?;
    anyhow::ensure!(
        report.get("dry_run") == Some(&serde_json::Value::Bool(false)),
        "the confirmed reset must report a real run: {report}"
    );
    assert!(
        std::fs::symlink_metadata(&schedules)?.file_type().is_dir(),
        "the reset must leave the empty schedules directory in place"
    );
    let present = scheduler_state_entries(&h)?;
    assert_eq!(
        present,
        vec![schedules.clone()],
        "the Core reset must create no scheduler state beside the planted empty directory"
    );

    h.respawn_with(|_| {}).await?;
    let present = scheduler_state_entries(&h)?;
    assert_eq!(
        present,
        vec![schedules],
        "a ready Core daemon must hold no scheduler state beside the planted empty directory"
    );
    assert_eq!(
        observe(&runtime_dir(&h), MAIN)?,
        None,
        "a ready Core daemon must not construct a scheduler database"
    );
    Ok(())
}

/// A signed schedule source left in `.ai/node/schedules` on a Core node refuses
/// the reset with the shared remedy, and the source stays byte-for-byte and
/// inode-for-inode untouched.
#[tokio::test(flavor = "multi_thread")]
async fn core_only_reset_refuses_a_signed_schedule_source_with_remedy() -> Result<()> {
    let h = stopped_core_only_node().await?;
    let schedules = node_schedules_dir(&h);
    std::fs::create_dir(&schedules)?;
    let source = schedules.join("maintenance-gc.yaml");
    let source_bytes = b"signed schedule source from a scheduling build".to_vec();
    std::fs::write(&source, &source_bytes)?;
    let planted = observe(&schedules, "maintenance-gc.yaml")?
        .context("planted schedule source must exist")?;

    let refusal = reset_refused(&h).await?;
    assert_names_remedy(
        &refusal,
        "signed scheduler sources require their selected reset participant",
    );
    assert_eq!(
        observe(&schedules, "maintenance-gc.yaml")?,
        Some(planted),
        "the schedule source must keep its inode"
    );
    assert_eq!(
        std::fs::read(&source)?,
        source_bytes,
        "the schedule source bytes must be untouched"
    );
    let present = scheduler_state_entries(&h)?;
    assert_eq!(
        present,
        vec![schedules, source],
        "the refused reset must create no other scheduler state"
    );
    Ok(())
}

/// Run one stopped-node `ryeos` command against the harness node and return
/// its stdout, requiring success.
async fn ryeos_offline(h: &DaemonHarness, args: &[&OsStr]) -> Result<String> {
    let output = tokio::process::Command::new(ryeos_binary())
        .args(args)
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("HOME", h.user_space.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn ryeos {args:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "ryeos {args:?} exited {}; stdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The schedule the remedy case registers: a one-shot fire a few seconds
/// out, so the node holds real fire history from the real scheduler.
const REMEDY_SCHEDULE_ID: &str = "remedy-fired";

/// Switch the stopped harness node to the publisher-signed `profile` from the
/// workspace's prepared source root, exactly as the remedy's
/// `ryeos node reset policy-generation` step does.
async fn switch_node_profile(h: &DaemonHarness, profile: &str) -> Result<String> {
    // The profile source is the prepared bundles with the profile narrowed to
    // what a directly spawned daemon can prove, signed by the fixture publisher.
    let authored = tempfile::tempdir().context("create the profile source root")?;
    common::fast_fixture::author_direct_host_profile_source(
        &common::workspace_root().join("bundles"),
        profile,
        authored.path(),
    )?;
    let source = authored.path().to_path_buf();
    // The remedy uses the host class the node recorded at initialization.
    let substrate: serde_json::Value = serde_json::from_slice(&std::fs::read(
        h.state_path.join(".ai/node/substrate-identity.json"),
    )?)?;
    let host_class = substrate["host_class"]
        .as_str()
        .context("the node's substrate identity records no host class")?
        .to_owned();
    let output = ryeos_offline(
        h,
        &[
            OsStr::new("node"),
            OsStr::new("reset"),
            OsStr::new("policy-generation"),
            OsStr::new("--node-profile"),
            OsStr::new(profile),
            OsStr::new("--host-class"),
            OsStr::new(&host_class),
            OsStr::new("--source"),
            source.as_os_str(),
            OsStr::new("--app-root"),
            h.state_path.as_os_str(),
            OsStr::new("--confirm"),
        ],
    )
    .await?;
    Ok(output)
}

/// The shared remedy converges, step for step as `SCHEDULER_STATE_REMEDY`
/// names it. A Core node switches to the `core-scheduling` profile (which
/// installs the scheduling bundle and grants its command registration),
/// then holds a scheduler database, fire history from a schedule that fired,
/// and a signed schedule source. Deregistering every schedule, stopping,
/// resetting from the scheduling-enabled build and switching back to the
/// `core` profile (which removes the scheduling bundle) leaves a Core node
/// that reaches readiness with no scheduler state. The only survivor is the
/// empty `.ai/node/schedules` directory deregistration leaves, which holds
/// no schedule source.
#[tokio::test(flavor = "multi_thread")]
async fn scheduling_node_follows_the_remedy_to_a_ready_core_node() -> Result<()> {
    common::require_bundles_prepared(&["core", "scheduling"])?;
    let mut h = stopped_core_only_node().await?;
    switch_node_profile(&h, "core-scheduling").await?;
    anyhow::ensure!(
        h.state_path
            .join(".ai/node/bundles/scheduling.yaml")
            .is_file(),
        "the core-scheduling profile must register the scheduling bundle"
    );

    // The installed composition now selects the scheduler: a boot creates
    // its database and fires root, a registration signs a source, and the
    // schedule's fire leaves a fire journal.
    h.respawn_with(|_| {}).await?;
    let runtime = runtime_dir(&h);
    anyhow::ensure!(
        observe(&runtime, MAIN)?.is_some(),
        "a scheduling-selected boot must construct scheduler.sqlite3"
    );
    let fire_at = (chrono::Utc::now() + chrono::Duration::seconds(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (status, body) = h
        .post_execute(
            "service:scheduler/register",
            ".",
            serde_json::json!({
                "schedule_id": REMEDY_SCHEDULE_ID,
                "item_ref": "directive:test/hello",
                "ref_bindings": {"model": "directive:test/hello"},
                "schedule_type": "at",
                "expression": fire_at,
                "params": {},
                "timezone": "UTC",
                "overlap_policy": "skip",
                "misfire_policy": "skip",
                "lateness_grace_secs": 60,
                "enabled": true,
                "capabilities": ["ryeos.execute.directive.test/*"],
                "execution_policy": {
                    "schema_version": 2,
                    "ownership": "daemon_owned",
                    "recovery": "restart_recoverable",
                    "response": "accepted",
                    "target": {"kind": "here"},
                    "environment": {"kind": "none"},
                    "project": {"kind": "projectless"}
                },
            }),
        )
        .await?;
    anyhow::ensure!(
        status.is_success(),
        "scheduler/register refused with {status}: {body}"
    );
    let source = node_schedules_dir(&h).join(format!("{REMEDY_SCHEDULE_ID}.yaml"));
    anyhow::ensure!(
        std::fs::symlink_metadata(&source)?.file_type().is_file(),
        "registration must sign a schedule source at {}",
        source.display()
    );
    // Wait for the real scheduler to record the fire. Whatever the fired
    // item's outcome, the fire's snapshot reaches its journal.
    let fire_journal = runtime
        .join("schedules")
        .join(REMEDY_SCHEDULE_ID)
        .join("fires.jsonl");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while !fire_journal.is_file() {
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "the schedule did not fire within 120s: no {}",
            fire_journal.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    // Remedy: deregister every signed schedule while the daemon runs.
    let (status, body) = h
        .post_execute("service:scheduler/list", ".", serde_json::json!({}))
        .await?;
    anyhow::ensure!(
        status.is_success(),
        "scheduler/list refused with {status}: {body}"
    );
    let listed = body.get("result").unwrap_or(&body)["schedules"]
        .as_array()
        .with_context(|| format!("scheduler/list returned no schedules array: {body}"))?
        .iter()
        .map(|schedule| {
            schedule["schedule_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        listed.iter().any(|id| id == REMEDY_SCHEDULE_ID),
        "scheduler/list must list the registered schedule: {listed:?}"
    );
    for schedule_id in &listed {
        let (status, body) = h
            .post_execute(
                "service:scheduler/deregister",
                ".",
                serde_json::json!({ "schedule_id": schedule_id }),
            )
            .await?;
        anyhow::ensure!(
            status.is_success(),
            "scheduler/deregister {schedule_id} refused with {status}: {body}"
        );
    }

    // Remedy: stop, then reset from the scheduling-enabled build. Before the
    // State acknowledgement the reset retires the database and the fire
    // history; once acknowledged, its own journals and the empty fires root.
    h.stop_gracefully().await?;
    let report = reset_to_completion(&h).await?;
    anyhow::ensure!(
        report.get("dry_run") == Some(&serde_json::Value::Bool(false)),
        "the confirmed reset must report a real run: {report}"
    );
    let schedules = node_schedules_dir(&h);
    let present = scheduler_state_entries(&h)?;
    assert_eq!(
        present,
        vec![schedules.clone()],
        "a completed scheduler reset must leave no scheduler state beside the empty \
         schedules directory deregistration leaves"
    );

    // Remedy: switch back to the Core profile, which removes the scheduling
    // bundle; the node is Core again.
    switch_node_profile(&h, "core").await?;
    for removed in [".ai/node/bundles/scheduling.yaml", ".ai/bundles/scheduling"] {
        anyhow::ensure!(
            std::fs::symlink_metadata(h.state_path.join(removed)).is_err(),
            "the core profile must remove the scheduling bundle's {removed}"
        );
    }
    h.respawn_with(|_| {}).await?;
    let present = scheduler_state_entries(&h)?;
    assert_eq!(
        present,
        vec![schedules],
        "a ready Core daemon after the remedy must hold no scheduler state"
    );
    assert_eq!(
        observe(&runtime, MAIN)?,
        None,
        "a ready Core daemon must not construct a scheduler database"
    );
    Ok(())
}
