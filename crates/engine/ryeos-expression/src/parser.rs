use std::collections::HashSet;
use std::sync::Arc;

use super::ast::{BinaryOperator, BuiltinFunction, Expr, ExprKind, Literal, UnaryOperator};
use super::error::{ErrorPhase, ExpressionError, SourceSpan};
use super::limits::CompilationLimits;
use super::token::{Token, TokenKind};
use super::value::Numeric;

pub(crate) fn parse(
    source: Arc<str>,
    field: Option<Arc<str>>,
    tokens: Vec<Token>,
    limits: &CompilationLimits,
) -> Result<Expr, ExpressionError> {
    let mut parser = Parser {
        source,
        field,
        tokens,
        cursor: 0,
        limits,
        literal_elements: 0,
    };
    let expression = parser.expression(0, 1)?;
    parser.validate_ast_depth(&expression)?;
    if !matches!(&parser.current().kind, TokenKind::Eof) {
        return Err(parser.error(
            parser.current().span,
            format!(
                "unexpected {}; expected end of expression",
                parser.current().kind.description()
            ),
        ));
    }
    Ok(expression)
}

struct Parser<'a> {
    source: Arc<str>,
    field: Option<Arc<str>>,
    tokens: Vec<Token>,
    cursor: usize,
    limits: &'a CompilationLimits,
    literal_elements: usize,
}

