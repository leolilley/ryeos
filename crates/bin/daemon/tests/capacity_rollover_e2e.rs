//! Native-capacity epochs end to end.
//!
//! The fast tests seal a real issuance (schema 6) on a disposable root and
//! drive operator re-issuance through the public `ryeos-node` surface: the
//! next epoch opens exactly once, reruns converge, a held state lock (what a
//! running daemon holds) refuses the command, and a tampered document is
//! refused without repair.
//!
//! The daemon-level rollover loop needs real served windows. A window cannot
//! be shorter than the drain margin plus the cleanup occupancy the reservation
//! requires, so the fixture uses 150 s windows and runs 21 rollovers in about
//! an hour. It is ignored unless `RYEOS_LONG_E2E=1`.

mod common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{self, FastFixtureProfile};
use lillux::crypto::SigningKey;
use ryeos_node::native_capacity::{
    ReissueRequest, inspect_capacity_account, read_capacity_reissuances, reissue_capacity,
};
use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};

const COMMAND: &str = "capacity_rollover_e2e";

/// A sealed Core root: real issuance, no daemon, no SQLite.
fn sealed_core_root() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    tempfile::TempDir,
    fast_fixture::FastFixture,
) {
    let (node_dir, app_root) = common::copy_core_to_temp();
    let user_dir = tempfile::tempdir().expect("temporary fixture user");
    let fixture = fast_fixture::populate_initialized_state_with_profile(
        &app_root,
        user_dir.path(),
        FastFixtureProfile::Core,
    )
    .expect("prepare isolated Core fixture");
    fast_fixture::register_core_bundle_at_state(&app_root, &fixture)
        .expect("register prepared Core");
    fast_fixture::seal_initialized_state(&app_root).expect("seal fixture issuance");
    (node_dir, app_root, user_dir, fixture)
}

/// The operator key as `ryeos node capacity reissue` reads it from the root.
fn operator_key(app_root: &Path) -> SigningKey {
    let pem = String::from_utf8(
        lillux::read_regular_file_bounded_no_follow(
            &app_root.join(".ai/config/keys/signing/private_key.pem"),
            32 * 1024,
        )
        .expect("read operator key"),
    )
    .expect("operator key is utf-8");
    use lillux::crypto::DecodePrivateKey as _;
    lillux::crypto::SigningKey::from_pkcs8_pem(&pem).expect("decode operator key")
}

fn reissue(
    app_root: &Path,
    lock: &StateLock,
    operator: &SigningKey,
    reason: &str,
) -> anyhow::Result<ryeos_node::native_capacity::ReissueReport> {
    reissue_capacity(ReissueRequest {
        app_root,
        state_lock: lock,
        operator_key: operator,
        reason,
        command: COMMAND,
    })
}

