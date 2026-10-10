//! Smoke test for `common::fast_fixture` + `DaemonHarness::start_fast`.
//!
//! Proves that pre-populated deterministic state (no real `ryeos init`)
//! is sufficient for the daemon to boot, accept HTTP traffic, and
//! return its own self-introspection via `/health` + `service:node/status`.

mod common;

use common::DaemonHarness;
use common::fast_fixture::register_config_fixture_bundle;

fn prepared_core_fixture() -> (tempfile::TempDir, std::path::PathBuf, tempfile::TempDir) {
    let (node_dir, app_root) = common::copy_core_to_temp();
    let user_dir = tempfile::tempdir().expect("temporary fixture user");
    let fixture = common::fast_fixture::populate_initialized_state_with_profile(
        &app_root,
        user_dir.path(),
        common::fast_fixture::FastFixtureProfile::Core,
    ).expect("prepare isolated Core fixture");
    common::fast_fixture::register_core_bundle_at_state(&app_root, &fixture)
        .expect("register prepared Core");
    (node_dir, app_root, user_dir)
}

/// Exercises real signed issuance/capture without starting a daemon, reserving
/// native capacity or opening SQLite. Prepared bundle inputs remain required.
#[test]
fn fixture_capacity_issuance_is_real_and_reseal_preserves_its_account() {
    let (_node_dir, app_root, _user_dir) = prepared_core_fixture();
    common::fast_fixture::seal_initialized_state(&app_root).expect("first fixture seal");
    let first = ryeos_node::init::verify_init_completion(&app_root)
        .expect("verify first completion").expect("completion exists");
    let paths = [
        app_root.join(".ai/node/native-capacity/account"),
        app_root.join(".ai/node/native-capacity/anchor"),
        app_root.join(".ai/config/onboarding/native-capacity-issuance.json"),
    ];
    let before = paths.each_ref().map(|path| std::fs::read(path).expect("read issued capacity"));
    common::fast_fixture::seal_initialized_state(&app_root).expect("reseal existing fixture");
    let after = ryeos_node::init::verify_init_completion(&app_root)
        .expect("verify reseal").expect("completion exists");
    assert_eq!(first.capacity_account, after.capacity_account);
    assert_eq!(before, paths.each_ref().map(|path| std::fs::read(path).expect("read preserved capacity")));

    let lock = ryeos_node_runtime::state_lock::StateLock::acquire(
        &ryeos_node_runtime::state_lock::default_lock_path(&app_root),
    ).expect("hold actual fixture exclusion");
    let inputs = ryeos_node::native_capacity::LiveNativeCapacityInputs::capture(
        &app_root, std::sync::Arc::new(lock.retain()),
    ).expect("capture actual authenticated inputs from fixture issuance");
    inputs.require_original_cleanup().expect("original fixture input custody");
    drop(inputs);
    drop(lock);
}

#[test]
fn fixture_reseal_refuses_a_replaced_account_without_rebinding_it() {
    let (_node_dir, app_root, _user_dir) = prepared_core_fixture();
    common::fast_fixture::seal_initialized_state(&app_root).expect("first fixture seal");
    let capacity = app_root.join(".ai/node/native-capacity");
    let account = capacity.join("account");
    let original = std::fs::read(&account).expect("original account");
    // Byte-identical content in a different inode must not become the original
    // signed account. These are disposable fixture files only.
    let replacement = capacity.join("replacement");
    std::fs::copy(&account, &replacement).expect("stage foreign account inode");
    std::fs::rename(&replacement, &account).expect("replace fixture account");
    let issuance = app_root.join(".ai/config/onboarding/native-capacity-issuance.json");
    let issued = std::fs::read(&issuance).expect("original issuance");
    assert!(ryeos_node::init::verify_init_completion(&app_root).is_err());
    assert!(common::fast_fixture::seal_initialized_state(&app_root).is_err());
    assert_eq!(std::fs::read(&account).unwrap(), original);
    assert_eq!(std::fs::read(&issuance).unwrap(), issued);
}

#[test]
fn fixture_reseal_does_not_recreate_a_lost_issuance() {
    let (_node_dir, app_root, _user_dir) = prepared_core_fixture();
    common::fast_fixture::seal_initialized_state(&app_root).expect("first fixture seal");
    let issuance = app_root.join(".ai/config/onboarding/native-capacity-issuance.json");
    let account = app_root.join(".ai/node/native-capacity/account");
    let before = std::fs::read(&account).expect("original account");
    std::fs::remove_file(&issuance).expect("inject lost fixture issuance");
    assert!(common::fast_fixture::seal_initialized_state(&app_root).is_err());
    assert!(!issuance.exists());
    assert_eq!(std::fs::read(&account).unwrap(), before);
}

