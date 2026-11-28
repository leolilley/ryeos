// A resumed live-root run re-admits its project root through the admission a
// launch of that path goes through, for the key that requested the run, at the
// node's time now. A revoked or retired key, an expired operator link and an
// unregistered checkout each end the run with the typed refusal.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryeos_app::state::AppState;
use ryeos_engine::contracts::{EffectivePrincipal, Principal};
use ryeos_engine::trust::{
    SignerRecord, SignerRole, SignerScope, SignerStatus, TrustEvent, TrustEventKind, TrustStore,
    compute_fingerprint,
};
use ryeos_executor::execution::runner::{RESUME_PROJECT_NOT_ADMITTED, readmit_live_project};
use ryeos_state::project_identity::ProjectIdentity;
use ryeos_ui::test_support as test_state;

fn requested_by(key: &str) -> EffectivePrincipal {
    EffectivePrincipal::Local(Principal {
        fingerprint: key.to_owned(),
        scopes: vec!["*".to_owned()],
    })
}

fn key_of(seed: u8) -> (lillux::crypto::SigningKey, String) {
    let signing = lillux::crypto::SigningKey::from_bytes(&[seed; 32]);
    let fingerprint = compute_fingerprint(&signing.verifying_key());
    (signing, fingerprint)
}

fn record(key: &lillux::crypto::SigningKey, status: SignerStatus) -> SignerRecord {
    SignerRecord::new(
        &key.verifying_key(),
        "resume-readmission-test",
        SignerRole::ProjectAuthor,
        SignerScope::authoring_default(),
        status,
    )
}

/// A checkout on disk, registered to the owner `key_fingerprint` (bare hex)
/// roots as a solo principal.
fn register_for(
    state: &AppState,
    key_fingerprint: &str,
    name: &str,
) -> (tempfile::TempDir, PathBuf) {
    let folder = tempfile::TempDir::new().unwrap();
    let canonical = folder.path().canonicalize().unwrap();
    std::fs::create_dir_all(canonical.join(".ai")).unwrap();
    std::fs::write(
        canonical.join(ryeos_state::project_identity::PROJECT_DECLARATION_PATH),
        format!("name: {name}\n"),
    )
    .unwrap();
    let owner = ryeos_engine::trust::SuccessionGraph::default()
        .ownership_of(key_fingerprint)
        .unwrap()
        .principal;
    let identity = ProjectIdentity::new(owner, name.parse().unwrap());
    ryeos_node::project_registry::register(&ryeos_node::project_registry::RegistrationChange {
        app_root: &state.config.app_root,
        node: &state.identity,
        request_digest: &"0".repeat(64),
        path: &canonical,
        identity: &identity,
    })
    .expect("register the checkout");
    (folder, canonical)
}

fn set_trust(state: &mut AppState, trust: TrustStore) {
    state.engine = Arc::new((*state.engine).clone().with_trust_store(trust));
}

fn refused(state: &AppState, key: &str, root: &Path) -> String {
    let refusal = readmit_live_project(state, &requested_by(key), root)
        .expect_err("the run's project root must be refused");
    refusal.to_string()
}

#[test]
fn everything_unchanged_is_admitted_and_an_unregistered_checkout_is_refused() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let (_signing, fingerprint) = key_of(11);
    let key = format!("fp:{fingerprint}");
    let (_folder, root) = register_for(&state, &fingerprint, "alpha");
    readmit_live_project(&state, &requested_by(&key), &root).expect("unchanged run is admitted");

    let unregistered = tempfile::TempDir::new().unwrap();
    let text = refused(&state, &key, unregistered.path());
    assert!(text.starts_with(RESUME_PROJECT_NOT_ADMITTED), "{text}");
}

#[test]
fn a_revoked_key_is_refused() {
    let (_tmp, _owner, mut state) = test_state::build_test_state();
    let (revoked, revoked_fp) = key_of(12);
    let (active, active_fp) = key_of(13);
    let trust = TrustStore::from_registry(
        lillux::crypto::SigningKey::from_bytes(&[1u8; 32]).verifying_key(),
        vec![
            record(&revoked, SignerStatus::Revoked),
            record(&active, SignerStatus::Active),
        ],
    )
    .unwrap()
    .with_succession_chain(&[]);
    set_trust(&mut state, trust);
    let (_revoked_folder, revoked_root) = register_for(&state, &revoked_fp, "revoked");
    let (_active_folder, active_root) = register_for(&state, &active_fp, "active");

    readmit_live_project(
        &state,
        &requested_by(&format!("fp:{active_fp}")),
        &active_root,
    )
    .expect("an active key at its own checkout is admitted");
    let text = refused(&state, &format!("fp:{revoked_fp}"), &revoked_root);
    assert!(text.starts_with(RESUME_PROJECT_NOT_ADMITTED), "{text}");
}

#[test]
fn a_retired_key_is_refused_while_its_successor_keeps_the_checkout() {
    let (_tmp, _owner, mut state) = test_state::build_test_state();
    let (retired, retired_fp) = key_of(14);
    let (successor, successor_fp) = key_of(15);
    let succeeded = TrustEvent {
        schema: "x".into(),
        seq: 1,
        previous: None,
        kind: TrustEventKind::Succeeded,
        signer: successor_fp.clone(),
        record_digest: String::new(),
        request_digest: String::new(),
        operator: String::new(),
        predecessor: Some(retired_fp.clone()),
    };
    let trust = TrustStore::from_registry(
        lillux::crypto::SigningKey::from_bytes(&[1u8; 32]).verifying_key(),
        vec![
            record(&retired, SignerStatus::Retired),
            record(&successor, SignerStatus::Active),
        ],
    )
    .unwrap()
    .with_succession_chain(std::slice::from_ref(&succeeded));
    set_trust(&mut state, trust);
    let (_folder, root) = register_for(&state, &retired_fp, "rotated");

    readmit_live_project(&state, &requested_by(&format!("fp:{successor_fp}")), &root)
        .expect("the successor speaks for the registered owner");
    let text = refused(&state, &format!("fp:{retired_fp}"), &root);
    assert!(text.starts_with(RESUME_PROJECT_NOT_ADMITTED), "{text}");
}

#[test]
fn an_expired_operator_link_ends_the_operator_naming_an_unregistered_directory() {
    let (_tmp, _owner, mut state) = test_state::build_test_state();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let linked = lillux::crypto::SigningKey::from_bytes(&[16u8; 32]);
    let linked_key = format!(
        "fp:{}",
        lillux::sha256_hex(linked.verifying_key().as_bytes())
    );
    let engine = Arc::make_mut(&mut state.engine);
    engine.trust_store = engine.trust_store.clone().with_succession_chain(&[]);
    let owner = engine
        .trust_store
        .owner_for(&operator.principal_id())
        .unwrap()
        .principal
        .to_string();
    let link = ryeos_engine::trust::pin_link_for_tests(
        &state.config.app_root,
        state.identity.signing_key(),
        operator.signing_key(),
        &linked,
        &owner,
        1,
    )
    .unwrap();
    engine.trust_store = engine.trust_store.clone().with_key_links(vec![link]);

    let unregistered = tempfile::TempDir::new().unwrap();
    readmit_live_project(&state, &requested_by(&linked_key), unregistered.path())
        .expect("a key with a live operator link names any directory the node allows");

    std::thread::sleep(std::time::Duration::from_millis(2_500));
    let text = refused(&state, &linked_key, unregistered.path());
    assert!(text.starts_with(RESUME_PROJECT_NOT_ADMITTED), "{text}");
}
