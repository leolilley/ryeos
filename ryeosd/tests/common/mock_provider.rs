//! Mock LLM provider for V5.4 P3b directive-runtime e2e tests.
//!
//! Spins a tiny axum HTTP server on an ephemeral port that mimics the
//! OpenAI Chat Completions surface that
//! `ryeos-directive-runtime/src/adapter.rs::call_provider` POSTs to
//! (`<base_url>/chat/completions`).
//!
//! The server is fed a `Vec<MockResponse>` at start time and pops one
//! response per incoming request (sequential, FIFO). When the canned
//! list is exhausted, additional requests get HTTP 500 — useful for
//! pinning that a test-under-test didn't loop more times than expected.
//!
//! The OpenAI shape we emit (only fields the adapter actually reads —
//! see `parse_response` in `adapter.rs`):
//!
//! ```json
//! {
//!   "choices": [{
//!     "message": {
//!       "role": "assistant",
//!       "content": "...",
//!       "tool_calls": [...]
//!     },
//!     "finish_reason": "stop" | "tool_calls"
//!   }],
//!   "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
//! }
//! ```
//!
//! Auth is disabled — the harness expects directives under test to
//! configure `model-providers/mock.yaml` with `auth: {}` so the
//! adapter at `adapter.rs:38-43` skips its `Authorization` header.
//! See V5.4-PLAN P3b.1 commentary for the env-injection gap that
//! makes a real-provider test impossible to write without changing
//! the daemon's runtime spawn env.
//!
//! NOTE: this module is `#[allow(dead_code)]` because not every
//! integration test bin in this crate uses it. Mirrors the pattern in
//! `common/mod.rs`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::common::pick_free_port;

/// One canned LLM response. The mock pops these FIFO.
#[derive(Debug, Clone)]
pub enum MockResponse {
    /// Plain assistant text — `finish_reason = "stop"`.
    Text(String),
    /// Tool-call response — `finish_reason = "tool_calls"`. `arguments`
    /// is the raw JSON string the adapter forwards into
    /// `parse_tool_arguments` (see `adapter.rs:166`).
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
}

impl MockResponse {
    fn into_choice(self) -> Value {
        match self {
            MockResponse::Text(text) => json!({
                "message": {
                    "role": "assistant",
                    "content": text,
                },
                "finish_reason": "stop",
            }),
            MockResponse::ToolCall { id, name, arguments } => json!({
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }]
                },
                "finish_reason": "tool_calls",
            }),
        }
    }
}

/// Shared mock-provider state. The queue is popped FIFO per request;
/// each request's headers are appended to `captured_headers` so a
/// test can assert that env-var-injected secrets reach the wire.
struct MockState {
    queue: Vec<MockResponse>,
    captured_headers: Vec<HashMap<String, String>>,
}

/// A live mock provider. Drop signals shutdown to the server task.
pub struct MockProvider {
    /// Base URL clients should POST `/chat/completions` to. NB the
    /// adapter at `adapter.rs:24` appends `/chat/completions`, so
    /// `base_url` itself is the bare `http://127.0.0.1:<port>`.
    pub base_url: String,
    state: Arc<Mutex<MockState>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl MockProvider {
    /// Spawn a mock provider with the given canned responses (popped
    /// FIFO). Returns once the server is bound and listening.
    pub async fn start(canned: Vec<MockResponse>) -> Self {
        let port = pick_free_port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let base_url = format!("http://127.0.0.1:{port}");

        let state = Arc::new(Mutex::new(MockState {
            queue: canned,
            captured_headers: Vec::new(),
        }));

        let app = Router::new()
            .route("/chat/completions", post(handle_chat_completions))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .expect("mock provider bind 127.0.0.1");

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            let server = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                });
            let _ = server.await;
        });

        Self {
            base_url,
            state,
            shutdown_tx: Some(shutdown_tx),
            join: Some(join),
        }
    }

    /// Snapshot of all request-header maps observed since startup,
    /// in arrival order. Header names are lowercased and values are
    /// taken from the first occurrence (axum's `HeaderMap`
    /// preserves order; this helper flattens to `String -> String`).
    pub async fn captured_headers(&self) -> Vec<HashMap<String, String>> {
        self.state.lock().await.captured_headers.clone()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

fn flatten_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for (k, v) in headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        let value = v.to_str().unwrap_or("").to_string();
        out.entry(name).or_insert(value);
    }
    out
}

async fn handle_chat_completions(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut g = state.lock().await;
    g.captured_headers.push(flatten_headers(&headers));
    if g.queue.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "mock_provider: canned response queue exhausted".to_string(),
        )
            .into_response();
    }
    let next = g.queue.remove(0);
    let streaming = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if streaming {
        // OpenAI-compatible SSE chunked response. The directive
        // runtime defaults to `delta_merge` mode (see
        // streaming.rs::stream_mode), which expects
        // `choices[0].delta.{content,tool_calls}` per chunk and
        // optional `usage` on the final chunk. The runtime's tool-use
        // accumulator threads tool-call args across chunks via index;
        // for the mock we emit the whole tool-call in a single chunk
        // (id + name + arguments together) to keep the harness
        // minimal.
        sse_response(next).into_response()
    } else {
        let choice = next.into_choice();
        let resp = json!({
            "choices": [choice],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
            }
        });
        Json(resp).into_response()
    }
}

/// Build an OpenAI-style streaming SSE response from a single
/// canned mock response. Two `data:` events: the assistant content
/// (or tool call) chunk, then the final chunk carrying `usage` +
/// `finish_reason`. Closes with `data: [DONE]`.
fn sse_response(resp: MockResponse) -> Response {
    let (delta_payload, finish_reason) = match resp {
        MockResponse::Text(text) => (
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": text,
                    },
                    "finish_reason": null,
                }]
            }),
            "stop",
        ),
        MockResponse::ToolCall { id, name, arguments } => (
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "tool_calls": [{
                            "index": 0,
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments,
                            }
                        }]
                    },
                    "finish_reason": null,
                }]
            }),
            "tool_calls",
        ),
    };
    let final_payload = json!({
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
        }
    });
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        delta_payload, final_payload
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(body))
        .expect("build sse response")
}
