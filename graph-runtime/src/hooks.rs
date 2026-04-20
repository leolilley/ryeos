use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct HookContext<'a> {
    pub graph_id: &'a str,
    pub graph_run_id: &'a str,
    pub thread_id: &'a str,
    pub step: u32,
    pub current_node: &'a str,
    pub state: &'a Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct HookEvent {
    pub event: String,
    pub graph_id: String,
    pub graph_run_id: String,
    pub thread_id: String,
    pub step: u32,
    pub node: String,
    pub state: Value,
}

pub fn fire_hook(
    hooks: &[Value],
    event: &str,
    ctx: &HookContext,
) -> Vec<Value> {
    let mut results = Vec::new();
    for hook in hooks {
        let hook_event = hook.get("event").and_then(|e| e.as_str());
        let matches_event = hook_event.map_or(false, |e| e == event || e == "*");

        let hook_events = hook.get("events").and_then(|e| e.as_array());
        let matches_any = hook_events.map_or(false, |events| {
            events.iter().any(|e| {
                e.as_str().map_or(false, |s| s == event || s == "*")
            })
        });

        if !matches_event && !matches_any {
            continue;
        }

        let context = serde_json::json!({
            "event": event,
            "graph_id": ctx.graph_id,
            "graph_run_id": ctx.graph_run_id,
            "thread_id": ctx.thread_id,
            "step": ctx.step,
            "node": ctx.current_node,
            "state": ctx.state,
        });

        let condition = hook.get("condition");
        if let Some(cond) = condition {
            if !cond.is_null() && !rye_runtime::matches(cond, &context).unwrap_or(false) {
                continue;
            }
        }

        results.push(context);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
