//! Who may mint runtime authority from a project's own signed manifest is
//! judged by key alone: the node's configured operator, or a key that speaks
//! for the owner the operator key acts for. Anything else, including a failure
//! to establish the operator, is not the operator.

use ryeos_api::test_support as test_state;
use ryeos_app::runtime_capability_admission::ProjectMint;

/// Pin a link adding `added` to the operator's owner for `lifetime_secs`, and
/// return the added key's `fp:` id.
fn link_into_operator_owner(
    state: &mut ryeos_app::state::AppState,
    added: &lillux::crypto::SigningKey,
    lifetime_secs: u64,
) -> String {
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let engine = std::sync::Arc::make_mut(&mut state.engine);
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
        added,
        &owner,
        lifetime_secs,
    )
    .unwrap();
    engine.trust_store = engine.trust_store.clone().with_key_links(vec![link]);
    format!(
        "fp:{}",
        lillux::sha256_hex(added.verifying_key().as_bytes())
    )
}

#[test]
fn the_operator_and_keys_speaking_for_its_owner_mint_and_no_other_key_does() {
    let (_tmp, _owner, mut state) = test_state::build_test_state();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let linked = lillux::crypto::SigningKey::from_bytes(&[31u8; 32]);
    let linked_key = link_into_operator_owner(&mut state, &linked, 3_600);
    let app_root = state.config.app_root.clone();
    let trust_store = &state.engine.trust_store;
    let now = lillux::time::unix_secs_now();

    assert_eq!(
        ProjectMint::for_requesting_key(&app_root, trust_store, &operator.principal_id()),
        ProjectMint::OperatorKey,
        "the configured operator"
    );
    assert_eq!(
        ProjectMint::for_requesting_key_at(&app_root, trust_store, &linked_key, now),
        ProjectMint::OperatorKey,
        "a key with a live link into the operator's owner"
    );
    assert_eq!(
        ProjectMint::for_requesting_key_at(&app_root, trust_store, &linked_key, now + 7_200),
        ProjectMint::NoProjectMint,
        "the same key once its operator link has expired"
    );
    assert_eq!(
        ProjectMint::for_requesting_key(&app_root, trust_store, &format!("fp:{}", "b".repeat(64))),
        ProjectMint::NoProjectMint,
        "a key with its own owner"
    );
    let requested_by =
        ryeos_engine::contracts::EffectivePrincipal::Local(ryeos_engine::contracts::Principal {
            fingerprint: format!("fp:{}", "c".repeat(64)),
            scopes: vec!["*".to_owned()],
        });
    assert_eq!(
        ProjectMint::for_requested_by(&app_root, trust_store, &requested_by),
        ProjectMint::NoProjectMint,
        "a plan requested by a key that is not the operator, whatever its scopes"
    );
}

#[test]
fn a_node_whose_operator_cannot_be_established_mints_for_no_key() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap();
    let no_operator = tempfile::tempdir().unwrap();
    assert_eq!(
        ProjectMint::for_requesting_key(
            no_operator.path(),
            &state.engine.trust_store,
            &operator.principal_id()
        ),
        ProjectMint::NoProjectMint,
        "an app root with no operator genesis establishes no operator"
    );
}
