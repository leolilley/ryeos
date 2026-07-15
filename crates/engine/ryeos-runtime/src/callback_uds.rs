use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::callback::*;
use crate::daemon_rpc::{DaemonRpcClient, RpcError};

pub struct UdsRuntimeClient {
    rpc: DaemonRpcClient,
    callback_token: String,
    thread_auth_token: String,
}

impl UdsRuntimeClient {
    pub fn new(socket_path: PathBuf, callback_token: String, thread_auth_token: String) -> Self {
        Self {
            rpc: DaemonRpcClient::new(socket_path),
            callback_token,
            thread_auth_token,
        }
    }

    pub fn from_env() -> Result<Self, CallbackError> {
        let path = crate::daemon_rpc::resolve_daemon_socket_path(None);
        let token = std::env::var("RYEOSD_CALLBACK_TOKEN").map_err(|_| {
            CallbackError::Transport(anyhow::anyhow!(
                "RYEOSD_CALLBACK_TOKEN must be set by daemon"
            ))
        })?;
        let tat = std::env::var("RYEOSD_THREAD_AUTH_TOKEN").map_err(|_| {
            CallbackError::Transport(anyhow::anyhow!(
                "RYEOSD_THREAD_AUTH_TOKEN must be set by daemon"
            ))
        })?;
        Ok(Self::new(path, token, tat))
    }

    fn map_rpc_error(err: RpcError) -> CallbackError {
        match err {
            RpcError::RequestFailed {
                code,
                message,
                retryable,
                ..
            } => CallbackError::ActionFailed {
                code,
                message,
                retryable,
            },
            other => CallbackError::Transport(other.into()),
        }
    }

    fn inject_callback_token(&self, params: &mut Value) {
        if let Some(map) = params.as_object_mut() {
            if !self.callback_token.is_empty() {
                map.insert("callback_token".to_string(), json!(self.callback_token));
            }
            if !self.thread_auth_token.is_empty() {
                map.insert(
                    "thread_auth_token".to_string(),
                    json!(self.thread_auth_token),
                );
            }
        }
    }
}

#[async_trait]
impl RuntimeCallbackAPI for UdsRuntimeClient {
    async fn dispatch_action(
        &self,
        request: DispatchActionRequest,
    ) -> Result<Value, CallbackError> {
        // Serialize the whole typed `ActionPayload` rather than hand-listing
        // its fields, so wire and struct can't drift (a hand-rolled list
        // silently dropped `call`, defaulting graph method dispatch to the
        // kind default). The daemon deserializes this back into `ActionPayload`.
        let action = serde_json::to_value(&request.action)
            .map_err(|e| CallbackError::Transport(anyhow::anyhow!("serialize action: {e}")))?;
        let inline = request.action.thread == "inline";
        let mut params = json!({
            "thread_id": request.thread_id,
            "project_path": request.project_path,
            "action": action,
        });
        self.inject_callback_token(&mut params);
        if inline {
            // An inline dispatch's response arrives only after the leaf
            // settles — legitimately unbounded, and it holds the wire the
            // whole time. Run it on a DEDICATED connection so it neither
            // serializes other callbacks behind the shared connection's
            // mutex nor serializes against sibling inline dispatches on the
            // daemon's per-connection loop (parallel foreach over tools is
            // only parallel if each iteration gets its own connection).
            self.rpc
                .request_dedicated("runtime.dispatch_action", params, None)
                .await
                .map_err(Self::map_rpc_error)
        } else {
            // A detached fanout spawn is a prompt daemon-side mint: shared
            // connection, default roundtrip bound — a spawn the daemon never
            // answers must surface as an error, not park the connection (and
            // everything queued behind it) forever.
            self.rpc
                .request_with_timeout(
                    "runtime.dispatch_action",
                    params,
                    Some(crate::daemon_rpc::DEFAULT_RPC_TIMEOUT),
                )
                .await
                .map_err(Self::map_rpc_error)
        }
    }

