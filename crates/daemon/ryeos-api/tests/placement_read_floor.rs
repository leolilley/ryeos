//! A placement root is readable only by the one node a retained handoff job
//! names as its peer. A third node that knows the placement id is refused with
//! the same typed refusal as an unknown placement.

use base64::Engine as _;
use ryeos_api::objects_read_scope::{OBJECT_NOT_IN_SCOPE, ReadRoot, resolve_scope};
use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

const OPERATION_TYPE: &str = "worker_session_handoff";

fn node_key(seed: u8) -> lillux::crypto::SigningKey {
    lillux::crypto::SigningKey::from_bytes(&[seed; 32])
}

fn principal_of(key: &lillux::crypto::SigningKey) -> String {
    format!(
        "fp:{}",
        lillux::signature::compute_fingerprint(&key.verifying_key())
    )
}

fn remote_node_ctx(key: &lillux::crypto::SigningKey) -> HandlerContext {
    HandlerContext::new_with_authority(
        principal_of(key),
        Vec::new(),
        true,
        Some(AuthorizedKeyPrincipalClass::RemoteNode),
        Some("site:origin".to_owned()),
    )
}

#[test]
fn a_third_node_is_refused_a_placement_root_it_is_not_named_on() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let peer_b = node_key(61);
    let node_c = node_key(62);
    let placement = "handoff-op-1";

    // The remote entry for node B is a node-signed pin under the app root.
    ryeos_api::remote::config::write_remote_pin(
        &state.config.app_root,
        state.identity.signing_key(),
        &ryeos_api::remote::config::RemoteConfig {
            name: "node-b".to_owned(),
            url: None,
            reachability: ryeos_api::remote::config::Reachability::InboundOnly,
            principal_id: principal_of(&peer_b),
            signing_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(peer_b.verifying_key().as_bytes())
            ),
            site_id: "site:node-b".to_owned(),
            vault_fingerprint: "vault".to_owned(),
            ingest_ignore: None,
            project_bindings: Default::default(),
        },
        ryeos_api::remote::config::PinSource::DescriptorWithoutContact,
        None,
    )
    .unwrap();

    // The retained handoff job for the placement names node B as its peer.
    state
        .state_store
        .with_state_db(|db| {
            db.create_sync_job(&ryeos_state::NewSyncJob {
                job_id: format!("worker-handoff-source:{placement}"),
                operation_type: OPERATION_TYPE.to_owned(),
                operation: serde_json::json!({"operation_type": OPERATION_TYPE}),
                peer: Some("node-b".to_owned()),
                roots: Vec::new(),
                heads: Vec::new(),
                max_attempts: 4,
            })
        })
        .unwrap();

    let root = ReadRoot::Placement {
        placement_id: placement.to_owned(),
    };

    let refused = resolve_scope(&root, &remote_node_ctx(&node_c), &state)
        .expect_err("a node the job does not name reads nothing of the placement");
    let HandlerError::Structured { code, status, .. } = refused else {
        panic!("expected a typed refusal, got {refused:?}");
    };
    assert_eq!(code, OBJECT_NOT_IN_SCOPE);
    assert_eq!(status, 403);

    // An unknown placement is refused the same way, so the refusal does not
    // tell node C which placements exist.
    let unknown = resolve_scope(
        &ReadRoot::Placement {
            placement_id: "no-such-placement".to_owned(),
        },
        &remote_node_ctx(&node_c),
        &state,
    )
    .expect_err("an unknown placement is not readable");
    let HandlerError::Structured { code, status, .. } = unknown else {
        panic!("expected a typed refusal, got {unknown:?}");
    };
    assert_eq!(code, OBJECT_NOT_IN_SCOPE);
    assert_eq!(status, 403);

    // The configured peer resolves the same root.
    resolve_scope(&root, &remote_node_ctx(&peer_b), &state)
        .expect("the node the job names reads its placement");
}
