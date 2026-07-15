use std::sync::Arc;

use super::ast::{BinaryOperator, BuiltinFunction, Expr, ExprKind, UnaryOperator};
use super::{ErrorPhase, ExpressionError, ExpressionValueType};

pub(super) fn validate(
    root: &Expr,
    source: Arc<str>,
    field: Option<Arc<str>>,
) -> Result<(), ExpressionError> {
    StaticTypeValidator { source, field }.expression(root)
}

struct StaticTypeValidator {
    source: Arc<str>,
    field: Option<Arc<str>>,
}

impl StaticTypeValidator {
    fn expression(&self, expression: &Expr) -> Result<(), ExpressionError> {
        match &expression.kind {
            ExprKind::Literal(_) | ExprKind::Variable(_) => {}
            ExprKind::Member { target, .. } => self.expression(target)?,
            ExprKind::Index { target, index } => {
                self.expression(target)?;
                self.expression(index)?;
            }
            ExprKind::Array(elements) => {
                for element in elements {
                    self.expression(element)?;
                }
            }
            ExprKind::Object(entries) => {
                for (_, value) in entries {
                    self.expression(value)?;
                }
            }
            ExprKind::Unary { operator, operand } => {
                self.expression(operand)?;
                self.unary(*operator, operand)?;
            }
            ExprKind::Binary {
                operator,
                left,
                right,
            } => {
                self.expression(left)?;
                self.expression(right)?;
                self.binary(*operator, left, right)?;
            }
            ExprKind::Conditional {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expression(condition)?;
                self.expression(then_branch)?;
                self.expression(else_branch)?;
                self.require(
                    condition,
                    &[ExpressionValueType::Bool],
                    "ternary condition",
                    "bool",
                )?;
            }
            ExprKind::Call {
                function,
                arguments,
            } => {
                for argument in arguments {
                    self.expression(argument)?;
                }
                self.call(*function, arguments)?;
            }
            ExprKind::Group(inner) => self.expression(inner)?,
        }
        Ok(())
    }

    fn unary(&self, operator: UnaryOperator, operand: &Expr) -> Result<(), ExpressionError> {
        match operator {
            UnaryOperator::Not => self.require(
                operand,
                &[ExpressionValueType::Bool],
                "operator `!` operand",
                "bool",
            ),
            UnaryOperator::Plus | UnaryOperator::Minus => self.require(
                operand,
                &[ExpressionValueType::Number],
                &format!("operator `{}` operand", unary_symbol(operator)),
                "number",
            ),
        }
    }

    fn binary(
        &self,
        operator: BinaryOperator,
        left: &Expr,
        right: &Expr,
    ) -> Result<(), ExpressionError> {
        match operator {
            BinaryOperator::And | BinaryOperator::Or => {
                self.require_boolean_operand(operator, left, "left")?;
                self.require_boolean_operand(operator, right, "right")
            }
            BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Remainder => {
                let purpose = format!("operator `{}`", binary_symbol(operator));
                self.require(
                    left,
                    &[ExpressionValueType::Number],
                    &format!("{purpose} left operand"),
                    "number",
                )?;
                self.require(
                    right,
                    &[ExpressionValueType::Number],
                    &format!("{purpose} right operand"),
                    "number",
                )
            }
            BinaryOperator::Add => self.require_matching_scalar_pair(
                left,
                right,
                "operator `+`",
                "number/number or string/string",
            ),
            BinaryOperator::Less
            | BinaryOperator::LessEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterEqual => self.require_matching_scalar_pair(
                left,
                right,
                &format!("operator `{}`", binary_symbol(operator)),
                "number/number or string/string",
            ),
            BinaryOperator::In => self.require_membership(left, right, "operator `in`"),
            BinaryOperator::Equal | BinaryOperator::NotEqual | BinaryOperator::Coalesce => Ok(()),
        }
    }

    fn require_boolean_operand(
        &self,
        operator: BinaryOperator,
        operand: &Expr,
        side: &str,
    ) -> Result<(), ExpressionError> {
        let Some(actual) = super::infer_static_type(operand) else {
            return Ok(());
        };
        if actual == ExpressionValueType::Bool {
            return Ok(());
        }
        let symbol = match operator {
            BinaryOperator::And => "&&",
            BinaryOperator::Or => "||",
            _ => unreachable!(),
        };
        let error = self.error(
            operand,
            format!(
                "operator `{symbol}` {side} operand is statically {}; expected bool",
                actual.as_str()
            ),
        );
        if operator == BinaryOperator::Or {
            Err(error
                .correction("use `??` for missing/null fallback; `||` accepts bool operands only"))
        } else {
            Err(error)
        }
    }

