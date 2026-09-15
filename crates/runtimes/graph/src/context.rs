use serde_json::Value;

#[derive(Clone)]
pub struct ExecutionContext {
    /// Exact executing thread from the admitted launch envelope, not graph input.
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub limits: Value,
    pub depth: u32,
    pub schedule: Option<ryeos_engine::contracts::ScheduledFireContext>,
}

impl ExecutionContext {
    pub fn as_context_value(&self) -> Value {
        let mut context =
            ryeos_engine::scheduled_fire_context::execution_context_value(self.schedule.as_ref());
        if let Some(context) = context.as_object_mut() {
            context.insert("thread_id".to_owned(), self.thread_id.clone().into());
            context.insert(
                "parent_thread_id".to_owned(),
                serde_json::json!(self.parent_thread_id.clone()),
            );
            context.insert("limits".to_owned(), self.limits.clone());
            context.insert("depth".to_owned(), self.depth.into());
        }
        context
    }
}

/// Build ExecutionContext from envelope fields.
///
/// D16: the walker no longer self-polices permissions — the daemon
/// enforces caps at the callback boundary (`enforce_callback_caps` in
/// runtime_dispatch.rs).  The `capabilities` field was removed from
/// `ExecutionContext` entirely. Parent budget/depth inheritance is also
/// daemon-owned now: callback tokens carry trusted parent context out-of-band,
/// so graph actions do not mutate params with parent limits.
pub fn execution_context_from_envelope(
    thread_id: String,
    parent_thread_id: Option<String>,
    depth: u32,
    hard_limits: Value,
    schedule: Option<ryeos_engine::contracts::ScheduledFireContext>,
) -> ExecutionContext {
    ExecutionContext {
        thread_id,
        parent_thread_id,
        limits: hard_limits,
        depth,
        schedule,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_fire_is_exposed_only_under_execution_schedule() {
        let schedule = ryeos_engine::contracts::ScheduledFireContext::new(
            "nightly.solve".to_owned(),
            "nightly.solve@1700000000000".to_owned(),
            1_700_000_000_000,
            1_700_000_000_100,
            "normal".to_owned(),
            "a".repeat(64),
        )
        .unwrap();
        let value = execution_context_from_envelope(
            "T-self".into(),
            None,
            0,
            serde_json::json!({}),
            Some(schedule),
        )
        .as_context_value();

        assert_eq!(value["schedule"]["fire_id"], "nightly.solve@1700000000000");
        assert_eq!(value["schedule"]["scheduled_at_ms"], 1_700_000_000_000_i64);
    }

    #[test]
    fn ordinary_execution_has_explicit_null_schedule() {
        let value =
            execution_context_from_envelope("T-self".into(), None, 0, serde_json::json!({}), None)
                .as_context_value();
        assert!(value["schedule"].is_null());
        assert_eq!(value["thread_id"], "T-self");
        assert!(value["parent_thread_id"].is_null());
    }
}
