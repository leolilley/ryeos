use std::cmp::Ordering;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::ast::{BinaryOperator, Expr, ExprKind, Literal, UnaryOperator};
use super::error::{ErrorPhase, ExpressionError, SourceSpan};
use super::functions;
use super::limits::EvaluationLimits;
use super::value::Numeric;
use super::EvaluationContext;

#[derive(Clone, Copy)]
pub(crate) enum ContextView<'a> {
    Value(&'a Value),
    Roots(&'a EvaluationContext<'a>),
}

impl<'a> ContextView<'a> {
    fn get(self, root: &str) -> Result<Option<&'a Value>, &'static str> {
        match self {
            Self::Value(Value::Object(object)) => Ok(object.get(root)),
            Self::Value(_) => Err("expression context must be an object"),
            Self::Roots(context) => Ok(context.get(root)),
        }
    }
}

#[derive(Debug)]
pub(crate) enum RuntimeValue<'a> {
    Borrowed(&'a Value),
    Owned(Value),
    Missing(String),
}

impl RuntimeValue<'_> {
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Self::Missing(_) => "missing",
            Self::Borrowed(value) => json_type(value),
            Self::Owned(value) => json_type(value),
        }
    }

    pub(crate) fn as_json(&self) -> Option<&Value> {
        match self {
            Self::Borrowed(value) => Some(value),
            Self::Owned(value) => Some(value),
            Self::Missing(_) => None,
        }
    }
}

pub(crate) struct Budget<'a> {
    pub(crate) limits: &'a EvaluationLimits,
    fuel_remaining: usize,
    allocation_bytes: usize,
    traversed_elements: usize,
}

impl<'a> Budget<'a> {
    pub(crate) fn new(limits: &'a EvaluationLimits) -> Self {
        Self {
            limits,
            fuel_remaining: limits.fuel,
            allocation_bytes: 0,
            traversed_elements: 0,
        }
    }

    pub(crate) fn remaining_fuel(&self) -> usize {
        self.fuel_remaining
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.allocation_bytes
    }
}

pub(crate) struct Evaluator<'context, 'budget, 'limits> {
    context: ContextView<'context>,
    pub(crate) budget: &'budget mut Budget<'limits>,
    source: Arc<str>,
    field: Option<Arc<str>>,
}

impl<'context, 'budget, 'limits> Evaluator<'context, 'budget, 'limits> {
    pub(crate) fn new(
        context: ContextView<'context>,
        budget: &'budget mut Budget<'limits>,
        source: Arc<str>,
        field: Option<Arc<str>>,
    ) -> Self {
        Self {
            context,
            budget,
            source,
            field,
        }
    }

    pub(crate) fn evaluate(
        &mut self,
        expression: &Expr,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        self.spend_fuel(1, expression.span, "evaluating expression node")?;
        match &expression.kind {
            ExprKind::Literal(literal) => self.literal(literal, expression.span),
            ExprKind::Variable(name) => self.variable(name, expression.span),
            ExprKind::Member { target, key } => self.member(target, key, expression.span),
            ExprKind::Index { target, index } => self.index(target, index, expression.span),
            ExprKind::Array(elements) => self.array(elements, expression.span),
            ExprKind::Object(entries) => self.object(entries, expression.span),
            ExprKind::Unary { operator, operand } => {
                self.unary(*operator, operand, expression.span)
            }
            ExprKind::Binary {
                operator,
                left,
                right,
            } => self.binary(*operator, left, right, expression.span),
            ExprKind::Conditional {
                condition,
                then_branch,
                else_branch,
            } => {
                let condition_value = self.evaluate(condition)?;
                if self.require_bool(&condition_value, condition.span, "ternary condition")? {
                    self.evaluate(then_branch)
                } else {
                    self.evaluate(else_branch)
                }
            }
            ExprKind::Call {
                function,
                arguments,
            } => functions::call(*function, arguments, self, expression.span),
            ExprKind::Group(inner) => self.evaluate(inner),
        }
    }

