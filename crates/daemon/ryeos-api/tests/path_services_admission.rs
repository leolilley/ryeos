//! Services that take a host path from the request either admit it like any
//! other live project path or are the operator's alone.
//!
//! Admitting services: a key that is not the operator gets one refusal for a
//! path that does not exist and for an existing directory nobody registered to
//! it, and the operator is not refused by admission. Reaching the success path
//! for a registered checkout needs a registered owner, a signed manifest and a
//! chain or snapshot per service, so that side is covered by the admission
//! tests of `admit_live_project_path` itself; these tests pin the refusals and
//! the operator's pass.
//!
//! Operator-only services: a key that is not the operator is refused before
//! the request is read and before any path is touched.

use ryeos_api::registry::build_service_registry;
use ryeos_api::test_support as test_state;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::live_project_path::LIVE_PROJECT_NOT_REGISTERED_TO_CALLER;
use ryeos_app::state::AppState;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;
use serde_json::{Value, json};
use std::sync::Arc;

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
/// key that is not the configured operator.
fn another_local_key() -> HandlerContext {
    HandlerContext::new_with_authority(
        format!("fp:{}", "b".repeat(64)),
        vec!["*".to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::LocalClient),
        None,
    )
}

/// A forwarded remote principal that does not speak for the operator's owner.
fn foreign_remote_principal() -> HandlerContext {
    HandlerContext::new_with_authority(
        format!("fp:{}", "a".repeat(64)),
        vec!["*".to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::RemotePrincipal),
        Some(format!("site:{}", "c".repeat(64))),
    )
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

fn execution_policy() -> Value {
    json!({
        "schema_version": 2,
        "ownership": "daemon_owned",
        "recovery": "restart_recoverable",
        "response": "accepted",
        "target": {"kind": "here"},
        "environment": {"kind": "none"},
        "project": {
            "kind": "pinned",
            "source": {
                "kind": "current_head",
                "project": {"kind": "named", "name": "fixture"}
            },
            "realization": {
                "kind": "cow",
                "terminal_publication": {"kind": "retain_current_head"}
            },
            "child_policy": {"kind": "inherit"}
        }
    })
}

/// Each service that admits a caller-named path, with a request naming `path`.
fn admitting_services() -> Vec<(&'static str, Box<dyn Fn(&str) -> Value>)> {
    vec![
        (
            "project.status",
            Box::new(|path| json!({ "project_path": path })),
        ),
        (
            "revisions.list",
            Box::new(|path| json!({ "project_path": path, "item_ref": "tool:fixture/a" })),
        ),
        (
            "revisions.get",
            Box::new(
                |path| json!({ "project_path": path, "item_ref": "tool:fixture/a", "seq": 1 }),
            ),
        ),
        (
            "items.revised",
            Box::new(|path| json!({ "project_path": path, "item_refs": ["tool:fixture/a"] })),
        ),
        (
            "project.apply-snapshot",
            Box::new(|path| {
                json!({
                    "project_path": path,
                    "project_name": "fixture",
                    "snapshot_hash": "0".repeat(64),
                    "expected_deployed_hash": null,
                })
            }),
        ),
        (
            "remote.execute",
            Box::new(|path| {
                json!({
                    "remote": "nowhere",
                    "item_ref": "tool:fixture/a",
                    "ref_bindings": {},
                    "project": path,
                    "execution_policy": execution_policy(),
                })
            }),
        ),
        (
            "remote.run",
            Box::new(|path| {
                json!({
                    "remote": "nowhere",
                    "item_ref": "tool:fixture/a",
                    "ref_bindings": {},
                    "project": path,
                    "execution_policy": execution_policy(),
                })
            }),
        ),
        (
            "remote.push",
            Box::new(|path| json!({ "remote": "nowhere", "project": path })),
        ),
    ]
}

#[tokio::test]
async fn admitting_services_refuse_a_non_operator_the_same_way_for_every_path() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let unregistered = tempfile::TempDir::new().unwrap();
    let existing = unregistered.path().to_str().unwrap().to_owned();
    let missing = unregistered
        .path()
        .join("does-not-exist")
        .to_str()
        .unwrap()
        .to_owned();
    for (endpoint, request) in admitting_services() {
        let for_missing = call(
            state.clone(),
            endpoint,
            another_local_key(),
            request(missing.as_str()),
        )
        .await
        .expect_err(endpoint);
        let for_existing = call(
            state.clone(),
            endpoint,
            another_local_key(),
            request(existing.as_str()),
        )
        .await
        .expect_err(endpoint);
        let (missing_text, existing_text) =
            (format!("{for_missing:#}"), format!("{for_existing:#}"));
        assert!(
            missing_text.contains(LIVE_PROJECT_NOT_REGISTERED_TO_CALLER),
            "{endpoint}: {missing_text}"
        );
        assert_eq!(
            missing_text, existing_text,
            "{endpoint}: a nonexistent path and an unregistered directory must answer alike"
        );
    }
}

