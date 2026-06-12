//! Daemon client — signed HTTP/SSE client for ryeosd.
//!
//! Reuses transport layer from ryeos-cli.

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::Request;
use ryeos_cli::transport::discovery::discover_audience;
use ryeos_cli::transport::http::resolve_daemon_url;
use ryeos_cli::transport::signing::{SignHeaders, Signer};

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("no identity found — run ryeos init")]
    NoIdentity,

    #[error("no system directory found")]
    NoSystemDir,

    #[error("transport: {0}")]
    Transport(#[from] ryeos_cli::error::CliTransportError),

    #[error("dispatch: {0}")]
    Dispatch(#[from] ryeos_cli::error::CliDispatchError),

    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("daemon not running at {url}")]
    DaemonDown { url: String },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Daemon client
// ---------------------------------------------------------------------------

pub struct DaemonClient {
    #[allow(dead_code)]
    app_root: PathBuf,
    base_url: String,
    audience: String,
    signer: Option<Signer>,
    ui_session_id: Option<String>,
}

impl DaemonClient {
    /// Try to connect to the daemon.
    pub async fn try_connect() -> Result<Self, ClientError> {
        let app_root = std::env::var_os("RYEOS_APP_ROOT")
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("ryeos")))
            .ok_or(ClientError::NoSystemDir)?;

        let base_url = resolve_daemon_url(&app_root).await?;

        let signer = Signer::resolve(&app_root).ok();

        let audience = if signer.is_some() {
            discover_audience(&base_url).await?
        } else {
            String::new()
        };

        Ok(Self {
            app_root,
            base_url,
            audience,
            signer,
            ui_session_id: None,
        })
    }

    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[allow(dead_code)]
    pub fn has_identity(&self) -> bool {
        self.signer.is_some()
    }

    fn sign(&self, method: &str, path: &str, body: &[u8]) -> Result<SignHeaders, ClientError> {
        let signer = self.signer.as_ref().ok_or(ClientError::NoIdentity)?;
        signer
            .sign(method, path, body, &self.audience)
            .map_err(ClientError::Transport)
    }

    pub async fn mint_ui_session(
        &mut self,
        surface_ref: &str,
        project_path: Option<&str>,
        read_only: bool,
    ) -> Result<(), ClientError> {
        let body = serde_json::json!({
            "surface_ref": surface_ref,
            "project_path": project_path,
            "read_only": read_only,
        });
        let response = self.signed_post("/ui/api/launch/mint", &body).await?;
        let Some(session_id) = response.get("session_id").and_then(|value| value.as_str()) else {
            return Err(ClientError::DaemonDown {
                url: format!("{}/ui/api/launch/mint", self.base_url.trim_end_matches('/')),
            });
        };
        self.ui_session_id = Some(session_id.to_string());
        Ok(())
    }

    fn ui_cookie(&self, path: &str) -> Option<String> {
        path.starts_with("/ui/")
            .then(|| self.ui_session_id.as_ref())
            .flatten()
            .map(|session_id| format!("ryeos_session={session_id}"))
    }

    /// GET request with signed headers.
    pub async fn get_json(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let headers = self.sign("GET", path, b"")?;

        let uri: hyper::Uri =
            url.parse()
                .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                    bind: url.clone(),
                    detail: format!("invalid URL: {e}"),
                })?;

        let host = uri.host().unwrap_or("127.0.0.1");
        let port = uri.port_u16().unwrap_or(80);
        let bind = format!("{host}:{port}");

        let mut builder = Request::builder()
            .method("GET")
            .uri(uri.to_string())
            .header("host", &bind)
            .header("accept", "application/json")
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature);
        if let Some(cookie) = self.ui_cookie(path) {
            builder = builder.header("cookie", cookie);
        }
        let req = builder.body(Full::new(Bytes::new())).map_err(|e| {
            ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: format!("failed to build request: {e}"),
            }
        })?;

        let stream = tokio::net::TcpStream::connect(&bind).await.map_err(|e| {
            ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: e.to_string(),
            }
        })?;

        let io = hyper_util::rt::TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: format!("HTTP handshake: {e}"),
            })?;

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("daemon get connection error: {e}");
            }
        });

        let resp = sender.send_request(req).await.map_err(|e| {
            ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: format!("request send: {e}"),
            }
        })?;

        let status = resp.status();
        let body_bytes = collect_body(resp.into_body()).await?;

        if !status.is_success() {
            return Err(ClientError::DaemonDown { url });
        }

        let value: serde_json::Value = serde_json::from_slice(&body_bytes)?;
        Ok(value)
    }

    /// POST request with signed headers.
    pub async fn signed_post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let body_bytes = serde_json::to_vec(body)?;
        let headers = self.sign("POST", path, &body_bytes)?;
        let uri: hyper::Uri =
            url.parse()
                .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                    bind: url.clone(),
                    detail: format!("invalid URL: {e}"),
                })?;
        let host = uri.host().unwrap_or("127.0.0.1");
        let port = uri.port_u16().unwrap_or(80);
        let bind = format!("{host}:{port}");

        let mut builder = Request::builder()
            .method("POST")
            .uri(uri.to_string())
            .header("host", &bind)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature);
        if let Some(cookie) = self.ui_cookie(path) {
            builder = builder.header("cookie", cookie);
        }
        let req = builder
            .body(Full::new(Bytes::from(body_bytes)))
            .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: format!("failed to build request: {e}"),
            })?;

        let stream = tokio::net::TcpStream::connect(&bind).await.map_err(|e| {
            ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: e.to_string(),
            }
        })?;

        let io = hyper_util::rt::TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: format!("HTTP handshake: {e}"),
            })?;

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("daemon post connection error: {e}");
            }
        });

        let resp = sender.send_request(req).await.map_err(|e| {
            ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: format!("request send: {e}"),
            }
        })?;

        let status = resp.status();
        let body_bytes = collect_body(resp.into_body()).await?;
        if !status.is_success() {
            return Err(ClientError::DaemonDown { url });
        }
        Ok(serde_json::from_slice(&body_bytes)?)
    }

    /// Tail a turn's chain braid live over SSE
    /// (`GET /chains/{chain_root_id}/events/stream`). Each turn is its
    /// own chain root; the chain tail covers the turn AND its child
    /// threads (tools, composes). The conversation feed is the route
    /// ratchet concatenating per-turn tails. The braid is the truth;
    /// this stream is it arriving now.
    pub async fn open_thread_events(&self, thread_id: &str) -> Result<SseStream, ClientError> {
        let path = format!("/chains/{thread_id}/events/stream");
        self.open_sse(&path).await
    }

    /// Tail the UI session bus (transient hints — "look", never truth).
    pub async fn open_session_events(&self) -> Result<SseStream, ClientError> {
        let session_id = self.ui_session_id.clone().ok_or(ClientError::NoIdentity)?;
        let path = format!("/ui/events/session/{session_id}");
        self.open_sse(&path).await
    }

    async fn open_sse(&self, path: &str) -> Result<SseStream, ClientError> {
        let path = path.to_string();
        let path = &path;
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let headers = self.sign("GET", &path, b"")?;

        let uri: hyper::Uri = url.parse().map_err(|e| ClientError::DaemonDown {
            url: format!("invalid URL: {e}"),
        })?;
        let host = uri.host().unwrap_or("127.0.0.1");
        let port = uri.port_u16().unwrap_or(80);
        let bind = format!("{host}:{port}");

        let req = Request::builder()
            .method("GET")
            .uri(uri.to_string())
            .header("accept", "text/event-stream")
            .header("host", &bind)
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .body(Full::new(Bytes::new()))
            .map_err(|e| ClientError::DaemonDown {
                url: format!("build request: {e}"),
            })?;

        let stream = tokio::net::TcpStream::connect(&bind).await.map_err(|e| {
            ClientError::Dispatch(ryeos_cli::error::CliDispatchError::Transport(
                ryeos_cli::error::CliTransportError::Unreachable {
                    bind: bind.clone(),
                    detail: e.to_string(),
                },
            ))
        })?;

        let io = hyper_util::rt::TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| {
                ClientError::Dispatch(ryeos_cli::error::CliDispatchError::Transport(
                    ryeos_cli::error::CliTransportError::Unreachable {
                        bind: bind.clone(),
                        detail: format!("HTTP handshake: {e}"),
                    },
                ))
            })?;

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("SSE connection error: {e}");
            }
        });

        let resp = sender.send_request(req).await.map_err(|e| {
            ClientError::Dispatch(ryeos_cli::error::CliDispatchError::Transport(
                ryeos_cli::error::CliTransportError::Unreachable {
                    bind,
                    detail: format!("send request: {e}"),
                },
            ))
        })?;

        Ok(SseStream::new(resp.into_body()))
    }

    /// Execute an item via the daemon and return the SSE stream.
    /// The stream yields SSE events as they arrive from the daemon.
    #[allow(dead_code)]
    pub async fn execute_stream(
        &self,
        item_ref: &str,
        project_path: &str,
        parameters: &serde_json::Value,
    ) -> Result<SseStream, ClientError> {
        let body = serde_json::json!({
            "item_ref": item_ref,
            "project_path": project_path,
            "parameters": parameters,
            "stream": true,
        });
        let url = format!("{}/execute", self.base_url.trim_end_matches('/'));
        let body_bytes = serde_json::to_vec(&body)?;
        let headers = self.sign("POST", "/execute", &body_bytes)?;

        let uri: hyper::Uri = url.parse().map_err(|e| ClientError::DaemonDown {
            url: format!("invalid URL: {e}"),
        })?;
        let host = uri.host().unwrap_or("127.0.0.1");
        let port = uri.port_u16().unwrap_or(80);
        let bind = format!("{host}:{port}");

        let req = Request::builder()
            .method("POST")
            .uri(uri.to_string())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("host", &bind)
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .body(Full::new(Bytes::from(body_bytes)))
            .map_err(|e| ClientError::DaemonDown {
                url: format!("build request: {e}"),
            })?;

        let stream = tokio::net::TcpStream::connect(&bind).await.map_err(|e| {
            ClientError::Dispatch(ryeos_cli::error::CliDispatchError::Transport(
                ryeos_cli::error::CliTransportError::Unreachable {
                    bind: bind.clone(),
                    detail: e.to_string(),
                },
            ))
        })?;

        let io = hyper_util::rt::TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| {
                ClientError::Dispatch(ryeos_cli::error::CliDispatchError::Transport(
                    ryeos_cli::error::CliTransportError::Unreachable {
                        bind: bind.clone(),
                        detail: format!("HTTP handshake: {e}"),
                    },
                ))
            })?;

        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!("SSE connection error: {e}");
            }
        });

        let resp = sender.send_request(req).await.map_err(|e| {
            ClientError::Dispatch(ryeos_cli::error::CliDispatchError::Transport(
                ryeos_cli::error::CliTransportError::Unreachable {
                    bind,
                    detail: format!("send request: {e}"),
                },
            ))
        })?;

        Ok(SseStream::new(resp.into_body()))
    }

    /// Resolve an effective surface via the daemon's items.effective service.
    /// Resolve any effective item by canonical ref (kind-agnostic; the
    /// engine decides what the ref is).
    pub async fn resolve_effective_item(
        &self,
        canonical_ref: &str,
        expected_kind: &str,
        project_path: Option<&str>,
    ) -> Result<serde_json::Value, ClientError> {
        let mut params = serde_json::json!({
            "canonical_ref": canonical_ref,
            "expected_kind": expected_kind,
        });
        if let Some(pp) = project_path {
            params["project_path"] = serde_json::Value::String(pp.to_string());
        }
        let mut body = serde_json::json!({
            "item_ref": "service:items/effective",
            "parameters": params,
        });
        if let Some(pp) = project_path {
            body["project_path"] = serde_json::Value::String(pp.to_string());
        }
        let response = self.signed_post("/execute", &body).await?;
        Ok(response.get("result").cloned().unwrap_or(response))
    }

    pub async fn resolve_effective_surface(
        &self,
        canonical_ref: &str,
        project_path: Option<&str>,
    ) -> Result<serde_json::Value, ClientError> {
        let params = if let Some(pp) = project_path {
            serde_json::json!({
                "canonical_ref": canonical_ref,
                "expected_kind": "surface",
                "project_path": pp,
            })
        } else {
            serde_json::json!({
                "canonical_ref": canonical_ref,
                "expected_kind": "surface",
            })
        };
        let mut body = serde_json::json!({
            "item_ref": "service:items/effective",
            "parameters": params,
        });
        if let Some(pp) = project_path {
            body["project_path"] = serde_json::Value::String(pp.to_string());
        }
        let response = self.signed_post("/execute", &body).await?;
        Ok(response.get("result").cloned().unwrap_or(response))
    }
}

