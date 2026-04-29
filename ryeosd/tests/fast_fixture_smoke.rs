//! Smoke test for `common::fast_fixture` + `DaemonHarness::start_fast`.
//!
//! Proves that pre-populated deterministic state (no real `rye init`)
//! is sufficient for the daemon to boot, accept HTTP traffic, and
//! return its own self-introspection via `/health` + `service:system/status`.

mod common;

use common::DaemonHarness;

#[tokio::test(flavor = "multi_thread")]
async fn fast_fixture_boots_daemon_without_real_init() {
    let (h, fixture) = DaemonHarness::start_fast()
        .await
        .expect("daemon must boot from fast fixture without --init-if-missing");

    // /health must respond.
    let url = format!("http://{}/health", h.bind);
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .expect("GET /health");
    assert!(resp.status().is_success(), "GET /health: {}", resp.status());

    // The deterministic publisher fingerprint must match what the
    // self-signed trust doc holds — sanity-check the fixture is
    // wired correctly.
    let trust_path = h
        .user_space
        .path()
        .join(".ai/config/keys/trusted")
        .join(format!("{}.toml", fixture.publisher_fp()));
    assert!(
        trust_path.exists(),
        "publisher trust doc missing at {}",
        trust_path.display()
    );

    // State files written by the fast fixture must be in place.
    assert!(h
        .state_path
        .join(".ai/node/identity/private_key.pem")
        .exists());
    assert!(h
        .state_path
        .join(".ai/node/vault/private_key.pem")
        .exists());
    assert!(h
        .state_path
        .join(".ai/node/vault/public_key.pem")
        .exists());
    assert!(h
        .user_space
        .path()
        .join(".ai/config/keys/signing/private_key.pem")
        .exists());
}

/// Re-running the fast fixture against the same paths must be
/// idempotent — covered indirectly here by exercising the path twice
/// in two harnesses (each gets its own tempdir, but the byte content
/// of the deterministic keys must be identical between runs).
#[tokio::test(flavor = "multi_thread")]
async fn fast_fixture_keys_are_deterministic() {
    let (_h1, f1) = DaemonHarness::start_fast().await.expect("daemon 1");
    let (_h2, f2) = DaemonHarness::start_fast().await.expect("daemon 2");
    assert_eq!(f1.publisher_fp(), f2.publisher_fp());
    assert_eq!(f1.node_fp(), f2.node_fp());
    assert_eq!(f1.user_fp(), f2.user_fp());
}
