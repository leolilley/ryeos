use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

use crate::callback::CallbackError;
use crate::envelope::{
    HookDispatchFailureKind, HookDispatchOutput, RuntimeCost, COST_BASIS_ROLLUP,
    HOOK_INTEGRITY_FAILURE_CODE,
};
use crate::expression::{EvaluationLimits, EvaluationSession};
use crate::hooks_loader::CompiledHook;

pub type HookDispatcher = Box<
    dyn Fn(
            Value,
            String,
        )
            -> Pin<Box<dyn Future<Output = Result<HookDispatchOutput, CallbackError>> + Send>>
        + Send
        + Sync,
>;

#[derive(Debug)]
pub struct HookRunResult {
    pub control: Option<Value>,
    pub cost: Option<RuntimeCost>,
}

/// A hook run failed after zero or more hook children had already executed.
/// Parsed child cost stays attached so a later hook failure cannot erase spend
/// that has already occurred.
#[derive(Debug)]
pub struct HookRunError {
    pub message: String,
    pub cost: Option<RuntimeCost>,
    pub kind: HookRunErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRunErrorKind {
    Evaluation,
    Dispatch,
    Accounting,
    Integrity,
}

impl HookRunError {
    fn new(kind: HookRunErrorKind, message: String, cost: &Option<RuntimeCost>) -> Self {
        Self {
            message,
            cost: cost.clone(),
            kind,
        }
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.message.contains(needle)
    }
}

impl std::fmt::Display for HookRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HookRunError {}

pub async fn run_hooks(
    event: &str,
    context: &Value,
    hooks: &[CompiledHook],
    project_path: &str,
    dispatcher: &HookDispatcher,
) -> Result<HookRunResult, HookRunError> {
    let mut control_result: Option<Value> = None;
    let mut aggregate_cost: Option<RuntimeCost> = None;
    let evaluation_limits = EvaluationLimits::default();

    for (idx, hook) in hooks.iter().enumerate() {
        if hook.event() != event {
            continue;
        }

        hook.context_schema()
            .validate_context(context)
            .map_err(|error| {
                HookRunError::new(
                    HookRunErrorKind::Evaluation,
                    format!(
                        "hook[{idx}] (id={}): context validation failed: {error}",
                        hook.id()
                    ),
                    &aggregate_cost,
                )
            })?;
        let mut session = EvaluationSession::new(context, &evaluation_limits);
        let condition_passes = hook.condition().evaluate(&mut session).map_err(|error| {
            HookRunError::new(
                HookRunErrorKind::Evaluation,
                format!(
                    "hook[{idx}] (id={}): condition evaluation failed: {error}; expression {:?}",
                    hook.id(),
                    error.source()
                ),
                &aggregate_cost,
            )
        })?;
        if !condition_passes {
            continue;
        }

        let rendered = hook.action().render(&mut session).map_err(|error| {
            HookRunError::new(
                HookRunErrorKind::Evaluation,
                format!(
                    "hook[{idx}] (id={}): action evaluation failed: {error}; expression {:?}",
                    hook.id(),
                    error.source()
                ),
                &aggregate_cost,
            )
        })?;

        let dispatched = match dispatcher(rendered, project_path.to_string()).await {
            Ok(val) => val,
            Err(e) => {
                let kind = match &e {
                    CallbackError::ActionFailed { code, .. }
                        if code == HOOK_INTEGRITY_FAILURE_CODE =>
                    {
                        HookRunErrorKind::Integrity
                    }
                    _ => HookRunErrorKind::Dispatch,
                };
                return Err(HookRunError::new(
                    kind,
                    format!("hook[{idx}] (id={}): dispatch failed: {e:#}", hook.id()),
                    &aggregate_cost,
                ));
            }
        };

        if let Some(cost) = dispatched.cost {
            let aggregate = aggregate_cost.get_or_insert_with(|| RuntimeCost {
                input_tokens: 0,
                output_tokens: 0,
                total_usd: 0.0,
                basis: Some(COST_BASIS_ROLLUP.to_string()),
            });
            if let Err(error) = aggregate.checked_accumulate(&cost) {
                return Err(HookRunError::new(
                    HookRunErrorKind::Accounting,
                    format!(
                        "hook[{idx}] (id={}): cost accumulation failed: {error}",
                        hook.id()
                    ),
                    &aggregate_cost,
                ));
            }
        }
        if let Some(failure) = dispatched.failure {
            let kind = match failure.kind {
                HookDispatchFailureKind::Child => HookRunErrorKind::Dispatch,
                HookDispatchFailureKind::Integrity => HookRunErrorKind::Integrity,
            };
            return Err(HookRunError::new(
                kind,
                format!("hook[{idx}] (id={}): dispatch failed: {failure}", hook.id()),
                &aggregate_cost,
            ));
        }

        if hook.layer().is_observer_only() {
            continue;
        }

        if control_result.is_none() && dispatched.value.get("action").is_some() {
            control_result = Some(dispatched.value);
        }
    }

    Ok(HookRunResult {
        control: control_result,
        cost: aggregate_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::HookDispatchOutput;
    use crate::hooks_loader::{
        compile_hooks, CompiledHook, HookContextSchema, HookDefinition, HookLayer, HookSources,
    };
    use crate::{CompilationLimits, ExpressionCondition};
    use serde_json::json;

    fn make_hook(id: &str, event: &str) -> HookDefinition {
        HookDefinition {
            id: id.to_string(),
            event: event.to_string(),
            condition: ExpressionCondition::Absent,
            action: json!({"primary": "execute", "item_id": "tool:test/noop", "params": {}}),
        }
    }

    fn schemas() -> Vec<HookContextSchema> {
        vec![
            HookContextSchema::new("step_complete", ["state"]),
            HookContextSchema::new("error", ["state"]),
            HookContextSchema::new("after_step", ["turn"]),
            HookContextSchema::new("continuation", ["event"]),
        ]
    }

    fn compile(hooks: Vec<HookDefinition>) -> Vec<CompiledHook> {
        compile_hooks(
            HookSources {
                authored: hooks,
                ..HookSources::default()
            },
            &schemas(),
            &CompilationLimits::default(),
        )
        .unwrap()
    }

    #[test]
    fn compile_hooks_uses_fixed_source_precedence() {
        let compiled = compile_hooks(
            HookSources {
                authored: vec![make_hook("authored", "step_complete")],
                builtin: vec![make_hook("builtin", "step_complete")],
                infrastructure: vec![make_hook("infra", "step_complete")],
                context: vec![make_hook("context", "step_complete")],
                operator: vec![make_hook("operator", "step_complete")],
                project: vec![make_hook("project", "step_complete")],
            },
            &schemas(),
            &CompilationLimits::default(),
        )
        .unwrap();
        assert_eq!(
            compiled.iter().map(CompiledHook::layer).collect::<Vec<_>>(),
            vec![
                HookLayer::Authored,
                HookLayer::Builtin,
                HookLayer::Infrastructure,
                HookLayer::Context,
                HookLayer::Operator,
                HookLayer::Project,
            ]
        );
    }

    #[test]
    fn compile_hooks_rejects_duplicate_ids_across_layers() {
        let error = compile_hooks(
            HookSources {
                authored: vec![make_hook("duplicate", "step_complete")],
                project: vec![make_hook("duplicate", "step_complete")],
                ..HookSources::default()
            },
            &schemas(),
            &CompilationLimits::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("duplicate hook id `duplicate`"));
        assert!(error.to_string().contains("authored and project"));
    }

    #[tokio::test]
    async fn run_hooks_filters_by_event() {
        let hooks = compile(vec![
            make_hook("h1", "step_complete"),
            make_hook("h2", "error"),
        ]);
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async { Ok(HookDispatchOutput::bare(json!({"dispatched": true}))) })
        });
        let ctx = json!({"state": {}});
        let result = run_hooks("error", &ctx, &hooks, "/tmp", &dispatcher)
            .await
            .unwrap();
        assert!(result.control.is_none());
    }

