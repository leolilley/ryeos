use serde_json::{json, Value};

use rye_runtime::callback::{ActionPayload, DispatchActionRequest, RuntimeCallbackAPI};

use crate::context::ExecutionContext;

pub async fn dispatch_action(
    client: &dyn RuntimeCallbackAPI,
    action: &Value,
    thread_id: &str,
    project_path: &str,
    exec_ctx: Option<&ExecutionContext>,
) -> anyhow::Result<Value> {
    let mut action = action.clone();

    if let Some(ctx) = exec_ctx {
        let primary = action.get("primary").and_then(|v| v.as_str()).unwrap_or("");
        if primary == "thread_directive" {
            inject_parent_context(&mut action, ctx);
        }
    }

    let payload: ActionPayload = serde_json::from_value(action)
        .map_err(|e| anyhow::anyhow!("invalid action payload: {e}"))?;

    let request = DispatchActionRequest {
        thread_id: thread_id.to_string(),
        project_path: project_path.to_string(),
        action: payload,
    };

    let result = client
        .dispatch_action(request)
        .await
        .map_err(|e| anyhow::anyhow!("dispatch failed: {e}"))?;

    let followed = follow_continuation(client, &result, thread_id, project_path, 0).await?;
    Ok(followed)
}

fn inject_parent_context(action: &mut Value, ctx: &ExecutionContext) {
    if let Value::Object(ref mut map) = action {
        if let Some(ref parent_id) = ctx.parent_thread_id {
            map.insert("parent_thread_id".into(), Value::String(parent_id.clone()));
        }
        if !ctx.capabilities.is_empty() {
            map.insert(
                "parent_capabilities".into(),
                serde_json::to_value(&ctx.capabilities).unwrap_or(Value::Array(vec![])),
            );
        }
        if !ctx.limits.is_null() {
            map.insert("parent_limits".into(), ctx.limits.clone());
        }
        map.insert("depth".into(), serde_json::json!(ctx.depth));
    }
}

fn follow_continuation<'a>(
    client: &'a dyn RuntimeCallbackAPI,
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

        let thread_result = client
            .get_thread(cont_id)
            .await
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
    fn inject_parent_context_adds_fields() {
        let mut action = json!({"primary": "thread_directive", "item_id": "directive:test"});
        let ctx = ExecutionContext {
            parent_thread_id: Some("T-parent".to_string()),
            capabilities: vec!["rye.execute.*".to_string()],
            limits: json!({"turns": 10}),
            depth: 2,
        };
        inject_parent_context(&mut action, &ctx);
        assert_eq!(action["parent_thread_id"], "T-parent");
        assert_eq!(action["depth"], 2);
    }

    struct ContinuationMock;

    #[async_trait::async_trait]
    impl RuntimeCallbackAPI for ContinuationMock {
        async fn dispatch_action(
            &self,
            _request: DispatchActionRequest,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"data": {"continuation_id": "cont-1"}}))
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({"thread": {"status": "continued", "successor_thread_id": "cont-next"}}))
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

    #[tokio::test]
    async fn follow_continuation_respects_max_depth() {
        let client = ContinuationMock;
        let action = json!({"primary": "execute", "item_id": "tool:test/deep"});
        let result = dispatch_action(&client, &action, "t-1", "/tmp/test", None).await.unwrap();
        assert!(result.get("data").and_then(|d| d.get("continuation_id")).is_some());
    }
}
