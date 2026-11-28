//! Services that make the node key sign or act are the operator's alone: a
//! key that is not the node's admitted operator is refused before its request
//! is read, and `outbound_principal: node` is refused with a typed code. No
//! shipped profile hands these capabilities to any key.

use ryeos_api::handlers::remote_push::{OUTBOUND_PRINCIPAL_NODE_OPERATOR_ONLY, OutboundPrincipal};
use ryeos_api::registry::build_service_registry;
use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;
use serde_json::{Value, json};
use std::sync::Arc;

/// The services whose effect is the node key's: each is operator-only.
const NODE_SIGNING_CAPS: &[&str] = &[
    "ryeos.execute.service.remote/push",
    "ryeos.execute.service.remote/run",
    "ryeos.execute.service.remote/descriptor",
    "ryeos.execute.service.remote/sync-project-ai",
    "ryeos.execute.service.remote/project-status",
    "ryeos.execute.service.remote/doctor",
];

fn operator(state: &AppState) -> HandlerContext {
    let key =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap()
            .principal_id();
    HandlerContext::new_with_authority(
        key,
        vec!["*".to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

/// A verified key holding every capability that is not the operator: a local
/// key that is not the configured operator, and a forwarded remote principal
/// that does not speak for the operator's owner.
fn non_operators() -> Vec<(&'static str, HandlerContext)> {
    vec![
        (
            "another local key",
            HandlerContext::new_with_authority(
                format!("fp:{}", "b".repeat(64)),
                vec!["*".to_owned()],
                true,
                Some(AuthorizedKeyPrincipalClass::LocalClient),
                None,
            ),
        ),
        (
            "a remote principal not speaking for the operator's owner",
            HandlerContext::new_with_authority(
                format!("fp:{}", "a".repeat(64)),
                vec!["*".to_owned()],
                true,
                Some(AuthorizedKeyPrincipalClass::RemotePrincipal),
                Some(format!("site:{}", "c".repeat(64))),
            ),
        ),
    ]
}

async fn call(
    state: Arc<AppState>,
    endpoint: &str,
    ctx: HandlerContext,
    params: Value,
) -> anyhow::Result<Value> {
    let registry = build_service_registry();
    registry
        .get(endpoint)
        .unwrap_or_else(|| panic!("{endpoint} registered"))(params, ctx, state)
    .await
}

fn is_not_operator_refusal(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("local_operator_required")
        || text.contains("operator_owner_not_spoken_for")
        || text.contains("operator_class_required")
}

#[tokio::test]
async fn services_where_the_node_key_acts_refuse_every_key_but_the_operator() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    // Requests that would otherwise be read: the refusal comes first.
    let cases = [
        (
            "remote.descriptor",
            json!({ "name": "n", "url": "https://example.invalid" }),
        ),
        (
            "remote.sync-project-ai",
            json!({ "project": "/nonexistent" }),
        ),
        (
            "remote.project-status",
            json!({ "project": "/nonexistent" }),
        ),
        ("remote.doctor", json!({})),
    ];
    for (endpoint, params) in cases {
        for (label, ctx) in non_operators() {
            let error = call(state.clone(), endpoint, ctx, params.clone())
                .await
                .expect_err(endpoint);
            assert!(
                is_not_operator_refusal(&error),
                "{endpoint}, {label}: {error:#}"
            );
        }
        // The operator is past the operator check: whatever it is refused
        // for now is never the operator check.
        if let Err(error) = call(state.clone(), endpoint, operator(&state), params.clone()).await {
            assert!(
                !is_not_operator_refusal(&error),
                "{endpoint}, operator: {error:#}"
            );
        }
    }
}

#[tokio::test]
async fn remote_push_refuses_outbound_principal_node_for_a_non_operator_before_any_path() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    for (label, ctx) in non_operators() {
        let error = call(
            state.clone(),
            "remote.push",
            ctx,
            json!({ "project": "/nonexistent-ih77", "outbound_principal": "node" }),
        )
        .await
        .expect_err(label);
        assert!(
            format!("{error:#}").contains(OUTBOUND_PRINCIPAL_NODE_OPERATOR_ONLY),
            "{label}: {error:#}"
        );
    }
}

#[test]
fn outbound_principal_node_is_the_operators_or_the_nodes_alone() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    assert_eq!(
        OutboundPrincipal::resolve_admitted(
            Some(OutboundPrincipal::Node),
            &state,
            &operator(&state)
        )
        .unwrap(),
        OutboundPrincipal::Node
    );
    let node = HandlerContext::new_with_authority(
        state.identity.principal_id(),
        vec![],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    assert_eq!(
        OutboundPrincipal::resolve_admitted(None, &state, &node).unwrap(),
        OutboundPrincipal::Node,
        "the node itself has no delegation to present"
    );
    for (label, ctx) in non_operators() {
        let refused =
            OutboundPrincipal::resolve_admitted(Some(OutboundPrincipal::Node), &state, &ctx)
                .expect_err(label);
        assert!(
            format!("{refused}").contains(OUTBOUND_PRINCIPAL_NODE_OPERATOR_ONLY)
                || format!("{refused:?}").contains(OUTBOUND_PRINCIPAL_NODE_OPERATOR_ONLY),
            "{label}: {refused:?}"
        );
        assert_eq!(
            OutboundPrincipal::resolve_admitted(None, &state, &ctx).unwrap(),
            OutboundPrincipal::Delegated,
            "{label}: with no choice a caller pushes as itself"
        );
    }
}

/// Every string under a key naming scopes or capabilities, anywhere in `value`.
fn granted_scopes(value: &serde_yaml::Value, under_scope_key: bool, out: &mut Vec<String>) {
    match value {
        serde_yaml::Value::String(text) if under_scope_key => out.push(text.clone()),
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                granted_scopes(item, under_scope_key, out);
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (key, item) in map {
                let scope_key = key.as_str().is_some_and(|key| {
                    key.ends_with("scopes")
                        || key.ends_with("capabilities")
                        || key.ends_with("caps")
                });
                granted_scopes(item, scope_key, out);
            }
        }
        _ => {}
    }
}

/// Until the node-signing refusals are on every node, no shipped profile may
/// name a scope that covers them. The member class does not exist yet; when
/// it does, its section is read here too.
#[test]
fn no_shipped_profile_grants_a_node_signing_capability() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../bundles/.ai/node/init/profiles");
    let mut read = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("yaml") {
            continue;
        }
        let profile: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut scopes = Vec::new();
        granted_scopes(&profile, false, &mut scopes);
        for scope in &scopes {
            for cap in NODE_SIGNING_CAPS {
                assert!(
                    !ryeos_runtime::authorizer::cap_matches(scope, cap),
                    "{}: scope `{scope}` covers `{cap}`",
                    path.display()
                );
            }
        }
        read += 1;
    }
    assert!(read >= 15, "read {read} profiles from {}", dir.display());
}
