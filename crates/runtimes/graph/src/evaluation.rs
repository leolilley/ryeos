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

/// Borrowed runtime roots for one compiled graph evaluation. The only owned
/// value is the small `_run` object; state, inputs, result, execution, and a
/// foreach item are never cloned merely to assemble an expression context.
pub(crate) struct ExpressionScope<'a> {
    state: &'a Value,
    inputs: &'a Value,
    result: Option<&'a Value>,
    execution: Option<&'a Value>,
    run: Option<Value>,
    dispatch: Option<&'a ryeos_runtime::callback_contract::RuntimeDispatchEvidence>,
    foreach: Option<(&'a str, &'a Value)>,
    limits: EvaluationLimits,
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
        }
    }

    #[test]
    fn post_dispatch_scope_exposes_exact_run_and_dispatch_evidence() {
        let source = json!({
            "run": "${_run}",
            "dispatch": "${_dispatch}",
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
        let result = json!({"answer": 7});
        let dispatch = live_dispatch();
        let rendered = ExpressionScope::new(&state, &inputs, None, Some("run-1"))
            .with_run_identity("graph:test/solve", &"d".repeat(64))
            .with_result(&result)
            .with_dispatch(&dispatch)
            .render_json(&compiled)
            .unwrap();

        assert_eq!(rendered["run"]["graph_run_id"], "run-1");
        assert_eq!(rendered["run"]["definition_ref"], "graph:test/solve");
        assert_eq!(
            rendered["run"]["effective_definition_digest"],
            "d".repeat(64)
        );
        assert_eq!(
            rendered["dispatch"],
            serde_json::to_value(dispatch).unwrap()
        );
        assert_eq!(rendered["answer"], 7);
    }
}

impl<'a> ExpressionScope<'a> {
    pub(crate) fn new(
        state: &'a Value,
        inputs: &'a Value,
        execution: Option<&'a Value>,
        graph_run_id: Option<&str>,
    ) -> Self {
        Self {
            state,
            inputs,
            result: None,
            execution,
            run: graph_run_id.map(|id| json!({"graph_run_id": id})),
            dispatch: None,
            foreach: None,
            limits: EvaluationLimits::default(),
        }
    }

    pub(crate) fn with_result(mut self, result: &'a Value) -> Self {
        self.result = Some(result);
        self
    }

    pub(crate) fn with_run_identity(
        mut self,
        definition_ref: &str,
        effective_definition_digest: &str,
    ) -> Self {
        if let Some(run) = self.run.as_mut().and_then(Value::as_object_mut) {
            run.insert("definition_ref".to_owned(), json!(definition_ref));
            run.insert(
                "effective_definition_digest".to_owned(),
                json!(effective_definition_digest),
            );
        }
        self
    }

    pub(crate) fn with_dispatch(
        mut self,
        dispatch: &'a ryeos_runtime::callback_contract::RuntimeDispatchEvidence,
    ) -> Self {
        self.dispatch = Some(dispatch);
        self
    }

    pub(crate) fn with_dispatch_option(
        self,
        dispatch: Option<&'a ryeos_runtime::callback_contract::RuntimeDispatchEvidence>,
    ) -> Self {
        match dispatch {
            Some(dispatch) => self.with_dispatch(dispatch),
            None => self,
        }
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
            context.insert("_execution", execution);
        }
        if let Some(run) = self.run.as_ref() {
            context.insert("_run", run);
        }
        let dispatch_value;
        if let Some(dispatch) = self.dispatch {
            dispatch_value = serde_json::to_value(dispatch)
                .expect("typed dispatch evidence is infallibly serializable");
            context.insert("_dispatch", &dispatch_value);
        }
        if let Some((name, item)) = self.foreach {
            context.insert(name, item);
        }
        let mut session = EvaluationSession::with_context(&context, &self.limits);
        evaluate(&mut session)
    }
}
