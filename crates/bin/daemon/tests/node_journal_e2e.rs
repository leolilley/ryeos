//! The node journal over a real daemon's lifetime: each start and each handled
//! stop leaves a node-signed record, and the chain verifies across a restart.

use ryeosd::test_support as common;

use common::DaemonHarness;
use ryeos_node::node_journal::{JournalType, NodeJournal};
use ryeos_node_runtime::identity::NodeIdentity;

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_journals_its_starts_and_handled_exits() {
    let (mut harness, _fixture) = DaemonHarness::start_fast()
        .await
        .expect("start fast daemon");
    let app_root = harness.state_path.clone();
    harness.stop_gracefully().await.expect("orderly stop");
    harness
        .respawn_with(|_| {})
        .await
        .expect("the node starts again");
    harness.stop_gracefully().await.expect("final stop");

    let node = NodeIdentity::load(&app_root.join(".ai/node/identity/private_key.pem"))
        .expect("load the node identity");
    let read = NodeJournal::for_app_root(&app_root, node)
        .read()
        .expect("the journal verifies against the node key");
    let types: Vec<JournalType> = read
        .records
        .iter()
        .map(|record| record.body.record_type)
        .collect();
    assert_eq!(
        types,
        vec![
            JournalType::NodeStarted,
            JournalType::NodeExited,
            JournalType::NodeStarted,
            JournalType::NodeExited,
        ],
        "records: {:?}",
        read.records
    );
    assert_eq!(read.truncated_before, None);
    for record in &read.records {
        assert!(record.body.detail.contains_key("pid"));
        assert!(record.body.recorded_at.ends_with('Z'));
    }
}
