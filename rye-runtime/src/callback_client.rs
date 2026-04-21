use std::sync::Arc;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::callback::RuntimeCallbackAPI;
use crate::envelope::EnvelopeCallback;

pub struct CallbackClient {
    inner: Option<Arc<dyn RuntimeCallbackAPI>>,
    thread_id: String,
    project_path: String,
    allowed_primaries: Vec<String>,
}

impl CallbackClient {
    /// Construct from a pre-built runtime API implementation (for tests).
    pub fn from_inner(
        inner: Arc<dyn RuntimeCallbackAPI>,
        thread_id: &str,
        project_path: &str,
        allowed_primaries: Vec<String>,
    ) -> Self {
        Self {
            inner: Some(inner),
            thread_id: thread_id.to_string(),
            project_path: project_path.to_string(),
            allowed_primaries,
        }
    }
}

impl Clone for CallbackClient {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            thread_id: self.thread_id.clone(),
            project_path: self.project_path.clone(),
            allowed_primaries: self.allowed_primaries.clone(),
        }
    }
}

impl CallbackClient {
    pub fn new(callback: &EnvelopeCallback, thread_id: &str, project_path: &str) -> Self {
        let inner: Option<Arc<dyn RuntimeCallbackAPI>> = if callback.socket_path.exists() {
            Some(Arc::new(
                crate::callback_uds::UdsRuntimeClient::new(
                    callback.socket_path.clone(),
                    callback.token.clone(),
                )
            ))
        } else {
            None
        };
        tracing::info!(
            socket = %callback.socket_path.display(),
            thread_id = %thread_id,
            has_uds = callback.socket_path.exists(),
            "callback client initialized"
        );
        Self {
            inner,
            thread_id: thread_id.to_string(),
            project_path: project_path.to_string(),
            allowed_primaries: callback.allowed_primaries.clone(),
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn project_path(&self) -> &str {
        &self.project_path
    }

    pub async fn dispatch_action(
        &self,
        req: crate::callback::DispatchActionRequest,
    ) -> Result<Value> {
        let primary = &req.action.primary;
        let allowed = &self.allowed_primaries;
        if !allowed.is_empty() && !allowed.contains(&"*".to_string()) && !allowed.contains(primary)
        {
            bail!("disallowed primary: {}", primary);
        }
        match &self.inner {
            Some(client) => Ok(client.dispatch_action(req).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    pub async fn append_event(&self, event_type: &str, payload: Value) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.append_event(&self.thread_id, event_type, payload, "transient").await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn reserve_budget(&self, amount: f64) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.reserve_budget(&self.thread_id, amount).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn report_budget(&self, usage: Value) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.report_budget(&self.thread_id, usage).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn release_budget(&self) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.release_budget(&self.thread_id).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn mark_running(&self) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.mark_running(&self.thread_id).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn finalize_thread(&self, status: &str) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.finalize_thread(&self.thread_id, status).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn request_continuation(&self, prompt: &str) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.request_continuation(&self.thread_id, prompt).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    pub async fn publish_artifact(&self, artifact: Value) -> Result<()> {
        match &self.inner {
            Some(client) => {
                client.publish_artifact(&self.thread_id, artifact).await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub async fn get_thread(&self) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.get_thread(&self.thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    pub async fn get_thread_by_id(&self, thread_id: &str) -> Result<Value> {
        match &self.inner {
            Some(client) => Ok(client.get_thread(thread_id).await
                .map_err(|e| anyhow::anyhow!("{e}"))?),
            None => Ok(Value::Null),
        }
    }

    // Typed event emission methods (merged from EventEmitter)

    pub async fn emit_turn_start(&self, turn: u32) -> Result<()> {
        self.append_event("turn_start", serde_json::json!({"turn": turn})).await
    }

    pub async fn emit_turn_complete(&self, turn: u32, tokens: Option<(u64, u64)>) -> Result<()> {
        let mut data = serde_json::json!({"turn": turn});
        if let Some((input, output)) = tokens {
            data["input_tokens"] = serde_json::json!(input);
            data["output_tokens"] = serde_json::json!(output);
        }
        self.append_event("turn_complete", data).await
    }

    pub async fn emit_tool_dispatch(&self, tool: &str, call_id: Option<&str>) -> Result<()> {
        let mut data = serde_json::json!({"tool": tool});
        if let Some(id) = call_id {
            data["call_id"] = serde_json::json!(id);
        }
        self.append_event("tool_dispatch", data).await
    }

    pub async fn emit_tool_result(&self, call_id: &str, truncated: bool) -> Result<()> {
        self.append_event(
            "tool_result",
            serde_json::json!({"call_id": call_id, "truncated": truncated}),
        ).await
    }

    pub async fn emit_error(&self, error: &str) -> Result<()> {
        self.append_event("error", serde_json::json!({"message": error})).await
    }

    pub async fn emit_thread_continued(&self, previous_id: &str) -> Result<()> {
        self.append_event(
            "thread_continued",
            serde_json::json!({"previous_thread_id": previous_id}),
        ).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callback::{ActionPayload, DispatchActionRequest};
    use serde_json::json;
    use std::path::PathBuf;

    fn make_callback(allowed_primaries: Vec<String>) -> EnvelopeCallback {
        EnvelopeCallback {
            socket_path: PathBuf::from("/nonexistent/test.sock"),
            token: "test-token".to_string(),
            allowed_primaries,
        }
    }

    fn make_client(allowed_primaries: Vec<String>) -> CallbackClient {
        CallbackClient::new(&make_callback(allowed_primaries), "T-test", "/project")
    }

    #[tokio::test]
    async fn dispatch_action_with_disallowed_primary_rejected() {
        let client = make_client(vec!["execute".to_string()]);
        let req = DispatchActionRequest {
            thread_id: "T-test".to_string(),
            project_path: "/project".to_string(),
            action: ActionPayload {
                primary: "sign".to_string(),
                item_id: "my/item".to_string(),
                kind: None,
                params: json!({}),
                thread: "inline".to_string(),
            },
        };
        let result = client.dispatch_action(req).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("disallowed primary: sign"), "{}", err);
    }

    #[tokio::test]
    async fn dispatch_action_with_allowed_primary_succeeds_when_disconnected() {
        let client = make_client(vec!["execute".to_string()]);
        let req = DispatchActionRequest {
            thread_id: "T-test".to_string(),
            project_path: "/project".to_string(),
            action: ActionPayload {
                primary: "execute".to_string(),
                item_id: "my/tool".to_string(),
                kind: None,
                params: json!({}),
                thread: "inline".to_string(),
            },
        };
        let result = client.dispatch_action(req).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Value::Null);
    }

    #[tokio::test]
    async fn append_event_noop_when_disconnected() {
        let client = make_client(vec!["execute".to_string()]);
        client
            .append_event("turn_start", json!({"turn": 1}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mark_running_noop_when_disconnected() {
        let client = make_client(vec!["execute".to_string()]);
        client.mark_running().await.unwrap();
    }

    #[tokio::test]
    async fn all_methods_noop_when_disconnected() {
        let client = make_client(vec!["*".to_string()]);

        client.reserve_budget(1.0).await.unwrap();
        client.report_budget(json!({"tokens": 100})).await.unwrap();
        client.release_budget().await.unwrap();
        client.finalize_thread("completed").await.unwrap();
        client
            .publish_artifact(json!({"type": "summary", "content": "done"}))
            .await
            .unwrap();
        let cont = client.request_continuation("continue?").await.unwrap();
        assert_eq!(cont, Value::Null);
    }

    #[tokio::test]
    async fn wildcard_primary_allowed() {
        let client = make_client(vec!["*".to_string()]);
        let req = DispatchActionRequest {
            thread_id: "T-test".to_string(),
            project_path: "/project".to_string(),
            action: ActionPayload {
                primary: "anything_goes".to_string(),
                item_id: "x/y".to_string(),
                kind: None,
                params: json!({}),
                thread: "inline".to_string(),
            },
        };
        assert!(client.dispatch_action(req).await.is_ok());
    }

    #[test]
    fn thread_id_and_project_path_accessors() {
        let client = make_client(vec![]);
        assert_eq!(client.thread_id(), "T-test");
        assert_eq!(client.project_path(), "/project");
    }

    #[test]
    fn clone_preserves_fields() {
        let client = make_client(vec!["execute".to_string()]);
        let cloned = client.clone();
        assert_eq!(cloned.thread_id(), "T-test");
        assert_eq!(cloned.project_path(), "/project");
        assert_eq!(cloned.allowed_primaries, vec!["execute"]);
    }

    #[tokio::test]
    async fn emit_turn_start_noop_when_disconnected() {
        let client = make_client(vec![]);
        client.emit_turn_start(1).await.unwrap();
    }

    #[tokio::test]
    async fn emit_tool_dispatch_with_call_id() {
        let client = make_client(vec![]);
        client.emit_tool_dispatch("read_file", Some("call_123")).await.unwrap();
    }

    #[tokio::test]
    async fn emit_turn_complete_with_tokens() {
        let client = make_client(vec![]);
        client.emit_turn_complete(1, Some((100, 50))).await.unwrap();
    }

    #[tokio::test]
    async fn emit_tool_result_noop_when_disconnected() {
        let client = make_client(vec![]);
        client.emit_tool_result("call_1", false).await.unwrap();
    }

    #[tokio::test]
    async fn emit_error_noop_when_disconnected() {
        let client = make_client(vec![]);
        client.emit_error("something broke").await.unwrap();
    }

    #[tokio::test]
    async fn emit_thread_continued_noop_when_disconnected() {
        let client = make_client(vec![]);
        client.emit_thread_continued("T-prev").await.unwrap();
    }
}
