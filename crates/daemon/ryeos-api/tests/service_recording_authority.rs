mod test_state;

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request};
use ryeos_api::api_state::ApiState;
use ryeos_api::routes::dispatcher::route_dispatcher;
use ryeos_api::routes::invocation::{
    CompiledRouteInvocation, RouteInvocationContext, RouteInvocationResult, RoutePrincipal,
};
use ryeos_api::routes::invokers::service_invocation::CompiledServiceInvocation;
use ryeos_api::routes::response_modes::ResponseModeRegistry;
use ryeos_api::routes::webhook_dedupe::WebhookDedupeStore;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::route_raw::{RawLimits, RawRequest, RawRequestBody, RawResponseSpec, RawRouteSpec};
use ryeos_app::service_registry::ServiceRegistry;
use ryeos_app::state::AppState;
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
};
use ryeos_executor::executor::{
    execute_service, ExecutionContext, ExecutionMode, ServiceRecordingAuthoritySource,
    ServiceRecordingContext,
};
use serde_json::{json, Value};

static HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

fn counting_handler(
    _params: Value,
    _context: HandlerContext,
    _state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    Box::pin(async { Ok(json!({"unexpected": true})) })
}

fn route_handler(
    _params: Value,
    _context: HandlerContext,
    _state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
    Box::pin(async { Ok(json!({"route": "ok"})) })
}

