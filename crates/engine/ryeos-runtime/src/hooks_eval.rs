use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

use crate::callback::CallbackError;
use crate::hooks_loader::HookDefinition;

pub type HookDispatcher = Box<
    dyn Fn(Value, String) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send>>
        + Send
        + Sync,
>;

pub async fn run_hooks(
    event: &str,
    context: &Value,
    hooks: &[HookDefinition],
    project_path: &str,
    dispatcher: &HookDispatcher,
) -> Result<Option<Value>, String> {
    let mut control_result: Option<Value> = None;

    for (idx, hook) in hooks.iter().enumerate() {
        if hook.event != event {
            continue;
        }

        let condition = hook.condition.as_ref().cloned().unwrap_or(Value::Null);
        let condition_passes = match crate::condition::matches(context, &condition) {
            Ok(b) => b,
            Err(e) => {
                return Err(format!(
                    "hook[{idx}] (id={}): condition evaluation failed: {e:#}",
                    hook.id
                ))
            }
        };
        if !condition_passes {
            continue;
        }

        let interpolated = crate::interpolation::interpolate_action(&hook.action, context)
            .map_err(|e| {
                format!(
                    "hook[{idx}] (id={}): action interpolation failed: {e:#}",
                    hook.id
                )
            })?;

        let result = match dispatcher(interpolated, project_path.to_string()).await {
            Ok(val) => val,
            Err(e) => {
                return Err(format!(
                    "hook[{idx}] (id={}): dispatch failed: {e:#}",
                    hook.id
                ))
            }
        };

        let layer = hook.layer.unwrap_or(2);
        if layer == 3 {
            continue;
        }

        if control_result.is_none() && result.get("action").is_some() {
            control_result = Some(result);
        }
    }

    Ok(control_result)
}

