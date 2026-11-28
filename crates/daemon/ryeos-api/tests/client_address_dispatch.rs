//! The route dispatcher resolves the request's source address after
//! authentication, from the socket peer and the node's client-address policy,
//! and hands it to the service handler through its `HandlerContext`.

use ryeos_api::test_support as test_state;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Method, Request};
use ryeos_api::api_state::ApiState;
use ryeos_api::routes::dispatcher::route_dispatcher;
use ryeos_api::routes::response_modes::ResponseModeRegistry;
use ryeos_api::routes::webhook_dedupe::WebhookDedupeStore;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::route_raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec, RawRouteSpec};
use ryeos_app::service_registry::ServiceRegistry;
use ryeos_app::state::AppState;
use ryeos_node_runtime::node_policy::sections::client_address::{
    ClientAddressHeader, ClientAddressPolicy, SourceAddress,
};
use serde_json::{Value, json};

/// What the last handler invocation saw: outer `None` means the handler did
/// not run, inner is the context's `source_address`.
static OBSERVED: Mutex<Option<Option<SourceAddress>>> = Mutex::new(None);

fn recording_handler(
    _params: Value,
    context: HandlerContext,
    _state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
    *OBSERVED.lock().unwrap() = Some(context.source_address);
    Box::pin(async { Ok(json!({"ok": true})) })
}

/// Dispatch one request through a route backed by the recording handler and
/// return the source address the handler saw.
async fn observed_source_address(
    policy: ClientAddressPolicy,
    peer: Option<SocketAddr>,
    forwarded_for: Option<&str>,
) -> Option<SourceAddress> {
    let (_tmp, _state_owner, mut state) = test_state::build_test_state_with_bundles();
    let mut services = ServiceRegistry::new();
    services.register_raw("federation.capabilities", recording_handler);
    state.services = Arc::new(services);
    state.node_policy = Arc::new(
        (*state.node_policy)
            .clone()
            .with_test_record(Arc::new(policy)),
    );

    let raw = RawRouteSpec {
        id: "probe/source-address".to_string(),
        path: "/probe".to_string(),
        methods: ["GET".to_string()].into_iter().collect(),
        auth: "none".to_string(),
        auth_config: None,
        limits: RawLimits::default(),
        response: RawResponseSpec {
            mode: "json".to_string(),
            source: Some("service:federation/capabilities".to_string()),
            source_config: Value::Null,
            status: None,
            content_type: None,
            body_b64: None,
        },
        execute: None,
        request: RawRequest {
            body: RawRequestBody::None,
        },
        source_file: "/test/probe-source-address.yaml".into(),
        authority: None,
    };
    let modes = ResponseModeRegistry::with_builtins(Arc::new(state.engine.kinds.clone()));
    let table = ryeos_api::routes::build_route_table(&[raw], &modes).expect("compile probe route");
    let (mut composition, _) = ryeos_executor::SelectedInvocationCompositionBarrier::bootstrap();
    composition.arm_from_app_generation(&state).unwrap();
    let publication = composition.publication(Arc::new(state.clone())).unwrap();
    let api_state = ApiState {
        app: publication.app().clone(),
        route_table: Arc::new(table),
        publication: Some(publication),
        webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
    };

    let mut builder = Request::builder().method(Method::GET).uri("/probe");
    if let Some(value) = forwarded_for {
        builder = builder.header("x-forwarded-for", value);
    }
    let mut request = builder.body(Body::empty()).unwrap();
    if let Some(peer) = peer {
        request.extensions_mut().insert(ConnectInfo(peer));
    }

    *OBSERVED.lock().unwrap() = None;
    let response = route_dispatcher(State(api_state), request).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    OBSERVED
        .lock()
        .unwrap()
        .take()
        .expect("the route handler ran")
}

fn policy(trusted: &[&str]) -> ClientAddressPolicy {
    ClientAddressPolicy {
        schema: 1,
        trusted_proxies: trusted.iter().map(|cidr| (*cidr).to_string()).collect(),
        header: ClientAddressHeader::XForwardedFor,
    }
}

// One test function: the observation slot is process-global, so the cases
// run strictly in sequence.
#[tokio::test(flavor = "multi_thread")]
async fn dispatcher_resolves_the_source_address_from_peer_and_policy() {
    // An untrusted peer cannot choose its address with a forged header.
    let peer: SocketAddr = "192.0.2.50:4433".parse().unwrap();
    assert_eq!(
        observed_source_address(policy(&[]), Some(peer), Some("203.0.113.7")).await,
        Some(SourceAddress::Network("192.0.2.50".parse().unwrap())),
        "an untrusted peer's forwarding header must be ignored"
    );

    // A trusted proxy's header names the client: the right-most address that
    // is not itself a trusted proxy.
    let proxy: SocketAddr = "10.1.2.3:4433".parse().unwrap();
    assert_eq!(
        observed_source_address(
            policy(&["10.0.0.0/8"]),
            Some(proxy),
            Some("198.51.100.9, 10.1.2.3"),
        )
        .await,
        Some(SourceAddress::Network("198.51.100.9".parse().unwrap())),
    );

    // A request with no socket peer has no resolvable source: it stays
    // unresolved rather than becoming a local or network address.
    assert_eq!(
        observed_source_address(policy(&[]), None, Some("203.0.113.7")).await,
        None,
        "a request without a socket peer must stay unresolved"
    );
}
