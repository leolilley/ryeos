use serde_json::{json, Value};

use ryeos_runtime::callback_client::CallbackClient;

use crate::model::NodeReceipt;

pub async fn write_node_receipt(
    callback: &CallbackClient,
    graph_run_id: &str,
    receipt: &NodeReceipt,
) -> anyhow::Result<Value> {
    let receipt_json = json!({
        "node": receipt.node,
        "step": receipt.step,
        "graph_run_id": graph_run_id,
        "node_result_hash": receipt.result_hash,
        "cache_hit": receipt.cache_hit,
        "elapsed_ms": receipt.elapsed_ms,
        "timestamp": lillux::time::iso8601_now(),
        "error": receipt.error,
    });

    callback.publish_artifact(receipt_json.clone()).await?;

    Ok(receipt_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest, RuntimeCallbackAPI};
    use std::sync::{Arc, Mutex};

    use crate::model::NodeReceipt;

    struct MockCallback {
        facets: Mutex<serde_json::Map<String, Value>>,
        artifacts: Mutex<Vec<Value>>,
    }

    impl MockCallback {
        fn new() -> Self {
            Self {
                facets: Mutex::new(serde_json::Map::new()),
                artifacts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl RuntimeCallbackAPI for MockCallback {
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
        async fn publish_artifact(&self, _: &str, artifact: Value) -> Result<Value, CallbackError> {
            self.artifacts.lock().unwrap().push(artifact);
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            let stored = self.facets.lock().unwrap();
            Ok(Value::Object(stored.clone()))
        }
    }

    fn make_callback() -> (CallbackClient, Arc<MockCallback>) {
        let mock = Arc::new(MockCallback::new());
        let client = CallbackClient::from_inner(mock.clone(), "T-test", "/tmp/test", "tat-test");
        (client, mock)
    }

    #[tokio::test]
    async fn write_node_receipt_formats_correctly() {
        let receipt = NodeReceipt {
            node: "step1".to_string(),
            step: 1,
            result_hash: Some("abc123".to_string()),
            cache_hit: false,
            elapsed_ms: 142,
            error: None,
        };
        let (callback, mock) = make_callback();
        let output = write_node_receipt(&callback, "gr-1", &receipt).await.unwrap();
        assert_eq!(output["cache_hit"], false);
        assert_eq!(output["elapsed_ms"], 142);
        assert_eq!(output["node_result_hash"], "abc123");
        assert!(output.get("timestamp").is_some());

        let artifacts = mock.artifacts.lock().unwrap();
        assert_eq!(artifacts.len(), 1);
    }
}