    pub(crate) fn finish(
        &mut self,
        value: RuntimeValue<'context>,
        span: SourceSpan,
    ) -> Result<Value, ExpressionError> {
        let value = match value {
            RuntimeValue::Missing(path) => {
                return Err(self
                    .error(span, format!("required path `{path}` is missing"))
                    .correction("handle an optional path with `??` or exists(...)"));
            }
            RuntimeValue::Borrowed(value) => self.clone_bounded(value, 1, span)?,
            RuntimeValue::Owned(value) => value,
        };
        let mut nodes = 0;
        let mut bytes = 0;
        self.inspect_result(&value, 1, &mut nodes, &mut bytes, span)?;
        Ok(value)
    }

    pub(crate) fn clone_external(
        &mut self,
        value: &Value,
        span: SourceSpan,
    ) -> Result<Value, ExpressionError> {
        let value = self.clone_bounded(value, 1, span)?;
        let mut nodes = 0;
        let mut bytes = 0;
        self.inspect_result(&value, 1, &mut nodes, &mut bytes, span)?;
        Ok(value)
    }

    fn literal(
        &mut self,
        literal: &Literal,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        let value = match literal {
            Literal::Null => Value::Null,
            Literal::Bool(value) => Value::Bool(*value),
            Literal::String(value) => {
                self.check_scalar(value, span)?;
                self.allocate(value.len(), span)?;
                Value::String(value.clone())
            }
            Literal::Number(value) => value
                .to_json()
                .map_err(|message| self.error(span, message))?,
        };
        Ok(RuntimeValue::Owned(value))
    }

    fn variable(
        &mut self,
        name: &str,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        self.check_scalar(name, span)?;
        self.spend_fuel(name.len(), span, "looking up context root")?;
        match self
            .context
            .get(name)
            .map_err(|message| self.error(span, message))?
        {
            Some(value) => Ok(RuntimeValue::Borrowed(value)),
            None => {
                self.allocate(name.len(), span)?;
                let mut path = String::with_capacity(name.len());
                path.push_str(name);
                Ok(RuntimeValue::Missing(path))
            }
        }
    }

    fn member(
        &mut self,
        target: &Expr,
        key: &str,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        self.spend_fuel(1, span, "traversing path segment")?;
        self.check_scalar(key, span)?;
        self.spend_fuel(key.len(), span, "looking up object field")?;
        match self.evaluate(target)? {
            RuntimeValue::Missing(path) => self.extend_missing(path, ".", key, span),
            RuntimeValue::Borrowed(Value::Null) | RuntimeValue::Owned(Value::Null) => {
                self.expression_missing(target, ".", key, span)
            }
            RuntimeValue::Borrowed(Value::Object(object)) => match object.get(key) {
                Some(value) => Ok(RuntimeValue::Borrowed(value)),
                None => self.expression_missing(target, ".", key, span),
            },
            RuntimeValue::Owned(Value::Object(mut object)) => match object.remove(key) {
                Some(value) => Ok(RuntimeValue::Owned(value)),
                None => self.expression_missing(target, ".", key, span),
            },
            value => Err(self.error(
                span,
                format!(
                    "cannot access field `{key}` on {}; expected object",
                    value.type_name()
                ),
            )),
        }
    }

