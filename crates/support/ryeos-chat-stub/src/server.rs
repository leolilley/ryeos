//! The HTTP server: records each request, pops the next script step and
//! plays it.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::render::{non_stream_body, stream_frames, violated_requirement};
use crate::script::{Script, Step};

/// One request the stub received.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedRequest {
    /// Zero-based arrival ordinal.
    pub seq: u64,
    pub received_ms: u128,
    pub path: String,
    pub headers: HashMap<String, String>,
    /// The parsed JSON body, or the raw text when it was not JSON.
    pub body: Value,
    /// Why the stub answered 400, when the step's `require` was violated.
    pub rejected: Option<String>,
}

struct Inner {
    steps: Vec<Step>,
    step_idx: usize,
    served_of_step: u32,
    requests: Vec<RecordedRequest>,
    record_file: Option<PathBuf>,
}

impl Inner {
    fn next_step(&mut self) -> Option<Step> {
        loop {
            let step = self.steps.get(self.step_idx)?;
            if self.served_of_step < step.repeat {
                self.served_of_step += 1;
                return Some(step.clone());
            }
            self.step_idx += 1;
            self.served_of_step = 0;
        }
    }
}

type Shared = Arc<Mutex<Inner>>;

/// A running stub. Dropping it stops the server.
pub struct StubServer {
    /// `http://127.0.0.1:<port>`; clients append `/chat/completions`
    /// (a `/v1/chat/completions` route is served too).
    pub base_url: String,
    state: Shared,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl StubServer {
    /// Serve `script` on an ephemeral loopback port.
    pub async fn start(script: Script) -> Self {
        Self::bind("127.0.0.1:0".parse().expect("loopback"), script, None)
            .await
            .expect("stub bind")
    }

    /// Serve `script` on `addr`, optionally appending every request as one
    /// JSON line to `record_file`.
    pub async fn bind(
        addr: SocketAddr,
        script: Script,
        record_file: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let state: Shared = Arc::new(Mutex::new(Inner {
            steps: script.steps,
            step_idx: 0,
            served_of_step: 0,
            requests: Vec::new(),
            record_file,
        }));
        let app = Router::new()
            .route("/chat/completions", post(handle))
            .route("/v1/chat/completions", post(handle))
            .route("/_stub/requests", get(list_requests))
            .route("/_stub/health", get(|| async { "ok" }))
            // A request that carries images is megabytes; axum's default of
            // 2 MiB would refuse it before the stub could record it.
            .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        Ok(Self {
            base_url,
            state,
            shutdown_tx: Some(shutdown_tx),
            join: Some(join),
        })
    }

    /// Every request received so far, in arrival order.
    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().await.requests.clone()
    }

    /// Just the request bodies, in arrival order.
    pub async fn request_bodies(&self) -> Vec<Value> {
        self.requests().await.into_iter().map(|r| r.body).collect()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

async fn list_requests(State(state): State<Shared>) -> Json<Vec<RecordedRequest>> {
    Json(state.lock().await.requests.clone())
}

fn flatten_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in headers {
        out.entry(k.as_str().to_ascii_lowercase())
            .or_insert_with(|| v.to_str().unwrap_or("").to_string());
    }
    out
}

async fn handle(
    State(state): State<Shared>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&raw)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into_owned()));
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let (seq, step, rejection) = {
        let mut g = state.lock().await;
        let seq = g.requests.len() as u64;
        let mut record = RecordedRequest {
            seq,
            received_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
            path: uri.path().to_string(),
            headers: flatten_headers(&headers),
            body,
            rejected: None,
        };
        let step = g.next_step();
        let rejection = step
            .as_ref()
            .and_then(|step| step.require.as_ref())
            .and_then(|require| violated_requirement(require, &record.body));
        record.rejected = rejection.clone();
        if let Some(path) = &g.record_file
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            && let Ok(line) = serde_json::to_string(&record)
        {
            let _ = writeln!(file, "{line}");
        }
        g.requests.push(record);
        (seq, step, rejection)
    };
    let Some(step) = step else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "chat stub: script exhausted".to_string(),
        )
            .into_response();
    };
    if let Some(reason) = rejection {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": {"message": reason, "type": "invalid_request_error"}}),
            ),
        )
            .into_response();
    }
    if step.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(step.delay_ms)).await;
    }
    if let Some(fail) = &step.fail {
        let status = StatusCode::from_u16(fail.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, fail.body.clone()).into_response();
        if let Some(retry_after) = &fail.retry_after
            && let Ok(value) = axum::http::HeaderValue::from_str(retry_after)
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        return response;
    }
    if !streaming {
        return Json(non_stream_body(&step, seq)).into_response();
    }
    sse_response(step, seq)
}

/// Pause before a scripted cut so the events sent so far reach the peer;
/// an abort that follows a write immediately can discard the unsent buffer.
const CUT_FLUSH_GRACE: Duration = Duration::from_millis(50);

fn sse_response(step: Step, n: u64) -> Response {
    let frames = stream_frames(&step, n);
    let stream = async_stream::stream! {
        let stall = step.stall.clone();
        let mut sent = 0usize;
        for frame in frames {
            if let Some(s) = &stall
                && s.after_chunks == sent
            {
                tokio::time::sleep(Duration::from_millis(s.ms)).await;
            }
            if step.cut_after_chunks == Some(sent) {
                tokio::time::sleep(CUT_FLUSH_GRACE).await;
                yield Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "scripted mid-stream cut",
                ));
                return;
            }
            yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {frame}\n\n")));
            sent += 1;
        }
        if step.cut_after_chunks == Some(sent) {
            tokio::time::sleep(CUT_FLUSH_GRACE).await;
            yield Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "scripted mid-stream cut",
            ));
            return;
        }
        if step.stream.done_marker {
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .expect("build sse response")
}