#[test]
fn operator_reissue_opens_one_epoch_reruns_idempotently_and_refuses_tampering() {
    let (_node_dir, app_root, _user_dir, fixture) = sealed_core_root();
    let fresh = inspect_capacity_account(&app_root)
        .expect("inspect fresh account")
        .expect("issued");
    assert_eq!(fresh.epoch, 1);
    assert_eq!(fresh.incarnations_remaining, 16);
    assert_eq!(fresh.recovery_attempts_used, 0);
    assert!(
        fresh.renewal.operator_reissue,
        "Core pre-authorises operator re-issuance"
    );

    let operator = operator_key(&app_root);
    assert_eq!(
        operator.to_bytes(),
        fixture.user.to_bytes(),
        "the fixture operator is the user key"
    );
    let lock = StateLock::acquire(&default_lock_path(&app_root)).expect("stopped node lock");

    let first = reissue(&app_root, &lock, &operator, "e2e first epoch").expect("first re-issuance");
    assert_eq!(
        (first.from_epoch, first.to_epoch, first.already_applied),
        (1, 2, false)
    );
    assert_eq!(
        first.settled_incarnation, None,
        "a never-started node settles nothing"
    );
    let renewed = inspect_capacity_account(&app_root)
        .expect("inspect renewed")
        .expect("issued");
    assert_eq!(renewed.epoch, 2);
    assert_eq!(renewed.incarnations_remaining, 16);
    assert_eq!(renewed.recovery_attempts_used, 0);
    assert_eq!(renewed.last_operation, "operator_reissue");
    assert_eq!(renewed.sequence, fresh.sequence);

    let document_path = app_root.join(".ai/config/onboarding/native-capacity-reissuance.json");
    let document = std::fs::read(&document_path).expect("published re-issuance document");
    let evidence = read_capacity_reissuances(&app_root.join(".ai/state")).expect("read evidence");
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].document_digest, first.document_digest);
    assert_eq!((evidence[0].from_epoch, evidence[0].to_epoch), (1, 2));
    assert_eq!(evidence[0].command, COMMAND);

    // Rerunning converges on the same epoch and writes nothing new.
    let again = reissue(&app_root, &lock, &operator, "e2e rerun").expect("idempotent rerun");
    assert!(again.already_applied);
    assert_eq!((again.from_epoch, again.to_epoch), (1, 2));
    assert_eq!(again.document_digest, first.document_digest);
    assert_eq!(std::fs::read(&document_path).unwrap(), document);
    assert_eq!(
        read_capacity_reissuances(&app_root.join(".ai/state"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        inspect_capacity_account(&app_root).unwrap().unwrap().epoch,
        2
    );

    // A tampered document is not the one the anchor names: refused, nothing repaired.
    let mut tampered: serde_json::Value = serde_json::from_slice(&document).unwrap();
    tampered["body"]["reason"] = serde_json::Value::String("tampered".to_owned());
    std::fs::write(&document_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    let error =
        reissue(&app_root, &lock, &operator, "e2e tampered").expect_err("tampered document");
    assert!(format!("{error:#}").contains("stale"), "{error:#}");
    assert_eq!(
        inspect_capacity_account(&app_root).unwrap().unwrap().epoch,
        2
    );
    assert_eq!(
        read_capacity_reissuances(&app_root.join(".ai/state"))
            .unwrap()
            .len(),
        1
    );

    // A document signed by another key for the same body is foreign for the
    // same reason: its bytes are not the ones the anchor authorised.
    let foreign = SigningKey::from_bytes(&[5; 32]);
    let body = serde_json::from_slice::<serde_json::Value>(&document).unwrap()["body"].clone();
    let canonical = lillux::canonical_json(&body).unwrap();
    let signature = lillux::crypto::Signer::sign(&foreign, canonical.as_bytes());
    let foreign_document = serde_json::json!({
        "body": body,
        "signature": format!("ed25519:{}", base64_standard(&signature.to_bytes())),
    });
    std::fs::write(
        &document_path,
        serde_json::to_vec(&foreign_document).unwrap(),
    )
    .unwrap();
    assert!(reissue(&app_root, &lock, &operator, "e2e foreign").is_err());
    assert_eq!(
        inspect_capacity_account(&app_root).unwrap().unwrap().epoch,
        2
    );

    // The original document restores the idempotent path.
    std::fs::write(&document_path, &document).unwrap();
    assert!(
        reissue(&app_root, &lock, &operator, "e2e restored")
            .unwrap()
            .already_applied
    );

    // Another operator key is refused before anything is read or written.
    let other = SigningKey::from_bytes(&[11; 32]);
    let error = reissue(&app_root, &lock, &other, "e2e other key").expect_err("foreign operator");
    assert!(
        format!("{error:#}").contains("operator key differs"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(&document_path).unwrap(), document);
}

fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
fn reissue_is_refused_while_another_owner_holds_the_state_lock() {
    let (_node_dir, app_root, _user_dir, _fixture) = sealed_core_root();
    let operator = operator_key(&app_root);
    // The command acquires the node's state lock first; whoever already holds
    // it (a running daemon, a stop in progress, another operator command) is
    // refused before the account is opened.
    let held = StateLock::acquire(&default_lock_path(&app_root)).expect("first owner");
    assert!(
        StateLock::acquire(&default_lock_path(&app_root)).is_err(),
        "the lock is exclusive"
    );
    assert!(
        !app_root
            .join(".ai/config/onboarding/native-capacity-reissuance.json")
            .exists()
    );
    assert_eq!(
        inspect_capacity_account(&app_root).unwrap().unwrap().epoch,
        1
    );
    // A lock protecting a different root is not this node's exclusion either.
    let other_root = tempfile::tempdir().unwrap();
    let foreign_lock =
        StateLock::acquire(&default_lock_path(other_root.path())).expect("other root lock");
    let error =
        reissue(&app_root, &foreign_lock, &operator, "e2e wrong lock").expect_err("wrong lock");
    assert!(!format!("{error:#}").is_empty());
    assert_eq!(
        inspect_capacity_account(&app_root).unwrap().unwrap().epoch,
        1
    );
    drop(held);
    let report = reissue(
        &app_root,
        &StateLock::acquire(&default_lock_path(&app_root)).unwrap(),
        &operator,
        "e2e after release",
    )
    .unwrap();
    assert_eq!(report.to_epoch, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn reissue_is_refused_while_the_daemon_runs() {
    let (harness, _fixture) = DaemonHarness::start_fast_core_only_prepared()
        .await
        .expect("start fast daemon");
    let app_root = harness.state_path.clone();
    // The live daemon holds the state lock for its whole life.
    let refused = StateLock::acquire(&default_lock_path(&app_root));
    assert!(
        refused.is_err(),
        "a running daemon must hold the state lock"
    );
    assert!(
        !app_root
            .join(".ai/config/onboarding/native-capacity-reissuance.json")
            .exists()
    );
    let summary = inspect_capacity_account(&app_root)
        .expect("inspect live account")
        .expect("issued");
    assert_eq!(summary.epoch, 1);
    assert!(
        summary.outstanding_incarnation,
        "the running daemon's incarnation is charged"
    );
    assert_eq!(summary.incarnations_remaining, 15);
    drop(harness);
}

/// Rewrite the fixture's native-capacity policy member with a short service
/// window, re-signed by the fixture node key before the init completion seals
/// the generation. The window must still exceed the drain margin plus the
/// cleanup occupancy the reservation requires (60 s each).
fn shorten_service_window(state_path: &Path, node: &SigningKey) -> anyhow::Result<()> {
    let path = state_path.join(".ai/node/policies/native_capacity.yaml");
    let raw = std::fs::read_to_string(&path)?;
    let body = lillux::signature::strip_signature_lines(&raw);
    let mut policy: serde_yaml::Value = serde_yaml::from_str(&body)?;
    let incarnation = policy["incarnation"]
        .as_mapping_mut()
        .expect("incarnation envelope");
    incarnation.insert("maximum_occupancy_ms".into(), 150_000u64.into());
    incarnation.insert("cleanup_occupancy_ms".into(), 60_000u64.into());
    incarnation.insert("rollover_drain_margin_ms".into(), 60_000u64.into());
    let body = serde_yaml::to_string(&policy)?;
    let signed =
        lillux::signature::sign_content_at(&body, node, "#", None, fast_fixture::FAST_FIXTURE_TIME);
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Twenty-one orderly rollovers, each renewing the epoch, on a sixteen
/// incarnation allowance: the account never exhausts and recovery stays at
/// zero. Each window is 150 s, so the whole run takes about an hour.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs about an hour of real service windows; set RYEOS_LONG_E2E=1 and run with --ignored"]
async fn twenty_one_rollovers_renew_the_epoch_without_exhausting() {
    if std::env::var("RYEOS_LONG_E2E").as_deref() != Ok("1") {
        eprintln!("RYEOS_LONG_E2E is not 1; skipping the long rollover loop");
        return;
    }
    let (mut harness, _fixture) = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| shorten_service_window(state_path, &fixture.node),
        |_| {},
    )
    .await
    .expect("start fast daemon with a 150 s window");
    let app_root = harness.state_path.clone();
    const ROLLOVERS: u64 = 21;
    for rollover in 1..=ROLLOVERS {
        let status =
            tokio::time::timeout(std::time::Duration::from_secs(300), harness.child.wait())
                .await
                .expect("the service window ends within its bound")
                .expect("wait for daemon exit");
        assert_eq!(
            status.code(),
            Some(75),
            "rollover {rollover} exits as a service-window rollover: {status:?}"
        );
        let summary = inspect_capacity_account(&app_root)
            .expect("inspect after rollover")
            .expect("issued");
        assert_eq!(
            summary.epoch,
            1 + rollover,
            "rollover {rollover} opened the next epoch"
        );
        assert_eq!(
            summary.recovery_attempts_used, 0,
            "rollover {rollover} charged no recovery"
        );
        assert_eq!(
            summary.incarnations_remaining, 16,
            "rollover {rollover} renewed the allowance"
        );
        assert!(
            !summary.outstanding_incarnation,
            "rollover {rollover} settled its incarnation"
        );
        assert_eq!(summary.last_operation, "rollover_renewal");
        harness
            .respawn_with(|_| {})
            .await
            .expect("restart after rollover");
        let debited = inspect_capacity_account(&app_root).unwrap().unwrap();
        assert_eq!(debited.epoch, 1 + rollover);
        assert_eq!(debited.incarnations_remaining, 15);
    }
    harness.stop_gracefully().await.expect("final stop");
}
