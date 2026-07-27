use std::sync::Arc;

use super::error::{ErrorPhase, ExpressionError, SourceSpan};
use super::limits::CompilationLimits;
use super::{compile_expression_arc, CompiledTemplate, TemplatePart};

pub(crate) fn compile(
    source: Arc<str>,
    field: Option<Arc<str>>,
    limits: &CompilationLimits,
) -> Result<CompiledTemplate, ExpressionError> {
    if source.len() > limits.max_template_bytes {
        return Err(ExpressionError::new(
            ErrorPhase::Limit,
            field,
            source.clone(),
            SourceSpan::new(0, source.len()),
            format!(
                "template source is {} bytes; limit is {}",
                source.len(),
                limits.max_template_bytes
            ),
        ));
    }
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut cursor = 0;
    let mut expressions = 0;
    while cursor < source.len() {
        let bytes = source.as_bytes();
        if bytes.get(cursor..cursor + 7) == Some(b"{input:") {
            return Err(ExpressionError::new(
                ErrorPhase::Parse,
                field.clone(),
                source.clone(),
                SourceSpan::new(cursor, cursor + 7),
                "removed `{input:...}` interpolation is not valid in rye-expr/1",
            )
            .correction("write `${inputs.name}` and use `??` for an explicit fallback"));
        }
        if bytes.get(cursor..cursor + 3) == Some(b"$${") {
            literal.push_str("${");
            cursor += 3;
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"${") {
            push_literal(&mut parts, &mut literal);
            expressions += 1;
            if expressions > limits.max_expressions_per_template {
                return Err(ExpressionError::new(
                    ErrorPhase::Limit,
                    field.clone(),
                    source.clone(),
                    SourceSpan::new(cursor, cursor + 2),
                    format!(
                        "template exceeds expression limit of {}",
                        limits.max_expressions_per_template
                    ),
                ));
            }
            let close = find_expression_end(&source, cursor + 2, field.clone())?;
            let expression_source = &source[cursor + 2..close];
            if expression_source.trim().is_empty() {
                return Err(ExpressionError::new(
                    ErrorPhase::Parse,
                    field.clone(),
                    Arc::from(expression_source),
                    SourceSpan::new(0, expression_source.len()),
                    "template expression cannot be empty",
                ));
            }
            let expression =
                compile_expression_arc(Arc::from(expression_source), field.clone(), limits)?;
            parts.push(TemplatePart::Expression(expression));
            cursor = close + 1;
            continue;
        }
        let character = source[cursor..].chars().next().unwrap();
        literal.push(character);
        cursor += character.len_utf8();
    }
    push_literal(&mut parts, &mut literal);
    if parts.is_empty() {
        parts.push(TemplatePart::Literal(String::new()));
    }
    let whole_expression = matches!(parts.as_slice(), [TemplatePart::Expression(_)]);
    Ok(CompiledTemplate::new(
        source,
        field,
        parts,
        whole_expression,
    ))
}

fn push_literal(parts: &mut Vec<TemplatePart>, literal: &mut String) {
    if !literal.is_empty() {
        parts.push(TemplatePart::Literal(std::mem::take(literal)));
    }
}

fn find_expression_end(
    source: &Arc<str>,
    start: usize,
    field: Option<Arc<str>>,
) -> Result<usize, ExpressionError> {
    let mut stack = vec![b'}'];
    let mut cursor = start;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    while cursor < source.len() {
        let byte = source.as_bytes()[cursor];
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
            cursor += if byte.is_ascii() {
                1
            } else {
                source[cursor..].chars().next().unwrap().len_utf8()
            };
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'(' => stack.push(b')'),
            b'}' | b']' | b')' => {
                let Some(expected) = stack.pop() else {
                    return Err(scan_error(
                        source,
                        field,
                        cursor,
                        "unexpected closing delimiter",
                    ));
                };
                if byte != expected {
                    return Err(scan_error(
                        source,
                        field,
                        cursor,
                        format!(
                            "mismatched closing delimiter `{}`; expected `{}`",
                            byte as char, expected as char
                        ),
                    ));
                }
                if stack.is_empty() {
                    return Ok(cursor);
                }
            }
            _ => {}
        }
        cursor += if byte.is_ascii() {
            1
        } else {
            source[cursor..].chars().next().unwrap().len_utf8()
        };
    }
    Err(ExpressionError::new(
        ErrorPhase::Scan,
        field,
        source.clone(),
        SourceSpan::new(start.saturating_sub(2), source.len()),
        if quote.is_some() {
            "unterminated quoted string in template expression"
        } else {
            "unmatched `${` in template"
        },
    ))
}

fn scan_error(
    source: &Arc<str>,
    field: Option<Arc<str>>,
    offset: usize,
    message: impl Into<String>,
) -> ExpressionError {
    ExpressionError::new(
        ErrorPhase::Scan,
        field,
        source.clone(),
        SourceSpan::new(offset, offset + 1),
        message,
    )
}
