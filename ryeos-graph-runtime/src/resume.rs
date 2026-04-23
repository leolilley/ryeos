use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_runtime::callback_client::CallbackClient;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    pub current_node: String,
    pub step_count: u32,
    pub state: Value,
    pub graph_run_id: String,
}

pub async fn load_resume_state(
    callback: &CallbackClient,
    graph_run_id: &str,
) -> Result<Option<ResumeState>> {
    let facets = callback.get_facets().await?;

    let checkpoint_key = format!("graph_checkpoint:{}", graph_run_id);
    let checkpoint_str = facets.get(&checkpoint_key).and_then(|v| v.as_str());

    let Some(checkpoint_str) = checkpoint_str else {
        let ref_key = format!("graph_ref:{}", graph_run_id);
        let ref_str = facets.get(&ref_key).and_then(|v| v.as_str());
        let Some(ref_str) = ref_str else {
            return Ok(None);
        };
        let ref_data: Value = serde_json::from_str(ref_str)?;
        return load_from_ref_data(&ref_data, graph_run_id);
    };

    let checkpoint: Value = serde_json::from_str(checkpoint_str)?;

    let current_node = checkpoint
        .get("current_node")
        .and_then(|v| v.as_str())
        .unwrap_or("start")
        .to_string();
    let step_count = checkpoint
        .get("step_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let state = checkpoint.get("state").cloned().unwrap_or(Value::Object(Default::default()));

    Ok(Some(ResumeState {
        current_node,
        step_count,
        state,
        graph_run_id: graph_run_id.to_string(),
    }))
}

fn load_from_ref_data(ref_data: &Value, graph_run_id: &str) -> Result<Option<ResumeState>> {
    let current_node = ref_data
        .get("current_node")
        .and_then(|v| v.as_str())
        .map(String::from);
    let step_count = ref_data
        .get("step_count")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let state = ref_data.get("state").cloned();

    match (current_node, step_count, state) {
        (Some(node), Some(steps), Some(st)) => Ok(Some(ResumeState {
            current_node: node,
            step_count: steps,
            state: st,
            graph_run_id: graph_run_id.to_string(),
        })),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest, RuntimeCallbackAPI};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct FacetsMock {
        facets: HashMap<String, String>,
    }

    #[async_trait]
    impl RuntimeCallbackAPI for FacetsMock {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_event(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({"events": []})) }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(serde_json::to_value(&self.facets).unwrap())
        }
    }

    fn make_callback(facets: HashMap<String, String>) -> CallbackClient {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(FacetsMock { facets });
        CallbackClient::from_inner(inner, "T-test", "/tmp/test")
    }

    #[tokio::test]
    async fn load_resume_state_returns_none_when_no_data() {
        let callback = make_callback(HashMap::new());
        let result = load_resume_state(&callback, "gr-1").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn load_resume_state_from_facets() {
        let checkpoint = json!({
            "current_node": "step3",
            "step_count": 5,
            "state": {"key": "value"}
        });
        let mut facets = HashMap::new();
        facets.insert(
            "graph_checkpoint:gr-test123".to_string(),
            serde_json::to_string(&checkpoint).unwrap(),
        );

        let callback = make_callback(facets);
        let result = load_resume_state(&callback, "gr-test123").await.unwrap();
        assert!(result.is_some());
        let state = result.unwrap();
        assert_eq!(state.current_node, "step3");
        assert_eq!(state.step_count, 5);
    }

    #[tokio::test]
    async fn load_resume_state_from_ref_facet() {
        let ref_data = json!({
            "current_node": "step3",
            "step_count": 5,
            "state": {"key": "value"}
        });
        let mut facets = HashMap::new();
        facets.insert(
            "graph_ref:gr-refonly".to_string(),
            serde_json::to_string(&ref_data).unwrap(),
        );

        let callback = make_callback(facets);
        let result = load_resume_state(&callback, "gr-refonly").await.unwrap();
        assert!(result.is_some());
        let state = result.unwrap();
        assert_eq!(state.current_node, "step3");
        assert_eq!(state.step_count, 5);
    }
}