    fn index(
        &mut self,
        target: &Expr,
        index: &Expr,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        self.spend_fuel(1, span, "traversing dynamic index")?;
        let target_value = self.evaluate(target)?;
        let index_value = self.evaluate(index)?;
        let index_json = index_value.as_json().ok_or_else(|| {
            self.error(index.span, "dynamic index expression resolved to missing")
        })?;
        match target_value {
            RuntimeValue::Missing(path) => {
                self.validate_unknown_target_index(index_json, index.span)?;
                self.extend_missing(path, "[?]", "", span)
            }
            RuntimeValue::Borrowed(Value::Null) | RuntimeValue::Owned(Value::Null) => {
                self.validate_unknown_target_index(index_json, index.span)?;
                self.expression_missing(target, "[?]", "", span)
            }
            RuntimeValue::Borrowed(Value::Array(array)) => {
                let position = self.array_position(index_json, index.span)?;
                match position.and_then(|position| array.get(position)) {
                    Some(value) => Ok(RuntimeValue::Borrowed(value)),
                    None => self.expression_missing(target, "[?]", "", span),
                }
            }
            RuntimeValue::Owned(Value::Array(array)) => {
                let position = self.array_position(index_json, index.span)?;
                if let Some(position) = position {
                    for _ in 0..array.len().min(position.saturating_add(1)) {
                        self.traverse_element(span)?;
                    }
                }
                match position.and_then(|position| array.into_iter().nth(position)) {
                    Some(value) => Ok(RuntimeValue::Owned(value)),
                    None => self.expression_missing(target, "[?]", "", span),
                }
            }
            RuntimeValue::Borrowed(Value::Object(object)) => {
                let key = index_json.as_str().ok_or_else(|| {
                    self.error(index.span, "object index must evaluate to a string")
                })?;
                self.check_scalar(key, index.span)?;
                self.spend_fuel(key.len(), index.span, "looking up object key")?;
                match object.get(key) {
                    Some(value) => Ok(RuntimeValue::Borrowed(value)),
                    None => self.expression_missing(target, "[?]", "", span),
                }
            }
            RuntimeValue::Owned(Value::Object(mut object)) => {
                let key = index_json.as_str().ok_or_else(|| {
                    self.error(index.span, "object index must evaluate to a string")
                })?;
                self.check_scalar(key, index.span)?;
                self.spend_fuel(key.len(), index.span, "looking up object key")?;
                match object.remove(key) {
                    Some(value) => Ok(RuntimeValue::Owned(value)),
                    None => self.expression_missing(target, "[?]", "", span),
                }
            }
            value => Err(self.error(
                span,
                format!(
                    "cannot index {}; expected array or object",
                    value.type_name()
                ),
            )),
        }
    }

    /// A missing/null target does not reveal whether the eventual container
    /// would be an array or object. Only index values valid for at least one of
    /// those domains may propagate Missing; universally invalid index values
    /// remain loud errors.
    fn validate_unknown_target_index(
        &mut self,
        value: &Value,
        span: SourceSpan,
    ) -> Result<(), ExpressionError> {
        match value {
            Value::String(key) => {
                self.check_scalar(key, span)?;
                self.spend_fuel(key.len(), span, "validating object index")
            }
            Value::Number(_) => self.array_position(value, span).map(|_| ()),
            _ => Err(self.error(
                span,
                "dynamic index must be a string or non-negative integer",
            )),
        }
    }

    fn array_position(
        &self,
        value: &Value,
        span: SourceSpan,
    ) -> Result<Option<usize>, ExpressionError> {
        let number = value
            .as_number()
            .ok_or_else(|| self.error(span, "array index must be a non-negative integer"))?;
        Numeric::from_json(number)
            .map_err(|message| self.error(span, message))?
            .as_array_index()
            .map_err(|message| self.error(span, message))
    }

    fn extend_missing(
        &mut self,
        mut path: String,
        separator: &str,
        suffix: &str,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        let added = separator.len().saturating_add(suffix.len());
        self.allocate(added, span)?;
        path.reserve(added);
        path.push_str(separator);
        path.push_str(suffix);
        Ok(RuntimeValue::Missing(path))
    }

    fn expression_missing(
        &mut self,
        target: &Expr,
        separator: &str,
        suffix: &str,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        let length = path_text_len(target)
            .saturating_add(separator.len())
            .saturating_add(suffix.len());
        self.allocate(length, span)?;
        let mut path = String::with_capacity(length);
        write_path_text(target, &mut path);
        path.push_str(separator);
        path.push_str(suffix);
        Ok(RuntimeValue::Missing(path))
    }

