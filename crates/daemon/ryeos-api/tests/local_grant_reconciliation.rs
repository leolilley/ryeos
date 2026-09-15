mod test_state;

use std::sync::Arc;

use base64::Engine as _;
use ryeos_api::handlers::authorize_client::{Request, handle};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use serde_json::json;

#[test]
fn signed_command_selects_node_service_without_a_private_key_tool_bridge() {
    let core = ryeos_engine::test_support::core_bundle_root();
    let read = |relative: &str| -> serde_yaml::Value {
        serde_yaml::from_str(&std::fs::read_to_string(core.join(relative)).unwrap()).unwrap()
    };
    let command = read(".ai/node/commands/authorize-client.yaml");
    assert_eq!(
        command["dispatch"]["execute"].as_str(),
        Some("service:identity/authorize-client")
    );
    let service = read(".ai/services/identity/authorize-client.yaml");
    assert_eq!(service["availability"].as_str(), Some("both"));
    assert_eq!(service["state_access"].as_str(), Some("read_only_existing"));
    assert_eq!(service["record_thread"].as_bool(), Some(false));
    assert!(service.get("local_execute").is_none());
    assert!(
        !core
            .join(".ai/tools/ryeos/core/authorize-client.yaml")
            .exists()
    );
    assert!(
        ryeos_api::handlers::ALL
            .iter()
            .any(
                |descriptor| descriptor.service_ref == "service:identity/authorize-client"
                    && descriptor.availability
                        == ryeos_executor::executor::ServiceAvailability::Both
            )
    );
}

#[tokio::test]
async fn local_operator_merges_exact_scopes_without_changing_remote_identity() {
    let (_node, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let client = NodeIdentity::create(&client_dir.path().join("client.pem")).unwrap();
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(client.verifying_key().as_bytes());
    let context = HandlerContext::new_with_authority(
        operator.principal_id(),
        vec!["*".into()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let request = |scopes: &str, merge: bool| -> Request {
        serde_json::from_value(json!({
            "public_key": public_key, "scopes": scopes,
            "origin_site_id": "site:source", "merge_scopes": merge,
        }))
        .unwrap()
    };
    let first = handle(
        request("ryeos.execute.service.health/status", false),
        context.clone(),
        state.clone(),
    )
    .await
    .unwrap();
    let merged = handle(
        request("ryeos.execute.tool.project/check", true),
        context.clone(),
        state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(first["fingerprint"], merged["fingerprint"]);
    assert_eq!(merged["principal_class"], "remote_operator");
    assert_eq!(merged["previous_principal_class"], "remote_operator");
    assert_eq!(merged["origin_site_id"], "site:source");
    assert_eq!(merged["previous_origin_site_id"], "site:source");
    assert_eq!(merged["dropped_scopes"], json!([]));
    let grant = ryeos_app::identity::load_verified_authorized_key(
        client.fingerprint(),
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .unwrap()
    .unwrap();
    assert_eq!(grant.scopes.len(), 2);
    assert!(
        grant
            .scopes
            .iter()
            .any(|scope| scope == "ryeos.execute.service.health/status")
    );
    assert!(
        grant
            .scopes
            .iter()
            .any(|scope| scope == "ryeos.execute.tool.project/check")
    );
    // No wildcard expansion or implicit class/origin conversion is permitted.
    for value in [
        json!({"public_key":public_key,"scopes":"*","origin_site_id":"site:source"}),
        json!({"public_key":public_key,"scopes":"ryeos.execute.tool.project/check","origin_site_id":"site:other"}),
        json!({"public_key":public_key,"scopes":"ryeos.execute.tool.project/check"}),
    ] {
        assert!(
            handle(
                serde_json::from_value(value).unwrap(),
                context.clone(),
                state.clone()
            )
            .await
            .is_err()
        );
    }
    let after = ryeos_app::identity::load_verified_authorized_key(
        client.fingerprint(),
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .unwrap()
    .unwrap();
    assert_eq!(after.source_file_hash, grant.source_file_hash);
    // This fixture deliberately puts identity/auth files outside the default
    // RuntimeRoot paths: the service must use its selected node authority.
    assert!(!state.config.runtime_root().node_signing_key_path().exists());
}

#[tokio::test]
async fn non_operator_callers_are_rejected_before_decoding_or_grant_access() {
    let (_node, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    for context in [
        HandlerContext::new_with_authority(
            operator.principal_id(),
            vec!["*".into()],
            true,
            Some(AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:source".into()),
        ),
        HandlerContext::new_with_authority(
            "fp:not-the-operator".into(),
            vec!["*".into()],
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        ),
        HandlerContext::new_with_authority(
            operator.principal_id(),
            vec!["*".into()],
            false,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        ),
    ] {
        let request =
            serde_json::from_value(json!({"public_key":"not-base64","scopes":"*"})).unwrap();
        let error = handle(request, context, state.clone()).await.unwrap_err();
        assert!(format!("{error:#}").contains("configured local operator"));
        assert!(!format!("{error:#}").contains("invalid base64"));
    }
    for field in [
        "app_root",
        "node_signing_key_path",
        "authorized_keys_dir",
        "allow_wildcard",
    ] {
        let mut value = json!({"public_key":"key","scopes":"scope"});
        value[field] = json!("caller-owned");
        assert!(serde_json::from_value::<Request>(value).is_err());
    }
}

#[tokio::test]
async fn semantic_conversion_borrows_only_the_exact_standalone_lock() {
    let (_node, state) = test_state::build_test_state();
    let mut state = Arc::new(state);
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let client = NodeIdentity::create(&client_dir.path().join("client.pem")).unwrap();
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(client.verifying_key().as_bytes());
    let context = HandlerContext::new_with_authority(
        operator.principal_id(),
        vec!["*".into()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let request = |origin: &str, conversion: bool| -> Request {
        serde_json::from_value(json!({
            "public_key":public_key, "scopes":"ryeos.execute.service.health/status",
            "origin_site_id":origin, "allow_semantic_conversion":conversion,
        }))
        .unwrap()
    };
    handle(
        request("site:source", false),
        context.clone(),
        state.clone(),
    )
    .await
    .unwrap();
    assert!(
        handle(request("site:other", true), context.clone(), state.clone())
            .await
            .is_err()
    );

    let other = tempfile::tempdir().unwrap();
    let wrong_lock = ryeos_app::state_lock::StateLock::acquire(
        &ryeos_app::state_lock::default_lock_path(other.path()),
    )
    .unwrap();
    let mut extensions = ryeos_app::extension_state::ExtensionState::new();
    extensions.insert(Arc::new(wrong_lock));
    Arc::get_mut(&mut state).unwrap().extensions = Arc::new(extensions);
    assert!(
        handle(request("site:other", true), context.clone(), state.clone())
            .await
            .is_err()
    );

    let lock_path = ryeos_app::state_lock::default_lock_path(&state.config.app_root);
    let lock = Arc::new(ryeos_app::state_lock::StateLock::acquire(&lock_path).unwrap());
    let mut extensions = ryeos_app::extension_state::ExtensionState::new();
    extensions.insert(lock.clone());
    Arc::get_mut(&mut state).unwrap().extensions = Arc::new(extensions);
    assert!(ryeos_app::state_lock::StateLock::acquire(&lock_path).is_err());
    let result = handle(request("site:other", true), context, state.clone())
        .await
        .unwrap();
    assert_eq!(result["previous_origin_site_id"], "site:source");
    assert_eq!(result["origin_site_id"], "site:other");
    assert!(ryeos_app::state_lock::StateLock::acquire(&lock_path).is_err());
}
