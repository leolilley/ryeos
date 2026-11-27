//! The services a node answers with no key. A service is keyless exactly when
//! its `disclosure` block declares a `public` tier, so the set of such
//! services is pinned here: a new keyless service fails this test until it is
//! reviewed and added.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryeos_api::handlers::{health_status, identity_public_key};
use ryeos_api::test_support as test_state;
use ryeos_node_runtime::disclosure::{DisclosureClass, ServiceDisclosure, shape_result};
use serde_json::Value;

/// Every service that still declares a `public` tier. Each is keyless on
/// purpose: liveness, the node's public identity, the token-gated admission
/// claim, and the one-time browser launch link.
const KEYLESS: &[&str] = &[
    "admission/claim",
    "health/status",
    "identity/public_key",
    "ui/launch",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "yaml") {
            out.push(path);
        }
    }
}

/// Every service item in every bundle: its name and parsed body.
fn services() -> Vec<(String, Value)> {
    let mut found = Vec::new();
    for bundle in std::fs::read_dir(repo().join("bundles")).unwrap().flatten() {
        let root = bundle.path().join(".ai/services");
        let mut files = Vec::new();
        collect(&root, &mut files);
        for file in files {
            let name = file
                .strip_prefix(&root)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .into_owned();
            let body: Value = serde_yaml::from_str(&std::fs::read_to_string(&file).unwrap())
                .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            found.push((name, body));
        }
    }
    found
}

fn disclosure_of(name: &str) -> ServiceDisclosure {
    let (_, body) = services()
        .into_iter()
        .find(|(found, _)| found == name)
        .unwrap_or_else(|| panic!("no service {name}"));
    ServiceDisclosure::parse(body.get("disclosure").expect("disclosure block")).unwrap()
}

#[test]
fn the_services_with_a_public_tier_are_exactly_the_pinned_list() {
    let declared: BTreeSet<String> = services()
        .into_iter()
        .filter(|(_, body)| {
            body.get("disclosure")
                .and_then(|block| block.get("public"))
                .and_then(Value::as_array)
                .is_some_and(|entries| !entries.is_empty())
        })
        .map(|(name, _)| name)
        .collect();
    let pinned: BTreeSet<String> = KEYLESS.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        declared, pinned,
        "the keyless services changed: a service with a `public` tier answers with no key, \
         so review it and update KEYLESS"
    );
}

#[test]
fn the_keyless_health_answer_is_liveness_alone() {
    let (_tmp, _owner, state) = test_state::build_test_state();
    let full = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(health_status::handle(
            health_status::Request::default(),
            Arc::new(state),
        ))
        .unwrap();
    let disclosure = disclosure_of("health/status");
    let shaped = shape_result(
        DisclosureClass::Public,
        Some(&disclosure),
        full.clone(),
        disclosure.bounds(),
    )
    .unwrap();
    let keys: Vec<&String> = shaped.as_object().expect("an object").keys().collect();
    assert_eq!(keys, vec!["liveness"], "{shaped}");
    assert_eq!(shaped["liveness"], "alive");
    // The handler answers more; a keyless caller gets none of it.
    assert!(full.get("operational_services").is_some());
    assert!(full.get("status").is_some());
}

#[test]
fn the_keyless_public_key_answer_is_exactly_the_nodes_own_five_fields() {
    let (_tmp, _owner, mut state) = test_state::build_test_state();
    state.vault_fingerprint = Some("vault-fingerprint-of-this-node".to_owned());
    let node_principal = state.identity.principal_id();
    let state = Arc::new(state);
    let full = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(identity_public_key::handle(
            identity_public_key::Request::default(),
            state,
        ))
        .unwrap();
    let disclosure = disclosure_of("identity/public_key");
    let shaped = shape_result(
        DisclosureClass::Public,
        Some(&disclosure),
        full.clone(),
        disclosure.bounds(),
    )
    .unwrap();
    let keys: BTreeSet<&str> = shaped
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    let expected: BTreeSet<&str> = [
        "signing_key",
        "fingerprint",
        "principal_id",
        "site_id",
        "vault_fingerprint",
    ]
    .into_iter()
    .collect();
    assert_eq!(keys, expected, "{shaped}");
    // The one principal id in the answer is the node's own.
    assert_eq!(shaped["principal_id"], serde_json::json!(node_principal));
    assert_eq!(
        shaped["vault_fingerprint"],
        "vault-fingerprint-of-this-node"
    );
    assert!(full.get("version").is_some());
}

#[test]
fn ingest_ignore_needs_a_key_on_the_service_and_the_route() {
    let declared = services()
        .into_iter()
        .find(|(name, _)| name == "system/ingest-ignore")
        .expect("service")
        .1;
    assert!(
        declared["disclosure"].get("public").is_none(),
        "ingest-ignore must have no public tier"
    );
    let route: Value = serde_yaml::from_str(
        &std::fs::read_to_string(repo().join("bundles/core/.ai/node/routes/ingest-ignore.yaml"))
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    assert_eq!(route["auth"], "ryeos_signed");
}

#[tokio::test]
async fn an_unsigned_request_to_ingest_ignore_is_refused() {
    use axum::http::{HeaderMap, Method};
    use ryeos_api::route_error::RouteDispatchError;
    use ryeos_api::routes::invocation::{CompiledRouteInvocation, RouteInvocationContext};
    use ryeos_api::routes::invokers::ryeos_signed_invocation::CompiledRyeosSignedVerifier;
    use ryeos_api::routes::webhook_dedupe::WebhookDedupeStore;

    let (_tmp, _owner, state) = test_state::build_test_state();
    let result = CompiledRyeosSignedVerifier
        .invoke(RouteInvocationContext {
            route_id: "core/ingest-ignore".into(),
            method: Method::GET,
            uri: "/ingest-ignore".parse().unwrap(),
            captures: Default::default(),
            headers: HeaderMap::new(),
            body_raw: Vec::new(),
            input: Value::Null,
            principal: None,
            workspace_lifeline: None,
            launch_timings: None,
            state,
            publication: None,
            webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
        })
        .await;
    assert!(matches!(result, Err(RouteDispatchError::Unauthorized)));
}