    fn array(
        &mut self,
        elements: &[Expr],
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        self.allocate(elements.len() * std::mem::size_of::<Value>(), span)?;
        let mut output = Vec::with_capacity(elements.len());
        for element in elements {
            let evaluated = self.evaluate(element)?;
            output.push(self.own(evaluated, element.span, "array element")?);
        }
        Ok(RuntimeValue::Owned(Value::Array(output)))
    }

    fn object(
        &mut self,
        entries: &[(String, Expr)],
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        self.allocate(
            entries
                .len()
                .saturating_mul(std::mem::size_of::<(String, Value)>() * 2),
            span,
        )?;
        let mut output = Map::new();
        for (key, expression) in entries {
            self.check_scalar(key, expression.span)?;
            self.allocate(key.len(), expression.span)?;
            let evaluated = self.evaluate(expression)?;
            let value = self.own(evaluated, expression.span, "object value")?;
            output.insert(key.clone(), value);
        }
        Ok(RuntimeValue::Owned(Value::Object(output)))
    }

    fn unary(
        &mut self,
        operator: UnaryOperator,
        operand: &Expr,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        let value = self.evaluate(operand)?;
        match operator {
            UnaryOperator::Not => Ok(RuntimeValue::Owned(Value::Bool(!self.require_bool(
                &value,
                operand.span,
                "operator `!`",
            )?))),
            UnaryOperator::Plus | UnaryOperator::Minus => {
                let number = self.require_number(&value, operand.span, "unary numeric operator")?;
                let result = if operator == UnaryOperator::Plus {
                    number.positive()
                } else {
                    number.negate()
                }
                .map_err(|message| self.error(span, message))?;
                Ok(RuntimeValue::Owned(
                    result
                        .to_json()
                        .map_err(|message| self.error(span, message))?,
                ))
            }
        }
    }

    fn binary(
        &mut self,
        operator: BinaryOperator,
        left: &Expr,
        right: &Expr,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        match operator {
            BinaryOperator::And => {
                let left_value = self.evaluate(left)?;
                if !self.require_bool(&left_value, left.span, "operator `&&`")? {
                    return Ok(RuntimeValue::Owned(Value::Bool(false)));
                }
                let right_value = self.evaluate(right)?;
                return Ok(RuntimeValue::Owned(Value::Bool(self.require_bool(
                    &right_value,
                    right.span,
                    "operator `&&`",
                )?)));
            }
            BinaryOperator::Or => {
                let left_value = self.evaluate(left)?;
                if self.require_bool(&left_value, left.span, "operator `||`")? {
                    return Ok(RuntimeValue::Owned(Value::Bool(true)));
                }
                let right_value = self.evaluate(right)?;
                return Ok(RuntimeValue::Owned(Value::Bool(self.require_bool(
                    &right_value,
                    right.span,
                    "operator `||`",
                )?)));
            }
            BinaryOperator::Coalesce => {
                let left_value = self.evaluate(left)?;
                if matches!(
                    &left_value,
                    RuntimeValue::Missing(_)
                        | RuntimeValue::Borrowed(Value::Null)
                        | RuntimeValue::Owned(Value::Null)
                ) {
                    return self.evaluate(right);
                }
                return Ok(left_value);
            }
            _ => {}
        }
        let left_value = self.evaluate(left)?;
        let right_value = self.evaluate(right)?;
        match operator {
            BinaryOperator::Add => self.add(left_value, right_value, span),
            BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Remainder => {
                self.numeric_binary(operator, &left_value, &right_value, span)
            }
            BinaryOperator::Equal | BinaryOperator::NotEqual => {
                let equal = self.deep_equal(&left_value, &right_value, 1, span)?;
                Ok(RuntimeValue::Owned(Value::Bool(
                    if operator == BinaryOperator::Equal {
                        equal
                    } else {
                        !equal
                    },
                )))
            }
            BinaryOperator::Less
            | BinaryOperator::LessEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterEqual => {
                let ordering = self.order(&left_value, &right_value, span)?;
                let result = match operator {
                    BinaryOperator::Less => ordering == Ordering::Less,
                    BinaryOperator::LessEqual => ordering != Ordering::Greater,
                    BinaryOperator::Greater => ordering == Ordering::Greater,
                    BinaryOperator::GreaterEqual => ordering != Ordering::Less,
                    _ => unreachable!(),
                };
                Ok(RuntimeValue::Owned(Value::Bool(result)))
            }
            BinaryOperator::In => functions::contains_values(&right_value, &left_value, self, span),
            BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Coalesce => unreachable!(),
        }
    }

