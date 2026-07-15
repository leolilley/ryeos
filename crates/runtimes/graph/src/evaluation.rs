use serde_json::{json, Value};

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
    foreach: Option<(&'a str, &'a Value)>,
    limits: EvaluationLimits,
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
            foreach: None,
            limits: EvaluationLimits::default(),
        }
    }

    pub(crate) fn with_result(mut self, result: &'a Value) -> Self {
        self.result = Some(result);
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
            context.insert("_execution", execution);
        }
        if let Some(run) = self.run.as_ref() {
            context.insert("_run", run);
        }
        if let Some((name, item)) = self.foreach {
            context.insert(name, item);
        }
        let mut session = EvaluationSession::with_context(&context, &self.limits);
        evaluate(&mut session)
    }
}