    fn require_matching_scalar_pair(
        &self,
        left: &Expr,
        right: &Expr,
        purpose: &str,
        expected: &str,
    ) -> Result<(), ExpressionError> {
        const SCALARS: &[ExpressionValueType] =
            &[ExpressionValueType::Number, ExpressionValueType::String];
        self.require(left, SCALARS, &format!("{purpose} left operand"), expected)?;
        self.require(
            right,
            SCALARS,
            &format!("{purpose} right operand"),
            expected,
        )?;

        let (Some(left_type), Some(right_type)) = (
            super::infer_static_type(left),
            super::infer_static_type(right),
        ) else {
            return Ok(());
        };
        if left_type == right_type {
            return Ok(());
        }
        Err(self.error(
            right,
            format!(
                "{purpose} requires {expected}; operands are statically {}/{}",
                left_type.as_str(),
                right_type.as_str()
            ),
        ))
    }

    fn require_membership(
        &self,
        needle: &Expr,
        container: &Expr,
        purpose: &str,
    ) -> Result<(), ExpressionError> {
        const CONTAINERS: &[ExpressionValueType] = &[
            ExpressionValueType::Array,
            ExpressionValueType::Object,
            ExpressionValueType::String,
        ];
        self.require(
            container,
            CONTAINERS,
            &format!("{purpose} container"),
            "array, object, or string",
        )?;
        if matches!(
            super::infer_static_type(container),
            Some(ExpressionValueType::Object) | Some(ExpressionValueType::String)
        ) {
            self.require(
                needle,
                &[ExpressionValueType::String],
                &format!("{purpose} key/needle"),
                "string",
            )?;
        }
        Ok(())
    }

    fn call(&self, function: BuiltinFunction, arguments: &[Expr]) -> Result<(), ExpressionError> {
        let first = &arguments[0];
        match function {
            BuiltinFunction::Length => self.require(
                first,
                &[
                    ExpressionValueType::Array,
                    ExpressionValueType::Object,
                    ExpressionValueType::String,
                ],
                "function `length` argument",
                "array, object, or string",
            ),
            BuiltinFunction::Contains => {
                self.require_membership(&arguments[1], first, "function `contains`")
            }
            BuiltinFunction::Keys => self.require(
                first,
                &[ExpressionValueType::Object],
                "function `keys` argument",
                "object",
            ),
            BuiltinFunction::Upper | BuiltinFunction::Lower => self.require(
                first,
                &[ExpressionValueType::String],
                &format!("function `{}` argument", function.name()),
                "string",
            ),
            BuiltinFunction::FromJson => self.require(
                first,
                &[ExpressionValueType::String],
                "function `from_json` argument",
                "string",
            ),
            BuiltinFunction::Matches => {
                self.require(
                    first,
                    &[ExpressionValueType::String],
                    "function `matches` value",
                    "string",
                )?;
                self.require(
                    &arguments[1],
                    &[ExpressionValueType::String],
                    "function `matches` pattern",
                    "string",
                )
            }
            BuiltinFunction::Number => self.require(
                first,
                &[ExpressionValueType::Number, ExpressionValueType::String],
                "function `number` argument",
                "number or string",
            ),
            BuiltinFunction::Json
            | BuiltinFunction::Type
            | BuiltinFunction::Exists
            | BuiltinFunction::String => Ok(()),
        }
    }

    fn require(
        &self,
        operand: &Expr,
        allowed: &[ExpressionValueType],
        purpose: &str,
        expected: &str,
    ) -> Result<(), ExpressionError> {
        let Some(actual) = super::infer_static_type(operand) else {
            return Ok(());
        };
        if allowed.contains(&actual) {
            return Ok(());
        }
        Err(self.error(
            operand,
            format!(
                "{purpose} is statically {}; expected {expected}",
                actual.as_str()
            ),
        ))
    }

    fn error(&self, expression: &Expr, message: impl Into<String>) -> ExpressionError {
        ExpressionError::new(
            ErrorPhase::Parse,
            self.field.clone(),
            self.source.clone(),
            expression.span,
            message,
        )
    }
}

fn unary_symbol(operator: UnaryOperator) -> &'static str {
    match operator {
        UnaryOperator::Not => "!",
        UnaryOperator::Plus => "+",
        UnaryOperator::Minus => "-",
    }
}

fn binary_symbol(operator: BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Add => "+",
        BinaryOperator::Subtract => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
        BinaryOperator::Remainder => "%",
        BinaryOperator::Equal => "==",
        BinaryOperator::NotEqual => "!=",
        BinaryOperator::Less => "<",
        BinaryOperator::LessEqual => "<=",
        BinaryOperator::Greater => ">",
        BinaryOperator::GreaterEqual => ">=",
        BinaryOperator::In => "in",
        BinaryOperator::And => "&&",
        BinaryOperator::Or => "||",
        BinaryOperator::Coalesce => "??",
    }
}
