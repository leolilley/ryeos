mod ast;
mod error;
mod evaluator;
mod functions;
mod lexer;
mod limits;
mod parser;
mod references;
mod runtime_json;
mod static_types;
mod template;
mod token;
mod value;

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

pub use error::{ErrorPhase, ExpressionError, SourceSpan};
pub(crate) use evaluator::json_string_bytes;
pub use limits::{CompilationLimits, EvaluationLimits};
pub use references::{Reference, ReferenceSegment, ReferenceSet};
pub use runtime_json::{RuntimeJsonArrayBudget, RuntimeJsonObjectBudget};

use ast::{BinaryOperator, BuiltinFunction, Expr, ExprKind, Literal, UnaryOperator};
use evaluator::{Budget, ContextView, Evaluator, RuntimeValue};
use value::Numeric;

#[derive(Debug, Clone)]
pub struct CompiledExpression {
    source: Arc<str>,
    field: Option<Arc<str>>,
    root: Expr,
    references: ReferenceSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpressionValueType {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl ExpressionValueType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool => "bool",
            Self::Number => "number",
            Self::String => "string",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

impl CompiledExpression {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    pub fn span(&self) -> SourceSpan {
        self.root.span
    }

    pub fn references(&self) -> &ReferenceSet {
        &self.references
    }

    /// Return the result type when it follows entirely from expression shape.
    /// `None` means runtime data can affect the type; it is not a permissive
    /// coercion signal.
    pub fn static_result_type(&self) -> Option<ExpressionValueType> {
        infer_static_type(&self.root)
    }
}

fn infer_static_type(expression: &Expr) -> Option<ExpressionValueType> {
    match &expression.kind {
        ExprKind::Literal(Literal::Null) => Some(ExpressionValueType::Null),
        ExprKind::Literal(Literal::Bool(_)) => Some(ExpressionValueType::Bool),
        ExprKind::Literal(Literal::String(_)) => Some(ExpressionValueType::String),
        ExprKind::Literal(Literal::Number(_)) => Some(ExpressionValueType::Number),
        ExprKind::Array(_) => Some(ExpressionValueType::Array),
        ExprKind::Object(_) => Some(ExpressionValueType::Object),
        ExprKind::Variable(_) | ExprKind::Member { .. } | ExprKind::Index { .. } => None,
        ExprKind::Group(inner) => infer_static_type(inner),
        ExprKind::Unary { operator, .. } => Some(match operator {
            UnaryOperator::Not => ExpressionValueType::Bool,
            UnaryOperator::Plus | UnaryOperator::Minus => ExpressionValueType::Number,
        }),
        ExprKind::Binary {
            operator,
            left,
            right,
        } => match operator {
            BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterEqual
            | BinaryOperator::In
            | BinaryOperator::And
            | BinaryOperator::Or => Some(ExpressionValueType::Bool),
            BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Remainder => Some(ExpressionValueType::Number),
            BinaryOperator::Add => {
                let left = infer_static_type(left);
                let right = infer_static_type(right);
                match (left, right) {
                    (Some(ExpressionValueType::String), _)
                    | (_, Some(ExpressionValueType::String)) => Some(ExpressionValueType::String),
                    (Some(ExpressionValueType::Number), _)
                    | (_, Some(ExpressionValueType::Number)) => Some(ExpressionValueType::Number),
                    _ => None,
                }
            }
            BinaryOperator::Coalesce => match infer_static_type(left) {
                Some(ExpressionValueType::Null) => infer_static_type(right),
                Some(kind) => Some(kind),
                None => None,
            },
        },
        ExprKind::Conditional {
            then_branch,
            else_branch,
            ..
        } => {
            let then_type = infer_static_type(then_branch);
            if then_type == infer_static_type(else_branch) {
                then_type
            } else {
                None
            }
        }
        ExprKind::Call { function, .. } => Some(match function {
            BuiltinFunction::Length | BuiltinFunction::Number => ExpressionValueType::Number,
            BuiltinFunction::Contains | BuiltinFunction::Exists | BuiltinFunction::Matches => {
                ExpressionValueType::Bool
            }
            BuiltinFunction::Keys => ExpressionValueType::Array,
            BuiltinFunction::Upper
            | BuiltinFunction::Lower
            | BuiltinFunction::Json
            | BuiltinFunction::Type
            | BuiltinFunction::String => ExpressionValueType::String,
            BuiltinFunction::FromJson => return None,
        }),
    }
}

#[derive(Debug, Clone)]
pub struct CompiledTemplate {
    source: Arc<str>,
    field: Option<Arc<str>>,
    parts: Vec<TemplatePart>,
    whole_expression: bool,
    references: ReferenceSet,
}

impl CompiledTemplate {
    fn new(
        source: Arc<str>,
        field: Option<Arc<str>>,
        parts: Vec<TemplatePart>,
        whole_expression: bool,
    ) -> Self {
        let mut references = ReferenceSet::default();
        for part in &parts {
            if let TemplatePart::Expression(expression) = part {
                references.extend(expression.references());
            }
        }
        Self {
            source,
            field,
            parts,
            whole_expression,
            references,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    pub fn is_whole_expression(&self) -> bool {
        self.whole_expression
    }

    pub fn references(&self) -> &ReferenceSet {
        &self.references
    }
}

#[derive(Debug, Clone)]
pub enum TemplatePart {
    Literal(String),
    Expression(CompiledExpression),
}

/// Borrowed root map for evaluating without first cloning a combined context.
#[derive(Debug, Default)]
pub struct EvaluationContext<'a> {
    roots: BTreeMap<String, &'a Value>,
}

impl<'a> EvaluationContext<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, root: impl Into<String>, value: &'a Value) -> Option<&'a Value> {
        self.roots.insert(root.into(), value)
    }

