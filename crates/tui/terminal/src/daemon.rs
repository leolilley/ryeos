//! Daemon client — signed HTTP/SSE client for ryeosd.
//!
//! Reuses transport layer from ryeos-cli.

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::Request;
use ryeos_cli::transport::discovery::discover_audience;
use ryeos_cli::transport::http::{post_json, read_daemon_bind, resolve_daemon_url};
use ryeos_cli::transport::signing::{SignHeaders, Signer};
use ryeos_tui_core::ids::RemoteId;
use ryeos_tui_core::update::{PollSnapshot, RemoteSummary, ThreadSummary};

use crate::sse::SseEvent;
use crate::transport::{DaemonRequest, DaemonResponse, TransportError};
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
    system_space_dir: PathBuf,
    base_url: String,
    audience: String,
    signer: Option<Signer>,
}

impl DaemonClient {
    /// Try to connect to the daemon.
    pub async fn try_connect() -> Result<Self, ClientError> {
        let system_space_dir = dirs::data_local_dir()
            .ok_or(ClientError::NoSystemDir)?
            .join("ryeos")
            .join(".ai");

        let base_url = resolve_daemon_url(&system_space_dir).await?;

        let signer = Signer::resolve(&system_space_dir).ok();

        let audience = if signer.is_some() {
            discover_audience(&base_url).await?
        } else {
            String::new()
        };

        Ok(Self {
            system_space_dir,
            base_url,
            audience,
            signer,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn has_identity(&self) -> bool {
        self.signer.is_some()
    }

    fn sign(&self, method: &str, path: &str, body: &[u8]) -> Result<SignHeaders, ClientError> {
        let signer = self.signer.as_ref().ok_or(ClientError::NoIdentity)?;
        signer
            .sign(method, path, body, &self.audience)
            .map_err(ClientError::Transport)
    }

    /// GET request with signed headers.
    async fn get_json(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let headers = self.sign("GET", path, b"")?;

        let uri: hyper::Uri = url
            .parse()
            .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                bind: url.clone(),
                detail: format!("invalid URL: {e}"),
            })?;

        let host = uri.host().unwrap_or("127.0.0.1");
        let port = uri.port_u16().unwrap_or(80);
        let bind = format!("{host}:{port}");

        let req = Request::builder()
            .method("GET")
            .uri(uri.to_string())
            .header("host", &bind)
            .header("accept", "application/json")
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .body(Full::new(Bytes::new()))
            .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: format!("failed to build request: {e}"),
            })?;

        let stream = tokio::net::TcpStream::connect(&bind)
            .await
            .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: e.to_string(),
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

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| ryeos_cli::error::CliTransportError::Unreachable {
                bind: bind.clone(),
                detail: format!("request send: {e}"),
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
        let result = post_json(&url, &headers, &body_bytes).await?;
        Ok(result)
    }

    /// Check if the daemon is reachable.
    pub async fn is_alive(&self) -> bool {
        self.get_json("/status").await.is_ok()
    }

    /// Fetch threads list.
    pub async fn get_threads(&self) -> Result<Vec<ThreadSummary>, ClientError> {
        let value = self.get_json("/threads").await?;
        Ok(parse_thread_summaries(&value))
    }

    /// Fetch status.
    pub async fn get_status(&self) -> Result<serde_json::Value, ClientError> {
        self.get_json("/status").await
    }

    /// Fetch remotes list.
    pub async fn get_remotes(&self) -> Result<Vec<RemoteSummary>, ClientError> {
        let value = self.get_json("/remote/list").await?;
        Ok(parse_remote_summaries(&value))
    }

    /// Execute an item via the daemon.
    pub async fn execute(
        &self,
        item_ref: &str,
        project_path: &str,
        parameters: &serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        self.signed_post(
            "/execute",
            &serde_json::json!({
                "item_id": item_ref,
                "project_path": project_path,
                "parameters": parameters,
            }),
        )
        .await
    }

    /// Fetch a full poll snapshot (threads + remotes + status).
    pub async fn poll_snapshot(&self) -> Result<PollSnapshot, ClientError> {
        let threads = self.get_threads().await.unwrap_or_default();
        let remotes = self.get_remotes().await.unwrap_or_default();
        let alive = self.is_alive().await;

        Ok(PollSnapshot {
            threads,
            remotes,
            daemon_url: Some(self.base_url.clone()),
            daemon_alive: alive,
        })
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn parse_thread_summaries(value: &serde_json::Value) -> Vec<ThreadSummary> {
    let threads = match value.as_array() {
        Some(arr) => arr,
        None => match value.get("threads").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return Vec::new(),
        },
    };

    threads
        .iter()
        .filter_map(|t| {
            Some(ThreadSummary {
                id: ryeos_tui_core::ids::ThreadId::new(
                    t.get("id")?.as_str()?.parse().ok()?,
                ),
                status: t
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                item_ref: t.get("item_id").and_then(|v| v.as_str()).map(String::from),
                parent_id: t
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .map(ryeos_tui_core::ids::ThreadId::new),
                started_at_ms: t.get("started_at").and_then(|v| v.as_i64()),
                duration_ms: t.get("duration_ms").and_then(|v| v.as_u64()),
                cost_usd: t.get("cost_usd").and_then(|v| v.as_f64()),
            })
        })
        .collect()
}

fn parse_remote_summaries(value: &serde_json::Value) -> Vec<RemoteSummary> {
    let remotes = match value.as_array() {
        Some(arr) => arr,
        None => match value.get("remotes").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return Vec::new(),
        },
    };

    remotes
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            Some(RemoteSummary {
                id: RemoteId::new(i as u64),
                name: r.get("name")?.as_str()?.to_string(),
                url: r.get("url")?.as_str()?.to_string(),
                alive: r.get("alive").and_then(|v| v.as_bool()).unwrap_or(false),
            })
        })
        .collect()
}

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
