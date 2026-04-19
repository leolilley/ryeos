use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::callback::*;
use crate::daemon_rpc::{DaemonRpcClient, RpcError};

pub struct UdsRuntimeClient {
    rpc: DaemonRpcClient,
}

impl UdsRuntimeClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            rpc: DaemonRpcClient::new(socket_path),
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
}

#[async_trait]
impl RuntimeCallbackAPI for UdsRuntimeClient {
    async fn dispatch_action(
        &self,
        request: DispatchActionRequest,
    ) -> Result<Value, CallbackError> {
        self.rpc
            .request(
                "runtime.dispatch_action",
                json!({
                    "thread_id": request.thread_id,
                    "project_path": request.project_path,
                    "action": {
                        "primary": request.action.primary,
                        "item_id": request.action.item_id,
                        "kind": request.action.kind,
                        "params": request.action.params,
                        "thread": request.action.thread,
                    },
                }),
            )
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
        self.rpc
            .request("threads.mark_running", json!({"thread_id": thread_id}))
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn finalize_thread(
        &self,
        thread_id: &str,
        status: &str,
    ) -> Result<Value, CallbackError> {
        self.rpc
            .request(
                "threads.finalize",
                json!({"thread_id": thread_id, "status": status}),
            )
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
        self.rpc
            .request(
                "threads.request_continuation",
                json!({"thread_id": thread_id, "prompt": prompt}),
            )
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
        self.rpc
            .request(
                "events.append",
                json!({
                    "thread_id": thread_id,
                    "event_type": event_type,
                    "payload": payload,
                    "storage_class": storage_class,
                }),
            )
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn append_events(
        &self,
        thread_id: &str,
        events: Vec<Value>,
    ) -> Result<Value, CallbackError> {
        self.rpc
            .request(
                "events.append_batch",
                json!({"thread_id": thread_id, "events": events}),
            )
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn replay_events(&self, thread_id: &str) -> Result<Value, CallbackError> {
        self.rpc
            .request("events.replay", json!({"thread_id": thread_id}))
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
        self.rpc
            .request(
                "budgets.reserve",
                json!({"thread_id": thread_id, "amount": amount}),
            )
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn report_budget(
        &self,
        thread_id: &str,
        usage: Value,
    ) -> Result<Value, CallbackError> {
        self.rpc
            .request(
                "budgets.report",
                json!({"thread_id": thread_id, "usage": usage}),
            )
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn release_budget(&self, thread_id: &str) -> Result<Value, CallbackError> {
        self.rpc
            .request("budgets.release", json!({"thread_id": thread_id}))
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn get_budget(&self, thread_id: &str) -> Result<Value, CallbackError> {
        self.rpc
            .request("budgets.get", json!({"thread_id": thread_id}))
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: Value,
    ) -> Result<Value, CallbackError> {
        self.rpc
            .request(
                "artifacts.publish",
                json!({"thread_id": thread_id, "artifact": artifact}),
            )
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn set_facets(
        &self,
        thread_id: &str,
        facets: Value,
    ) -> Result<Value, CallbackError> {
        self.rpc
            .request(
                "threads.set_facets",
                json!({"thread_id": thread_id, "facets": facets}),
            )
            .await
            .map_err(Self::map_rpc_error)
    }

    async fn get_facets(&self, thread_id: &str) -> Result<Value, CallbackError> {
        self.rpc
            .request(
                "threads.get_facets",
                json!({"thread_id": thread_id}),
            )
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
}
