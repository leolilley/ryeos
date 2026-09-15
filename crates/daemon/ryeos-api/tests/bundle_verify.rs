mod test_state;

use std::sync::Arc;

use ryeos_api::handlers::bundle_verify::{DESCRIPTOR, Request};
use ryeos_api::registry::build_service_registry;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use serde_json::{Value, json};

const CAP: &str = "ryeos.execute.service.bundle/verify";

#[test]
fn signed_command_and_service_use_the_same_dual_mode_node_endpoint() {
    let command: ryeos_runtime::CommandDef = serde_yaml::from_str(include_str!(
        "../../../../bundles/core/.ai/node/commands/bundle-verify.yaml"
    ))
    .unwrap();
    let service: Value = serde_yaml::from_str(include_str!(
        "../../../../bundles/core/.ai/services/bundle/verify.yaml"
    ))
    .unwrap();
    let ryeos_runtime::CommandDispatch::ExecuteRef {
        execute,
        availability,
    } = &command.dispatch
    else {
        panic!("expected ordinary service dispatch")
    };
    assert_eq!(execute, DESCRIPTOR.service_ref);
    assert_eq!(*availability, ryeos_runtime::CommandAvailability::Both);
    assert_eq!(service["availability"], "both");
    assert_eq!(service["endpoint"], DESCRIPTOR.endpoint);
    assert_eq!(service["record_thread"], false);
    assert_eq!(service["state_access"], "read_only_existing");
    assert!(service.get("local_execute").is_none());
    assert_eq!(service["required_caps"], json!([CAP]));
    assert_eq!(DESCRIPTOR.required_caps, [CAP]);
    assert_eq!(
        DESCRIPTOR.availability,
        ryeos_api::ServiceAvailability::Both
    );

    let contract =
        ryeos_runtime::InvocationInputContract::from_lightweight_schema_value(&service["schema"])
            .unwrap()
            .unwrap();
    let argv = ["/source", "--registry-root", "/dependency"].map(str::to_owned);
    let params = ryeos_runtime::arg_binder::bind_argv_with_command(&argv, Some(&command)).unwrap();
    let params =
        ryeos_runtime::arg_binder::normalize_params_with_contract(params, Some(&contract)).unwrap();
    let request: Request = serde_json::from_value(params).unwrap();
    assert_eq!(request.source, std::path::Path::new("/source"));
    assert_eq!(
        request.registry_root.as_deref(),
        Some(std::path::Path::new("/dependency"))
    );
}

#[tokio::test]
async fn node_path_inspection_refuses_unauthenticated_remote_and_wrong_local_callers() {
    let (_tmp, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let local = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let contexts = [
        HandlerContext::anonymous(),
        HandlerContext::new_with_authority(
            local.principal_id(),
            vec![CAP.into()],
            true,
            Some(AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:remote".into()),
        ),
        HandlerContext::new_with_authority(
            local.principal_id(),
            vec![CAP.into()],
            true,
            Some(AuthorizedKeyPrincipalClass::RemoteNode),
            Some("site:remote".into()),
        ),
        HandlerContext::new_with_authority(
            state.identity.principal_id(),
            vec![CAP.into()],
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        ),
    ];
    let registry = build_service_registry();
    for context in contexts {
        // Deliberately invalid: authority must be rejected before the source
        // path is interpreted, rather than leaking a filesystem diagnostic.
        let error = registry.get("bundle.verify").unwrap()(
            json!({"source":"relative"}),
            context,
            state.clone(),
        )
        .await
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("requires the configured local operator"),
            "{error}"
        );
        assert!(!error.contains("source must be an absolute"), "{error}");
    }
}

#[tokio::test]
async fn configured_local_operator_reaches_strict_request_validation() {
    let (_tmp, state) = test_state::build_test_state();
    let local = NodeIdentity::load(&state.config.operator_signing_key_path).unwrap();
    let context = HandlerContext::new_with_authority(
        local.principal_id(),
        vec![CAP.into()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let registry = build_service_registry();
    let error = registry.get("bundle.verify").unwrap()(
        json!({"source":"relative"}),
        context,
        Arc::new(state),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("source must be an absolute path"));
}
