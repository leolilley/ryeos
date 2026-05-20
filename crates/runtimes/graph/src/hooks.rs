#[cfg(test)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;

#[cfg(test)]
pub struct HookContext<'a> {
    pub graph_id: &'a str,
    pub graph_run_id: &'a str,
    pub thread_id: &'a str,
    pub step: u32,
    pub current_node: &'a str,
    pub state: &'a Value,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEvent {
    pub event: String,
    pub graph_id: String,
    pub graph_run_id: String,
    pub thread_id: String,
    pub step: u32,
    pub node: String,
    pub state: Value,
}

#[cfg(test)]

#[tracing::instrument(
    level = "debug",
    name = "graph:hook",
    skip(hooks, ctx),
    fields(
        event = %event,
        node = %ctx.current_node,
        step = ctx.step,
    )
)]
pub fn fire_hook(
    hooks: &[Value],
    event: &str,
    ctx: &HookContext,
) -> Vec<Value> {
    let mut results = Vec::new();
    for hook in hooks {
        let hook_event = hook.get("event").and_then(|e| e.as_str());
        let matches_event = hook_event.is_some_and(|e| e == event || e == "*");

        let hook_events = hook.get("events").and_then(|e| e.as_array());
        let matches_any = hook_events.is_some_and(|events| {
            events.iter().any(|e| {
                e.as_str().is_some_and(|s| s == event || s == "*")
            })
        });

        if !matches_event && !matches_any {
            continue;
        }

        let hook_evt = HookEvent {
            event: event.to_string(),
            graph_id: ctx.graph_id.to_string(),
            graph_run_id: ctx.graph_run_id.to_string(),
            thread_id: ctx.thread_id.to_string(),
            step: ctx.step,
            node: ctx.current_node.to_string(),
            state: ctx.state.clone(),
        };
        let context = serde_json::to_value(&hook_evt).unwrap_or_else(|_| serde_json::json!({}));

        let condition = hook.get("condition");
        if let Some(cond) = condition {
            if !cond.is_null() {
                match ryeos_runtime::matches(cond, &context) {
                    Ok(passes) => {
                        if !passes {
                            continue;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            hook_event = %event,
                            "graph hook condition evaluation failed, skipping: {e:#}"
                        );
                        continue;
                    }
                }
            }
        }

        results.push(context);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tracing::test as trace_test;

    #[test]
    fn fire_hook_matches_event() {
        let hooks = vec![serde_json::json!({
            "name": "test_hook",
            "events": ["after_step", "graph_completed"],
        })];
        let ctx = HookContext {
            graph_id: "test/graph",
            graph_run_id: "gr-abc",
            thread_id: "T-1",
            step: 1,
            current_node: "step1",
            state: &serde_json::json!({}),
        };
        let results = fire_hook(&hooks, "after_step", &ctx);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["event"], "after_step");
    }

    #[test]
    fn fire_hook_skips_non_matching() {
        let hooks = vec![serde_json::json!({
            "name": "test_hook",
            "events": ["graph_completed"],
        })];
        let ctx = HookContext {
            graph_id: "test",
            graph_run_id: "gr-1",
            thread_id: "T-1",
            step: 0,
            current_node: "",
            state: &serde_json::json!({}),
        };
        let results = fire_hook(&hooks, "after_step", &ctx);
        assert!(results.is_empty());
    }

    #[test]
    fn fire_hook_wildcard_matches_all() {
        let hooks = vec![serde_json::json!({
            "name": "catch_all",
            "events": ["*"],
        })];
        let ctx = HookContext {
            graph_id: "test",
            graph_run_id: "gr-1",
            thread_id: "T-1",
            step: 0,
            current_node: "",
            state: &serde_json::json!({}),
        };
        assert!(!fire_hook(&hooks, "graph_started", &ctx).is_empty());
        assert!(!fire_hook(&hooks, "error", &ctx).is_empty());
        assert!(!fire_hook(&hooks, "limit", &ctx).is_empty());
    }

    #[test]
    fn fire_hook_single_event_field() {
        let hooks = vec![serde_json::json!({
            "event": "after_step",
        })];
        let ctx = HookContext {
            graph_id: "test",
            graph_run_id: "gr-1",
            thread_id: "T-1",
            step: 1,
            current_node: "n1",
            state: &serde_json::json!({}),
        };
        assert_eq!(fire_hook(&hooks, "after_step", &ctx).len(), 1);
        assert!(fire_hook(&hooks, "error", &ctx).is_empty());
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn fire_hook_emits_span() {
        let hooks = vec![serde_json::json!({
            "event": "after_step",
        })];
        let ctx = HookContext {
            graph_id: "test",
            graph_run_id: "gr-trace",
            thread_id: "T-trace",
            step: 3,
            current_node: "n-trace",
            state: &serde_json::json!({}),
        };

        let (_, spans) = trace_test::capture_traces(|| {
            fire_hook(&hooks, "after_step", &ctx);
        });

        let span = trace_test::find_span(&spans, "graph:hook");
        assert!(span.is_some(), "expected graph:hook span, got: {:?}", spans.iter().map(|s: &ryeos_tracing::test::RecordedSpan| &s.name).collect::<Vec<_>>());

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("event"), Some("after_step"));
        assert_eq!(field_val("node"), Some("n-trace"));
        assert_eq!(field_val("step"), Some("3"));
    }
}
