use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayedEventRecord {
    pub event_type: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayResponse {
    pub events: Vec<ReplayedEventRecord>,
}

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    #[error("{code}: {message}")]
    ActionFailed {
        code: String,
        message: String,
        retryable: bool,
    },
    #[error("transport error: {0}")]
    Transport(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchActionRequest {
    pub thread_id: String,
    pub project_path: String,
    pub action: ActionPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionPayload {
    pub item_id: String,
    #[serde(default)]
    pub params: Value,
    pub thread: String,
}

#[async_trait]
pub trait RuntimeCallbackAPI: Send + Sync {
    async fn dispatch_action(
        &self,
        request: DispatchActionRequest,
    ) -> Result<Value, CallbackError>;

    async fn attach_process(
        &self,
        thread_id: &str,
        pid: u32,
    ) -> Result<Value, CallbackError>;

    async fn mark_running(&self, thread_id: &str) -> Result<Value, CallbackError>;

    async fn finalize_thread(
        &self,
        thread_id: &str,
        status: &str,
    ) -> Result<Value, CallbackError>;

    async fn get_thread(&self, thread_id: &str) -> Result<Value, CallbackError>;

    async fn request_continuation(
        &self,
        thread_id: &str,
        prompt: &str,
    ) -> Result<Value, CallbackError>;

    async fn append_event(
        &self,
        thread_id: &str,
        event_type: &str,
        payload: Value,
        storage_class: &str,
    ) -> Result<Value, CallbackError>;

    async fn append_events(
        &self,
        thread_id: &str,
        events: Vec<Value>,
    ) -> Result<Value, CallbackError>;

    async fn replay_events(&self, thread_id: &str) -> Result<Value, CallbackError>;

    async fn claim_commands(&self, thread_id: &str) -> Result<Value, CallbackError>;

    async fn complete_command(
        &self,
        thread_id: &str,
        command_id: &str,
        result: Value,
    ) -> Result<Value, CallbackError>;

    async fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: Value,
    ) -> Result<Value, CallbackError>;

    async fn get_facets(&self, thread_id: &str) -> Result<Value, CallbackError>;
}

pub fn client_from_env() -> Box<dyn RuntimeCallbackAPI> {
    let socket_path = crate::daemon_rpc::resolve_daemon_socket_path(None);
    let token = std::env::var("RYEOSD_CALLBACK_TOKEN")
        .expect("RYEOSD_CALLBACK_TOKEN must be set by daemon");
    let tat = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .expect("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon");
    if socket_path.exists() {
        Box::new(
            crate::callback_uds::UdsRuntimeClient::new(socket_path, token, tat),
        )
    } else {
        panic!(
            "UDS socket not found at {}",
            socket_path.display()
        );
    }
}