    pub fn with_root(mut self, root: impl Into<String>, value: &'a Value) -> Self {
        self.insert(root, value);
        self
    }

    pub fn get(&self, root: &str) -> Option<&'a Value> {
        self.roots.get(root).copied()
    }

    pub fn roots(&self) -> impl Iterator<Item = (&str, &'a Value)> + '_ {
        self.roots
            .iter()
            .map(|(root, value)| (root.as_str(), *value))
    }
}

pub struct EvaluationSession<'a> {
    context: ContextView<'a>,
    budget: Budget<'a>,
}

impl<'a> EvaluationSession<'a> {
    pub fn new(context: &'a Value, limits: &'a EvaluationLimits) -> Self {
        Self {
            context: ContextView::Value(context),
            budget: Budget::new(limits),
        }
    }

    pub fn with_context(context: &'a EvaluationContext<'a>, limits: &'a EvaluationLimits) -> Self {
        Self {
            context: ContextView::Roots(context),
            budget: Budget::new(limits),
        }
    }

    pub fn evaluate(&mut self, expression: &CompiledExpression) -> Result<Value, ExpressionError> {
        let mut evaluator = self.evaluator(expression.source.clone(), expression.field.clone());
        let value = evaluator.evaluate(&expression.root)?;
        evaluator.finish(value, expression.root.span)
    }

    pub fn evaluate_bool(
        &mut self,
        expression: &CompiledExpression,
    ) -> Result<bool, ExpressionError> {
        let value = self.evaluate(expression)?;
        value.as_bool().ok_or_else(|| {
            ExpressionError::new(
                ErrorPhase::Evaluate,
                expression.field.clone(),
                expression.source.clone(),
                expression.root.span,
                format!(
                    "condition expression must produce bool, received {}",
                    evaluator::json_type(&value)
                ),
            )
        })
    }

    pub fn render_template(
        &mut self,
        template: &CompiledTemplate,
    ) -> Result<Value, ExpressionError> {
        if template.whole_expression {
            let TemplatePart::Expression(expression) = &template.parts[0] else {
                unreachable!();
            };
            return self.evaluate(expression);
        }
        let mut output = String::new();
        for part in &template.parts {
            match part {
                TemplatePart::Literal(literal) => {
                    let mut evaluator =
                        self.evaluator(template.source.clone(), template.field.clone());
                    evaluator.check_produced_string(
                        output.len().saturating_add(literal.len()),
                        SourceSpan::at(0),
                    )?;
                    evaluator.allocate(literal.len(), SourceSpan::at(0))?;
                    output.push_str(literal);
                }
                TemplatePart::Expression(expression) => {
                    let mut evaluator =
                        self.evaluator(expression.source.clone(), expression.field.clone());
                    let value = evaluator.evaluate(&expression.root)?;
                    append_embedded(value, &mut evaluator, expression.root.span, &mut output)?;
                }
            }
        }
        let mut evaluator = self.evaluator(template.source.clone(), template.field.clone());
        evaluator.check_produced_string(output.len(), SourceSpan::new(0, template.source.len()))?;
        evaluator.finish(
            RuntimeValue::Owned(Value::String(output)),
            SourceSpan::new(0, template.source.len()),
        )
    }

