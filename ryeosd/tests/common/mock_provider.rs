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
//! configure `model_providers/mock.yaml` with `auth: {}` so the
//! adapter at `adapter.rs:38-43` skips its `Authorization` header.
//! See V5.4-PLAN P3b.1 commentary for the env-injection gap that
//! makes a real-provider test impossible to write without changing
//! the daemon's runtime spawn env.
//!
//! NOTE: this module is `#[allow(dead_code)]` because not every
//! integration test bin in this crate uses it. Mirrors the pattern in
//! `common/mod.rs`.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
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

/// A live mock provider. Drop signals shutdown to the server task.
pub struct MockProvider {
    /// Base URL clients should POST `/chat/completions` to. NB the
    /// adapter at `adapter.rs:24` appends `/chat/completions`, so
    /// `base_url` itself is the bare `http://127.0.0.1:<port>`.
    pub base_url: String,
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

        let state = Arc::new(Mutex::new(canned));

        let app = Router::new()
            .route("/chat/completions", post(handle_chat_completions))
            .with_state(state);

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
            shutdown_tx: Some(shutdown_tx),
            join: Some(join),
        }
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

async fn handle_chat_completions(
    State(state): State<Arc<Mutex<Vec<MockResponse>>>>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut queue = state.lock().await;
    if queue.is_empty() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "mock_provider: canned response queue exhausted".to_string(),
        ));
    }
    let next = queue.remove(0);
    let choice = next.into_choice();
    let resp = json!({
        "choices": [choice],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
        }
    });
    Ok(Json(resp))
}
