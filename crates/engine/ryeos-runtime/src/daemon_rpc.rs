use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::UnixStream;

use crate::framing;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Default bound on one request/response roundtrip (connect + write + read).
/// Its job is to convert a callback the daemon never answers into a loud,
/// attributable error — the client holds the shared connection mutex across
/// the roundtrip, so an unbounded wait silently stalls every later callback
/// in the process. Generous relative to any healthy control-plane RPC; the
/// one legitimately unbounded case (an inline dispatch, whose response
/// arrives only after the leaf settles) opts out via
/// [`DaemonRpcClient::request_with_timeout`].
pub const DEFAULT_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("{code}: {message}")]
    RequestFailed {
        code: String,
        message: String,
        retryable: bool,
        details: Option<Value>,
    },
    #[error("no response to {method} within {timeout_secs}s; connection discarded")]
    Timeout { method: String, timeout_secs: u64 },
    #[error("response request_id {actual:?} did not match {expected}")]
    InvalidResponseRequestId { expected: u64, actual: Option<u64> },
    #[error("daemon closed the socket mid-frame")]
    ConnectionClosed,
    #[error("messagepack encode failed: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("messagepack decode failed: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Serialize)]
struct RpcRequestFrame<'a> {
    request_id: u64,
    method: &'a str,
    params: &'a Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcResponseFrame {
    request_id: Option<u64>,
    result: Option<Value>,
    error: Option<RpcErrorPayload>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcErrorPayload {
    code: String,
    message: String,
    #[serde(default)]
    retryable: bool,
    #[serde(default)]
    details: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct DaemonRpcClient {
    socket_path: PathBuf,
    /// Cached connection, shared across clones. The daemon serves many
    /// frames per connection, so callbacks reuse one socket instead of
    /// paying connect/teardown per RPC (streaming runtimes issue one
    /// RPC per event). Requests serialize on the lock, which also keeps
    /// request/response frames paired.
    conn: Arc<tokio::sync::Mutex<Option<UnixStream>>>,
}

enum RoundtripFailure {
    /// The request write failed: the daemon saw none of this request.
    Send(std::io::Error),
    /// The request was (at least partially) delivered; the response is
    /// unknown. Never retried — appends are not idempotent.
    Recv(std::io::Error),
    Closed,
    /// The bound elapsed with the response still outstanding. Like `Recv`,
    /// the response is unknown, so never retried; the connection is
    /// discarded (a late response would desync request/response pairing).
    TimedOut(Duration),
}

impl RoundtripFailure {
    fn into_rpc_error(self, method: &str) -> RpcError {
        match self {
            RoundtripFailure::Send(e) | RoundtripFailure::Recv(e) => RpcError::Io(e),
            RoundtripFailure::Closed => RpcError::ConnectionClosed,
            RoundtripFailure::TimedOut(limit) => RpcError::Timeout {
                method: method.to_string(),
                timeout_secs: limit.as_secs(),
            },
        }
    }
}

/// Non-blocking probe for a connection the daemon already closed (or
/// desynchronized by sending unsolicited bytes). Healthy idle
/// connections report `WouldBlock`.
fn connection_is_broken(stream: &UnixStream) -> bool {
    let mut buf = [0u8; 1];
    match stream.try_read(&mut buf) {
        Ok(_) => true, // EOF or unsolicited bytes — either way unusable
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(_) => true,
    }
}

impl DaemonRpcClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            conn: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    async fn roundtrip(
        stream: &mut UnixStream,
        payload: &[u8],
    ) -> Result<Vec<u8>, RoundtripFailure> {
        framing::send_frame(stream, payload)
            .await
            .map_err(RoundtripFailure::Send)?;
        let bytes = framing::recv_frame(stream)
            .await
            .map_err(RoundtripFailure::Recv)?;
        if bytes.is_empty() {
            return Err(RoundtripFailure::Closed);
        }
        Ok(bytes)
    }

    async fn roundtrip_bounded(
        stream: &mut UnixStream,
        payload: &[u8],
        limit: Option<Duration>,
    ) -> Result<Vec<u8>, RoundtripFailure> {
        match limit {
            None => Self::roundtrip(stream, payload).await,
            Some(d) => match tokio::time::timeout(d, Self::roundtrip(stream, payload)).await {
                Ok(result) => result,
                Err(_) => Err(RoundtripFailure::TimedOut(d)),
            },
        }
    }

    async fn connect_bounded(
        socket_path: &Path,
        limit: Option<Duration>,
    ) -> std::io::Result<UnixStream> {
        match limit {
            None => UnixStream::connect(socket_path).await,
            Some(d) => tokio::time::timeout(d, UnixStream::connect(socket_path))
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("daemon socket connect timed out after {d:?}"),
                    )
                })?,
        }
    }

    /// One RPC bounded by [`DEFAULT_RPC_TIMEOUT`]. Use
    /// [`Self::request_with_timeout`] with `None` only for a call whose
    /// response is legitimately unbounded (an inline dispatch awaiting a
    /// leaf run).
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.request_with_timeout(method, params, Some(DEFAULT_RPC_TIMEOUT))
            .await
    }

    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        limit: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let request_id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let frame = RpcRequestFrame {
            request_id,
            method,
            params: &params,
        };
        let payload = rmp_serde::to_vec_named(&frame)?;

        let mut conn = self.conn.lock().await;

        // Discard a cached connection the daemon has since closed.
        if conn.as_ref().is_some_and(connection_is_broken) {
            *conn = None;
        }
        let reused = conn.is_some();
        if conn.is_none() {
            *conn = Some(Self::connect_bounded(&self.socket_path, limit).await?);
        }
        let stream = conn.as_mut().expect("connection just ensured");

        let response_bytes = match Self::roundtrip_bounded(stream, &payload, limit).await {
            Ok(bytes) => bytes,
            Err(RoundtripFailure::Send(_)) if reused => {
                // The write itself failed on a reused connection, so the
                // daemon received none of this request; one retry on a
                // fresh connection cannot double-apply.
                *conn = None;
                let mut fresh = Self::connect_bounded(&self.socket_path, limit).await?;
                let bytes = match Self::roundtrip_bounded(&mut fresh, &payload, limit).await {
                    Ok(bytes) => bytes,
                    Err(failure) => return Err(failure.into_rpc_error(method)),
                };
                *conn = Some(fresh);
                bytes
            }
            Err(failure) => {
                *conn = None;
                return Err(failure.into_rpc_error(method));
            }
        };

        Self::parse_response(&response_bytes, request_id)
    }

    /// One RPC on its OWN connection, bypassing the shared cached one — for
    /// calls that legitimately hold the wire (an inline dispatch awaiting a
    /// leaf run). The daemon serves each connection on its own task, so
    /// dedicated connections run concurrently daemon-side AND keep a slow
    /// call from serializing every other callback behind the shared
    /// connection's mutex. Connect cost is one UDS handshake per call —
    /// negligible next to any dispatch that warrants this path.
    pub async fn request_dedicated(
        &self,
        method: &str,
        params: Value,
        limit: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let request_id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let frame = RpcRequestFrame {
            request_id,
            method,
            params: &params,
        };
        let payload = rmp_serde::to_vec_named(&frame)?;

        let mut stream = Self::connect_bounded(&self.socket_path, limit).await?;
        let response_bytes = Self::roundtrip_bounded(&mut stream, &payload, limit)
            .await
            .map_err(|failure| failure.into_rpc_error(method))?;
        Self::parse_response(&response_bytes, request_id)
    }

    fn parse_response(response_bytes: &[u8], request_id: u64) -> Result<Value, RpcError> {
        let response: RpcResponseFrame = rmp_serde::from_slice(response_bytes)?;

        if let Some(actual_id) = response.request_id {
            if actual_id != request_id {
                return Err(RpcError::InvalidResponseRequestId {
                    expected: request_id,
                    actual: Some(actual_id),
                });
            }
        }

        if let Some(err) = response.error {
            return Err(RpcError::RequestFailed {
                code: err.code,
                message: err.message,
                retryable: err.retryable,
                details: err.details,
            });
        }

        Ok(response.result.unwrap_or(Value::Null))
    }
}

