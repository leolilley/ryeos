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
    pub fn new(socket_path: PathBuf) -> Self {
        let callback_token = std::env::var("RYEOSD_CALLBACK_TOKEN").unwrap_or_default();
        Self {
            rpc: DaemonRpcClient::new(socket_path),
            callback_token,
        }
    }

    pub fn from_env() -> Result<Self, CallbackError> {
        let path = crate::daemon_rpc::resolve_daemon_socket_path(None);
        Ok(Self::new(path))
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
                "primary": request.action.primary,
                "item_id": request.action.item_id,
                "kind": request.action.kind,
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
        self.rpc
            .request(
                "threads.attach_process",
                json!({"thread_id": thread_id, "pid": pid}),
            )
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
        self.rpc
            .request("threads.get", json!({"thread_id": thread_id}))
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
        self.rpc
            .request("commands.claim", json!({"thread_id": thread_id}))
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn complete_command(
        &self,
        thread_id: &str,
        command_id: &str,
        result: Value,
    ) -> Result<Value, CallbackError> {
        self.rpc
            .request(
                "commands.complete",
                json!({
                    "thread_id": thread_id,
                    "command_id": command_id,
                    "result": result,
                }),
            )
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn reserve_budget(
        &self,
        thread_id: &str,
        amount: f64,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({
            "thread_id": thread_id,
            "budget_parent_id": thread_id,
            "reserved_spend": amount,
        });
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.reserve_budget", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn report_budget(
        &self,
        thread_id: &str,
        usage: Value,
    ) -> Result<Value, CallbackError> {
        let actual_spend = usage.get("total_usd")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let mut params = json!({
            "thread_id": thread_id,
            "actual_spend": actual_spend,
        });
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.report_budget", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn release_budget(&self, thread_id: &str) -> Result<Value, CallbackError> {
        let mut params = json!({
            "thread_id": thread_id,
            "status": "completed",
        });
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.release_budget", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn get_budget(&self, thread_id: &str) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.get_budget", params)
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

    async fn set_facets(
        &self,
        thread_id: &str,
        facets: Value,
    ) -> Result<Value, CallbackError> {
        let mut params = json!({"thread_id": thread_id, "facets": facets});
        self.inject_callback_token(&mut params);
        self.rpc
            .request("runtime.set_facets", params)
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn get_facets(&self, thread_id: &str) -> Result<Value, CallbackError> {
        self.rpc
            .request("threads.get_facets", json!({"thread_id": thread_id}))
            .await
            .map_err(Self::map_rpc_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_returns_client() {
        let _ = UdsRuntimeClient::from_env();
    }

    #[test]
    fn inject_callback_token_adds_token() {
        let client = UdsRuntimeClient {
            rpc: crate::daemon_rpc::DaemonRpcClient::new(std::path::PathBuf::from("/tmp/test")),
            callback_token: "cbt-test123".to_string(),
        };
        let mut params = json!({"thread_id": "T-1"});
        client.inject_callback_token(&mut params);
        assert_eq!(params["callback_token"], "cbt-test123");
    }

    #[test]
    fn inject_callback_token_skips_if_empty() {
        let client = UdsRuntimeClient {
            rpc: crate::daemon_rpc::DaemonRpcClient::new(std::path::PathBuf::from("/tmp/test")),
            callback_token: String::new(),
        };
        let mut params = json!({"thread_id": "T-1"});
        client.inject_callback_token(&mut params);
        assert!(params.get("callback_token").is_none());
    }
}
