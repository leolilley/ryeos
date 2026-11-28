//! A remote pinned without its signed descriptor has no ingest rules: a push
//! to it is refused with a typed code, naming the fix, before any network call.

use base64::Engine as _;
use ryeos_api::registry::build_service_registry;
use ryeos_api::remote::config::{REMOTE_PINNED_WITHOUT_INGEST_RULES, pin_remote};
use ryeos_api::remote::descriptor::{
    REMOTE_DESCRIPTOR_VERSION, RemoteDescriptor, RemoteDescriptorNode,
};
use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::HandlerContext;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn push_to_a_remote_pinned_without_ingest_rules_is_refused_before_any_network_call() {
    assert_eq!(
        REMOTE_PINNED_WITHOUT_INGEST_RULES,
        "remote_ingest_rules_missing"
    );
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);

    // A descriptor with a URL nothing listens on and no ingest rules: had the
    // push dialed it, the failure would be a connection error.
    let remote_key = lillux::crypto::SigningKey::from_bytes(&[9; 32]);
    let verifying = remote_key.verifying_key();
    let fingerprint = lillux::crypto::fingerprint(&verifying);
    let descriptor = RemoteDescriptor {
        version: REMOTE_DESCRIPTOR_VERSION,
        name: "bare".into(),
        url: Some("http://127.0.0.1:1".into()),
        ingest_ignore: None,
        site_id: format!("site:{fingerprint}"),
        vault_fingerprint: "sha256:bare-vault".into(),
        node: RemoteDescriptorNode {
            public_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifying.as_bytes())
            ),
            fingerprint,
        },
        capabilities: vec![],
        admission: None,
        provider: None,
    }
    .render_signed(&remote_key)
    .unwrap();
    pin_remote(
        &state.config.app_root,
        state.identity.signing_key(),
        "bare",
        &descriptor,
        None,
    )
    .unwrap();

    let project = tempfile::tempdir().unwrap();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap()
            .principal_id();
    let ctx = HandlerContext::new_with_authority(
        operator,
        vec!["*".to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let registry = build_service_registry();
    let error = registry.get("remote.push").expect("remote.push registered")(
        json!({ "remote": "bare", "project": project.path() }),
        ctx,
        state.clone(),
    )
    .await
    .expect_err("a push to a remote with no ingest rules is refused");
    let Some(ryeos_api::handler_error::HandlerError::Structured { code, body, .. }) =
        error.downcast_ref::<ryeos_api::handler_error::HandlerError>()
    else {
        panic!("expected a structured refusal, got {error:#}");
    };
    assert_eq!(code, REMOTE_PINNED_WITHOUT_INGEST_RULES);
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("--descriptor"), "{body}");
}
