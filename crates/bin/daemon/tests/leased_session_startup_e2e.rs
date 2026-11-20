//! A real `ryeosd` started over what a daemon that died mid-lease leaves
//! behind: a dedicated session whose external allocation contacted its
//! provider and bound an occurrence, with no observed provider terminal.
//!
//! The node is a fast fixture daemon. While it is stopped the test seeds the
//! predecessor's rows through the node Store (a running placement thread whose
//! launch claim belongs to another daemon generation, the session holding its
//! credential profile lock, and the quarantine-bound allocation). The retained
//! binding names a backend no adapter provides, so the provider can never
//! answer: this is the "provider has not resolved" half of startup. The
//! provider-terminated half needs a launched leased session with a live
//! synthetic adapter, which this rig does not provide.
//!
//! Cases:
//!
//! 1. Two starts. Start 1 reaches ready with no operator action, drives the
//!    allocation toward its provider terminal, leaves it unsettled, and logs the
//!    predecessor-lease counts line. The daemon dies hard again. Start 2 over
//!    the same rows also reaches ready: the wedge that took two starts to show.
//! 2. The runtime database still refuses a local settlement of the session while
//!    its allocation is unsettled, after the restarts, and the refusal changes
//!    nothing.

#![cfg(unix)]

use ryeosd::test_support as common;

use anyhow::{Context, Result};
use common::DaemonHarness;
use ryeos_app::runtime_db::external_execution::ExternalAllocationPhase;

const PLACEMENT: &str = "T-leased-startup-e2e";
const WORKSPACE: &str = "W-leased-startup-e2e";
const WORKER: &str = "worker-leased-startup-e2e";
const COUNTS_LINE: &str = "predecessor external leases and session allocations driven toward";

fn strip_ansi_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A fast fixture node that has completed one start, killed hard, with the
/// predecessor's session and unsettled allocation left in its Store.
async fn node_killed_mid_lease() -> Result<DaemonHarness> {
    let (mut harness, _fixture) = DaemonHarness::start_fast_core_only_prepared()
        .await
        .context("start fast daemon")?;
    harness
        .kill_daemon()
        .await
        .context("kill the daemon hard")?;
    {
        let store =
            common::open_stopped_node_store(&harness.state_path).context("open stopped store")?;
        store
            .install_unsettled_dedicated_session_allocation(PLACEMENT, WORKSPACE, WORKER)
            .context("seed the predecessor's session and allocation")?;
    }
    Ok(harness)
}

/// The log of the current start with terminal colouring removed, or a failure
/// naming the whole log.
fn require_counted_start(harness: &DaemonHarness, label: &str) -> Result<String> {
    let log = strip_ansi_escapes(&harness.stderr_so_far());
    anyhow::ensure!(
        !log.contains("daemon startup failed") && !log.contains("restart inhibited"),
        "{label} failed to start:\n{log}"
    );
    let counts = log
        .lines()
        .find(|line| line.contains(COUNTS_LINE))
        .with_context(|| format!("{label} logged no predecessor-lease counts line:\n{log}"))?;
    anyhow::ensure!(
        counts.contains("discovered=1") && counts.contains("terminated=0"),
        "{label} counted the lease wrongly: {counts}\n{log}"
    );
    Ok(log)
}

/// What the stopped node's Store holds for the seeded session.
struct Held {
    phase: ExternalAllocationPhase,
    session_state: String,
    lock_owner: Option<String>,
}

fn held_after_stop(harness: &DaemonHarness) -> Result<Held> {
    let store =
        common::open_stopped_node_store(&harness.state_path).context("open stopped store")?;
    let allocation = store
        .external_allocation(PLACEMENT)?
        .context("the allocation was lost")?;
    let session = store
        .dedicated_session(PLACEMENT)?
        .context("the session was lost")?;
    let profile = store
        .credential_profile(&format!("P-{PLACEMENT}"))?
        .context("the credential profile was lost")?;
    Ok(Held {
        phase: allocation.phase,
        session_state: session.state,
        lock_owner: profile.lock_owner,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_killed_mid_lease_restarts_twice_without_operator_action() -> Result<()> {
    let mut harness = node_killed_mid_lease().await?;

    // Start 1: ready without operator action.
    harness
        .respawn_with(|_| {})
        .await
        .context("start 1 over the unsettled allocation")?;
    let first = require_counted_start(&harness, "start 1")?;
    assert!(
        first.contains("cleared launch claim left by a dead daemon generation"),
        "start 1 did not clear the dead generation's launch claim:\n{first}"
    );

    // The daemon dies hard again, as the derived-image node did.
    harness.kill_daemon().await.context("kill start 1 hard")?;

    // Start 2: also ready, over the same unsettled allocation.
    harness
        .respawn_with(|_| {})
        .await
        .context("start 2 over the unsettled allocation")?;
    require_counted_start(&harness, "start 2")?;

    harness.stop_gracefully().await.context("stop start 2")?;
    let held = held_after_stop(&harness)?;
    assert_eq!(
        held.phase,
        ExternalAllocationPhase::Quarantined,
        "the allocation must stay quarantined without an observed provider terminal"
    );
    assert_ne!(
        held.session_state, "terminal",
        "the session settled while its allocation was unsettled"
    );
    assert_eq!(
        held.lock_owner.as_deref(),
        Some(WORKER),
        "the session's credential lock was released while its allocation was unsettled"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn local_settlement_of_a_session_with_an_unsettled_allocation_stays_refused() -> Result<()> {
    let mut harness = node_killed_mid_lease().await?;
    harness
        .respawn_with(|_| {})
        .await
        .context("start over the unsettled allocation")?;
    harness.stop_gracefully().await.context("stop the node")?;

    let before = held_after_stop(&harness)?;
    {
        let store = common::open_stopped_node_store(&harness.state_path)?;
        let refusal = store
            .fail_dedicated_session_start(PLACEMENT, WORKER, 1, "ended host lifetime", true)
            .expect_err("local settlement must be refused while the allocation is unsettled");
        eprintln!("local settlement refusal: {refusal:#}");
    }
    let after = held_after_stop(&harness)?;
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.session_state, before.session_state);
    assert_ne!(after.session_state, "terminal");
    assert_eq!(after.lock_owner.as_deref(), Some(WORKER));
    Ok(())
}
