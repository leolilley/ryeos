use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::framing;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("{code}: {message}")]
    RequestFailed {
        code: String,
        message: String,
        retryable: bool,
        details: Option<Value>,
    },
    #[error("runtime.socket_path is required")]
    MissingSocketPath,
    #[error("response request_id {actual:?} did not match {expected}")]
    InvalidResponseRequestId {
        expected: u64,
        actual: Option<u64>,
    },
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
struct RpcResponseFrame {
    request_id: Option<u64>,
    result: Option<Value>,
    error: Option<RpcErrorPayload>,
}

#[derive(Deserialize)]
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
}

impl DaemonRpcClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let request_id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let frame = RpcRequestFrame {
            request_id,
            method,
            params: &params,
        };
        let payload = rmp_serde::to_vec_named(&frame)?;

        let mut stream = tokio::net::UnixStream::connect(&self.socket_path).await?;
        framing::send_frame(&mut stream, &payload).await?;

        let response_bytes = framing::recv_frame(&mut stream).await?;
        if response_bytes.is_empty() {
            return Err(RpcError::ConnectionClosed);
        }

        let response: RpcResponseFrame = rmp_serde::from_slice(&response_bytes)?;

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

#[derive(Clone, Debug)]
pub struct ThreadLifecycleClient {
    rpc: DaemonRpcClient,
}

impl ThreadLifecycleClient {
    pub fn from_request(request: &Value) -> Result<Self, RpcError> {
        let socket_path = request
            .get("runtime")
            .and_then(|r| r.get("socket_path"))
            .and_then(|s| s.as_str())
            .ok_or(RpcError::MissingSocketPath)?;
        Ok(Self {
            rpc: DaemonRpcClient::new(PathBuf::from(socket_path)),
        })
    }

    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            rpc: DaemonRpcClient::new(socket_path),
        }
    }

    pub async fn attach_process(
        &self,
        thread_id: &str,
        pid: u32,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "threads.attach_process",
                serde_json::json!({"thread_id": thread_id, "pid": pid}),
            )
            .await
    }

    pub async fn mark_running(&self, thread_id: &str) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "threads.mark_running",
                serde_json::json!({"thread_id": thread_id}),
            )
            .await
    }

    pub async fn finalize_thread(
        &self,
        thread_id: &str,
        status: &str,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "threads.finalize",
                serde_json::json!({"thread_id": thread_id, "status": status}),
            )
            .await
    }

    pub async fn get_thread(&self, thread_id: &str) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "threads.get",
                serde_json::json!({"thread_id": thread_id}),
            )
            .await
    }

    pub async fn list_threads(&self) -> Result<Value, RpcError> {
        self.rpc
            .request("threads.list", serde_json::json!({}))
            .await
    }

    pub async fn list_children(&self, thread_id: &str) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "threads.list_children",
                serde_json::json!({"thread_id": thread_id}),
            )
            .await
    }

    pub async fn get_chain(&self, thread_id: &str) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "threads.get_chain",
                serde_json::json!({"thread_id": thread_id}),
            )
            .await
    }

    pub async fn request_continuation(
        &self,
        thread_id: &str,
        prompt: &str,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "threads.request_continuation",
                serde_json::json!({"thread_id": thread_id, "prompt": prompt}),
            )
            .await
    }

    pub async fn append_event(
        &self,
        thread_id: &str,
        event_type: &str,
        payload: Value,
        storage_class: &str,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "events.append",
                serde_json::json!({
                    "thread_id": thread_id,
                    "event_type": event_type,
                    "payload": payload,
                    "storage_class": storage_class,
                }),
            )
            .await
    }

    pub async fn append_events(
        &self,
        thread_id: &str,
        events: Vec<Value>,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "events.append_batch",
                serde_json::json!({"thread_id": thread_id, "events": events}),
            )
            .await
    }

    pub async fn replay_events(&self, thread_id: &str) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "events.replay",
                serde_json::json!({"thread_id": thread_id}),
            )
            .await
    }

    pub async fn send_command(
        &self,
        thread_id: &str,
        command: &str,
        payload: Value,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "commands.send",
                serde_json::json!({
                    "thread_id": thread_id,
                    "command": command,
                    "payload": payload,
                }),
            )
            .await
    }

    pub async fn claim_commands(
        &self,
        thread_id: &str,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "commands.claim",
                serde_json::json!({"thread_id": thread_id}),
            )
            .await
    }

    pub async fn complete_command(
        &self,
        thread_id: &str,
        command_id: &str,
        result: Value,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "commands.complete",
                serde_json::json!({
                    "thread_id": thread_id,
                    "command_id": command_id,
                    "result": result,
                }),
            )
            .await
    }

    pub async fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: Value,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "artifacts.publish",
                serde_json::json!({"thread_id": thread_id, "artifact": artifact}),
            )
            .await
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
            framing::send_frame(&mut stream, &resp_bytes)
                .await
                .unwrap();
        });
        (dir, socket_path, task)
    }

    #[tokio::test]
    async fn request_round_trips_msgpack() {
        let (_dir, socket_path, task) = spawn_one_shot_server(|req| {
            assert_eq!(req["method"], "threads.get");
            serde_json::json!({"request_id": req["request_id"].clone(), "result": {"ok": true}})
        })
        .await;
        let client = DaemonRpcClient::new(socket_path);
        let result = client
            .request("threads.get", serde_json::json!({"thread_id": "t-1"}))
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
            .request("threads.get", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RpcError::RequestFailed { ref code, .. } if code == "bad")
        );
        task.await.unwrap();
    }

    #[test]
    fn from_request_requires_socket_path() {
        let err = ThreadLifecycleClient::from_request(&serde_json::json!({})).unwrap_err();
        assert!(matches!(err, RpcError::MissingSocketPath));
    }

    #[test]
    fn resolve_daemon_socket_path_uses_explicit() {
        let p = Path::new("/custom/sock");
        let result = resolve_daemon_socket_path(Some(p));
        assert_eq!(result, PathBuf::from("/custom/sock"));
    }
}
