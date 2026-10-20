//! The signed channel stream route through the real dispatcher on a real
//! socket: it compiles from its signed YAML, accepts no body, keeps the
//! connection's upgrade through the dispatcher, and refuses by name.

use ryeos_api::test_support as test_state;

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use ryeos_api::api_state::ApiState;
use ryeos_api::routes::dispatcher::route_dispatcher;
use ryeos_api::routes::response_modes::ResponseModeRegistry;
use ryeos_api::routes::webhook_dedupe::WebhookDedupeStore;
use ryeos_app::route_raw::{RawRequestBody, RawRouteSpec};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const ROUTE_YAML: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../bundles/core/.ai/node/routes/external-execution-channel-stream.yaml"
);

/// Send one raw request and read until `expected` appears or the peer stops.
async fn exchange(address: std::net::SocketAddr, request: &str, expected: &str) -> String {
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut received = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buffer)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(read)) => {
                received.extend_from_slice(&buffer[..read]);
                if String::from_utf8_lossy(&received).contains(expected) {
                    break;
                }
            }
            Ok(Err(error)) => panic!("dispatcher connection failed: {error}"),
        }
    }
    String::from_utf8_lossy(&received).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_stream_route_keeps_the_upgrade_through_the_dispatcher_and_refuses_by_name() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let raw: RawRouteSpec =
        serde_yaml::from_str(&std::fs::read_to_string(ROUTE_YAML).unwrap()).unwrap();
    assert_eq!(raw.auth, "none");
    assert_eq!(raw.limits.body_bytes_max, 0);
    assert_eq!(raw.request.body, RawRequestBody::None);
    assert_eq!(raw.response.mode, "channel_stream");

    let modes = ResponseModeRegistry::with_builtins(Arc::new(state.engine.kinds.clone()));
    let table = ryeos_api::routes::build_route_table(&[raw], &modes)
        .unwrap_or_else(|errors| panic!("signed stream route does not compile: {errors:?}"));
    let api_state = ApiState {
        app: Arc::new(state.clone()),
        route_table: Arc::new(ArcSwap::from_pointee(table)),
        publication: None,
        webhook_dedupe: Arc::new(WebhookDedupeStore::new()),
    };
    let app = axum::Router::new()
        .fallback(route_dispatcher)
        .with_state(api_state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    // A complete upgrade request reaches occurrence admission, which needs the
    // connection's upgrade handle: had the dispatcher dropped it, the refusal
    // would be `not_an_upgrade` instead. This node has no such channel.
    let upgrade = format!(
        "GET /external-execution/channel/T-none/occ-none/stream HTTP/1.1\r\n\
         Host: {address}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Protocol: {}\r\n\r\n",
        ryeos_state::external_execution::transport::EXTERNAL_CHANNEL_STREAM_CONTRACT
    );
    let response = exchange(address, &upgrade, "external_channel_stream_").await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(
        response.contains("external_channel_stream_no_live_channel"),
        "{response}"
    );

    // A plain GET of the same path is refused as no upgrade at all.
    let plain = format!(
        "GET /external-execution/channel/T-none/occ-none/stream HTTP/1.1\r\n\
         Host: {address}\r\n\
         Connection: close\r\n\r\n"
    );
    let response = exchange(address, &plain, "external_channel_stream_").await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(
        response.contains("external_channel_stream_not_an_upgrade"),
        "{response}"
    );

    // The route accepts no body.
    let with_body = format!(
        "GET /external-execution/channel/T-none/occ-none/stream HTTP/1.1\r\n\
         Host: {address}\r\n\
         Content-Length: 1\r\n\
         Connection: close\r\n\r\nx"
    );
    let response = exchange(address, &with_body, "\r\n\r\n").await;
    assert!(
        !response.starts_with("HTTP/1.1 101") && !response.starts_with("HTTP/1.1 2"),
        "{response}"
    );
    server.abort();
}