impl Parser<'_> {
    fn expression(&mut self, min_binding_power: u8, depth: usize) -> Result<Expr, ExpressionError> {
        self.check_depth(depth)?;
        let mut left = self.prefix(depth)?;
        loop {
            if matches!(
                &self.current().kind,
                TokenKind::Dot | TokenKind::LeftBracket
            ) {
                if 20 < min_binding_power {
                    break;
                }
                left = self.postfix(left, depth)?;
                continue;
            }
            if matches!(&self.current().kind, TokenKind::Question) {
                if 1 < min_binding_power {
                    break;
                }
                let start = left.span.start;
                self.advance();
                let then_branch = self.expression(0, depth + 1)?;
                self.expect(
                    |kind| matches!(kind, TokenKind::Colon),
                    "`:` in conditional expression",
                )?;
                let else_branch = self.expression(1, depth + 1)?;
                let end = else_branch.span.end;
                left = Expr {
                    kind: ExprKind::Conditional {
                        condition: Box::new(left),
                        then_branch: Box::new(then_branch),
                        else_branch: Box::new(else_branch),
                    },
                    span: SourceSpan::new(start, end),
                };
                continue;
            }
            let Some((left_bp, right_bp, operator)) = infix_operator(&self.current().kind) else {
                break;
            };
            if left_bp < min_binding_power {
                break;
            }
            let operator_span = self.advance().span;
            let right = self.expression(right_bp, depth + 1)?;
            if mixes_nullish_and_boolean(operator, &left, &right) {
                return Err(self
                    .error(
                        operator_span,
                        "`??` cannot be mixed with `&&` or `||` without parentheses",
                    )
                    .correction("parenthesize either the nullish or boolean expression"));
            }
            let span = SourceSpan::new(left.span.start, right.span.end);
            left = Expr {
                kind: ExprKind::Binary {
                    operator,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }
        Ok(left)
    }

    fn prefix(&mut self, depth: usize) -> Result<Expr, ExpressionError> {
        let token = self.advance();
        let span = token.span;
        match token.kind {
            TokenKind::Null => Ok(literal(Literal::Null, span)),
            TokenKind::True => Ok(literal(Literal::Bool(true), span)),
            TokenKind::False => Ok(literal(Literal::Bool(false), span)),
            TokenKind::String(value) => Ok(literal(Literal::String(value), span)),
            TokenKind::Number(value) => {
                let numeric = Numeric::parse_unsigned_token(&value)
                    .map_err(|message| self.error(span, message))?;
                Ok(literal(Literal::Number(numeric), span))
            }
            TokenKind::Identifier(name) => self.identifier(name, span, depth),
            kind @ (TokenKind::Bang | TokenKind::Plus | TokenKind::Minus) => {
                let operator = match kind {
                    TokenKind::Bang => UnaryOperator::Not,
                    TokenKind::Plus => UnaryOperator::Plus,
                    TokenKind::Minus => UnaryOperator::Minus,
                    _ => unreachable!(),
                };
                let operand = self.expression(18, depth + 1)?;
                Ok(Expr {
                    span: SourceSpan::new(span.start, operand.span.end),
                    kind: ExprKind::Unary {
                        operator,
                        operand: Box::new(operand),
                    },
                })
            }
            TokenKind::LeftParen => {
                let expression = self.expression(0, depth + 1)?;
                let close = self.expect(|kind| matches!(kind, TokenKind::RightParen), "`)`")?;
                Ok(Expr {
                    span: SourceSpan::new(span.start, close.span.end),
                    kind: ExprKind::Group(Box::new(expression)),
                })
            }
            TokenKind::LeftBracket => self.array(span, depth),
            TokenKind::LeftBrace => self.object(span, depth),
            other => Err(self.error(
                span,
                format!("expected an expression, found {}", other.description()),
            )),
        }
    }

    fn identifier(
        &mut self,
        name: String,
        span: SourceSpan,
        depth: usize,
    ) -> Result<Expr, ExpressionError> {
        if !matches!(&self.current().kind, TokenKind::LeftParen) {
            return Ok(Expr {
                kind: ExprKind::Variable(name),
                span,
            });
        }
        self.advance();
        let Some(function) = BuiltinFunction::from_name(&name) else {
            return Err(self.error(span, format!("unknown function `{name}`")));
        };
        let mut arguments = Vec::new();
        if !matches!(&self.current().kind, TokenKind::RightParen) {
            loop {
                if arguments.len() >= self.limits.max_function_arguments {
                    return Err(self.limit_error(
                        self.current().span,
                        format!(
                            "function call exceeds argument limit of {}",
                            self.limits.max_function_arguments
                        ),
                    ));
                }
                arguments.push(self.expression(0, depth + 1)?);
                if !matches!(&self.current().kind, TokenKind::Comma) {
                    break;
                }
                self.advance();
                if matches!(&self.current().kind, TokenKind::RightParen) {
                    return Err(self.error(self.current().span, "trailing function argument comma"));
                }
            }
        }
        let close = self.expect(|kind| matches!(kind, TokenKind::RightParen), "`)`")?;
        if arguments.len() != function.arity() {
            return Err(self.error(
                SourceSpan::new(span.start, close.span.end),
                format!(
                    "function `{}` expects {} argument{}, received {}",
                    function.name(),
                    function.arity(),
                    if function.arity() == 1 { "" } else { "s" },
                    arguments.len()
                ),
            ));
        }
        if function == BuiltinFunction::Exists && !is_path_expression(&arguments[0]) {
            return Err(self
                .error(arguments[0].span, "exists(...) requires a path expression")
                .correction("pass a context path such as exists(inputs.optional)"));
        }
        Ok(Expr {
            kind: ExprKind::Call {
                function,
                arguments,
            },
            span: SourceSpan::new(span.start, close.span.end),
        })
    }

    fn postfix(&mut self, target: Expr, depth: usize) -> Result<Expr, ExpressionError> {
        match self.advance() {
            Token {
                kind: TokenKind::Dot,
                ..
            } => {
                let token = self.advance();
                let TokenKind::Identifier(key) = token.kind else {
                    return Err(self
                        .error(token.span, "dot access requires a non-reserved identifier")
                        .correction("use bracket access with a quoted key"));
                };
                Ok(Expr {
                    span: SourceSpan::new(target.span.start, token.span.end),
                    kind: ExprKind::Member {
                        target: Box::new(target),
                        key,
                    },
                })
            }
            Token {
                kind: TokenKind::LeftBracket,
                ..
            } => {
                let index = self.expression(0, depth + 1)?;
                let close = self.expect(|kind| matches!(kind, TokenKind::RightBracket), "`]`")?;
                Ok(Expr {
                    span: SourceSpan::new(target.span.start, close.span.end),
                    kind: ExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                })
            }
            _ => unreachable!(),
        }
    }

    fn array(&mut self, open: SourceSpan, depth: usize) -> Result<Expr, ExpressionError> {
        let mut elements = Vec::new();
        if !matches!(&self.current().kind, TokenKind::RightBracket) {
            loop {
                self.charge_literal_element(self.current().span)?;
                elements.push(self.expression(0, depth + 1)?);
                if !matches!(&self.current().kind, TokenKind::Comma) {
                    break;
                }
                self.advance();
                if matches!(&self.current().kind, TokenKind::RightBracket) {
                    return Err(self.error(self.current().span, "trailing array element comma"));
                }
            }
        }
        let close = self.expect(|kind| matches!(kind, TokenKind::RightBracket), "`]`")?;
        Ok(Expr {
            kind: ExprKind::Array(elements),
            span: SourceSpan::new(open.start, close.span.end),
        })
    }

    fn object(&mut self, open: SourceSpan, depth: usize) -> Result<Expr, ExpressionError> {
        let mut entries = Vec::new();
        let mut keys = HashSet::new();
        if !matches!(&self.current().kind, TokenKind::RightBrace) {
            loop {
                self.charge_literal_element(self.current().span)?;
                let key_token = self.advance();
                let key = match key_token.kind {
                    TokenKind::Identifier(key) | TokenKind::String(key) => key,
                    other => {
                        return Err(self.error(
                            key_token.span,
                            format!(
                                "object key must be an identifier or quoted string, found {}",
                                other.description()
                            ),
                        ));
                    }
                };
                if !keys.insert(key.clone()) {
                    return Err(self.error(key_token.span, format!("duplicate object key `{key}`")));
                }
                self.expect(
                    |kind| matches!(kind, TokenKind::Colon),
                    "`:` after object key",
                )?;
                let value = self.expression(0, depth + 1)?;
                entries.push((key, value));
                if !matches!(&self.current().kind, TokenKind::Comma) {
                    break;
                }
                self.advance();
                if matches!(&self.current().kind, TokenKind::RightBrace) {
                    return Err(self.error(self.current().span, "trailing object entry comma"));
                }
            }
        }
        let close = self.expect(|kind| matches!(kind, TokenKind::RightBrace), "`}`")?;
        Ok(Expr {
            kind: ExprKind::Object(entries),
            span: SourceSpan::new(open.start, close.span.end),
        })
    }

    fn check_depth(&self, depth: usize) -> Result<(), ExpressionError> {
        if depth > self.limits.max_ast_depth {
            return Err(self.limit_error(
                self.current().span,
                format!(
                    "expression exceeds AST depth limit of {}",
                    self.limits.max_ast_depth
                ),
            ));
        }
        Ok(())
    }

    fn validate_ast_depth(&self, root: &Expr) -> Result<(), ExpressionError> {
        let mut stack = vec![(root, 1usize)];
        while let Some((expression, depth)) = stack.pop() {
            if depth > self.limits.max_ast_depth {
                return Err(self.limit_error(
                    expression.span,
                    format!(
                        "expression exceeds AST depth limit of {}",
                        self.limits.max_ast_depth
                    ),
                ));
            }
            let next = depth + 1;
            match &expression.kind {
                ExprKind::Literal(_) | ExprKind::Variable(_) => {}
                ExprKind::Member { target, .. } => stack.push((target, next)),
                ExprKind::Index { target, index } => {
                    stack.push((target, next));
                    stack.push((index, next));
                }
                ExprKind::Array(elements) => {
                    stack.extend(elements.iter().map(|element| (element, next)));
                }
                ExprKind::Object(entries) => {
                    stack.extend(entries.iter().map(|(_, value)| (value, next)));
                }
                ExprKind::Unary { operand, .. } | ExprKind::Group(operand) => {
                    stack.push((operand, next));
                }
                ExprKind::Binary { left, right, .. } => {
                    stack.push((left, next));
                    stack.push((right, next));
                }
                ExprKind::Conditional {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    stack.push((condition, next));
                    stack.push((then_branch, next));
                    stack.push((else_branch, next));
                }
                ExprKind::Call { arguments, .. } => {
                    stack.extend(arguments.iter().map(|argument| (argument, next)));
                }
            }
        }
        Ok(())
    }

    fn charge_literal_element(&mut self, span: SourceSpan) -> Result<(), ExpressionError> {
        self.literal_elements += 1;
        if self.literal_elements > self.limits.max_literal_elements {
            return Err(self.limit_error(
                span,
                format!(
                    "expression exceeds literal element limit of {}",
                    self.limits.max_literal_elements
                ),
            ));
        }
        Ok(())
    }

    fn expect(
        &mut self,
        predicate: impl FnOnce(&TokenKind) -> bool,
        expected: &str,
    ) -> Result<Token, ExpressionError> {
        if predicate(&self.current().kind) {
            Ok(self.advance())
        } else {
            Err(self.error(
                self.current().span,
                format!(
                    "expected {expected}, found {}",
                    self.current().kind.description()
                ),
            ))
        }
    }

    fn current(&self) -> &Token {
        &self.tokens[self.cursor]
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.cursor].clone();
        if !matches!(&token.kind, TokenKind::Eof) {
            self.cursor += 1;
        }
        token
    }

    fn error(&self, span: SourceSpan, message: impl Into<String>) -> ExpressionError {
        ExpressionError::new(
            ErrorPhase::Parse,
            self.field.clone(),
            self.source.clone(),
            span,
            message,
        )
    }

    fn limit_error(&self, span: SourceSpan, message: impl Into<String>) -> ExpressionError {
        ExpressionError::new(
            ErrorPhase::Limit,
            self.field.clone(),
            self.source.clone(),
            span,
            message,
        )
    }
}