#[test]
fn fixture_first_seal_refuses_an_orphan_capacity_namespace() {
    let (_node_dir, app_root, _user_dir) = prepared_core_fixture();
    let capacity = app_root.join(".ai/node/native-capacity");
    std::fs::create_dir(&capacity).expect("inject orphan fixture namespace");
    let orphan = capacity.join("account");
    std::fs::write(&orphan, b"unissued preserved obligation").expect("orphan evidence");
    assert!(common::fast_fixture::seal_initialized_state(&app_root).is_err());
    assert_eq!(std::fs::read(&orphan).unwrap(), b"unissued preserved obligation");
    assert!(!app_root.join(".ai/config/onboarding/native-capacity-issuance.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn fast_fixture_boots_daemon_without_real_init() {
    let (h, fixture) = DaemonHarness::start_fast()
        .await
        .expect("daemon must boot from fast fixture without ");

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
    // wired correctly. Operator config lives under the app root
    // (`<app_root>/.ai/config/keys`), see `config.rs` /
    // `node::init`, so it is rooted at `state_path`.
    let trust_path = h
        .state_path
        .join(".ai/config/keys/trusted")
        .join(format!("{}.toml", fixture.publisher_fp()));
    assert!(
        trust_path.exists(),
        "publisher trust doc missing at {}",
        trust_path.display()
    );

    // State files written by the fast fixture must be in place.
    assert!(
        h.state_path
            .join(".ai/node/identity/private_key.pem")
            .exists()
    );
    assert!(h.state_path.join(".ai/node/vault/private_key.pem").exists());
    assert!(h.state_path.join(".ai/node/vault/public_key.pem").exists());
    assert!(h.state_path.join(".ai/node/config.yaml").exists());
    assert!(
        h.state_path
            .join(".ai/config/keys/signing/private_key.pem")
            .exists()
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn legacy_bundle_execution_config_cannot_replace_node_policy_generation() {
    let (h, _) = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| {
            register_config_fixture_bundle(
                state_path,
                "unsigned-execution-limit",
                fixture,
                |bundle_root| {
                    let directory = bundle_root.join(".ai/config/execution");
                    std::fs::create_dir_all(&directory)?;
                    std::fs::write(
                        directory.join("execution.yaml"),
                        "node:\n  max_live_fanout: 2\n  max_private_materialization_copy_bytes: 1024\n",
                    )?;
                    Ok(())
                },
            )
        },
        |_| {},
    )
    .await
    .expect("legacy bundle execution config must not replace node-wide policy");

    let trust = ryeos_engine::trust::TrustStore::load(None, &h.state_path.join(".ai/config"))
        .expect("load exact fixture trust");
    let generation = ryeos_node_runtime::node_policy::generation::load_policy_generation(
        &h.state_path,
        &trust,
        &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
    )
    .expect("load signed node policy generation");
    assert_eq!(
        generation.policies()["execution"]["max_live_fanout"],
        8,
        "unsigned legacy bundle config cannot override the signed node-wide limit"
    );
}

/// Byte-stability: every file the fast fixture pins must hash
/// identically across two fresh invocations. This is the regression
/// net for changes to signing helpers, public-identity serialization,
/// or the fixture itself.
#[tokio::test(flavor = "multi_thread")]
async fn fast_fixture_output_is_byte_stable_across_runs() {
    use sha2::{Digest, Sha256};

    fn hash_file(p: &std::path::Path) -> String {
        let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        let mut h = Sha256::new();
        h.update(&bytes);
        format!("{:x}", h.finalize())
    }

    async fn run_once() -> Vec<(String, String)> {
        let (h, fixture) = DaemonHarness::start_fast().await.expect("daemon must boot");
        let state = h.state_path.clone();
        // Operator config (signing key + trust docs) is rooted at the
        // app root alongside node state — `<app_root>/.ai/config/keys`.
        let mut entries: Vec<(String, String)> = vec![
            (
                "state/identity/private_key.pem".into(),
                hash_file(&state.join(".ai/node/identity/private_key.pem")),
            ),
            (
                "state/identity/public-identity.json".into(),
                hash_file(&state.join(".ai/node/identity/public-identity.json")),
            ),
            (
                "state/vault/private_key.pem".into(),
                hash_file(&state.join(".ai/node/vault/private_key.pem")),
            ),
            (
                "state/vault/public_key.pem".into(),
                hash_file(&state.join(".ai/node/vault/public_key.pem")),
            ),
            (
                "config/signing/private_key.pem".into(),
                hash_file(&state.join(".ai/config/keys/signing/private_key.pem")),
            ),
            (
                "config/trusted/publisher.toml".into(),
                hash_file(&state.join(format!(
                    ".ai/config/keys/trusted/{}.toml",
                    fixture.publisher_fp()
                ))),
            ),
            (
                "config/trusted/node.toml".into(),
                hash_file(&state.join(format!(
                    ".ai/config/keys/trusted/{}.toml",
                    fixture.node_fp()
                ))),
            ),
            (
                "config/trusted/user.toml".into(),
                hash_file(&state.join(format!(
                    ".ai/config/keys/trusted/{}.toml",
                    fixture.user_fp()
                ))),
            ),
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
