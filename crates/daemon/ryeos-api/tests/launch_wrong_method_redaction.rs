//! A wrong-method request to a launch URL is refused without echoing the
//! one-time token in the response body or headers.

use ryeos_api::test_support as test_state;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use ryeos_api::api_state::ApiState;
use ryeos_api::routes::dispatcher::route_dispatcher;
use ryeos_api::routes::response_modes::ResponseModeRegistry;
use ryeos_api::routes::webhook_dedupe::WebhookDedupeStore;
use ryeos_app::route_raw::RawRouteSpec;

const ROUTE_YAML: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../bundles/ryeos-ui/.ai/node/routes/ui/launch/launch.yaml"
);

const TOKEN: &str = "one-time-secret-token-9f3a";

#[tokio::test(flavor = "multi_thread")]
async fn wrong_method_on_a_launch_url_does_not_echo_the_token() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_bundles();
    let mut raw: RawRouteSpec =
        serde_yaml::from_str(&std::fs::read_to_string(ROUTE_YAML).unwrap()).unwrap();
    // The shipped launch route keeps its own path and methods. Its browser
    // launch mode and service belong to the UI crate, which this crate cannot
    // compose; a wrong method is answered by the dispatcher before any mode or
    // service runs, so a JSON service this crate holds stands in for them.
    raw.response.mode = "json".to_owned();
    raw.response.source = Some("service:admission/claim".to_owned());
    raw.response.source_config = serde_json::Value::Null;
    let modes = ResponseModeRegistry::with_builtins(Arc::new(state.engine.kinds.clone()));
    let table = ryeos_api::routes::build_route_table(&[raw], &modes)
        .unwrap_or_else(|errors| panic!("launch route does not compile: {errors:?}"));
    let api_state = ApiState {
        app: Arc::new(state.clone()),
        route_table: Arc::new(table),
        publication: None,
        webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
    };

    for method in [Method::POST, Method::DELETE] {
        let response = route_dispatcher(
            State(api_state.clone()),
            Request::builder()
                .method(method)
                .uri(format!("/ui/launch/{TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let headers = format!("{:?}", response.headers());
        assert!(!headers.contains(TOKEN), "{headers}");
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(!body.contains(TOKEN), "{body}");
        assert!(body.contains("method not allowed"), "{body}");
    }
}
