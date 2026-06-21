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
        "definition_ref": receipt.definition_ref,
        "definition_hash": receipt.definition_hash,
        "graph_run_id": graph_run_id,
        "node_result_hash": receipt.result_hash,
        "cache_hit": receipt.cache_hit,
        "elapsed_ms": receipt.elapsed_ms,
        "timestamp": lillux::time::iso8601_now(),
        "error": receipt.error,
        "cost": receipt.cost,
    });

    callback
        .publish_artifact(json!({
            "artifact_type": "graph_node_receipt",
            "uri": format!("graph://runs/{graph_run_id}/node-receipts/{}", receipt.step),
            "metadata": receipt_json.clone(),
        }))
        .await?;

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
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            _: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
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
            definition_ref: "graph:test".to_string(),
            definition_hash: "def123".to_string(),
            result_hash: Some("abc123".to_string()),
            cache_hit: false,
            elapsed_ms: 142,
            error: None,
            cost: None,
        };
        let (callback, mock) = make_callback();
        let output = write_node_receipt(&callback, "gr-1", &receipt)
            .await
            .unwrap();
        assert_eq!(output["cache_hit"], false);
        assert_eq!(output["elapsed_ms"], 142);
        assert_eq!(output["definition_ref"], "graph:test");
        assert_eq!(output["definition_hash"], "def123");
        assert_eq!(output["node_result_hash"], "abc123");
        assert!(output.get("timestamp").is_some());

        let artifacts = mock.artifacts.lock().unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0]["artifact_type"], "graph_node_receipt");
        assert_eq!(artifacts[0]["uri"], "graph://runs/gr-1/node-receipts/1");
        assert_eq!(artifacts[0]["metadata"], output);
    }

    #[tokio::test]
    async fn write_node_receipt_formats_error_receipt() {
        let receipt = NodeReceipt {
            node: "step1".to_string(),
            step: 0,
            definition_ref: "graph:test".to_string(),
            definition_hash: "def123".to_string(),
            result_hash: None,
            cache_hit: false,
            elapsed_ms: 5,
            error: Some("boom".to_string()),
            cost: None,
        };

        let (callback, _mock) = make_callback();
        let output = write_node_receipt(&callback, "gr-err", &receipt)
            .await
            .unwrap();

        assert_eq!(output["graph_run_id"], "gr-err");
        assert_eq!(output["definition_ref"], "graph:test");
        assert_eq!(output["definition_hash"], "def123");
        assert_eq!(output["node_result_hash"], Value::Null);
        assert_eq!(output["error"], "boom");

        let artifacts = _mock.artifacts.lock().unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0]["artifact_type"], "graph_node_receipt");
        assert_eq!(artifacts[0]["uri"], "graph://runs/gr-err/node-receipts/0");
        assert_eq!(artifacts[0]["metadata"], output);
    }

    #[tokio::test]
    async fn write_node_receipt_includes_cost_in_metadata() {
        // A cost-bearing node's receipt must persist `cost` into the
        // published artifact metadata, not just the in-memory model.
        let receipt = NodeReceipt {
            node: "reason".to_string(),
            step: 1,
            definition_ref: "graph:test".to_string(),
            definition_hash: "def123".to_string(),
            result_hash: Some("h".to_string()),
            cache_hit: false,
            elapsed_ms: 12,
            error: None,
            cost: Some(ryeos_runtime::envelope::RuntimeCost {
                input_tokens: 100,
                output_tokens: 20,
                total_usd: 0.001,
            }),
        };
        let (callback, mock) = make_callback();
        let output = write_node_receipt(&callback, "gr-cost", &receipt)
            .await
            .unwrap();

        assert_eq!(output["cost"]["input_tokens"], 100);
        assert_eq!(output["cost"]["output_tokens"], 20);
        let artifacts = mock.artifacts.lock().unwrap();
        assert_eq!(artifacts[0]["metadata"]["cost"]["input_tokens"], 100);
    }
}
