//! Daemon client — signed HTTP/SSE client for ryeosd.
//!
//! Reuses the shared client transport: discovery, signing, and the
//! TLS-capable (`reqwest`) signed client. The CLI and this client speak one
//! transport, so an `https://` node negotiates TLS and neither
//! audience discovery nor a signed request can move to another origin.

use ryeos_client_transport::TransportError;
use ryeos_client_transport::discovery::discover_audience;
use ryeos_client_transport::http::{resolve_daemon_url, signed_client};
use ryeos_client_transport::signing::{SignHeaders, Signer};

use std::path::PathBuf;
use std::sync::RwLock;

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
    Transport(#[from] TransportError),

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

    #[error("UI binding request: {0}")]
    UiBindingRequest(String),
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

fn seat_history_stream_response(
    status: reqwest::StatusCode,
    content_type: Option<&reqwest::header::HeaderValue>,
    content_length: Option<u64>,
) -> bool {
    status == reqwest::StatusCode::OK
        && content_length != Some(0)
        && content_type
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"))
}

// ---------------------------------------------------------------------------
// Daemon client
// ---------------------------------------------------------------------------

pub struct DaemonClient {
    base_url: String,
    audience: String,
    signer: Option<Signer>,
    ui_session_id: RwLock<Option<String>>,
    /// Ephemeral admitted producer coordinate, never a grant or seat registry.
    pub(crate) ui_producer: RwLock<Option<ryeos_client_base::ui::binding::UiPresentationProducer>>,
    pub(crate) seat_delivery: tokio::sync::Mutex<crate::app::seat::SeatDelivery>,
    /// Single-use authenticated stream handoff from replacement admission to
    /// the terminal loop; never serialized into a reducer effect result.
    pub(crate) seat_live_handoff: tokio::sync::Mutex<Option<crate::app::seat::LiveSeatHandoff>>,
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

        // Discovery and signed requests share one normalized base and both
        // refuse redirects; the configured origin cannot move between them.
        let audience = if signer.is_some() {
            let discovered = discover_audience(&base_url).await?;
            base_url = discovered.base_url;
            discovered.principal_id
        } else {
            String::new()
        };

        Ok(Self {
            base_url,
            audience,
            signer,
            ui_session_id: RwLock::new(None),
            ui_producer: RwLock::new(None),
            seat_delivery: tokio::sync::Mutex::new(crate::app::seat::SeatDelivery::default()),
            seat_live_handoff: tokio::sync::Mutex::new(None),
            http: signed_client()?,
        })
    }

    fn sign(&self, method: &str, path: &str, body: &[u8]) -> Result<SignHeaders, ClientError> {
        let signer = self.signer.as_ref().ok_or(ClientError::NoIdentity)?;
        signer
            .sign(method, path, body, &self.audience)
            .map_err(ClientError::Transport)
    }

    pub async fn mint_ui_session(
        &self,
        config_ref: &str,
        project_path: Option<&str>,
    ) -> Result<(), ClientError> {
        if self
            .ui_session_id
            .read()
            .expect("session lock poisoned")
            .is_some()
        {
            return Ok(());
        }
        let user_principal_id = self
            .signer
            .as_ref()
            .map(|signer| format!("fp:{}", signer.fingerprint));
        let body = ui_session_mint_body(config_ref, project_path, user_principal_id.as_deref());
        let response = self.signed_post("/ui/api/launch/mint", &body).await?;
        if response
            .get("ui_binding_contract_revision")
            .and_then(serde_json::Value::as_str)
            != Some(ryeos_client_base::UI_BINDING_CONTRACT_REVISION)
        {
            return Err(ClientError::Transport(TransportError::BodyDecode {
                detail: format!(
                    "UI binding contract mismatch: daemon must advertise '{}'",
                    ryeos_client_base::UI_BINDING_CONTRACT_REVISION
                ),
            }));
        }
        let Some(session_id) = response.get("session_id").and_then(|value| value.as_str()) else {
            return Err(ClientError::DaemonDown {
                url: format!("{}/ui/api/launch/mint", self.base_url.trim_end_matches('/')),
            });
        };
        let Some(launch_url) = response.get("launch_url").and_then(|value| value.as_str()) else {
            return Err(ClientError::DaemonDown {
                url: format!("{}/ui/api/launch/mint", self.base_url.trim_end_matches('/')),
            });
        };
        let origin = self.base_url.trim_end_matches('/');
        let launch_path = launch_url.strip_prefix(origin).ok_or_else(|| {
            ClientError::UiBindingRequest(
                "minted UI launch URL does not retain the configured daemon origin".into(),
            )
        })?;
        self.redeem_ui_session(session_id, launch_path).await?;
        Ok(())
    }

    pub fn adopt_ui_session(&self, session_id: &str) -> Result<(), ClientError> {
        if session_id.is_empty()
            || session_id.len() > 64
            || !session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(ClientError::UiBindingRequest(
                "replacement UI session id is not a bounded transport token".into(),
            ));
        }
        *self.ui_session_id.write().expect("session lock poisoned") = Some(session_id.to_string());
        Ok(())
    }

    /// Activate a daemon-minted immutable session and adopt only the exact
    /// authenticated result. Activation is replayable for response-loss
    /// recovery; replacement keeps the predecessor cookie until commit.
    pub async fn redeem_ui_session(
        &self,
        session_id: &str,
        launch_path: &str,
    ) -> Result<ryeos_client_base::ui::BrowserSession, ClientError> {
        let valid_launch_path = launch_path
            .strip_prefix("/ui/launch/")
            .is_some_and(|token| {
                !token.is_empty()
                    && token.len() <= 128
                    && token
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if !valid_launch_path {
            return Err(ClientError::UiBindingRequest(
                "UI activation path is not a local token route".into(),
            ));
        }
        let prior = self
            .ui_session_id
            .read()
            .expect("session lock poisoned")
            .clone();
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), launch_path);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(55);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                *self.ui_session_id.write().expect("session lock poisoned") = prior;
                return Err(ClientError::Transport(TransportError::Unreachable {
                    bind: url,
                    detail: "UI activation could not be observed before its recovery deadline"
                        .into(),
                }));
            }
            let attempt_timeout = remaining.min(std::time::Duration::from_secs(5));
            *self.ui_session_id.write().expect("session lock poisoned") = prior.clone();
            let headers = self.sign("GET", launch_path, b"")?;
            let mut request = self
                .http
                .get(&url)
                .header("x-ryeos-key-id", &headers.key_id)
                .header("x-ryeos-timestamp", &headers.timestamp)
                .header("x-ryeos-nonce", &headers.nonce)
                .header("x-ryeos-signature", &headers.signature);
            if let Some(cookie) = self.ui_cookie(launch_path) {
                request = request.header("cookie", cookie);
            }
            let unknown = match tokio::time::timeout(attempt_timeout, request.send()).await {
                Err(_) => "activation request timed out before an outcome was observed".to_string(),
                Ok(Ok(response))
                    if response.status().is_success() || response.status().is_redirection() =>
                {
                    self.adopt_ui_session(session_id)?;
                    let observation_timeout = deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .min(std::time::Duration::from_secs(5));
                    match tokio::time::timeout(observation_timeout, self.current_ui_session()).await
                    {
                        Ok(Ok(session)) if session.session_id == session_id => return Ok(session),
                        Ok(Ok(_)) => {
                            *self.ui_session_id.write().expect("session lock poisoned") = prior;
                            return Err(ClientError::UiBindingRequest(
                                "UI activation authenticated a different session".into(),
                            ));
                        }
                        Ok(Err(error)) => {
                            format!("activation committed; observation failed: {error}")
                        }
                        Err(_) => "activation committed; session observation timed out".to_string(),
                    }
                }
                Ok(Ok(response)) if response.status().is_client_error() => {
                    *self.ui_session_id.write().expect("session lock poisoned") = prior;
                    return Err(ClientError::DaemonError {
                        path: launch_path.to_string(),
                        status: response.status().as_u16(),
                        message: "UI session activation refused".into(),
                    });
                }
                Ok(Ok(response)) => format!(
                    "activation server outcome was uncertain (HTTP {})",
                    response.status().as_u16()
                ),
                Ok(Err(error)) => format!("activation contact failed: {error}"),
            };
            if tokio::time::Instant::now() >= deadline {
                *self.ui_session_id.write().expect("session lock poisoned") = prior;
                return Err(ClientError::Transport(TransportError::Unreachable {
                    bind: url,
                    detail: unknown,
                }));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    pub async fn current_ui_session(
        &self,
    ) -> Result<ryeos_client_base::ui::BrowserSession, ClientError> {
        let value = self.get_json("/ui/api/session/current").await?;
        serde_json::from_value(value).map_err(ClientError::Json)
    }

    fn ui_cookie(&self, path: &str) -> Option<String> {
        path.starts_with("/ui/")
            .then(|| {
                self.ui_session_id
                    .read()
                    .expect("session lock poisoned")
                    .clone()
            })
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
            .map_err(|e| TransportError::Unreachable {
                bind: url.clone(),
                detail: format!("request send: {e}"),
            })?;

        let status = resp.status();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| TransportError::BodyDecode {
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
        let wrapped;
        let body = if matches!(
            path,
            "/ui/api/invocations/dispatch" | "/ui/api/session/attachments/detach"
        ) {
            let producer = self
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .clone()
                .ok_or_else(|| {
                    ClientError::UiBindingRequest("presentation producer is not ready".into())
                })?;
            wrapped = if path == "/ui/api/invocations/dispatch" {
                let binding = serde_json::from_value::<
                    ryeos_client_base::ui::binding::UiBindingRequest,
                >(body.clone())?;
                serde_json::to_value(ryeos_client_base::ui::binding::UiInvocationRequest {
                    producer,
                    binding,
                    presentation_operation_id: None,
                })?
            } else {
                let mut request = body.clone();
                if !request.is_object() {
                    return Err(ClientError::UiBindingRequest(
                        "attachment detach request must be a record".into(),
                    ));
                }
                request["producer"] = serde_json::to_value(producer)?;
                request
            };
            &wrapped
        } else {
            body
        };
        self.signed_post_exact(path, body).await
    }

    pub(crate) async fn invoke_presentation_operation(
        &self,
        binding: &ryeos_client_base::ui::binding::UiBindingRequest,
        operation_id: &str,
    ) -> Result<serde_json::Value, ClientError> {
        let producer = self
            .ui_producer
            .read()
            .expect("producer lock poisoned")
            .clone()
            .ok_or_else(|| {
                ClientError::UiBindingRequest("presentation producer is not ready".into())
            })?;
        let body = serde_json::to_value(ryeos_client_base::ui::binding::UiInvocationRequest {
            producer,
            binding: binding.clone(),
            presentation_operation_id: Some(operation_id.to_owned()),
        })?;
        self.signed_post_exact("/ui/api/invocations/dispatch", &body)
            .await
    }

    async fn signed_post_exact(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        let (url, req) = self.signed_post_request(path, body, "application/json")?;
        let resp = req
            .send()
            .await
            .map_err(|e| TransportError::Unreachable {
                bind: url.clone(),
                detail: format!("request send: {e}"),
            })?;

        let status = resp.status();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| TransportError::BodyDecode {
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

    fn signed_post_request(
        &self,
        path: &str,
        body: &serde_json::Value,
        accept: &str,
    ) -> Result<(String, reqwest::RequestBuilder), ClientError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let body_bytes = serde_json::to_vec(body)?;
        let headers = self.sign("POST", path, &body_bytes)?;

        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", accept)
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .body(body_bytes);
        if let Some(cookie) = self.ui_cookie(path) {
            req = req.header("cookie", cookie);
        }
        Ok((url, req))
    }

    /// Authenticated, request-body-bound POST stream for one signed UI seat
    /// history cut. This is not the ephemeral session hint or generic thread
    /// tail stream.
    pub async fn open_seat_history(
        &self,
        presentation_id: &str,
        chain_root_id: &str,
    ) -> Result<SseStream, ClientError> {
        let path = "/ui/api/session/seat/history";
        let body = serde_json::json!({
            "presentation_id": presentation_id,
            "chain_root_id": chain_root_id,
        });
        let (url, req) = self.signed_post_request(path, &body, "text/event-stream")?;
        let resp = req
            .send()
            .await
            .map_err(|e| TransportError::Unreachable {
                bind: url.clone(),
                detail: format!("request send: {e}"),
            })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| TransportError::BodyDecode {
                    detail: format!("read body: {e}"),
                })?;
            return Err(ClientError::DaemonError {
                path: path.to_string(),
                status,
                message: daemon_error_message(&body_bytes),
            });
        }
        if !seat_history_stream_response(
            resp.status(),
            resp.headers().get(reqwest::header::CONTENT_TYPE),
            resp.content_length(),
        ) {
            return Err(ClientError::UiBindingRequest(
                "seat history did not return a nonempty event stream response".into(),
            ));
        }
        Ok(SseStream::new(resp))
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
        let session_id = self
            .ui_session_id
            .read()
            .expect("session lock poisoned")
            .clone()
            .ok_or(ClientError::NoIdentity)?;
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
            .map_err(|e| TransportError::Unreachable {
                bind: url.clone(),
                detail: format!("request send: {e}"),
            })?;

        Ok(SseStream::new(resp))
    }
}

