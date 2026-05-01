use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::callback::*;
use crate::daemon_rpc::{DaemonRpcClient, RpcError};

pub struct UdsRuntimeClient {
    rpc: DaemonRpcClient,
    callback_token: String,
}

impl UdsRuntimeClient {
    pub fn new(socket_path: PathBuf, callback_token: String) -> Self {
        Self {
            rpc: DaemonRpcClient::new(socket_path),
            callback_token,
        }
    }

    pub fn from_env() -> Result<Self, CallbackError> {
        let path = crate::daemon_rpc::resolve_daemon_socket_path(None);
        let token = std::env::var("RYEOSD_CALLBACK_TOKEN")
            .map_err(|_| CallbackError::Transport(
                anyhow::anyhow!("RYEOSD_CALLBACK_TOKEN must be set by daemon")
            ))?;
        Ok(Self::new(path, token))
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
            if !map.contains_key("callback_token") && !self.callback_token.is_empty() {
                map.insert(
                    "callback_token".to_string(),
                    json!(self.callback_token),
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
        let mut params = json!({
            "thread_id": request.thread_id,
            "project_path": request.project_path,
            "action": {
                "item_id": request.action.item_id,
                "params": request.action.params,
                "thread": request.action.thread,
            },
        });
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.dispatch_action", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn attach_process(
        &self,
        thread_id: &str,
        pid: u32,
    ) -> Result<Value, CallbackError> {
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
        status: &str,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id, "status": status});
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
        prompt: &str,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id, "reason": prompt});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.request_continuation", params)
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

    async fn replay_events(&self, thread_id: &str) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.replay_events", params)
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
        command_id: &str,
        result: Value,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({
            "thread_id": thread_id,
            "command_id": command_id,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_returns_error_without_token() {
        std::env::remove_var("RYEOSD_CALLBACK_TOKEN");
        let result = UdsRuntimeClient::from_env();
        assert!(result.is_err(), "from_env() should fail when RYEOSD_CALLBACK_TOKEN is not set");
    }

    #[test]
    fn new_accepts_token() {
        let client = UdsRuntimeClient::new(
            std::path::PathBuf::from("/tmp/test"),
            "my-token".to_string(),
        );
        assert_eq!(client.callback_token, "my-token");
    }

    #[test]
    fn inject_callback_token_adds_token() {
        let client = UdsRuntimeClient::new(
            std::path::PathBuf::from("/tmp/test"),
            "cbt-test123".to_string(),
        );
        let mut params = json!({"thread_id": "T-1"});
        client.inject_callback_token(&mut params);
        assert_eq!(params["callback_token"], "cbt-test123");
    }

    #[test]
    fn inject_callback_token_skips_if_empty() {
        let client = UdsRuntimeClient::new(
            std::path::PathBuf::from("/tmp/test"),
            String::new(),
        );
        let mut params = json!({"thread_id": "T-1"});
        client.inject_callback_token(&mut params);
        assert!(params.get("callback_token").is_none());
    }
}