#[tokio::test]
async fn admitting_services_do_not_refuse_the_operator_at_admission() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let unregistered = tempfile::TempDir::new().unwrap();
    let existing = unregistered.path().to_str().unwrap().to_owned();
    for (endpoint, request) in admitting_services() {
        // Whatever the operator is refused for later (no such remote, no
        // manifest, no chain), it is never the non-operator admission refusal.
        if let Err(error) = call(
            state.clone(),
            endpoint,
            operator(&state),
            request(existing.as_str()),
        )
        .await
        {
            let text = format!("{error:#}");
            assert!(
                !text.contains(LIVE_PROJECT_NOT_REGISTERED_TO_CALLER),
                "{endpoint}: the operator was refused by admission: {text}"
            );
            assert!(!is_not_operator_refusal(&error), "{endpoint}: {text}");
        }
    }
}

/// The bundle-release services that read a project path, a release input's
/// project path or an executable path from the request.
const BUNDLE_RELEASE_PATH_SERVICES: &[&str] = &[
    "bundle-release.authority-measure",
    "bundle-release.input-inspect",
    "bundle-release.core-seed-inspect",
    "bundle-release.core-seed-build",
    "bundle-release.core-seed-capture",
    "bundle-release.core-seed-qualify",
    "bundle-release.generation-build",
    "bundle-release.generation-capture",
    "bundle-release.generation-qualify",
    "bundle-release.substrate-build",
    "bundle-release.substrate-qualify",
];

#[tokio::test]
async fn bundle_release_path_services_are_the_operators_alone() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    // A request that names paths: the refusal comes before it is read.
    let request = json!({
        "project_path": "/nonexistent-ih77-bundle-release",
        "publisher_executable_path": "/nonexistent-ih77-bundle-release/publisher",
        "release_input": {"project_path": "/nonexistent-ih77-bundle-release"},
    });
    for endpoint in BUNDLE_RELEASE_PATH_SERVICES {
        for (label, ctx) in [
            ("another local key", another_local_key()),
            ("a foreign remote principal", foreign_remote_principal()),
        ] {
            let error = call(state.clone(), endpoint, ctx, request.clone())
                .await
                .expect_err(endpoint);
            assert!(
                is_not_operator_refusal(&error),
                "{endpoint}, {label}: {error:#}"
            );
        }
        // The operator is past the operator check: this request is not a valid
        // one, so it is refused for that, never as a non-operator.
        if let Err(error) = call(state.clone(), endpoint, operator(&state), request.clone()).await {
            assert!(
                !is_not_operator_refusal(&error),
                "{endpoint}, operator: {error:#}"
            );
        }
    }
}

/// Bundle-release operations served by the generic release handler, which
/// requires the admitted operator before it reads any request.
const GENERIC_RELEASE_SERVICES: &[&str] = &[
    "bundle-release.status",
    "bundle-release.request-authorization",
    "bundle-release.substrate-release-authorization",
];

#[tokio::test]
async fn the_generic_release_handler_refuses_every_key_but_the_operator() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    for endpoint in GENERIC_RELEASE_SERVICES {
        for (label, ctx) in [
            ("another local key", another_local_key()),
            ("a foreign remote principal", foreign_remote_principal()),
        ] {
            let error = call(state.clone(), endpoint, ctx, json!({}))
                .await
                .expect_err(endpoint);
            assert!(
                is_not_operator_refusal(&error),
                "{endpoint}, {label}: {error:#}"
            );
        }
        if let Err(error) = call(state.clone(), endpoint, operator(&state), json!({})).await {
            assert!(
                !is_not_operator_refusal(&error),
                "{endpoint}, operator: {error:#}"
            );
        }
    }
}

#[tokio::test]
async fn remote_pull_is_the_operators_alone_and_touches_no_path_for_anyone_else() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let workspace = tempfile::TempDir::new().unwrap();
    let parent = workspace.path().join("not-yet-created");
    let output_dir = parent.join("out");
    // The request is refused before it is read, so no valid root is needed.
    let request = json!({
        "remote": "nowhere",
        "output_dir": output_dir.to_str().unwrap(),
    });
    for (label, ctx) in [
        ("another local key", another_local_key()),
        ("a foreign remote principal", foreign_remote_principal()),
    ] {
        let error = call(state.clone(), "remote.pull", ctx, request.clone())
            .await
            .expect_err(label);
        assert!(is_not_operator_refusal(&error), "{label}: {error:#}");
        assert!(
            !parent.exists(),
            "{label}: the output directory's parent was created"
        );
    }
}

/// A revision is announced only on a request made to this node directly.
/// Admission alone would let the operator's own key, forwarded from another
/// of its nodes, name any checkout; items/revised refuses the forwarded
/// request before the path is admitted.
#[tokio::test]
async fn items_revised_refuses_a_forwarded_request_even_from_the_operators_key() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let state = Arc::new(state);
    let operator_key =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap()
            .principal_id();
    let forwarded_operator = HandlerContext::new_with_authority(
        operator_key,
        vec!["*".to_owned()],
        true,
        Some(AuthorizedKeyPrincipalClass::RemotePrincipal),
        Some(format!("site:{}", "c".repeat(64))),
    );
    let project = tempfile::TempDir::new().unwrap();
    let error = call(
        state,
        "items.revised",
        forwarded_operator,
        json!({
            "project_path": project.path().to_str().unwrap(),
            "item_refs": ["tool:fixture/a"],
        }),
    )
    .await
    .expect_err("a forwarded request is refused");
    let text = format!("{error:#}");
    assert!(text.contains("not a forwarded one"), "{text}");
}