fn ui_session_mint_body(
    config_ref: &str,
    project_path: Option<&str>,
    user_principal_id: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "ui_binding_contract_revision": ryeos_client_base::UI_BINDING_CONTRACT_REVISION,
        "composition": { "kind": "shared_presentation", "config_ref": config_ref },
        "project_path": project_path,
        "user_principal_id": user_principal_id,
    })
}

// ---------------------------------------------------------------------------
// SSE Stream wrapper
// ---------------------------------------------------------------------------

pub struct SseStream {
    body: reqwest::Response,
    buffer: Vec<u8>,
    scan_from: usize,
    done: bool,
}

impl SseStream {
    pub fn new(body: reqwest::Response) -> Self {
        Self {
            body,
            buffer: Vec::new(),
            scan_from: 0,
            done: false,
        }
    }

    /// Poll for the next SSE frame. Returns None when the stream is done.
    pub async fn next_event(&mut self) -> Option<crate::transport::sse::SseFrame> {
        if self.done {
            return None;
        }
        loop {
            match self.try_parse_event() {
                Ok(Some(frame)) => return Some(frame),
                Ok(None) => {}
                Err(()) => {
                    self.done = true;
                    return None;
                }
            }
            match self.body.chunk().await {
                Ok(Some(bytes)) => {
                    self.buffer.extend_from_slice(&bytes);
                    if self.buffer.len() > 32 * 1024 * 1024 {
                        self.done = true;
                        return None;
                    }
                }
                Ok(None) | Err(_) => {
                    self.done = true;
                    return None;
                }
            }
        }
    }

