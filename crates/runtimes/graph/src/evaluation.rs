use serde_json::{Value, json};

use ryeos_runtime::checkpoint::{checkpoint_shape_limits, validate_checkpoint_shape};
use ryeos_runtime::{
    CompiledActionTemplate, CompiledJsonTemplate, CompiledTemplate, EvaluationContext,
    EvaluationLimits, EvaluationSession, ExpressionError,
};

pub(crate) fn validate_runtime_value(value: &Value, field: &str) -> Result<(), ExpressionError> {
    let context = EvaluationContext::new();
    let limits = EvaluationLimits::default();
    EvaluationSession::with_context(&context, &limits).validate_value(value, field)
}

/// Validate a borrowed runtime envelope against the rye-expr JSON
/// depth/node/byte limits without treating the validation walk as expression
/// execution. Checkpoints and history snapshots can legitimately approach the
/// result-shape ceiling; provisioning inspection fuel from that ceiling keeps
/// write and resume acceptance identical.
pub(crate) fn validate_runtime_shape(value: &Value, field: &str) -> Result<(), ExpressionError> {
    validate_checkpoint_shape(value, field)
}

pub(crate) fn validate_runtime_array_shape(
    values: &[Value],
    field: &str,
) -> Result<(), ExpressionError> {
    let context = EvaluationContext::new();
    let limits = checkpoint_shape_limits();
    EvaluationSession::with_context(&context, &limits).validate_array(values, field)
}

/// Complete graph-run identity exposed to every expression scope as `run`.
///
/// Keeping the current step in the constructor makes it impossible for one
/// runtime evaluation path to admit `run.step` but omit it at execution.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GraphRunExpressionContext<'a> {
    pub(crate) graph_run_id: &'a str,
    pub(crate) step: u32,
    pub(crate) definition_ref: &'a str,
    pub(crate) effective_definition_digest: &'a str,
}

impl GraphRunExpressionContext<'_> {
    pub(crate) const fn new<'a>(
        graph_run_id: &'a str,
        step: u32,
        definition_ref: &'a str,
        effective_definition_digest: &'a str,
    ) -> GraphRunExpressionContext<'a> {
        GraphRunExpressionContext {
            graph_run_id,
            step,
            definition_ref,
            effective_definition_digest,
        }
    }

    pub(crate) fn to_value(self) -> Value {
        json!({
            "graph_run_id": self.graph_run_id,
            "step": self.step,
            "definition_ref": self.definition_ref,
            "effective_definition_digest": self.effective_definition_digest,
        })
    }
}

/// Borrowed runtime roots for one compiled graph evaluation. The only owned
/// value is the small `run` object; state, inputs, result, execution, and a
/// foreach item are never cloned merely to assemble an expression context.
pub(crate) struct ExpressionScope<'a> {
    state: &'a Value,
    inputs: &'a Value,
    result: Option<&'a Value>,
    execution: Option<&'a Value>,
    run: Value,
    post_action_dispatch: Option<PostActionDispatchExpressionContext<'a>>,
    foreach: Option<(&'a str, &'a Value)>,
    limits: EvaluationLimits,
}

#[derive(Clone, Copy)]
struct PostActionDispatchExpressionContext<'a> {
    evidence: Option<&'a ryeos_runtime::callback_contract::RuntimeDispatchEvidence>,
    child_thread_id: Option<&'a str>,
}