    pub fn clone_value(
        &mut self,
        value: &Value,
        field: impl Into<String>,
    ) -> Result<Value, ExpressionError> {
        let source: Arc<str> = Arc::from("<literal JSON>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        evaluator.clone_external(value, SourceSpan::new(0, source.len()))
    }

    /// Clone integration-owned text under the same scalar and allocation
    /// budget as expression-produced strings.
    pub fn clone_string(
        &mut self,
        value: &str,
        field: impl Into<String>,
    ) -> Result<String, ExpressionError> {
        let source: Arc<str> = Arc::from("<literal text>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        let span = SourceSpan::new(0, source.len());
        evaluator.check_scalar(value, span)?;
        evaluator.check_produced_string(value.len(), span)?;
        evaluator.spend_fuel(value.len(), span, "cloning integration text")?;
        evaluator.allocate(value.len(), span)?;
        Ok(value.to_string())
    }

    /// Serialize JSON canonically without escaping the session's traversal,
    /// fuel, produced-string, or cumulative-allocation limits.
    pub fn stringify_json(
        &mut self,
        value: &Value,
        field: impl Into<String>,
    ) -> Result<String, ExpressionError> {
        let source: Arc<str> = Arc::from("<assembled JSON>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        let span = SourceSpan::new(0, source.len());
        let mut output = String::new();
        functions::canonical_json(value, &mut evaluator, span, 1, &mut output)?;
        evaluator.check_produced_string(output.len(), span)?;
        Ok(output)
    }

    /// Validate borrowed runtime JSON without cloning it. Integrations use
    /// this before candidate-state or result collection copies so unrelated
    /// large runtime data cannot escape the expression bounds.
    pub fn validate_value(
        &mut self,
        value: &Value,
        field: impl Into<String>,
    ) -> Result<(), ExpressionError> {
        let source: Arc<str> = Arc::from("<runtime JSON>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        let span = SourceSpan::new(0, source.len());
        let mut nodes = 0;
        let mut bytes = 0;
        evaluator.inspect_result(value, 1, &mut nodes, &mut bytes, span)
    }

    /// Validate a borrowed vector as one aggregate JSON array without first
    /// cloning it into a temporary `Value::Array`.
    pub fn validate_array(
        &mut self,
        values: &[Value],
        field: impl Into<String>,
    ) -> Result<(), ExpressionError> {
        let source: Arc<str> = Arc::from("<runtime JSON array>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        evaluator.inspect_result_array(values, SourceSpan::new(0, source.len()))
    }

    /// Assemble integration-owned text while retaining the session-wide
    /// produced-string and allocation bounds.
    pub fn assemble_string(
        &mut self,
        parts: &[&str],
        field: impl Into<String>,
    ) -> Result<String, ExpressionError> {
        let source: Arc<str> = Arc::from("<assembled text>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        let span = SourceSpan::new(0, source.len());
        let bytes = parts
            .iter()
            .try_fold(0usize, |total, part| total.checked_add(part.len()))
            .ok_or_else(|| evaluator.limit_error(span, "produced string byte count overflow"))?;
        evaluator.check_produced_string(bytes, span)?;
        evaluator.spend_fuel(bytes, span, "assembling integration text")?;
        evaluator.allocate(bytes, span)?;
        let mut output = String::with_capacity(bytes);
        for part in parts {
            output.push_str(part);
        }
        Ok(output)
    }

    pub fn charge_allocation(
        &mut self,
        bytes: usize,
        field: impl Into<String>,
    ) -> Result<(), ExpressionError> {
        let source: Arc<str> = Arc::from("<literal JSON>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        evaluator.allocate(bytes, SourceSpan::new(0, source.len()))
    }

    pub fn charge_container_elements(
        &mut self,
        elements: usize,
        field: impl Into<String>,
    ) -> Result<(), ExpressionError> {
        let source: Arc<str> = Arc::from("<literal JSON>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        for _ in 0..elements {
            evaluator.traverse_element(SourceSpan::new(0, source.len()))?;
        }
        Ok(())
    }

    /// Check an integration-assembled result shape without charging fuel.
    /// Renderers use this before each container insertion so a combined limit
    /// cannot be exceeded while the aggregate is still being constructed.
    pub(crate) fn check_result_shape(
        &mut self,
        depth: usize,
        nodes: usize,
        bytes: usize,
        field: impl Into<String>,
    ) -> Result<(), ExpressionError> {
        let source: Arc<str> = Arc::from("<assembled JSON>");
        let evaluator = self.evaluator(source.clone(), Some(Arc::from(field.into())));
        let span = SourceSpan::new(0, source.len());
        if depth > evaluator.budget.limits.max_result_depth {
            return Err(evaluator.limit_error(span, "result exceeds JSON depth limit"));
        }
        if nodes > evaluator.budget.limits.max_result_nodes {
            return Err(evaluator.limit_error(span, "result exceeds JSON node limit"));
        }
        if bytes > evaluator.budget.limits.max_result_bytes {
            return Err(evaluator.limit_error(span, "result exceeds JSON byte limit"));
        }
        Ok(())
    }

    /// Validate and charge for an aggregate result assembled by an integration
    /// layer rather than by one expression AST. Dynamic leaves should already
    /// have passed ordinary expression result validation; this enforces the
    /// final container's combined depth, node, byte, and fuel bounds.
    pub fn charge_result_shape(
        &mut self,
        depth: usize,
        nodes: usize,
        bytes: usize,
        field: impl Into<String>,
    ) -> Result<(), ExpressionError> {
        let field = field.into();
        self.check_result_shape(depth, nodes, bytes, field.clone())?;
        let source: Arc<str> = Arc::from("<assembled JSON>");
        let mut evaluator = self.evaluator(source.clone(), Some(Arc::from(field)));
        let span = SourceSpan::new(0, source.len());
        evaluator.spend_fuel(
            nodes.saturating_add(bytes),
            span,
            "validating assembled result value",
        )
    }

    pub fn remaining_fuel(&self) -> usize {
        self.budget.remaining_fuel()
    }

    pub fn allocated_bytes(&self) -> usize {
        self.budget.allocated_bytes()
    }

    fn evaluator(&mut self, source: Arc<str>, field: Option<Arc<str>>) -> Evaluator<'a, '_, 'a> {
        Evaluator::new(self.context, &mut self.budget, source, field)
    }
}

pub fn compile_expression(
    source: &str,
    limits: &CompilationLimits,
) -> Result<CompiledExpression, ExpressionError> {
    compile_expression_arc(Arc::from(source), None, limits)
}

pub fn compile_expression_for(
    source: &str,
    field: impl Into<String>,
    limits: &CompilationLimits,
) -> Result<CompiledExpression, ExpressionError> {
    compile_expression_arc(Arc::from(source), Some(Arc::from(field.into())), limits)
}

pub(crate) fn compile_expression_arc(
    source: Arc<str>,
    field: Option<Arc<str>>,
    limits: &CompilationLimits,
) -> Result<CompiledExpression, ExpressionError> {
    let tokens = lexer::lex(source.clone(), field.clone(), limits)?;
    let root = parser::parse(source.clone(), field.clone(), tokens, limits)?;
    static_types::validate(&root, source.clone(), field.clone())?;
    let references = references::collect(&root);
    Ok(CompiledExpression {
        source,
        field,
        root,
        references,
    })
}

pub fn compile_template(
    source: &str,
    limits: &CompilationLimits,
) -> Result<CompiledTemplate, ExpressionError> {
    template::compile(Arc::from(source), None, limits)
}

pub fn compile_template_for(
    source: &str,
    field: impl Into<String>,
    limits: &CompilationLimits,
) -> Result<CompiledTemplate, ExpressionError> {
    template::compile(Arc::from(source), Some(Arc::from(field.into())), limits)
}

/// Compile a condition's canonical unwrapped expression or one whole `${...}`
/// wrapper. Embedded/multipart templates are rejected rather than coerced.
pub fn compile_condition_for(
    source: &str,
    field: impl Into<String>,
    limits: &CompilationLimits,
) -> Result<CompiledExpression, ExpressionError> {
    let source = source.trim();
    let field = field.into();
    if source.is_empty() {
        return Err(ExpressionError::new(
            ErrorPhase::Parse,
            Some(Arc::from(field)),
            Arc::from(source),
            SourceSpan::at(0),
            "condition expression cannot be empty",
        ));
    }
    if source.starts_with("${") {
        let template = compile_template_for(source, field, limits)?;
        if template.is_whole_expression() {
            let TemplatePart::Expression(expression) = &template.parts[0] else {
                unreachable!();
            };
            return require_boolean_condition(expression.clone());
        }
        return Err(ExpressionError::new(
            ErrorPhase::Parse,
            template.field.clone(),
            template.source.clone(),
            SourceSpan::new(0, template.source.len()),
            "condition wrapper must contain exactly one whole expression",
        ));
    }
    require_boolean_condition(compile_expression_for(source, field, limits)?)
}

fn require_boolean_condition(
    expression: CompiledExpression,
) -> Result<CompiledExpression, ExpressionError> {
    if let Some(kind) = expression.static_result_type() {
        if kind != ExpressionValueType::Bool {
            return Err(ExpressionError::new(
                ErrorPhase::Parse,
                expression.field.clone(),
                expression.source.clone(),
                expression.root.span,
                format!(
                    "condition expression is statically {}; expected bool",
                    kind.as_str()
                ),
            ));
        }
    }
    Ok(expression)
}

pub fn evaluate(
    expression: &CompiledExpression,
    context: &Value,
    limits: &EvaluationLimits,
) -> Result<Value, ExpressionError> {
    EvaluationSession::new(context, limits).evaluate(expression)
}

pub fn evaluate_bool(
    expression: &CompiledExpression,
    context: &Value,
    limits: &EvaluationLimits,
) -> Result<bool, ExpressionError> {
    EvaluationSession::new(context, limits).evaluate_bool(expression)
}

pub fn render_template(
    template: &CompiledTemplate,
    context: &Value,
    limits: &EvaluationLimits,
) -> Result<Value, ExpressionError> {
    EvaluationSession::new(context, limits).render_template(template)
}

pub fn compile_and_render(
    source: &str,
    context: &Value,
    compilation_limits: &CompilationLimits,
    evaluation_limits: &EvaluationLimits,
) -> Result<Value, ExpressionError> {
    let template = compile_template(source, compilation_limits)?;
    render_template(&template, context, evaluation_limits)
}

fn append_embedded(
    value: RuntimeValue<'_>,
    evaluator: &mut Evaluator<'_, '_, '_>,
    span: SourceSpan,
    output: &mut String,
) -> Result<(), ExpressionError> {
    let value = value
        .as_json()
        .ok_or_else(|| evaluator.error(span, "embedded expression resolved to missing"))?;
    let rendered = match value {
        Value::Null => return Ok(()),
        Value::Bool(true) => "true",
        Value::Bool(false) => "false",
        Value::Number(number) => {
            let rendered = Numeric::from_json(number)
                .and_then(Numeric::canonical)
                .map_err(|message| evaluator.error(span, message))?;
            evaluator.check_produced_string(output.len().saturating_add(rendered.len()), span)?;
            evaluator.allocate(rendered.len().saturating_mul(2), span)?;
            output.push_str(&rendered);
            return Ok(());
        }
        Value::String(value) => {
            evaluator.check_scalar(value, span)?;
            evaluator.check_produced_string(output.len().saturating_add(value.len()), span)?;
            evaluator.allocate(value.len(), span)?;
            output.push_str(value);
            return Ok(());
        }
        Value::Array(_) | Value::Object(_) => {
            return Err(evaluator
                .error(
                    span,
                    "embedded arrays and objects require json(...) or string(...)",
                )
                .correction("wrap the expression with json(...) for deterministic text"));
        }
    };
    evaluator.check_produced_string(output.len().saturating_add(rendered.len()), span)?;
    evaluator.allocate(rendered.len(), span)?;
    output.push_str(rendered);
    Ok(())
}

#[cfg(test)]
mod tests;
