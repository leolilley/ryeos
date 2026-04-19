use serde_json::{json, Value};

use rye_runtime::callback::{ActionPayload, DispatchActionRequest, RuntimeCallbackAPI};

pub async fn dispatch_action(
    client: &dyn RuntimeCallbackAPI,
    action: &Value,
    thread_id: &str,
    project_path: &str,
) -> anyhow::Result<Value> {
    let payload: ActionPayload = serde_json::from_value(action.clone())
        .map_err(|e| anyhow::anyhow!("invalid action payload: {e}"))?;

    let request = DispatchActionRequest {
        thread_id: thread_id.to_string(),
        project_path: project_path.to_string(),
        action: payload,
    };

    client
        .dispatch_action(request)
        .await
        .map_err(|e| anyhow::anyhow!("dispatch failed: {e}"))
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