fn failing_route_handler(
    _params: Value,
    _context: HandlerContext,
    _state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
    Box::pin(async {
        Err(anyhow::Error::new(HandlerError::Conflict(
            "authored route conflict".to_string(),
        )))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn unrecorded_only_rejects_a_recorded_service_before_handler_effects() {
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let (_tmp, mut state) = test_state::build_test_state_with_bundles();
    let mut services = ServiceRegistry::new();
    services.register_raw("identity.public_key", counting_handler);
    state.services = Arc::new(services);

    let principal = Principal {
        fingerprint: "fp:service-recording-test".to_string(),
        scopes: Vec::new(),
    };
    let execution = ExecutionContext {
        principal_fingerprint: principal.fingerprint.clone(),
        caller_scopes: principal.scopes.clone(),
        engine: state.engine.clone(),
        plan_ctx: PlanContext {
            requested_by: EffectivePrincipal::Local(principal),
            project_context: ProjectContext::None,
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        },
        requested_call: None,
    };

    let error = match execute_service(
        "service:identity/public_key",
        json!({}),
        ExecutionMode::Live,
        &execution,
        &state,
        ServiceRecordingContext {
            authority_source: ServiceRecordingAuthoritySource::UnrecordedOnly,
            usage_subject: None,
            usage_subject_asserted_by: None,
        },
    )
    .await
    {
        Ok(_) => panic!("recording-required service accepted an unrecorded-only caller"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("unrecorded-only"));
    assert_eq!(
        HANDLER_CALLS.load(Ordering::SeqCst),
        0,
        "recording authority rejection must happen before handler dispatch"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn compiled_recorded_route_returns_thread_identity_and_persists_attribution() {
    let (_tmp, mut state) = test_state::build_test_state_with_bundles();
    let mut services = ServiceRegistry::new();
    services.register_raw("identity.public_key", route_handler);
    state.services = Arc::new(services);

    let invoker = CompiledServiceInvocation {
        service_ref: "service:identity/public_key".to_string(),
        endpoint: "identity.public_key".to_string(),
    };
    let principal = RoutePrincipal {
        id: "route:identity/public-key".to_string(),
        scopes: Vec::new(),
        verifier_key: "none",
        verified: false,
        authenticated_origin_site_id: None,
        metadata: Default::default(),
    };
    let result = invoker
        .invoke(RouteInvocationContext {
            route_id: "identity/public-key".into(),
            method: Method::GET,
            uri: "/identity/public-key".parse().unwrap(),
            captures: Default::default(),
            headers: HeaderMap::new(),
            body_raw: Vec::new(),
            input: json!({}),
            principal: Some(principal),
            workspace_lifeline: None,
            launch_timings: None,
            state: state.clone(),
            webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
        })
        .await
        .unwrap();
    let RouteInvocationResult::Json { value, thread_id } = result else {
        panic!("compiled service route did not return JSON");
    };
    assert_eq!(value, json!({"route": "ok"}));
    let thread_id = thread_id.expect("recorded route must return its durable thread id");

    let thread = state
        .state_store
        .get_thread(&thread_id)
        .unwrap()
        .expect("recorded route thread");
    assert_eq!(thread.status, "completed");
    let created = state
        .state_store
        .replay_events(&thread_id, Some(&thread_id), None, 16, 1024 * 1024)
        .unwrap()
        .events
        .into_iter()
        .find(|event| event.event_type == ryeos_state::event_types::THREAD_CREATED)
        .expect("recorded route created event");
    assert_eq!(
        created.payload["usage_subject"],
        json!({"namespace": "route", "subject": "identity/public-key"})
    );
    assert_eq!(
        created.payload["usage_subject_asserted_by"],
        "route:identity/public-key"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_route_http_response_exposes_the_persisted_thread_identity() {
    let (_tmp, mut state) = test_state::build_test_state_with_bundles();
    let mut services = ServiceRegistry::new();
    services.register_raw("identity.public_key", route_handler);
    state.services = Arc::new(services);

    let raw = RawRouteSpec {
        id: "identity/public-key".to_string(),
        path: "/public-key".to_string(),
        methods: ["GET".to_string()].into_iter().collect(),
        auth: "none".to_string(),
        auth_config: None,
        limits: RawLimits::default(),
        response: RawResponseSpec {
            mode: "json".to_string(),
            source: Some("service:identity/public_key".to_string()),
            source_config: Value::Null,
            status: None,
            content_type: None,
            body_b64: None,
        },
        execute: None,
        request: RawRequest {
            body: RawRequestBody::None,
        },
        source_file: "/test/identity-public-key.yaml".into(),
    };
    let table =
        ryeos_api::routes::build_route_table(&[raw], &ResponseModeRegistry::with_builtins())
            .expect("compile recorded public route");
    let api_state = ApiState {
        app: Arc::new(state.clone()),
        route_table: Arc::new(ArcSwap::from_pointee(table)),
        webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
    };
    let response = route_dispatcher(
        State(api_state),
        Request::builder()
            .method(Method::GET)
            .uri("/public-key")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let thread_id = response
        .headers()
        .get("x-ryeos-thread-id")
        .expect("recorded HTTP route thread header")
        .to_str()
        .unwrap()
        .to_string();
    let thread = state
        .state_store
        .get_thread(&thread_id)
        .unwrap()
        .expect("header references a durable thread");
    assert_eq!(thread.status, "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_recorded_route_http_response_exposes_the_failed_thread_identity() {
    let (_tmp, mut state) = test_state::build_test_state_with_bundles();
    let mut services = ServiceRegistry::new();
    services.register_raw("identity.public_key", failing_route_handler);
    state.services = Arc::new(services);

    let raw = RawRouteSpec {
        id: "identity/public-key-failure".to_string(),
        path: "/public-key-failure".to_string(),
        methods: ["GET".to_string()].into_iter().collect(),
        auth: "none".to_string(),
        auth_config: None,
        limits: RawLimits::default(),
        response: RawResponseSpec {
            mode: "json".to_string(),
            source: Some("service:identity/public_key".to_string()),
            source_config: Value::Null,
            status: None,
            content_type: None,
            body_b64: None,
        },
        execute: None,
        request: RawRequest {
            body: RawRequestBody::None,
        },
        source_file: "/test/identity-public-key-failure.yaml".into(),
    };
    let table =
        ryeos_api::routes::build_route_table(&[raw], &ResponseModeRegistry::with_builtins())
            .expect("compile failing recorded public route");
    let api_state = ApiState {
        app: Arc::new(state.clone()),
        route_table: Arc::new(ArcSwap::from_pointee(table)),
        webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
    };
    let response = route_dispatcher(
        State(api_state),
        Request::builder()
            .method(Method::GET)
            .uri("/public-key-failure")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    let thread_id = response
        .headers()
        .get("x-ryeos-thread-id")
        .expect("failed recorded route thread header")
        .to_str()
        .unwrap()
        .to_string();
    let thread = state
        .state_store
        .get_thread(&thread_id)
        .unwrap()
        .expect("failure header references a durable thread");
    assert_eq!(thread.status, "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_cap_protected_route_is_unauthorized_before_service_execution() {
    let (_tmp, state) = test_state::build_test_state_with_bundles();
    let invoker = CompiledServiceInvocation {
        service_ref: "service:scheduler/pause".to_string(),
        endpoint: "scheduler.pause".to_string(),
    };
    let error = match invoker
        .invoke(RouteInvocationContext {
            route_id: "scheduler/pause".into(),
            method: Method::POST,
            uri: "/scheduler/pause".parse().unwrap(),
            captures: Default::default(),
            headers: HeaderMap::new(),
            body_raw: Vec::new(),
            input: json!({"schedule_id": "daily"}),
            principal: Some(RoutePrincipal {
                id: "route:scheduler/pause".to_string(),
                scopes: Vec::new(),
                verifier_key: "none",
                verified: false,
                authenticated_origin_site_id: None,
                metadata: Default::default(),
            }),
            workspace_lifeline: None,
            launch_timings: None,
            state,
            webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
        })
        .await
    {
        Ok(_) => panic!("anonymous cap-protected route accepted without authentication"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ryeos_api::route_error::RouteDispatchError::Unauthorized
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_route_cap_denial_is_forbidden_without_a_durable_thread() {
    let (_tmp, state) = test_state::build_test_state_with_bundles();
    let invoker = CompiledServiceInvocation {
        service_ref: "service:scheduler/pause".to_string(),
        endpoint: "scheduler.pause".to_string(),
    };
    let error = match invoker
        .invoke(RouteInvocationContext {
            route_id: "scheduler/pause".into(),
            method: Method::POST,
            uri: "/scheduler/pause".parse().unwrap(),
            captures: Default::default(),
            headers: HeaderMap::new(),
            body_raw: Vec::new(),
            input: json!({"schedule_id": "daily"}),
            principal: Some(RoutePrincipal {
                id: "route:test-denied".to_string(),
                scopes: Vec::new(),
                verifier_key: "ryeos_signed",
                verified: true,
                authenticated_origin_site_id: None,
                metadata: Default::default(),
            }),
            workspace_lifeline: None,
            launch_timings: None,
            state,
            webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
        })
        .await
    {
        Ok(_) => panic!("authenticated caller without the authored cap was accepted"),
        Err(error) => error,
    };
    match error {
        ryeos_api::route_error::RouteDispatchError::Structured {
            status, thread_id, ..
        } => {
            assert_eq!(status, axum::http::StatusCode::FORBIDDEN.as_u16());
            assert!(
                thread_id.is_none(),
                "pre-admission cap denial must not claim a durable audit root"
            );
        }
        other => panic!("expected structured cap denial, got {other}"),
    }
}
