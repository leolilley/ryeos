//! Services whose answer is refused to a non-operator must refuse before they
//! parse the request or act. Each case sends params no service would accept:
//! a caller that is let past the operator check meets the parse refusal, so an
//! operator refusal for a refused caller shows the check came first and that
//! no effect ran. The node's own key and the operator are never refused by
//! the operator check.

use ryeos_api::test_support as test_state;

use std::sync::Arc;

use ryeos_api::registry::build_service_registry;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;
use serde_json::{Value, json};

const SERVICES: &[&str] = &[
    "maintenance.gc",
    "external-execution.settle-unknown-allocation",
    "bundle-release.submit",
];

fn context(
    fingerprint: String,
    class: Option<AuthorizedKeyPrincipalClass>,
    origin: Option<String>,
) -> HandlerContext {
    HandlerContext::new_with_authority(fingerprint, vec!["*".to_owned()], true, class, origin)
}

fn site() -> Option<String> {
    Some(format!("site:{}", "c".repeat(64)))
}

fn operator_principal(state: &AppState) -> String {
    ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
        .unwrap()
        .principal_id()
}

async fn call(
    state: Arc<AppState>,
    endpoint: &str,
    ctx: HandlerContext,
    params: Value,
) -> anyhow::Result<Value> {
    build_service_registry()
        .get(endpoint)
        .unwrap_or_else(|| panic!("{endpoint} registered"))(params, ctx, state)
    .await
}

fn is_operator_refusal(error: &anyhow::Error) -> bool {
    matches!(
        ryeos_app::handler_error::extract_handler_error(error),
        Some(ryeos_app::handler_error::HandlerError::Structured { code, .. })
            if matches!(
                code.as_str(),
                "local_operator_required"
                    | "operator_owner_not_spoken_for"
                    | "operator_class_required"
                    | "operator_rejects_remote_node"
            )
    )
}

fn unparseable() -> Value {
    json!({"no_such_field_in_any_request": 1})
}

#[tokio::test]
async fn non_operators_are_refused_before_the_request_is_parsed_or_acted_on() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let member = format!("fp:{}", "a".repeat(64));
    let contexts = [
        (
            "member holding every cap",
            context(
                member.clone(),
                Some(AuthorizedKeyPrincipalClass::LocalClient),
                None,
            ),
        ),
        (
            "forwarded principal not speaking for the operator owner",
            context(
                member.clone(),
                Some(AuthorizedKeyPrincipalClass::RemotePrincipal),
                site(),
            ),
        ),
        (
            "remote node key",
            context(
                format!("fp:{}", "b".repeat(64)),
                Some(AuthorizedKeyPrincipalClass::RemoteNode),
                site(),
            ),
        ),
    ];
    for endpoint in SERVICES {
        for (name, ctx) in &contexts {
            let error = call(state.clone(), endpoint, ctx.clone(), unparseable())
                .await
                .unwrap_err();
            assert!(
                is_operator_refusal(&error),
                "{endpoint} / {name}: {error:#}"
            );
        }
    }
}

#[tokio::test]
async fn the_operator_is_not_refused_by_the_operator_check() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let owner = operator_principal(&state);
    for endpoint in SERVICES {
        let ctx = context(
            owner.clone(),
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        );
        let error = call(state.clone(), endpoint, ctx, unparseable())
            .await
            .unwrap_err();
        assert!(!is_operator_refusal(&error), "{endpoint}: {error:#}");
    }
}

#[tokio::test]
async fn maintenance_gc_admits_the_node_key_as_a_schedule_fire_presents_it() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    for fingerprint in [
        state.identity.principal_id(),
        state.identity.fingerprint().to_owned(),
    ] {
        for class in [None, Some(AuthorizedKeyPrincipalClass::LocalClient)] {
            let ctx = context(fingerprint.clone(), class, None);
            let error = call(state.clone(), "maintenance.gc", ctx, unparseable())
                .await
                .unwrap_err();
            assert!(!is_operator_refusal(&error), "{error:#}");
            assert!(
                format!("{error:#}").contains("invalid gc params"),
                "{error:#}"
            );
        }
    }
}

#[tokio::test]
async fn the_node_key_with_a_forwarding_origin_is_not_a_node_fire() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let ctx = context(
        state.identity.principal_id(),
        Some(AuthorizedKeyPrincipalClass::RemoteNode),
        site(),
    );
    let error = call(state, "maintenance.gc", ctx, unparseable())
        .await
        .unwrap_err();
    assert!(is_operator_refusal(&error), "{error:#}");
}
