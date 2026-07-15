//! Daemon client — signed HTTP/SSE client for ryeosd.
//!
//! Reuses the transport layer from ryeos-cli: discovery, signing, and the
//! shared TLS-capable (`reqwest`) signed client. Both the CLI and this client
//! speak one transport, so an `https://` node negotiates TLS and an http→https
//! edge redirect is handled at discovery — never on a signed request.

use ryeos_cli::error::CliTransportError;
use ryeos_cli::transport::discovery::discover_audience;
use ryeos_cli::transport::http::{resolve_daemon_url, signed_client};
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

    /// The daemon answered but with a non-2xx status. Carries the
    /// daemon's own error message so callers don't misreport a
    /// composition/validation failure as "daemon down".
    #[error("daemon returned {status} for {path}: {message}")]
    DaemonError {
        path: String,
        status: u16,
        message: String,
    },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Pull the daemon's `{code, error}` envelope into a readable line,
/// falling back to the raw body.
fn daemon_error_message(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|e| e.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
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
    /// One pooled connection for the client's lifetime. Every signed call
    /// rides this handle so requests reuse the kept-alive TCP/TLS session
    /// instead of paying a fresh connect per round trip.
    http: reqwest::Client,
}

impl DaemonClient {
    /// Try to connect to the daemon.
    pub async fn try_connect() -> Result<Self, ClientError> {
        let app_root = std::env::var_os("RYEOS_APP_ROOT")
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("ryeos")))
            .ok_or(ClientError::NoSystemDir)?;

        let mut base_url = resolve_daemon_url(&app_root).await?;
        let signer = Signer::resolve(&app_root).ok();

        // Discovery is an unsigned GET that may follow an http→https redirect;
        // it reports the EFFECTIVE base the daemon answered on. Subsequent
        // SIGNED requests target that base directly (a signed request must
        // never rely on a redirect — see `signed_client`).
        let audience = if signer.is_some() {
            let discovered = discover_audience(&base_url).await?;
            base_url = discovered.effective_base_url;
            discovered.principal_id
        } else {
            String::new()
        };

        Ok(Self {
            app_root,
            base_url,
            audience,
            signer,
            ui_session_id: None,
            http: signed_client()?,
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
            .then_some(self.ui_session_id.as_ref())
            .flatten()
            .map(|session_id| format!("ryeos_session={session_id}"))
    }

    /// GET request with signed headers.
    pub async fn get_json(&self, path: &str) -> Result<serde_json::Value, ClientError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let headers = self.sign("GET", path, b"")?;

        let mut req = self
            .http
            .get(&url)
            .header("accept", "application/json")
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature);
        if let Some(cookie) = self.ui_cookie(path) {
            req = req.header("cookie", cookie);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| CliTransportError::Unreachable {
                bind: url.clone(),
                detail: format!("request send: {e}"),
            })?;

        let status = resp.status();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| CliTransportError::BodyDecode {
                detail: format!("read body: {e}"),
            })?;

        if !status.is_success() {
            return Err(ClientError::DaemonError {
                path: path.to_string(),
                status: status.as_u16(),
                message: daemon_error_message(&body_bytes),
            });
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

        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .body(body_bytes);
        if let Some(cookie) = self.ui_cookie(path) {
            req = req.header("cookie", cookie);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| CliTransportError::Unreachable {
                bind: url.clone(),
                detail: format!("request send: {e}"),
            })?;

        let status = resp.status();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| CliTransportError::BodyDecode {
                detail: format!("read body: {e}"),
            })?;
        if !status.is_success() {
            return Err(ClientError::DaemonError {
                path: path.to_string(),
                status: status.as_u16(),
                message: daemon_error_message(&body_bytes),
            });
        }
        Ok(serde_json::from_slice(&body_bytes)?)
    }

    /// Tail a turn's chain braid live over SSE
    /// (`GET /chains/{chain_root_id}/events/stream`). Each turn is its
    /// own chain root; the chain tail covers the turn AND its child
    /// threads (tools, composes). The conversation feed is the route
    /// ratchet concatenating per-turn tails. The braid is the truth;
    /// this stream is it arriving now.
    pub async fn open_thread_events(&self, chain_root_id: &str) -> Result<SseStream, ClientError> {
        let path = format!("/chains/{chain_root_id}/events/stream");
        self.open_sse(&path).await
    }

    /// Tail the UI session bus (transient hints — "look", never truth).
    pub async fn open_session_events(&self) -> Result<SseStream, ClientError> {
        let session_id = self.ui_session_id.clone().ok_or(ClientError::NoIdentity)?;
        let path = format!("/ui/events/session/{session_id}");
        self.open_sse(&path).await
    }

    async fn open_sse(&self, path: &str) -> Result<SseStream, ClientError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let headers = self.sign("GET", path, b"")?;

        let mut req = self
            .http
            .get(&url)
            .header("accept", "text/event-stream")
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature);
        if let Some(cookie) = self.ui_cookie(path) {
            req = req.header("cookie", cookie);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| CliTransportError::Unreachable {
                bind: url.clone(),
                detail: format!("request send: {e}"),
            })?;

        Ok(SseStream::new(resp))
    }

    /// Execute an item via the daemon and return the SSE stream.
    /// The stream yields SSE events as they arrive from the daemon.
    #[allow(dead_code)]
    pub async fn execute_stream(
        &self,
        item_ref: &str,
        ref_bindings: &std::collections::BTreeMap<String, String>,
        project_path: &str,
        parameters: &serde_json::Value,
    ) -> Result<SseStream, ClientError> {
        let body = serde_json::json!({
            "item_ref": item_ref,
            "ref_bindings": ref_bindings,
            "project_path": project_path,
            "parameters": parameters,
            "stream": true,
        });
        let url = format!("{}/execute", self.base_url.trim_end_matches('/'));
        let body_bytes = serde_json::to_vec(&body)?;
        let headers = self.sign("POST", "/execute", &body_bytes)?;

        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| CliTransportError::Unreachable {
                bind: url.clone(),
                detail: format!("request send: {e}"),
            })?;

        Ok(SseStream::new(resp))
    }

    /// Resolve any effective item by canonical ref (kind-agnostic; the
    /// engine decides what the ref is). Used by the `--surface-file`
    /// local-preview path to fetch views for a spec only this client has.
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
            "ref_bindings": {},
            "parameters": params,
        });
        if let Some(pp) = project_path {
            body["project_path"] = serde_json::Value::String(pp.to_string());
        }
        let response = self.signed_post("/execute", &body).await?;
        Ok(response.get("result").cloned().unwrap_or(response))
    }

    /// Resolve an effective surface via the daemon's items.effective
    /// service. The response arrives with every bound view already
    /// embedded in `composed_value.views` (failures as degraded entries),
    /// so no per-view follow-up calls are needed.
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
            "ref_bindings": {},
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
    body: reqwest::Response,
    buffer: String,
    done: bool,
}

#[allow(dead_code)]
impl SseStream {
    pub fn new(body: reqwest::Response) -> Self {
        Self {
            body,
            buffer: String::new(),
            done: false,
        }
    }

    /// Poll for the next SSE frame. Returns None when the stream is done.
    pub async fn next_event(&mut self) -> Option<crate::transport::sse::SseFrame> {
        if self.done {
            return None;
        }

        // Try to parse a complete event from the buffer
        if let Some(frame) = self.try_parse_event() {
            return Some(frame);
        }

        // Read the next chunk of body bytes.
        match self.body.chunk().await {
            Ok(Some(bytes)) => {
                self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                self.try_parse_event()
            }
            Ok(None) | Err(_) => {
                self.done = true;
                None
            }
        }
    }

    fn try_parse_event(&mut self) -> Option<crate::transport::sse::SseFrame> {
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

        Some(crate::transport::sse::SseFrame {
            event_type: event_type.to_string(),
            data,
        })
    }
}
