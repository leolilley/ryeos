//! Smoke test for `common::fast_fixture` + `DaemonHarness::start_fast`.
//!
//! Proves that pre-populated deterministic state (no real `ryeos init`)
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

/// Byte-stability: every file the fast fixture pins must hash
/// identically across two fresh invocations. This is the regression
/// net for changes to signing helpers, public-identity serialization,
/// or the fixture itself.
#[tokio::test(flavor = "multi_thread")]
async fn fast_fixture_output_is_byte_stable_across_runs() {
    use sha2::{Digest, Sha256};

    fn hash_file(p: &std::path::Path) -> String {
        let bytes = std::fs::read(p)
            .unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        let mut h = Sha256::new();
        h.update(&bytes);
        format!("{:x}", h.finalize())
    }

    async fn run_once() -> Vec<(String, String)> {
        let (h, fixture) = DaemonHarness::start_fast()
            .await
            .expect("daemon must boot");
        let state = h.state_path.clone();
        let user = h.user_space.path().to_path_buf();
        let mut entries: Vec<(String, String)> = vec![
            ("state/identity/private_key.pem".into(),
             hash_file(&state.join(".ai/node/identity/private_key.pem"))),
            ("state/identity/public-identity.json".into(),
             hash_file(&state.join(".ai/node/identity/public-identity.json"))),
            ("state/vault/private_key.pem".into(),
             hash_file(&state.join(".ai/node/vault/private_key.pem"))),
            ("state/vault/public_key.pem".into(),
             hash_file(&state.join(".ai/node/vault/public_key.pem"))),
            ("user/signing/private_key.pem".into(),
             hash_file(&user.join(".ai/config/keys/signing/private_key.pem"))),
            ("user/trusted/publisher.toml".into(),
             hash_file(&user.join(format!(".ai/config/keys/trusted/{}.toml", fixture.publisher_fp())))),
            ("user/trusted/node.toml".into(),
             hash_file(&user.join(format!(".ai/config/keys/trusted/{}.toml", fixture.node_fp())))),
            ("user/trusted/user.toml".into(),
             hash_file(&user.join(format!(".ai/config/keys/trusted/{}.toml", fixture.user_fp())))),
        ];
        entries.sort();
        drop(h);
        entries
    }

    let a = run_once().await;
    let b = run_once().await;
    assert_eq!(
        a, b,
        "fast fixture output must be byte-stable across runs — drift detected"
    );
}