// ---------------------------------------------------------------------------
// SSE Stream wrapper
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub struct SseStream {
    body: hyper::body::Incoming,
    buffer: String,
    done: bool,
}

#[allow(dead_code)]
impl SseStream {
    pub fn new(body: hyper::body::Incoming) -> Self {
        Self {
            body,
            buffer: String::new(),
            done: false,
        }
    }

    /// Poll for the next SSE event. Returns None when the stream is done.
    pub async fn next_event(&mut self) -> Option<crate::sse::SseEvent> {
        if self.done {
            return None;
        }

        // Try to parse a complete event from the buffer
        if let Some(event) = self.try_parse_event() {
            return Some(event);
        }

        // Read more data
        use http_body_util::BodyExt;
        match self.body.frame().await {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    self.buffer.push_str(&String::from_utf8_lossy(data));
                }
                self.try_parse_event()
            }
            Some(Err(_)) | None => {
                self.done = true;
                None
            }
        }
    }

    fn try_parse_event(&mut self) -> Option<crate::sse::SseEvent> {
        // Look for double newline (event boundary)
        let event_end = self.buffer.find("\n\n")?;
        let event_text = self.buffer[..event_end].to_string();
        self.buffer = self.buffer[event_end + 2..].to_string();

        let mut event_type = "message";
        let mut data = String::new();

        for line in event_text.lines() {
            if let Some(t) = line.strip_prefix("event: ") {
                event_type = t.trim();
            } else if let Some(d) = line.strip_prefix("data: ") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.trim());
            }
        }

        if data.is_empty() {
            return None;
        }

        Some(crate::sse::SseEvent::parse(event_type, &data))
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

async fn collect_body(body: Incoming) -> Result<Vec<u8>, ryeos_cli::error::CliTransportError> {
    let mut bufs = Vec::new();
    let mut body = body;
    while let Some(chunk) = body.frame().await {
        let frame = chunk.map_err(|e| ryeos_cli::error::CliTransportError::BodyDecode {
            detail: format!("stream frame: {e}"),
        })?;
        if let Some(data) = frame.data_ref() {
            bufs.extend_from_slice(data);
        }
    }
    Ok(bufs)
}
