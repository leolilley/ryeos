use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Semaphore;

use crate::model::{GraphNode, WalkContext};
use rye_runtime::callback::RuntimeCallbackAPI;

pub async fn run_foreach_sequential(
    items: &[Value],
    var: &str,
    node: &GraphNode,
    state: &mut Value,
    inputs: &Value,
    thread_id: &str,
    project_path: &str,
    client: &dyn RuntimeCallbackAPI,
) -> Vec<Value> {
    let mut results = Vec::new();
    for item in items {
        let ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
        };
        let item_ctx_val = ctx.with_foreach_item(var, item);

        let action = match &node.action {
            Some(a) => a.clone(),
            None => continue,
        };

        let interpolated = rye_runtime::interpolate_action(&action, &item_ctx_val)
            .unwrap_or(action.clone());
        let stripped = strip_none_values(&interpolated);

        if let Ok(val) = crate::dispatch::dispatch_action(client, &stripped, thread_id, project_path, None).await {
            let unwrapped = crate::dispatch::unwrap_result(&val);
            results.push(unwrapped.clone());

            if let Some(ref assign) = node.assign {
                let mut assign_ctx_map = item_ctx_val.as_object().cloned().unwrap_or_default();
                assign_ctx_map.insert("result".into(), unwrapped);
                let assign_ctx = Value::Object(assign_ctx_map);
                if let Ok(interpolated) = rye_runtime::interpolate(assign, &assign_ctx) {
                    merge_into(state, &interpolated);
                }
            }
        }
    }
    results
}

pub async fn run_foreach_parallel(
    items: &[Value],
    var: &str,
    node: &GraphNode,
    state: &Value,
    inputs: &Value,
    thread_id: &str,
    project_path: &str,
    client: Arc<dyn RuntimeCallbackAPI>,
) -> Vec<Value> {
    let max_conc = node.max_concurrency.unwrap_or(8);
    let sem = Arc::new(Semaphore::new(max_conc));
    let mut handles = Vec::new();

    for item in items {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let ctx = WalkContext {
            state: state.clone(),
            inputs: inputs.clone(),
            result: None,
        };
        let item_ctx_val = ctx.with_foreach_item(var, item);
        let action = match &node.action {
            Some(a) => a.clone(),
            None => {
                drop(permit);
                continue;
            }
        };

        let client = client.clone();
        let thread_id = thread_id.to_string();
        let project_path = project_path.to_string();

        let handle = tokio::spawn(async move {
            let _permit = permit;
            let interpolated = rye_runtime::interpolate_action(&action, &item_ctx_val)
                .unwrap_or(action.clone());
            let stripped = strip_none_values(&interpolated);
            let val = crate::dispatch::dispatch_action(client.as_ref(), &stripped, &thread_id, &project_path, None).await;
            val.map(|v| crate::dispatch::unwrap_result(&v))
        });
        handles.push(handle);
    }

    let mut results = Vec::new();
    for handle in handles {
        match handle.await {
            Ok(Ok(val)) => results.push(val),
            _ => results.push(serde_json::json!({"error": "foreach iteration failed"})),
        }
    }
    results
}

fn merge_into(target: &mut Value, source: &Value) {
    if let (Value::Object(ref mut t_map), Value::Object(ref s_map)) = (target, source) {
        for (k, v) in s_map {
            t_map.insert(k.clone(), v.clone());
        }
    }
}

fn strip_none_values(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let cleaned: serde_json::Map<String, Value> = map
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_none_values(v)))
                .collect();
            Value::Object(cleaned)
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(strip_none_values).collect())
        }
        other => other.clone(),
    }
}
