use ryeos_api::test_support as test_state;

use std::sync::Arc;

use ryeos_api::registry::build_service_registry;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{DedicatedCandidateDisposition, NewCredentialProfile, NewDedicatedSession};
use ryeos_node_runtime::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use serde_json::{Value, json};

const CAP: &str = "ryeos.execute.service.external-execution/status";

fn local_operator(state: &AppState) -> HandlerContext {
    let owner = NodeIdentity::load(&state.config.operator_signing_key_path)
        .unwrap()
        .principal_id();
    HandlerContext::new_with_authority(
        owner,
        vec![CAP.to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

async fn status(state: Arc<AppState>, ctx: HandlerContext, params: Value) -> anyhow::Result<Value> {
    build_service_registry()
        .get("external-execution.status")
        .expect("status service registered")(params, ctx, state)
    .await
}

#[test]
fn cli_descriptor_dispatches_to_the_status_service() {
    let command: ryeos_runtime::CommandDef = serde_yaml::from_str(include_str!(
        "../../../../bundles/core/.ai/node/commands/external-execution-status.yaml"
    ))
    .unwrap();
    let service: Value = serde_yaml::from_str(include_str!(
        "../../../../bundles/core/.ai/services/external-execution/status.yaml"
    ))
    .unwrap();
    assert_eq!(service["endpoint"], "external-execution.status");
    assert_eq!(service["required_caps"][0], CAP);
    let params = ryeos_runtime::arg_binder::bind_argv_with_command(&[], Some(&command)).unwrap();
    assert_eq!(params, json!({}));
}

#[tokio::test]
async fn status_requires_operator() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let local = local_operator(&state);
    let site = Some(format!("site:{}", "b".repeat(64)));
    for (fingerprint, verified, class, origin) in [
        // A remote operator is refused: the view is node-wide.
        (
            local.fingerprint.clone(),
            true,
            AuthorizedKeyPrincipalClass::RemoteOperator,
            site.clone(),
        ),
        (
            local.fingerprint.clone(),
            false,
            AuthorizedKeyPrincipalClass::LocalClient,
            None,
        ),
        (
            format!("fp:{}", "a".repeat(64)),
            true,
            AuthorizedKeyPrincipalClass::LocalClient,
            None,
        ),
        (
            local.fingerprint.clone(),
            true,
            AuthorizedKeyPrincipalClass::RemoteNode,
            site.clone(),
        ),
    ] {
        let ctx = HandlerContext::new_with_authority(
            fingerprint,
            vec![CAP.to_owned()],
            verified,
            Some(class),
            origin,
        );
        let error = status(state.clone(), ctx, json!({})).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("local configured operator required"),
            "{error:#}"
        );
    }
    let error = status(state.clone(), local.clone(), json!({"extra": 1}))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("extra"), "{error:#}");
    let empty = status(state, local, json!({})).await.unwrap();
    assert_eq!(empty["schema"], "ryeos.external_execution_status.v1");
    assert_eq!(empty["allocations"], json!([]));
    assert_eq!(empty["qualification_local_starts"], json!([]));
    assert_eq!(empty["credential_profiles"], json!([]));
}

#[tokio::test]
async fn status_rows_carry_fix() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let operator = "fp:operator";
    for id in ["loose", "session", "free"] {
        state
            .state_store
            .create_credential_profile(NewCredentialProfile {
                profile_id: id,
                owner_principal: operator,
                home_id: &format!("home-{id}"),
            })
            .unwrap();
    }
    state
        .state_store
        .acquire_credential_profile("loose", operator, "lock-without-owner")
        .unwrap();
    state
        .state_store
        .acquire_credential_profile("session", operator, "worker-session")
        .unwrap();
    state
        .state_store
        .admit_dedicated_session(NewDedicatedSession {
            placement_thread_id: "T-session",
            chain_root_id: "T-session",
            owner_principal: operator,
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-session",
            candidate_required: false,
            candidate_disposition: DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: "session",
            credential_generation: 1,
            credential_lock_owner: "worker-session",
        })
        .unwrap();

    let view = status(state.clone(), local_operator(&state), json!({}))
        .await
        .unwrap();
    let held = view["credential_profiles"].as_array().unwrap();
    assert_eq!(held.len(), 2, "{view}");
    assert_eq!(held[0]["profile_id"], "loose");
    assert_eq!(held[0]["owner"]["kind"], "unresolved");
    assert_eq!(held[0]["owner"]["lock_owner"], "lock-without-owner");
    assert_eq!(held[0]["attention"], "operator");
    assert!(held[0]["fix"].as_str().unwrap().contains("restart"));
    assert_eq!(held[1]["profile_id"], "session");
    assert_eq!(held[1]["owner"]["kind"], "dedicated_session");
    assert_eq!(held[1]["owner"]["chain_root_id"], "T-session");
    assert_eq!(held[1]["owner"]["session_state"], "admitted");
    assert_eq!(held[1]["attention"], "in_use");
    assert!(
        held[1]["fix"]
            .as_str()
            .unwrap()
            .contains("ryeos worker session terminate T-session"),
        "{}",
        held[1]["fix"]
    );

    // Releasing the loose lock removes its row and nothing else.
    state
        .state_store
        .release_credential_profile("loose", "lock-without-owner")
        .unwrap();
    let view = status(state.clone(), local_operator(&state), json!({}))
        .await
        .unwrap();
    let held = view["credential_profiles"].as_array().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["profile_id"], "session");
}
