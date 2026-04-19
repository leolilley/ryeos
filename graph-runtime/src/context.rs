use std::path::Path;

use serde_json::{json, Value};

pub struct ExecutionContext {
    #[allow(dead_code)]
    pub parent_thread_id: Option<String>,
    pub capabilities: Vec<String>,
    #[allow(dead_code)]
    pub limits: Value,
    #[allow(dead_code)]
    pub depth: u32,
}

pub fn resolve_execution_context(
    params: &Value,
    project_path: &Path,
    graph_permissions: &[String],
) -> ExecutionContext {
    if let Ok(parent_id) = std::env::var("RYE_PARENT_THREAD_ID") {
        let meta_path = project_path
            .join(".ai/state/threads")
            .join(&parent_id)
            .join("thread.json");
        if let Ok(content) = std::fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<Value>(&content) {
                let caps = meta
                    .get("capabilities")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let limits = meta.get("limits").cloned().unwrap_or(json!({}));
                let depth = limits.get("depth").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                return ExecutionContext {
                    parent_thread_id: Some(parent_id),
                    capabilities: caps,
                    limits,
                    depth,
                };
            }
        }
    }

    if let Some(caps) = params.get("capabilities").and_then(|v| v.as_array()) {
        let capabilities = caps
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let limits = params.get("limits").cloned().unwrap_or(json!({}));
        let depth = params.get("depth").and_then(|v| v.as_u64()).unwrap_or(5) as u32;
        return ExecutionContext {
            parent_thread_id: None,
            capabilities,
            limits,
            depth,
        };
    }

    if !graph_permissions.is_empty() {
        return ExecutionContext {
            parent_thread_id: None,
            capabilities: graph_permissions.to_vec(),
            limits: json!({}),
            depth: 5,
        };
    }

    ExecutionContext {
        parent_thread_id: None,
        capabilities: vec![],
        limits: json!({}),
        depth: 0,
    }
}
