use serde_json::{json, Value};

#[derive(Clone)]
pub struct ExecutionContext {
    pub parent_thread_id: Option<String>,
    pub capabilities: Vec<String>,
    pub limits: Value,
    pub depth: u32,
}

pub fn resolve_execution_context(
    params: &Value,
    graph_permissions: &[String],
) -> ExecutionContext {
    if let Some(caps) = params.get("capabilities").and_then(|v| v.as_array()) {
        let capabilities = caps
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let limits = params.get("limits").cloned().unwrap_or(json!({}));
        let depth = params.get("depth").and_then(|v| v.as_u64()).unwrap_or(5) as u32;
        return ExecutionContext {
            parent_thread_id: params.get("parent_thread_id").and_then(|v| v.as_str()).map(String::from),
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

/// Build ExecutionContext from envelope fields (envelope mode).
pub fn execution_context_from_envelope(
    parent_thread_id: Option<String>,
    parent_capabilities: Option<Vec<String>>,
    depth: u32,
    effective_caps: Vec<String>,
    hard_limits: Value,
) -> ExecutionContext {
    ExecutionContext {
        parent_thread_id,
        capabilities: if !effective_caps.is_empty() { effective_caps } else { parent_capabilities.unwrap_or_default() },
        limits: hard_limits,
        depth,
    }
}
