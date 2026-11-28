//! End to end: the node's signed bundle registrations are validated by one
//! loader at start-up, and a registration set that does not pass it stops the
//! node with a stable code.
//!
//! Each test starts a real node, stops it, changes `.ai/node/bundles` the way
//! a hostile or careless operator could, reseals the initialised state so the
//! change is not caught by the completion record instead, and starts it again.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, seal_initialized_state};

/// A registration the node key did not sign: its body is valid, its signer is
/// a pinned publisher key.
fn plant_publisher_signed_registration(state: &Path, fixture: &FastFixture) {
    let bundle = state.join(".ai/bundles/extra");
    std::fs::create_dir_all(&bundle).expect("create the bundle directory");
    let body = format!("kind: node\npath: {}\n", bundle.display());
    let signed = lillux::signature::sign_content(&body, &fixture.publisher, "#", None);
    std::fs::write(state.join(".ai/node/bundles/extra.yaml"), signed)
        .expect("write the registration");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registration_signed_by_a_publisher_stops_the_node() {
    let (mut h, fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {})
        .await
        .expect("the node starts with its own registrations");
    h.kill_daemon().await.expect("stop the node");

    plant_publisher_signed_registration(&h.state_path, &fixture);
    seal_initialized_state(&h.state_path).expect("reseal the initialised state");

    let error = h
        .respawn_with(|_| {})
        .await
        .expect_err("a registration the node key did not sign must stop the node");
    let text = format!("{error:#}");
    assert!(
        text.contains("bundle_registration_not_node_signed"),
        "expected the typed refusal, got: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_no_registration_directory_does_not_start() {
    let (mut h, _fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {})
        .await
        .expect("the node starts with its own registrations");
    h.kill_daemon().await.expect("stop the node");

    std::fs::remove_dir_all(h.state_path.join(".ai/node/bundles"))
        .expect("remove the registration directory");
    // Resealing may itself refuse without registrations; the start-up check
    // under test is the loader's, so a stale completion record is acceptable
    // only if the loader's refusal is what the node reports.
    let _ = seal_initialized_state(&h.state_path);

    let error = h
        .respawn_with(|_| {})
        .await
        .expect_err("a node with no registration directory must not start");
    let text = format!("{error:#}");
    assert!(
        text.contains("bundle_registrations_missing"),
        "expected the typed refusal, got: {text}"
    );
}