    async fn attach_process(&self, thread_id: &str, pid: u32) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id, "pid": pid});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.attach_process", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn mark_running(&self, thread_id: &str) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.mark_running", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn finalize_thread(
        &self,
        thread_id: &str,
        completion: TerminalCompletion,
    ) -> Result<Value, CallbackError> {
        // Serialize the whole typed completion so wire and struct can't drift — a
        // hand-listed set previously dropped `outputs`/`warnings`, losing a follow
        // child's structured return. The daemon deserializes this back into
        // RuntimeFinalizeParams.
        let mut params = serde_json::to_value(&completion)
            .map_err(|e| CallbackError::Transport(anyhow::anyhow!("serialize completion: {e}")))?;
        params["thread_id"] = json!(thread_id);
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.finalize_thread", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn get_thread(&self, thread_id: &str) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.get_thread", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn request_continuation(
        &self,
        thread_id: &str,
        log_reason: Option<&str>,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({
            "thread_id": thread_id,
            "reason": log_reason,
        });
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.request_continuation", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn spawn_follow_child(
        &self,
        request: crate::callback::SpawnFollowChildRequest,
    ) -> Result<Value, CallbackError> {
        // Serialize the whole typed request so wire and struct can't drift; the
        // daemon deserializes it back (plus the injected tokens) server-side.
        let mut params = serde_json::to_value(&request).map_err(|e| {
            CallbackError::Transport(anyhow::anyhow!("serialize spawn_follow_child: {e}"))
        })?;
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.spawn_follow_child", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn append_event(
        &self,
        thread_id: &str,
        event_type: &str,
        payload: Value,
        storage_class: &str,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({
            "thread_id": thread_id,
            "event": {
                "event_type": event_type,
                "payload": payload,
                "storage_class": storage_class,
            }
        });
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.append_event", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn append_events(
        &self,
        thread_id: &str,
        events: Vec<Value>,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id, "events": events});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.append_events", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn replay_events(&self, mut params: Value) -> Result<Value, CallbackError> {
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.replay_events", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn bundle_events_append(
        &self,
        thread_id: &str,
        mut request: Value,
    ) -> Result<Value, CallbackError> {
        request["thread_id"] = json!(thread_id);
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.bundle_events_append", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn bundle_events_read_chain(
        &self,
        thread_id: &str,
        mut request: Value,
    ) -> Result<Value, CallbackError> {
        request["thread_id"] = json!(thread_id);
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.bundle_events_read_chain", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn bundle_events_scan(
        &self,
        thread_id: &str,
        mut request: Value,
    ) -> Result<Value, CallbackError> {
        request["thread_id"] = json!(thread_id);
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.bundle_events_scan", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn vault_put(&self, thread_id: &str, mut request: Value) -> Result<Value, CallbackError> {
        request["thread_id"] = json!(thread_id);
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.vault_put", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn vault_get(&self, thread_id: &str, mut request: Value) -> Result<Value, CallbackError> {
        request["thread_id"] = json!(thread_id);
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.vault_get", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn vault_delete(
        &self,
        thread_id: &str,
        mut request: Value,
    ) -> Result<Value, CallbackError> {
        request["thread_id"] = json!(thread_id);
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.vault_delete", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn vault_list(
        &self,
        thread_id: &str,
        mut request: Value,
    ) -> Result<Value, CallbackError> {
        request["thread_id"] = json!(thread_id);
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.vault_list", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn author_item(
        &self,
        thread_id: &str,
        mut request: Value,
    ) -> Result<Value, CallbackError> {
        let map = request.as_object_mut().ok_or_else(|| {
            CallbackError::Transport(anyhow::anyhow!(
                "runtime.author_item request must be a JSON object"
            ))
        })?;
        map.insert("thread_id".to_string(), json!(thread_id));
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.author_item", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn project_snapshot(
        &self,
        thread_id: &str,
        mut request: Value,
    ) -> Result<Value, CallbackError> {
        let map = request.as_object_mut().ok_or_else(|| {
            CallbackError::Transport(anyhow::anyhow!(
                "runtime.project_snapshot request must be a JSON object"
            ))
        })?;
        map.insert("thread_id".to_string(), json!(thread_id));
        self.inject_callback_token(&mut request);
        self.rpc
            .request("runtime.project_snapshot", request)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn claim_commands(&self, thread_id: &str) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.claim_commands", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn complete_command(
        &self,
        thread_id: &str,
        command_id: i64,
        status: &str,
        result: Value,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({
            "thread_id": thread_id,
            "command_id": command_id,
            "status": status,
            "result": result,
        });
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.complete_command", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: Value,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id});
        if let Some(obj) = artifact.as_object() {
            for (k, v) in obj {
                params[k] = v.clone();
            }
        }
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.publish_artifact", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn get_facets(&self, thread_id: &str) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.get_facets", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn poll_input(&self, thread_id: &str) -> Result<Value, CallbackError> {
        // `inject_callback_token` adds BOTH the callback token and the
        // thread_auth_token — the daemon's poll_input branch requires both.
        let mut params = json!({"thread_id": thread_id});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.poll_input", params)
            .await
            .map_err(Self::map_rpc_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_returns_error_without_token() {
        std::env::remove_var("RYEOSD_CALLBACK_TOKEN");
        let result = UdsRuntimeClient::from_env();
        assert!(
            result.is_err(),
            "from_env() should fail when RYEOSD_CALLBACK_TOKEN is not set"
        );
    }

    #[test]
    fn new_accepts_token() {
        let client = UdsRuntimeClient::new(
            std::path::PathBuf::from("/tmp/test"),
            "my-token".to_string(),
            "my-tat".to_string(),
        );
        assert_eq!(client.callback_token, "my-token");
        assert_eq!(client.thread_auth_token, "my-tat");
    }

    #[test]
    fn inject_callback_token_adds_token() {
        let client = UdsRuntimeClient::new(
            std::path::PathBuf::from("/tmp/test"),
            "cbt-test123".to_string(),
            "tat-test456".to_string(),
        );
        let mut params = json!({"thread_id": "T-1"});
        client.inject_callback_token(&mut params);
        assert_eq!(params["callback_token"], "cbt-test123");
        assert_eq!(params["thread_auth_token"], "tat-test456");
    }

    #[test]
    fn inject_callback_token_skips_if_empty() {
        let client = UdsRuntimeClient::new(
            std::path::PathBuf::from("/tmp/test"),
            String::new(),
            String::new(),
        );
        let mut params = json!({"thread_id": "T-1"});
        client.inject_callback_token(&mut params);
        assert!(params.get("callback_token").is_none());
        assert!(params.get("thread_auth_token").is_none());
    }
}