    #[tokio::test]
    async fn infrastructure_hooks_are_observer_only() {
        let hooks = compile_hooks(
            HookSources {
                infrastructure: vec![make_hook("infra", "step_complete")],
                ..HookSources::default()
            },
            &schemas(),
            &CompilationLimits::default(),
        )
        .unwrap();
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async { Ok(HookDispatchOutput::bare(json!({"should_be_ignored": true}))) })
        });
        let ctx = json!({"state": {}});
        let result = run_hooks("step_complete", &ctx, &hooks, "/tmp", &dispatcher)
            .await
            .unwrap();
        assert!(result.control.is_none());
    }

    #[tokio::test]
    async fn run_hooks_checked_accumulates_child_cost() {
        let hooks = compile(vec![
            make_hook("h1", "step_complete"),
            make_hook("h2", "step_complete"),
        ]);
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async {
                Ok(HookDispatchOutput {
                    value: json!({}),
                    cost: Some(RuntimeCost {
                        input_tokens: 2,
                        output_tokens: 3,
                        total_usd: 0.25,
                        basis: None,
                    }),
                    failure: None,
                })
            })
        });
        let result = run_hooks(
            "step_complete",
            &json!({"state": {}}),
            &hooks,
            "/tmp",
            &dispatcher,
        )
        .await
        .unwrap();
        let cost = result.cost.unwrap();
        assert_eq!(cost.input_tokens, 4);
        assert_eq!(cost.output_tokens, 6);
        assert_eq!(cost.total_usd, 0.5);
    }

    #[tokio::test]
    async fn run_hooks_propagates_dispatch_error() {
        let hooks = compile(vec![make_hook("h1", "step_complete")]);
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
    async fn run_hooks_propagates_action_evaluation_error() {
        let hooks = compile(vec![HookDefinition {
            id: "needs-missing".to_string(),
            event: "continuation".to_string(),
            condition: ExpressionCondition::Absent,
            action: json!({
                "primary": "execute",
                "item_id": "directive:test/hook",
                "params": {"reason": "${event.missing}"}
            }),
        }]);
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async { Ok(HookDispatchOutput::bare(json!({"action": "continue"}))) })
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
        assert!(err.contains("action evaluation failed"));
    }

    #[tokio::test]
    async fn run_hooks_evaluates_event_payload_preserving_types() {
        let hooks = compile(vec![HookDefinition {
            id: "typed-event".to_string(),
            event: "continuation".to_string(),
            condition: ExpressionCondition::Absent,
            action: json!({
                "primary": "execute",
                "item_id": "directive:test/hook",
                "params": {
                    "messages": "${event.messages}",
                    "usage": "${event.usage}"
                }
            }),
        }]);
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_dispatch = captured.clone();
        let dispatcher: HookDispatcher = Box::new(move |action, _project| {
            let captured = captured_for_dispatch.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = Some(action);
                Ok(HookDispatchOutput::bare(json!({"action": "continue"})))
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
        let hooks = compile(vec![
            make_hook("summary", "continuation"),
            make_hook("abort", "continuation"),
        ]);
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let calls_for_dispatch = calls.clone();
        let dispatcher: HookDispatcher = Box::new(move |_action, _project| {
            let calls = calls_for_dispatch.clone();
            Box::pin(async move {
                let mut count = calls.lock().unwrap();
                *count += 1;
                if *count == 1 {
                    Ok(HookDispatchOutput::bare(json!(
                        "CONTINUATION_HOOK_SUMMARY: ok"
                    )))
                } else {
                    Ok(HookDispatchOutput::bare(json!({"action": "abort"})))
                }
            })
        });

        let result = run_hooks("continuation", &json!({}), &hooks, "/tmp", &dispatcher)
            .await
            .unwrap();

        assert_eq!(result.control, Some(json!({"action": "abort"})));
    }

    #[tokio::test]
    async fn boolean_and_scalar_conditions_are_strict() {
        let mut constant_false = make_hook("constant-false", "continuation");
        constant_false.condition = ExpressionCondition::Boolean(false);
        let mut expression_false = make_hook("expression-false", "continuation");
        expression_false.condition =
            ExpressionCondition::Expression("event.ready == false".to_string());
        let mut selected = make_hook("select", "continuation");
        selected.condition = ExpressionCondition::Expression("${event.ready == true}".to_string());
        let hooks = compile(vec![constant_false, expression_false, selected]);
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let calls_for_dispatch = calls.clone();
        let dispatcher: HookDispatcher = Box::new(move |_action, _project| {
            let calls = calls_for_dispatch.clone();
            Box::pin(async move {
                *calls.lock().unwrap() += 1;
                Ok(HookDispatchOutput::bare(json!({"action": "continue"})))
            })
        });

        let result = run_hooks(
            "continuation",
            &json!({"event": {"ready": true}}),
            &hooks,
            "/tmp",
            &dispatcher,
        )
        .await
        .unwrap();

        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(result.control, Some(json!({"action": "continue"})));
    }

    #[tokio::test]
    async fn non_boolean_condition_is_an_evaluation_error() {
        let mut source = make_hook("bad-condition", "continuation");
        source.condition = ExpressionCondition::Expression("event.reason".to_string());
        let hooks = compile(vec![source]);
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async { Ok(HookDispatchOutput::bare(json!({}))) })
        });

        let error = run_hooks(
            "continuation",
            &json!({"event": {"reason": "limit"}}),
            &hooks,
            "/tmp",
            &dispatcher,
        )
        .await
        .unwrap_err();

        assert!(error.contains("condition evaluation failed"));
        assert!(error.contains("must produce bool"));
    }

    #[test]
    fn compile_hooks_rejects_condition_root_outside_event_schema() {
        let mut source = make_hook("wrong-root", "continuation");
        source.condition = ExpressionCondition::Expression("state.ready".to_string());

        let error = compile_hooks(
            HookSources {
                authored: vec![source],
                ..HookSources::default()
            },
            &schemas(),
            &CompilationLimits::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("undeclared root `state`"));
    }

    #[test]
    fn compile_hooks_rejects_action_root_outside_event_schema() {
        let mut source = make_hook("wrong-action-root", "continuation");
        source.action["params"] = json!({"value": "${state.value}"});

        let error = compile_hooks(
            HookSources {
                authored: vec![source],
                ..HookSources::default()
            },
            &schemas(),
            &CompilationLimits::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("undeclared root `state`"));
    }

    #[test]
    fn compile_hooks_rejects_event_without_context_schema() {
        let error = compile_hooks(
            HookSources {
                authored: vec![make_hook("unknown-event", "unregistered")],
                ..HookSources::default()
            },
            &schemas(),
            &CompilationLimits::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("has no HookContextSchema"));
    }

    #[tokio::test]
    async fn run_hooks_rejects_undeclared_context_roots() {
        let hooks = compile(vec![make_hook("strict-context", "continuation")]);
        let dispatcher: HookDispatcher = Box::new(|_action, _project| {
            Box::pin(async { Ok(HookDispatchOutput::bare(json!({}))) })
        });

        let error = run_hooks(
            "continuation",
            &json!({"event": {}, "ambient_secret": "not visible"}),
            &hooks,
            "/tmp",
            &dispatcher,
        )
        .await
        .unwrap_err();

        assert!(error.contains("undeclared root `ambient_secret`"));
    }

    #[tokio::test]
    async fn aggregate_cost_overflow_is_typed_and_retains_the_valid_prefix() {
        let hooks = compile(vec![
            make_hook("first", "continuation"),
            make_hook("overflow", "continuation"),
        ]);
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let calls_for_dispatch = calls.clone();
        let dispatcher: HookDispatcher = Box::new(move |_action, _project| {
            let calls = calls_for_dispatch.clone();
            Box::pin(async move {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                let input_tokens = if *calls == 1 { i64::MAX as u64 } else { 1 };
                Ok(HookDispatchOutput {
                    value: json!({}),
                    cost: Some(RuntimeCost {
                        input_tokens,
                        output_tokens: 0,
                        total_usd: 0.0,
                        basis: None,
                    }),
                    failure: None,
                })
            })
        });

        let error = run_hooks("continuation", &json!({}), &hooks, "/tmp", &dispatcher)
            .await
            .unwrap_err();

        assert_eq!(error.kind, HookRunErrorKind::Accounting);
        assert_eq!(error.cost.unwrap().input_tokens, i64::MAX as u64);
        assert!(error.contains("cost accumulation failed"));
    }
}
