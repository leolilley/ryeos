use serde_json::Value;

#[derive(Clone)]
pub struct ExecutionContext {
    pub parent_thread_id: Option<String>,
    pub limits: Value,
    pub depth: u32,
}

/// Build ExecutionContext from envelope fields.
///
/// D16: the walker no longer self-polices permissions — the daemon
/// enforces caps at the callback boundary (`enforce_callback_caps` in
/// runtime_dispatch.rs).  The `capabilities` field was removed from
/// `ExecutionContext` entirely; parent→child cap threading is the
/// daemon's responsibility.
pub fn execution_context_from_envelope(
    parent_thread_id: Option<String>,
    depth: u32,
    hard_limits: Value,
) -> ExecutionContext {
    ExecutionContext {
        parent_thread_id,
        limits: hard_limits,
        depth,
    }
}
