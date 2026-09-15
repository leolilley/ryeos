mod test_state;

use std::sync::Arc;

use ryeos_api::registry::build_service_registry;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewCredentialProfile;
use serde_json::{Value, json};

const LIST_CAP: &str = "ryeos.execute.service.credential-profiles/list";

#[test]
fn cli_descriptor_binds_pagination_flags_through_service_schema() {
    let command: ryeos_runtime::CommandDef = serde_yaml::from_str(include_str!(
        "../../../../bundles/codex/.ai/node/commands/profile-list.yaml"
    ))
    .unwrap();
    let service: Value = serde_yaml::from_str(include_str!(
        "../../../../bundles/core/.ai/services/credential-profiles/list.yaml"
    ))
    .unwrap();
    let contract =
        ryeos_runtime::InvocationInputContract::from_lightweight_schema_value(&service["schema"])
            .unwrap()
            .unwrap();
    for (argv, expected) in [
        (vec![], json!({})),
        (
            vec!["--limit", "10", "--after", "personal"],
            json!({"limit":10,"after":"personal"}),
        ),
        (
            vec!["--limit=1", "--after=002"],
            json!({"limit":1,"after":"002"}),
        ),
    ] {
        let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let params =
            ryeos_runtime::arg_binder::bind_argv_with_command(&argv, Some(&command)).unwrap();
        let params =
            ryeos_runtime::arg_binder::normalize_params_with_contract(params, Some(&contract))
                .unwrap();
        assert_eq!(params, expected);
    }
}

fn remote_operator(owner: &str) -> HandlerContext {
    HandlerContext::new_with_authority(
        owner.to_owned(),
        vec![LIST_CAP.to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::RemoteOperator),
        Some(format!("site:{}", "c".repeat(64))),
    )
}

fn add_profile(state: &AppState, id: &str, owner: &str) {
    state
        .state_store
        .create_credential_profile(NewCredentialProfile {
            profile_id: id,
            owner_principal: owner,
            home_id: &format!("private-{id}"),
        })
        .unwrap();
}

async fn list(state: Arc<AppState>, ctx: HandlerContext, params: Value) -> anyhow::Result<Value> {
    let registry = build_service_registry();
    registry
        .get("credential-profiles.list")
        .expect("list service registered")(params, ctx, state)
    .await
}

#[tokio::test]
async fn lists_only_owner_metadata_with_stable_pagination() {
    let (_tmp, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let alice = format!("fp:{}", "a".repeat(64));
    let bob = format!("fp:{}", "b".repeat(64));
    for (id, owner) in [("a", &alice), ("b", &bob), ("c", &alice), ("d", &alice)] {
        add_profile(&state, id, owner);
    }
    state
        .state_store
        .acquire_credential_profile("c", &alice, "private-lease-coordinate")
        .unwrap();
    let generation = state
        .state_store
        .begin_credential_profile_deletion("d", &alice, 1)
        .unwrap();
    state
        .state_store
        .finish_credential_profile_deletion("d", &alice, generation)
        .unwrap();

    let first = list(state.clone(), remote_operator(&alice), json!({"limit":1}))
        .await
        .unwrap();
    assert_eq!(first["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(first["profiles"][0]["profile_id"], "a");
    assert_eq!(first["profiles"][0]["in_use"], false);
    assert_eq!(first["next_cursor"], "a");
    let last = list(
        state.clone(),
        remote_operator(&alice),
        json!({"limit":1,"after":first["next_cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(last["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(last["profiles"][0]["profile_id"], "c");
    assert_eq!(last["profiles"][0]["in_use"], true);
    assert_eq!(last["profiles"][0]["credential_generation"], 1);
    assert!(last["next_cursor"].is_null());
    let keys = last["profiles"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec![
            "created_at_ms",
            "credential_generation",
            "in_use",
            "profile_id",
            "state",
            "updated_at_ms"
        ]
    );
    let other = list(state.clone(), remote_operator(&bob), json!({}))
        .await
        .unwrap();
    assert_eq!(other["profiles"].as_array().unwrap().len(), 1);
    assert_eq!(other["profiles"][0]["profile_id"], "b");
    assert!(
        !state
            .config
            .runtime_state_dir()
            .join("private-artifact-homes")
            .exists(),
        "listing must not create or open private homes"
    );
}

#[tokio::test]
async fn empty_list_works_for_configured_local_operator() {
    let (_tmp, state) = test_state::build_test_state();
    let owner = NodeIdentity::load(&state.config.operator_signing_key_path)
        .unwrap()
        .principal_id();
    let ctx = HandlerContext::new_with_authority(
        owner,
        vec![LIST_CAP.to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    assert_eq!(
        list(Arc::new(state), ctx, json!({})).await.unwrap(),
        json!({"profiles":[],"next_cursor":null})
    );
}

#[tokio::test]
async fn list_rejects_unverified_nonoperator_and_unforwarded_requests() {
    let (_tmp, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let owner = format!("fp:{}", "a".repeat(64));
    for (verified, class, origin) in [
        (
            false,
            AuthorizedKeyPrincipalClass::RemoteOperator,
            Some(format!("site:{}", "b".repeat(64))),
        ),
        (true, AuthorizedKeyPrincipalClass::RemoteOperator, None),
        (
            true,
            AuthorizedKeyPrincipalClass::RemoteNode,
            Some(format!("site:{}", "b".repeat(64))),
        ),
        (true, AuthorizedKeyPrincipalClass::LocalClient, None),
    ] {
        let ctx = HandlerContext::new_with_authority(
            owner.clone(),
            vec![LIST_CAP.to_owned()],
            verified,
            Some(class),
            origin,
        );
        let error = list(state.clone(), ctx, json!({})).await.unwrap_err();
        assert!(
            error.to_string().contains("admitted operator required"),
            "{error:#}"
        );
    }
}

#[tokio::test]
async fn list_rejects_invalid_bounds_and_caller_selected_ownership() {
    let (_tmp, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let owner = format!("fp:{}", "a".repeat(64));
    for request in [
        json!({"limit":0}),
        json!({"limit":201}),
        json!({"limit":-1}),
        json!({"after":""}),
        json!({"after":" padded "}),
        json!({"after":"x".repeat(257)}),
        json!({"after":"line\nbreak"}),
        json!({"owner_principal":owner}),
        json!({"all":true}),
    ] {
        assert!(
            list(state.clone(), remote_operator(&owner), request.clone())
                .await
                .is_err(),
            "{request}"
        );
    }
}