    fn add(
        &mut self,
        left: RuntimeValue<'context>,
        right: RuntimeValue<'context>,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        match (left.as_json(), right.as_json()) {
            (Some(Value::String(left)), Some(Value::String(right))) => {
                self.check_scalar(left, span)?;
                self.check_scalar(right, span)?;
                let length = left.len().checked_add(right.len()).ok_or_else(|| {
                    self.limit_error(span, "string concatenation length overflow")
                })?;
                self.spend_fuel(length, span, "concatenating strings")?;
                self.check_produced_string(length, span)?;
                self.allocate(length, span)?;
                let mut output = String::with_capacity(length);
                output.push_str(left);
                output.push_str(right);
                Ok(RuntimeValue::Owned(Value::String(output)))
            }
            _ => self.numeric_binary(BinaryOperator::Add, &left, &right, span),
        }
    }

    fn numeric_binary(
        &self,
        operator: BinaryOperator,
        left: &RuntimeValue<'_>,
        right: &RuntimeValue<'_>,
        span: SourceSpan,
    ) -> Result<RuntimeValue<'context>, ExpressionError> {
        let left_type = left.type_name();
        let right_type = right.type_name();
        let left =
            self.require_number(left, span, "numeric operator")
                .map_err(|_| {
                    self.error(
                span,
                format!("numeric operator requires numbers; received {left_type} and {right_type}"),
            )
                })?;
        let right =
            self.require_number(right, span, "numeric operator")
                .map_err(|_| {
                    self.error(
                span,
                format!("numeric operator requires numbers; received {left_type} and {right_type}"),
            )
                })?;
        let result = match operator {
            BinaryOperator::Add => left.add(right),
            BinaryOperator::Subtract => left.subtract(right),
            BinaryOperator::Multiply => left.multiply(right),
            BinaryOperator::Divide => left.divide(right),
            BinaryOperator::Remainder => left.remainder(right),
            _ => unreachable!(),
        }
        .map_err(|message| self.error(span, message))?;
        Ok(RuntimeValue::Owned(
            result
                .to_json()
                .map_err(|message| self.error(span, message))?,
        ))
    }

    pub(crate) fn own(
        &mut self,
        value: RuntimeValue<'context>,
        span: SourceSpan,
        purpose: &str,
    ) -> Result<Value, ExpressionError> {
        match value {
            RuntimeValue::Borrowed(value) => self.clone_bounded(value, 1, span),
            RuntimeValue::Owned(value) => Ok(value),
            RuntimeValue::Missing(path) => {
                Err(self.error(span, format!("{purpose} references missing path `{path}`")))
            }
        }
    }

    pub(crate) fn require_bool(
        &self,
        value: &RuntimeValue<'_>,
        span: SourceSpan,
        purpose: &str,
    ) -> Result<bool, ExpressionError> {
        match value.as_json() {
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(self.error(
                span,
                format!("{purpose} requires bool, received {}", value.type_name()),
            )),
        }
    }

    pub(crate) fn require_number(
        &self,
        value: &RuntimeValue<'_>,
        span: SourceSpan,
        purpose: &str,
    ) -> Result<Numeric, ExpressionError> {
        match value.as_json() {
            Some(Value::Number(number)) => {
                Numeric::from_json(number).map_err(|message| self.error(span, message))
            }
            _ => Err(self.error(
                span,
                format!("{purpose} requires number, received {}", value.type_name()),
            )),
        }
    }

    pub(crate) fn deep_equal(
        &mut self,
        left: &RuntimeValue<'_>,
        right: &RuntimeValue<'_>,
        depth: usize,
        span: SourceSpan,
    ) -> Result<bool, ExpressionError> {
        let left = left
            .as_json()
            .ok_or_else(|| self.error(span, "equality cannot be applied to a missing path"))?;
        let right = right
            .as_json()
            .ok_or_else(|| self.error(span, "equality cannot be applied to a missing path"))?;
        self.deep_equal_json(left, right, depth, span)
    }

    pub(crate) fn deep_equal_json(
        &mut self,
        left: &Value,
        right: &Value,
        depth: usize,
        span: SourceSpan,
    ) -> Result<bool, ExpressionError> {
        self.spend_fuel(1, span, "comparing values")?;
        if depth > self.budget.limits.max_traversal_depth {
            return Err(self.limit_error(span, "deep equality exceeds traversal depth limit"));
        }
        match (left, right) {
            (Value::Number(left), Value::Number(right)) => {
                let left = Numeric::from_json(left).map_err(|message| self.error(span, message))?;
                let right =
                    Numeric::from_json(right).map_err(|message| self.error(span, message))?;
                Ok(left.compare(right) == Ordering::Equal)
            }
            (Value::String(left), Value::String(right)) => {
                self.check_scalar(left, span)?;
                self.check_scalar(right, span)?;
                self.spend_fuel(
                    left.len().saturating_add(right.len()),
                    span,
                    "comparing strings",
                )?;
                Ok(left == right)
            }
            (Value::Array(left), Value::Array(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                for (left, right) in left.iter().zip(right) {
                    self.traverse_element(span)?;
                    if !self.deep_equal_json(left, right, depth + 1, span)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (Value::Object(left), Value::Object(right)) => {
                if left.len() != right.len() {
                    return Ok(false);
                }
                for (key, left) in left {
                    self.traverse_element(span)?;
                    self.check_scalar(key, span)?;
                    self.spend_fuel(key.len(), span, "comparing object keys")?;
                    let Some(right) = right.get(key) else {
                        return Ok(false);
                    };
                    if !self.deep_equal_json(left, right, depth + 1, span)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(left == right),
        }
    }

    fn order(
        &mut self,
        left: &RuntimeValue<'_>,
        right: &RuntimeValue<'_>,
        span: SourceSpan,
    ) -> Result<Ordering, ExpressionError> {
        match (left.as_json(), right.as_json()) {
            (Some(Value::Number(left)), Some(Value::Number(right))) => {
                let left = Numeric::from_json(left).map_err(|message| self.error(span, message))?;
                let right =
                    Numeric::from_json(right).map_err(|message| self.error(span, message))?;
                Ok(left.compare(right))
            }
            (Some(Value::String(left)), Some(Value::String(right))) => {
                self.check_scalar(left, span)?;
                self.check_scalar(right, span)?;
                self.spend_fuel(
                    left.len().saturating_add(right.len()),
                    span,
                    "ordering strings",
                )?;
                Ok(left.chars().cmp(right.chars()))
            }
            _ => Err(self.error(
                span,
                format!(
                    "ordering requires number/number or string/string; received {}/{}",
                    left.type_name(),
                    right.type_name()
                ),
            )),
        }
    }

    fn clone_bounded(
        &mut self,
        value: &Value,
        depth: usize,
        span: SourceSpan,
    ) -> Result<Value, ExpressionError> {
        if depth > self.budget.limits.max_traversal_depth {
            return Err(self.limit_error(span, "selected value exceeds traversal depth limit"));
        }
        self.spend_fuel(1, span, "materializing selected value")?;
        match value {
            Value::Null => Ok(Value::Null),
            Value::Bool(value) => Ok(Value::Bool(*value)),
            Value::Number(value) => Ok(Value::Number(value.clone())),
            Value::String(value) => {
                self.check_scalar(value, span)?;
                self.spend_fuel(value.len(), span, "materializing selected string")?;
                self.allocate(value.len(), span)?;
                Ok(Value::String(value.clone()))
            }
            Value::Array(values) => {
                self.allocate(values.len() * std::mem::size_of::<Value>(), span)?;
                let mut output = Vec::with_capacity(values.len());
                for value in values {
                    self.traverse_element(span)?;
                    output.push(self.clone_bounded(value, depth + 1, span)?);
                }
                Ok(Value::Array(output))
            }
            Value::Object(values) => {
                self.allocate(
                    values
                        .len()
                        .saturating_mul(std::mem::size_of::<(String, Value)>() * 2),
                    span,
                )?;
                let mut output = Map::new();
                for (key, value) in values {
                    self.traverse_element(span)?;
                    self.check_scalar(key, span)?;
                    self.spend_fuel(key.len(), span, "materializing object key")?;
                    self.allocate(key.len(), span)?;
                    output.insert(key.clone(), self.clone_bounded(value, depth + 1, span)?);
                }
                Ok(Value::Object(output))
            }
        }
    }

    pub(crate) fn inspect_result(
        &mut self,
        value: &Value,
        depth: usize,
        nodes: &mut usize,
        bytes: &mut usize,
        span: SourceSpan,
    ) -> Result<(), ExpressionError> {
        if depth > self.budget.limits.max_result_depth {
            return Err(self.limit_error(span, "result exceeds JSON depth limit"));
        }
        *nodes += 1;
        if *nodes > self.budget.limits.max_result_nodes {
            return Err(self.limit_error(span, "result exceeds JSON node limit"));
        }
        self.spend_fuel(1, span, "validating result value")?;
        match value {
            Value::Null => self.add_result_bytes(bytes, 4, span)?,
            Value::Bool(value) => self.add_result_bytes(bytes, if *value { 4 } else { 5 }, span)?,
            Value::Number(number) => {
                self.add_result_bytes(bytes, number.to_string().len(), span)?;
            }
            Value::String(value) => {
                self.check_scalar(value, span)?;
                self.add_result_bytes(bytes, json_string_bytes(value), span)?;
            }
            Value::Array(values) => {
                self.add_result_bytes(
                    bytes,
                    2usize.saturating_add(values.len().saturating_sub(1)),
                    span,
                )?;
                for value in values {
                    self.traverse_element(span)?;
                    self.inspect_result(value, depth + 1, nodes, bytes, span)?;
                }
            }
            Value::Object(values) => {
                self.add_result_bytes(
                    bytes,
                    2usize.saturating_add(values.len().saturating_sub(1)),
                    span,
                )?;
                for (key, value) in values {
                    self.traverse_element(span)?;
                    self.check_scalar(key, span)?;
                    self.add_result_bytes(bytes, json_string_bytes(key).saturating_add(1), span)?;
                    self.inspect_result(value, depth + 1, nodes, bytes, span)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn inspect_result_array(
        &mut self,
        values: &[Value],
        span: SourceSpan,
    ) -> Result<(), ExpressionError> {
        if self.budget.limits.max_result_depth < 1 {
            return Err(self.limit_error(span, "result exceeds JSON depth limit"));
        }
        if self.budget.limits.max_result_nodes < 1 {
            return Err(self.limit_error(span, "result exceeds JSON node limit"));
        }
        self.spend_fuel(1, span, "validating result value")?;
        let mut nodes = 1;
        let mut bytes = 0;
        self.add_result_bytes(
            &mut bytes,
            2usize.saturating_add(values.len().saturating_sub(1)),
            span,
        )?;
        for value in values {
            self.traverse_element(span)?;
            self.inspect_result(value, 2, &mut nodes, &mut bytes, span)?;
        }
        Ok(())
    }

    fn add_result_bytes(
        &mut self,
        total: &mut usize,
        amount: usize,
        span: SourceSpan,
    ) -> Result<(), ExpressionError> {
        *total = (*total).saturating_add(amount);
        if *total > self.budget.limits.max_result_bytes {
            return Err(self.limit_error(span, "result exceeds JSON byte limit"));
        }
        self.spend_fuel(amount, span, "validating serialized result bytes")
    }

    pub(crate) fn spend_fuel(
        &mut self,
        amount: usize,
        span: SourceSpan,
        purpose: &str,
    ) -> Result<(), ExpressionError> {
        if self.budget.fuel_remaining < amount {
            return Err(
                self.limit_error(span, format!("evaluation fuel exhausted while {purpose}"))
            );
        }
        self.budget.fuel_remaining -= amount;
        Ok(())
    }

    pub(crate) fn allocate(
        &mut self,
        bytes: usize,
        span: SourceSpan,
    ) -> Result<(), ExpressionError> {
        let Some(total) = self.budget.allocation_bytes.checked_add(bytes) else {
            return Err(self.limit_error(span, "evaluation allocation counter overflow"));
        };
        if total > self.budget.limits.max_allocation_bytes {
            return Err(self.limit_error(
                span,
                format!(
                    "evaluation exceeds cumulative allocation limit of {} bytes",
                    self.budget.limits.max_allocation_bytes
                ),
            ));
        }
        self.budget.allocation_bytes = total;
        Ok(())
    }

    pub(crate) fn traverse_element(&mut self, span: SourceSpan) -> Result<(), ExpressionError> {
        self.budget.traversed_elements += 1;
        if self.budget.traversed_elements > self.budget.limits.max_container_elements {
            return Err(self.limit_error(span, "container traversal element limit exceeded"));
        }
        self.spend_fuel(1, span, "traversing container element")
    }

    pub(crate) fn check_scalar(
        &self,
        value: &str,
        span: SourceSpan,
    ) -> Result<(), ExpressionError> {
        if value.len() > self.budget.limits.max_scalar_bytes {
            return Err(self.limit_error(
                span,
                format!(
                    "scalar is {} bytes; limit is {}",
                    value.len(),
                    self.budget.limits.max_scalar_bytes
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn check_produced_string(
        &self,
        bytes: usize,
        span: SourceSpan,
    ) -> Result<(), ExpressionError> {
        if bytes > self.budget.limits.max_produced_string_bytes {
            return Err(self.limit_error(
                span,
                format!(
                    "produced string is {bytes} bytes; limit is {}",
                    self.budget.limits.max_produced_string_bytes
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn error(&self, span: SourceSpan, message: impl Into<String>) -> ExpressionError {
        ExpressionError::new(
            ErrorPhase::Evaluate,
            self.field.clone(),
            self.source.clone(),
            span,
            message,
        )
    }

    pub(crate) fn limit_error(
        &self,
        span: SourceSpan,
        message: impl Into<String>,
    ) -> ExpressionError {
        ExpressionError::new(
            ErrorPhase::Limit,
            self.field.clone(),
            self.source.clone(),
            span,
            message,
        )
    }
}

pub(crate) fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn path_text_len(expression: &Expr) -> usize {
    match &expression.kind {
        ExprKind::Variable(root) => root.len(),
        ExprKind::Member { target, key } => path_text_len(target)
            .saturating_add(1)
            .saturating_add(key.len()),
        ExprKind::Index { target, .. } => path_text_len(target).saturating_add(3),
        ExprKind::Group(inner) => path_text_len(inner),
        _ => "<expression>".len(),
    }
}

fn write_path_text(expression: &Expr, output: &mut String) {
    match &expression.kind {
        ExprKind::Variable(root) => output.push_str(root),
        ExprKind::Member { target, key } => {
            write_path_text(target, output);
            output.push('.');
            output.push_str(key);
        }
        ExprKind::Index { target, .. } => {
            write_path_text(target, output);
            output.push_str("[?]");
        }
        ExprKind::Group(inner) => write_path_text(inner, output),
        _ => output.push_str("<expression>"),
    }
}

pub(crate) fn json_string_bytes(value: &str) -> usize {
    value.chars().fold(2usize, |bytes, character| {
        bytes.saturating_add(match character {
            '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
            character if character <= '\u{1f}' => 6,
            character => character.len_utf8(),
        })
    })
}