fn literal(value: Literal, span: SourceSpan) -> Expr {
    Expr {
        kind: ExprKind::Literal(value),
        span,
    }
}

fn infix_operator(kind: &TokenKind) -> Option<(u8, u8, BinaryOperator)> {
    Some(match kind {
        TokenKind::Coalesce => (2, 3, BinaryOperator::Coalesce),
        TokenKind::Or => (4, 5, BinaryOperator::Or),
        TokenKind::And => (6, 7, BinaryOperator::And),
        TokenKind::Equal => (8, 9, BinaryOperator::Equal),
        TokenKind::NotEqual => (8, 9, BinaryOperator::NotEqual),
        TokenKind::Less => (10, 11, BinaryOperator::Less),
        TokenKind::LessEqual => (10, 11, BinaryOperator::LessEqual),
        TokenKind::Greater => (10, 11, BinaryOperator::Greater),
        TokenKind::GreaterEqual => (10, 11, BinaryOperator::GreaterEqual),
        TokenKind::In => (10, 11, BinaryOperator::In),
        TokenKind::Plus => (12, 13, BinaryOperator::Add),
        TokenKind::Minus => (12, 13, BinaryOperator::Subtract),
        TokenKind::Star => (14, 15, BinaryOperator::Multiply),
        TokenKind::Slash => (14, 15, BinaryOperator::Divide),
        TokenKind::Percent => (14, 15, BinaryOperator::Remainder),
        _ => return None,
    })
}

fn mixes_nullish_and_boolean(operator: BinaryOperator, left: &Expr, right: &Expr) -> bool {
    match operator {
        BinaryOperator::Coalesce => is_boolean_root(left) || is_boolean_root(right),
        BinaryOperator::And | BinaryOperator::Or => {
            is_coalesce_root(left) || is_coalesce_root(right)
        }
        _ => false,
    }
}

fn is_boolean_root(expression: &Expr) -> bool {
    matches!(
        &expression.kind,
        ExprKind::Binary {
            operator: BinaryOperator::And | BinaryOperator::Or,
            ..
        }
    )
}

fn is_coalesce_root(expression: &Expr) -> bool {
    matches!(
        &expression.kind,
        ExprKind::Binary {
            operator: BinaryOperator::Coalesce,
            ..
        }
    )
}

fn is_path_expression(expression: &Expr) -> bool {
    match &expression.kind {
        ExprKind::Variable(_) => true,
        ExprKind::Member { target, .. } => is_path_expression(target),
        ExprKind::Index { target, .. } => is_path_expression(target),
        ExprKind::Group(inner) => is_path_expression(inner),
        _ => false,
    }
}
