//! Scratch demonstration for the X-38 acceptance: a daemon e2e that fails on
//! purpose after the daemon has started, so the shard's log and its uploaded
//! stderr artifact can be read. Never merged.

use ryeosd::test_support as common;

use common::DaemonHarness;

#[tokio::test]
async fn x38_deliberate_failure_after_the_daemon_started() {
    let harness = DaemonHarness::start().await.expect("daemon starts");
    panic!(
        "X-38 deliberate failure\n--- daemon stderr tail ---\n{}",
        harness.stderr_tail(20)
    );
}