    fn try_parse_event(&mut self) -> Result<Option<crate::transport::sse::SseFrame>, ()> {
        parse_sse_frame(&mut self.buffer, &mut self.scan_from)
    }
}

/// Parse only a complete UTF-8 frame. Transport chunks can split both SSE
/// delimiters and multi-byte code points; neither is an end-of-stream marker.
fn parse_sse_frame(
    buffer: &mut Vec<u8>,
    scan_from: &mut usize,
) -> Result<Option<crate::transport::sse::SseFrame>, ()> {
    while let Some((end, delimiter_len)) = {
        let suffix = &buffer[*scan_from..];
        let lf = suffix.windows(2).position(|part| part == b"\n\n");
        let crlf = suffix.windows(4).position(|part| part == b"\r\n\r\n");
        match (lf, crlf) {
            (Some(a), Some(b)) if a < b => Some((*scan_from + a, 2)),
            (Some(_), Some(b)) => Some((*scan_from + b, 4)),
            (Some(a), None) => Some((*scan_from + a, 2)),
            (None, Some(b)) => Some((*scan_from + b, 4)),
            (None, None) => None,
        }
    } {
        let event_text = std::str::from_utf8(&buffer[..end])
            .map_err(|_| ())?
            .to_owned();
        buffer.drain(..end + delimiter_len);
        *scan_from = 0;
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
        if !data.is_empty() {
            return Ok(Some(crate::transport::sse::SseFrame {
                event_type: event_type.to_string(),
                data,
            }));
        }
    }
    *scan_from = buffer.len().saturating_sub(3);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{parse_sse_frame, seat_history_stream_response, ui_session_mint_body};

    #[test]
    fn seat_history_requires_an_actual_sse_response() {
        use reqwest::StatusCode;
        use reqwest::header::HeaderValue;

        let sse = HeaderValue::from_static("text/event-stream; charset=utf-8");
        let json = HeaderValue::from_static("application/json");
        let lookalike = HeaderValue::from_static("text/event-streaming");
        assert!(seat_history_stream_response(
            StatusCode::OK,
            Some(&sse),
            None
        ));
        assert!(seat_history_stream_response(
            StatusCode::OK,
            Some(&sse),
            Some(42)
        ));
        assert!(!seat_history_stream_response(
            StatusCode::NO_CONTENT,
            Some(&sse),
            None
        ));
        assert!(!seat_history_stream_response(
            StatusCode::OK,
            Some(&sse),
            Some(0)
        ));
        assert!(!seat_history_stream_response(
            StatusCode::OK,
            Some(&json),
            None
        ));
        assert!(!seat_history_stream_response(
            StatusCode::OK,
            Some(&lookalike),
            None
        ));
        assert!(!seat_history_stream_response(StatusCode::OK, None, None));
    }

    #[test]
    fn sse_parser_waits_for_split_utf8_and_accepts_lf_or_crlf_frames() {
        let mut buffer = b"event: history\ndata: caf".to_vec();
        buffer.push(0xc3);
        let mut scan_from = 0;
        assert!(
            parse_sse_frame(&mut buffer, &mut scan_from)
                .unwrap()
                .is_none()
        );
        buffer.extend_from_slice(&[0xa9, b'\n', b'\n']);
        let first = parse_sse_frame(&mut buffer, &mut scan_from)
            .unwrap()
            .unwrap();
        assert_eq!(first.event_type, "history");
        assert_eq!(first.data, "café");
        buffer.extend_from_slice(b"event: next\r\ndata: {\"ok\":true}\r\n\r");
        assert!(
            parse_sse_frame(&mut buffer, &mut scan_from)
                .unwrap()
                .is_none()
        );
        buffer.push(b'\n');
        let second = parse_sse_frame(&mut buffer, &mut scan_from)
            .unwrap()
            .unwrap();
        assert_eq!(second.event_type, "next");
        assert_eq!(second.data, "{\"ok\":true}");
    }

    #[test]
    fn ui_session_is_bound_to_the_verified_operator_principal() {
        let body = ui_session_mint_body(
            "config:ryeos/ui/terminal-presentation",
            Some("/work/project"),
            Some("fp:operator"),
        );
        assert_eq!(body["user_principal_id"], "fp:operator");
        assert_eq!(body["project_path"], "/work/project");
        assert_eq!(body["composition"]["kind"], "shared_presentation");
        assert_eq!(
            body["composition"]["config_ref"],
            "config:ryeos/ui/terminal-presentation"
        );
        assert!(body["composition"].get("surface_ref").is_none());
        assert_eq!(
            body["ui_binding_contract_revision"],
            ryeos_client_base::UI_BINDING_CONTRACT_REVISION
        );
    }
}