pub fn resolve_daemon_socket_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(p) = std::env::var("RYEOSD_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(dir).join("ryeosd.sock");
        if p.exists() {
            return p;
        }
    }
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/ryeosd-{uid}/ryeosd.sock"))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from("/tmp/ryeosd.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_one_shot_server<F>(
        handler: F,
    ) -> (tempfile::TempDir, PathBuf, tokio::task::JoinHandle<()>)
    where
        F: FnOnce(Value) -> Value + Send + 'static,
    {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("ryeosd.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let req_bytes = framing::recv_frame(&mut stream).await.unwrap();
            let request: Value = rmp_serde::from_slice(&req_bytes).unwrap();
            let response = handler(request);
            let resp_bytes = rmp_serde::to_vec_named(&response).unwrap();
            framing::send_frame(&mut stream, &resp_bytes).await.unwrap();
        });
        (dir, socket_path, task)
    }

    #[tokio::test]
    async fn request_round_trips_msgpack() {
        let (_dir, socket_path, task) = spawn_one_shot_server(|req| {
            assert_eq!(req["method"], "system.health");
            serde_json::json!({"request_id": req["request_id"].clone(), "result": {"ok": true}})
        })
        .await;
        let client = DaemonRpcClient::new(socket_path);
        let result = client
            .request("system.health", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn request_maps_error_to_rpc_error() {
        let (_dir, socket_path, task) = spawn_one_shot_server(|req| {
            serde_json::json!({
                "request_id": req["request_id"].clone(),
                "error": {"code": "bad", "message": "nope"}
            })
        })
        .await;
        let client = DaemonRpcClient::new(socket_path);
        let err = client
            .request("system.health", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::RequestFailed { ref code, .. } if code == "bad"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn request_times_out_when_daemon_never_responds() {
        // A live daemon that consumes the request but never answers must
        // surface as a classified Timeout (with the connection discarded),
        // never an unbounded silent wait.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("ryeosd.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _req = framing::recv_frame(&mut stream).await.unwrap();
            // Hold the connection open without ever writing a response.
            std::future::pending::<()>().await;
        });

        let client = DaemonRpcClient::new(socket_path);
        let err = client
            .request_with_timeout(
                "system.health",
                serde_json::json!({"thread_id": "t-1"}),
                Some(Duration::from_millis(50)),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::Timeout { ref method, .. } if method == "system.health"),
            "expected Timeout, got: {err:?}"
        );
        task.abort();
    }

    #[test]
    fn resolve_daemon_socket_path_uses_explicit() {
        let p = Path::new("/custom/sock");
        let result = resolve_daemon_socket_path(Some(p));
        assert_eq!(result, PathBuf::from("/custom/sock"));
    }
}