/// Build the transient post-action dispatch root. The child coordinate comes
/// only from the daemon-parsed action outcome and is deliberately absent from
/// persisted `RuntimeDispatchEvidence`.
pub(crate) fn post_action_dispatch_value(
    evidence: Option<&ryeos_runtime::callback_contract::RuntimeDispatchEvidence>,
    child_thread_id: Option<&str>,
) -> Value {
    let mut value = evidence.map_or_else(
        || json!({}),
        |evidence| {
            serde_json::to_value(evidence)
                .expect("typed dispatch evidence is infallibly serializable")
        },
    );
    let prior = value
        .as_object_mut()
        .expect("typed dispatch evidence serializes as an object")
        .insert(
            "child_thread_id".to_string(),
            child_thread_id.map_or(Value::Null, |id| Value::String(id.to_string())),
        );
    assert!(
        prior.is_none(),
        "persisted dispatch evidence must not own child_thread_id"
    );
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::CompilationLimits;
    use ryeos_runtime::callback_contract::{
        RuntimeDispatchEffectClass, RuntimeDispatchEvidence, RuntimeDispatchPublication,
        RuntimeDispatchSource,
    };

    fn live_dispatch() -> RuntimeDispatchEvidence {
        RuntimeDispatchEvidence {
            source: RuntimeDispatchSource::Executed,
            effect_class: RuntimeDispatchEffectClass::Live,
            action_digest: "a".repeat(64),
            effect_identity: None,
            publication: RuntimeDispatchPublication::NotApplicable,
            record_hash: None,
            replayed_from: None,
            result_projection:
                ryeos_runtime::callback_contract::DispatchResultProjection::DispatchedSubject,
        }
    }

    #[test]
    fn post_dispatch_scope_exposes_exact_run_and_dispatch_evidence() {
        let source = json!({
            "execution": "${execution}",
            "run": "${run}",
            "dispatch": "${dispatch}",
            "answer": "${result.answer}",
        });
        let compiled = CompiledJsonTemplate::compile(
            &source,
            "test.post_dispatch",
            &CompilationLimits::default(),
        )
        .unwrap();
        let state = json!({});
        let inputs = json!({});
        let execution = json!({"thread_id": "thread-1"});
        let result = json!({"answer": 7});
        let dispatch = live_dispatch();
        let digest = "d".repeat(64);
        let rendered = ExpressionScope::new(
            &state,
            &inputs,
            Some(&execution),
            GraphRunExpressionContext {
                graph_run_id: "run-1",
                step: 7,
                definition_ref: "graph:test/solve",
                effective_definition_digest: &digest,
            },
        )
        .with_result(&result)
        .with_post_action_dispatch(Some(&dispatch), None)
        .render_json(&compiled)
        .unwrap();

        assert_eq!(rendered["run"]["graph_run_id"], "run-1");
        assert_eq!(rendered["execution"], execution);
        assert_eq!(rendered["run"]["step"], 7);
        assert_eq!(rendered["run"]["definition_ref"], "graph:test/solve");
        assert_eq!(
            rendered["run"]["effective_definition_digest"],
            "d".repeat(64)
        );
        let mut expected_dispatch = serde_json::to_value(dispatch).unwrap();
        expected_dispatch["child_thread_id"] = Value::Null;
        assert_eq!(rendered["dispatch"], expected_dispatch);
        assert_eq!(rendered["answer"], 7);
    }

    #[test]
    fn post_action_dispatch_child_is_explicit_and_never_inferred_from_result() {
        let compiled = CompiledJsonTemplate::compile(
            &json!({"child": "${dispatch.child_thread_id}"}),
            "test.post_action_child",
            &CompilationLimits::default(),
        )
        .unwrap();
        let state = json!({});
        let inputs = json!({});
        let spoofed_result = json!({"child_thread_id": "T-spoofed"});
        let dispatch = live_dispatch();
        let digest = "d".repeat(64);

        let exact = ExpressionScope::new(
            &state,
            &inputs,
            None,
            GraphRunExpressionContext::new("run-1", 1, "graph:test/solve", &digest),
        )
        .with_result(&spoofed_result)
        .with_post_action_dispatch(Some(&dispatch), Some("T-daemon-child"))
        .render_json(&compiled)
        .unwrap();
        assert_eq!(exact["child"], "T-daemon-child");

        let no_child = ExpressionScope::new(
            &state,
            &inputs,
            None,
            GraphRunExpressionContext::new("run-1", 1, "graph:test/solve", &digest),
        )
        .with_result(&spoofed_result)
        .with_post_action_dispatch(Some(&dispatch), None)
        .render_json(&compiled)
        .unwrap();
        assert_eq!(no_child["child"], Value::Null);

        let no_evidence = ExpressionScope::new(
            &state,
            &inputs,
            None,
            GraphRunExpressionContext::new("run-1", 1, "graph:test/solve", &digest),
        )
        .with_result(&spoofed_result)
        .with_post_action_dispatch(None, None)
        .render_json(&compiled)
        .unwrap();
        assert_eq!(no_evidence["child"], Value::Null);
    }
}

impl<'a> ExpressionScope<'a> {
    pub(crate) fn new(
        state: &'a Value,
        inputs: &'a Value,
        execution: Option<&'a Value>,
        run: GraphRunExpressionContext<'_>,
    ) -> Self {
        Self {
            state,
            inputs,
            result: None,
            execution,
            run: run.to_value(),
            post_action_dispatch: None,
            foreach: None,
            limits: EvaluationLimits::default(),
        }
    }

    pub(crate) fn with_result(mut self, result: &'a Value) -> Self {
        self.result = Some(result);
        self
    }

    pub(crate) fn with_post_action_dispatch(
        mut self,
        evidence: Option<&'a ryeos_runtime::callback_contract::RuntimeDispatchEvidence>,
        child_thread_id: Option<&'a str>,
    ) -> Self {
        self.post_action_dispatch = Some(PostActionDispatchExpressionContext {
            evidence,
            child_thread_id,
        });
        self
    }

    pub(crate) fn with_foreach(mut self, name: &'a str, item: &'a Value) -> Self {
        self.foreach = Some((name, item));
        self
    }

    pub(crate) fn render_action(
        &self,
        template: &CompiledActionTemplate,
    ) -> Result<Value, ExpressionError> {
        self.evaluate(|session| template.render(session))
    }

    pub(crate) fn render_json(
        &self,
        template: &CompiledJsonTemplate,
    ) -> Result<Value, ExpressionError> {
        self.evaluate(|session| template.render(session))
    }

    pub(crate) fn render_template(
        &self,
        template: &CompiledTemplate,
    ) -> Result<Value, ExpressionError> {
        self.evaluate(|session| session.render_template(template))
    }

    fn evaluate<T>(
        &self,
        evaluate: impl FnOnce(&mut EvaluationSession<'_>) -> Result<T, ExpressionError>,
    ) -> Result<T, ExpressionError> {
        let mut context = EvaluationContext::new()
            .with_root("state", self.state)
            .with_root("inputs", self.inputs);
        if let Some(result) = self.result {
            context.insert("result", result);
        }
        if let Some(execution) = self.execution {
            context.insert("execution", execution);
        }
        context.insert("run", &self.run);
        let dispatch_value;
        if let Some(dispatch) = self.post_action_dispatch {
            dispatch_value =
                post_action_dispatch_value(dispatch.evidence, dispatch.child_thread_id);
            context.insert("dispatch", &dispatch_value);
        }
        if let Some((name, item)) = self.foreach {
            context.insert(name, item);
        }
        let mut session = EvaluationSession::with_context(&context, &self.limits);
        evaluate(&mut session)
    }
}