pub fn merge_hooks(
    mut graph_hooks: Vec<HookDefinition>,
    mut builtin_hooks: Vec<HookDefinition>,
    mut infra_hooks: Vec<HookDefinition>,
    excluded_events: &[&str],
) -> Vec<HookDefinition> {
    builtin_hooks.retain(|h| !excluded_events.contains(&h.event.as_str()));
    infra_hooks.retain(|h| !excluded_events.contains(&h.event.as_str()));

    for h in &mut graph_hooks {
        if h.layer.is_none() {
            h.layer = Some(1);
        }
    }
    for h in &mut builtin_hooks {
        if h.layer.is_none() {
            h.layer = Some(2);
        }
    }
    for h in &mut infra_hooks {
        if h.layer.is_none() {
            h.layer = Some(3);
        }
    }

    let mut all = graph_hooks;
    all.extend(builtin_hooks);
    all.extend(infra_hooks);
    all.sort_by_key(|h| h.layer.unwrap_or(2));
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_hook(id: &str, event: &str, layer: u8) -> HookDefinition {
        HookDefinition {
            id: id.to_string(),
            event: event.to_string(),
            layer: Some(layer),
            condition: None,
            action: json!({"primary": "execute", "item_id": "tool:test/noop", "params": {}}),
        }
    }

    #[test]
    fn merge_hooks_sorts_by_layer() {
        let graph = vec![make_hook("g1", "step_complete", 1)];
        let builtin = vec![make_hook("b1", "step_complete", 2)];
        let infra = vec![make_hook("i1", "step_complete", 3)];
        let merged = merge_hooks(graph, builtin, infra, &[]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].id, "g1");
        assert_eq!(merged[1].id, "b1");
        assert_eq!(merged[2].id, "i1");
    }

    #[test]
    fn merge_hooks_excludes_events() {
        let builtin = vec![
            make_hook("b1", "step_complete", 2),
            make_hook("b2", "thread_started", 2),
        ];
        let merged = merge_hooks(vec![], builtin, vec![], &["thread_started"]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "b1");
    }

    #[tokio::test]
    async fn run_hooks_filters_by_event() {
        let hooks = vec![
            make_hook("h1", "step_complete", 1),
            make_hook("h2", "error", 1),
        ];
        let dispatcher: HookDispatcher =
            Box::new(|_action, _project| Box::pin(async { Ok(json!({"dispatched": true})) }));
        let ctx = json!({"state": {}});
        let result = run_hooks("error", &ctx, &hooks, "/tmp", &dispatcher)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn run_hooks_layer3_ignored_for_control() {
        let hooks = vec![make_hook("infra", "step_complete", 3)];
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async { Ok(json!({"should_be_ignored": true})) })
        });
        let ctx = json!({"state": {}});
        let result = run_hooks("step_complete", &ctx, &hooks, "/tmp", &dispatcher)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn run_hooks_propagates_dispatch_error() {
        let hooks = vec![make_hook("h1", "step_complete", 1)];
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async {
                Err(CallbackError::ActionFailed {
                    code: "timeout".to_string(),
                    message: "simulated timeout".to_string(),
                    retryable: false,
                })
            })
        });
        let ctx = json!({"state": {}});
        let result = run_hooks("step_complete", &ctx, &hooks, "/tmp", &dispatcher).await;
        assert!(
            result.is_err(),
            "dispatch failure should propagate as Err: {result:?}"
        );
        assert!(result.unwrap_err().contains("dispatch failed"));
    }

    #[tokio::test]
    async fn run_hooks_propagates_interpolation_error() {
        let hooks = vec![HookDefinition {
            id: "needs-missing".to_string(),
            event: "continuation".to_string(),
            layer: Some(1),
            condition: None,
            action: json!({
                "primary": "execute",
                "item_id": "directive:test/hook",
                "params": {"reason": "${event.missing}"}
            }),
        }];
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async { Ok(json!({"action": "continue"})) })
        });

        let result = run_hooks(
            "continuation",
            &json!({"event": {"reason": "context_window"}}),
            &hooks,
            "/tmp",
            &dispatcher,
        )
        .await;

        let err = result.unwrap_err();
        assert!(err.contains("needs-missing"));
        assert!(err.contains("action interpolation failed"));
    }

    #[tokio::test]
    async fn run_hooks_interpolates_event_payload_preserving_types() {
        let hooks = vec![HookDefinition {
            id: "typed-event".to_string(),
            event: "continuation".to_string(),
            layer: Some(1),
            condition: None,
            action: json!({
                "primary": "execute",
                "item_id": "directive:test/hook",
                "params": {
                    "messages": "${event.messages}",
                    "usage": "${event.usage}"
                }
            }),
        }];
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_dispatch = captured.clone();
        let dispatcher: HookDispatcher = Box::new(move |action, _project| {
            let captured = captured_for_dispatch.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some(action);
                Ok(json!({"action": "continue"}))
            })
        });

        run_hooks(
            "continuation",
            &json!({
                "event": {
                    "messages": [{"role": "assistant", "content": "hi"}],
                    "usage": {"input_tokens": 1, "output_tokens": 2, "total_usd": 0.0}
                }
            }),
            &hooks,
            "/tmp",
            &dispatcher,
        )
        .await
        .unwrap();

        let action = captured.lock().unwrap().clone().unwrap();
        assert!(action["params"]["messages"].is_array());
        assert!(action["params"]["usage"].is_object());
    }

    #[tokio::test]
    async fn run_hooks_non_control_result_does_not_mask_later_control() {
        let hooks = vec![
            make_hook("summary", "continuation", 1),
            make_hook("abort", "continuation", 1),
        ];
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let calls_for_dispatch = calls.clone();
        let dispatcher: HookDispatcher = Box::new(move |_action, _project| {
            let calls = calls_for_dispatch.clone();
            Box::pin(async move {
                let mut count = calls.lock().unwrap();
                *count += 1;
                if *count == 1 {
                    Ok(json!("CONTINUATION_HOOK_SUMMARY: ok"))
                } else {
                    Ok(json!({"action": "abort"}))
                }
            })
        });

        let result = run_hooks("continuation", &json!({}), &hooks, "/tmp", &dispatcher)
            .await
            .unwrap();

        assert_eq!(result, Some(json!({"action": "abort"})));
    }
}
