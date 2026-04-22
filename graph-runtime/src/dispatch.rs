use serde_json::{json, Value};

use rye_runtime::callback_client::CallbackClient;

use crate::context::ExecutionContext;

pub async fn dispatch_action(
    client: &CallbackClient,
    action: &Value,
    thread_id: &str,
    project_path: &str,
    exec_ctx: Option<&ExecutionContext>,
) -> anyhow::Result<Value> {
    let mut action = action.clone();

    if let Some(ctx) = exec_ctx {
        let item_id = action.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
        let thread = action.get("thread").and_then(|v| v.as_str()).unwrap_or("inline");
        // Inject parent context for child-spawning executes only
        if item_id.starts_with("directive:") || item_id.starts_with("graph:") || thread != "inline" {
            inject_parent_context(&mut action, ctx);
        }
    }

    let item_id = action.get("item_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let kind = action.get("kind").and_then(|v| v.as_str()).map(String::from);
    let params = action.get("params").cloned().unwrap_or(json!({}));
    let thread = action.get("thread").and_then(|v| v.as_str()).unwrap_or("inline");

    let request = rye_runtime::callback::DispatchActionRequest {
        thread_id: thread_id.to_string(),
        project_path: project_path.to_string(),
        action: rye_runtime::callback::ActionPayload {
            item_id: item_id.to_string(),
            kind,
            params,
            thread: thread.to_string(),
        },
    };

    let result = client.dispatch_action(request).await
        .map_err(|e| anyhow::anyhow!("dispatch failed: {e}"))?;

    let followed = follow_continuation(client, &result, thread_id, project_path, 0).await?;
    Ok(followed)
}

fn inject_parent_context(action: &mut Value, ctx: &ExecutionContext) {
    let Some(map) = action.as_object_mut() else { return; };

    // Ensure params exists as an object
    if !map.contains_key("params") || !map["params"].is_object() {
        map.insert("params".into(), json!({}));
    }
    let params = map.get_mut("params")
        .and_then(Value::as_object_mut)
        .unwrap();

    if let Some(ref parent_id) = ctx.parent_thread_id {
        params.entry("parent_thread_id").or_insert(json!(parent_id));
    }
    if !ctx.capabilities.is_empty() {
        params.entry("parent_capabilities").or_insert(json!(ctx.capabilities));
    }
    if !ctx.limits.is_null() {
        params.entry("parent_limits").or_insert(ctx.limits.clone());
    }
    params.entry("depth").or_insert(json!(ctx.depth + 1));
}

fn follow_continuation<'a>(
    client: &'a CallbackClient,
    result: &'a Value,
    thread_id: &'a str,
    project_path: &'a str,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        if depth >= 20 {
            return Ok(result.clone());
        }

        let continuation_id = result
            .get("data")
            .and_then(|d| d.get("continuation_id"))
            .or_else(|| result.get("continuation_id"))
            .and_then(|v| v.as_str());

        let Some(cont_id) = continuation_id else {
            return Ok(result.clone());
        };

        let thread_result = client.get_thread_by_id(cont_id).await
            .map_err(|e| anyhow::anyhow!("continuation thread lookup failed: {e}"))?;

        let thread_status = thread_result
            .get("thread")
            .and_then(|t| t.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");

        if thread_status == "continued" {
            let successor_id = thread_result
                .get("thread")
                .and_then(|t| t.get("successor_thread_id"))
                .or_else(|| {
                    thread_result
                        .get("continuation")
                        .and_then(|c| c.get("successor_thread_id"))
                })
                .and_then(|v| v.as_str())
                .unwrap_or(cont_id);

            let inner = json!({"data": {"continuation_id": successor_id}});
            return follow_continuation(client, &inner, thread_id, project_path, depth + 1).await;
        }

        let terminal_result = thread_result
            .get("result")
            .cloned()
            .unwrap_or(thread_result);

        Ok(json!({
            "status": "ok",
            "data": terminal_result,
        }))
    })
}

pub fn unwrap_result(raw: &Value) -> Value {
    if let Some(data) = raw.get("data") {
        let status = raw.get("status").and_then(|s| s.as_str());
        let success = raw.get("success").and_then(|s| s.as_bool());
        if status == Some("error") || success == Some(false) {
            let mut result = data.clone();
            if let Value::Object(ref mut map) = result {
                map.insert("status".into(), Value::String("error".into()));
            }
            return result;
        }
        return data.clone();
    }

    if let Some(status) = raw.get("status").and_then(|s| s.as_str()) {
        if status == "error" {
            return raw.clone();
        }
    }

    if raw.is_object() {
        raw.clone()
    } else {
        json!({"result": raw})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rye_runtime::callback::{CallbackError, DispatchActionRequest};
    use std::sync::{Arc, Mutex};

    fn make_mock_client(results: Vec<Value>) -> CallbackClient {
        let inner: Arc<dyn rye_runtime::callback::RuntimeCallbackAPI> = Arc::new(MockClient::new(results));
        CallbackClient::from_inner(inner, "T-test", "/project")
    }

    struct MockClient {
        results: Mutex<Vec<Value>>,
    }

    impl MockClient {
        fn new(results: Vec<Value>) -> Self {
            Self { results: Mutex::new(results) }
        }
    }

    #[async_trait::async_trait]
    impl rye_runtime::callback::RuntimeCallbackAPI for MockClient {
        async fn dispatch_action(&self, _request: DispatchActionRequest) -> Result<Value, CallbackError> {
            let mut results = self.results.lock().unwrap();
            if results.is_empty() { Ok(json!({"status": "ok", "data": {}})) } else { Ok(results.remove(0)) }
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_thread(&self, id: &str) -> Result<Value, CallbackError> {
            Ok(json!({"thread": {"status": "continued", "successor_thread_id": "cont-next", "id": id}}))
        }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_event(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn reserve_budget(&self, _: &str, _: f64) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn report_budget(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn release_budget(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_budget(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn set_facets(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
    }

    #[test]
    fn unwrap_result_extracts_data() {
        let raw = json!({"status": "ok", "data": {"msg": "hello"}});
        assert_eq!(unwrap_result(&raw), json!({"msg": "hello"}));
    }

    #[test]
    fn unwrap_result_error_status() {
        let raw = json!({"status": "error", "data": {"message": "boom"}});
        let result = unwrap_result(&raw);
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn inject_parent_context_into_params() {
        let mut action = json!({"item_id": "directive:test", "params": {}});
        let ctx = ExecutionContext {
            parent_thread_id: Some("T-parent".to_string()),
            capabilities: vec!["rye.execute.*".to_string()],
            limits: json!({"turns": 10}),
            depth: 2,
        };
        inject_parent_context(&mut action, &ctx);
        assert_eq!(action["params"]["parent_thread_id"], "T-parent");
        assert_eq!(action["params"]["depth"], 3);
        assert_eq!(action["params"]["parent_capabilities"][0], "rye.execute.*");
    }

    #[tokio::test]
    async fn follow_continuation_respects_max_depth() {
        let client = make_mock_client(vec![json!({"data": {"continuation_id": "cont-1"}})]);
        let action = json!({"item_id": "tool:test/deep"});
        let result = dispatch_action(&client, &action, "t-1", "/tmp/test", None).await.unwrap();
        assert!(result.get("data").and_then(|d| d.get("continuation_id")).is_some());
    }
}
