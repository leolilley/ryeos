use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::Value;

use super::ast::{BuiltinFunction, Expr};
use super::error::{ExpressionError, SourceSpan};
use super::evaluator::{Evaluator, RuntimeValue};
use super::value::Numeric;

pub(crate) fn call<'context>(
    function: BuiltinFunction,
    arguments: &[Expr],
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    if function == BuiltinFunction::Exists {
        return Ok(RuntimeValue::Owned(Value::Bool(matches!(
            evaluator.evaluate(&arguments[0])?,
            RuntimeValue::Borrowed(_) | RuntimeValue::Owned(_)
        ))));
    }
    evaluator.allocate(
        arguments.len() * std::mem::size_of::<RuntimeValue<'_>>(),
        span,
    )?;
    let mut values = Vec::with_capacity(arguments.len());
    for argument in arguments {
        let value = evaluator.evaluate(argument)?;
        if let RuntimeValue::Missing(path) = &value {
            return Err(evaluator.error(
                argument.span,
                format!(
                    "function `{}` received missing path `{path}`",
                    function.name()
                ),
            ));
        }
        values.push(value);
    }
    match function {
        BuiltinFunction::Length => length(&values[0], evaluator, span),
        BuiltinFunction::Contains => contains_values(&values[0], &values[1], evaluator, span),
        BuiltinFunction::Keys => keys(&values[0], evaluator, span),
        BuiltinFunction::Upper => case(&values[0], evaluator, span, true),
        BuiltinFunction::Lower => case(&values[0], evaluator, span, false),
        BuiltinFunction::Json => stringify_json(&values[0], evaluator, span),
        BuiltinFunction::String => stringify_explicit(&values[0], evaluator, span),
        BuiltinFunction::FromJson => from_json(&values[0], evaluator, span),
        BuiltinFunction::Type => {
            let name = values[0].type_name();
            evaluator.check_produced_string(name.len(), span)?;
            evaluator.allocate(name.len(), span)?;
            Ok(RuntimeValue::Owned(Value::String(name.to_string())))
        }
        BuiltinFunction::Matches => matches_regex(&values[0], &values[1], evaluator, span),
        BuiltinFunction::Number => number(&values[0], evaluator, span),
        BuiltinFunction::Exists => unreachable!(),
    }
}

fn length<'context>(
    value: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let length = match value.as_json() {
        Some(Value::Array(values)) => {
            charge_elements(values.len(), evaluator, span)?;
            values.len()
        }
        Some(Value::Object(values)) => {
            charge_elements(values.len(), evaluator, span)?;
            values.len()
        }
        Some(Value::String(value)) => {
            evaluator.check_scalar(value, span)?;
            evaluator.spend_fuel(value.len(), span, "counting string length")?;
            value.chars().count()
        }
        _ => {
            return Err(evaluator.error(
                span,
                format!(
                    "length(...) requires array, object, or string; received {}",
                    value.type_name()
                ),
            ));
        }
    };
    let numeric = if let Ok(value) = i64::try_from(length) {
        Numeric::Signed(value)
    } else {
        Numeric::Unsigned(length as u64)
    };
    Ok(RuntimeValue::Owned(
        numeric
            .to_json()
            .map_err(|message| evaluator.error(span, message))?,
    ))
}

pub(crate) fn contains_values<'context>(
    container: &RuntimeValue<'_>,
    needle: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let result = match container.as_json() {
        Some(Value::Array(values)) => {
            let needle = needle
                .as_json()
                .ok_or_else(|| evaluator.error(span, "array membership needle is missing"))?;
            let mut found = false;
            for value in values {
                evaluator.traverse_element(span)?;
                if evaluator.deep_equal_json(value, needle, 1, span)? {
                    found = true;
                    break;
                }
            }
            found
        }
        Some(Value::Object(values)) => {
            let Some(Value::String(key)) = needle.as_json() else {
                return Err(evaluator.error(span, "object membership requires a string key"));
            };
            evaluator.check_scalar(key, span)?;
            evaluator.spend_fuel(key.len(), span, "checking object membership")?;
            values.contains_key(key)
        }
        Some(Value::String(haystack)) => {
            let Some(Value::String(needle)) = needle.as_json() else {
                return Err(evaluator.error(span, "string membership requires a string needle"));
            };
            evaluator.check_scalar(haystack, span)?;
            evaluator.check_scalar(needle, span)?;
            evaluator.spend_fuel(
                haystack.len().saturating_add(needle.len()),
                span,
                "checking string membership",
            )?;
            haystack.contains(needle)
        }
        _ => {
            return Err(evaluator.error(
                span,
                format!(
                    "membership requires array, object, or string container; received {}",
                    container.type_name()
                ),
            ));
        }
    };
    Ok(RuntimeValue::Owned(Value::Bool(result)))
}

fn keys<'context>(
    value: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let Some(Value::Object(object)) = value.as_json() else {
        return Err(evaluator.error(span, "keys(...) requires object"));
    };
    charge_elements(object.len(), evaluator, span)?;
    let mut bytes = 0usize;
    for key in object.keys() {
        evaluator.check_scalar(key, span)?;
        evaluator.check_produced_string(key.len(), span)?;
        evaluator.spend_fuel(key.len(), span, "copying object key")?;
        bytes = bytes.saturating_add(key.len());
    }
    evaluator.allocate(
        bytes.saturating_add(object.len().saturating_mul(
            std::mem::size_of::<&String>().saturating_add(std::mem::size_of::<Value>()),
        )),
        span,
    )?;
    let mut keys: Vec<_> = object.keys().collect();
    keys.sort();
    Ok(RuntimeValue::Owned(Value::Array(
        keys.into_iter()
            .map(|key| Value::String(key.clone()))
            .collect(),
    )))
}

fn case<'context>(
    value: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
    upper: bool,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let Some(Value::String(value)) = value.as_json() else {
        return Err(evaluator.error(
            span,
            if upper {
                "upper(...) requires string"
            } else {
                "lower(...) requires string"
            },
        ));
    };
    evaluator.check_scalar(value, span)?;
    evaluator.spend_fuel(value.len(), span, "converting string case")?;
    let mut output = String::new();
    for character in value.chars() {
        if upper {
            for mapped in character.to_uppercase() {
                push_case_character(mapped, &mut output, evaluator, span)?;
            }
        } else {
            for mapped in character.to_lowercase() {
                push_case_character(mapped, &mut output, evaluator, span)?;
            }
        }
    }
    Ok(RuntimeValue::Owned(Value::String(output)))
}

fn push_case_character(
    character: char,
    output: &mut String,
    evaluator: &mut Evaluator<'_, '_, '_>,
    span: SourceSpan,
) -> Result<(), ExpressionError> {
    let mut encoded = [0u8; 4];
    let mapped = character.encode_utf8(&mut encoded);
    evaluator.check_produced_string(output.len().saturating_add(mapped.len()), span)?;
    evaluator.allocate(mapped.len(), span)?;
    output.push_str(mapped);
    Ok(())
}

fn stringify_json<'context>(
    value: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let value = value
        .as_json()
        .ok_or_else(|| evaluator.error(span, "string conversion received missing path"))?;
    let mut output = String::new();
    canonical_json(value, evaluator, span, 1, &mut output)?;
    evaluator.check_produced_string(output.len(), span)?;
    Ok(RuntimeValue::Owned(Value::String(output)))
}

fn stringify_explicit<'context>(
    value: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let value = value
        .as_json()
        .ok_or_else(|| evaluator.error(span, "string conversion received missing path"))?;
    let output = match value {
        Value::String(value) => {
            evaluator.check_scalar(value, span)?;
            evaluator.check_produced_string(value.len(), span)?;
            evaluator.allocate(value.len(), span)?;
            value.clone()
        }
        Value::Null => {
            evaluator.check_produced_string(4, span)?;
            evaluator.allocate(4, span)?;
            "null".to_string()
        }
        Value::Bool(value) => {
            let output = value.to_string();
            evaluator.check_produced_string(output.len(), span)?;
            evaluator.allocate(output.len(), span)?;
            output
        }
        Value::Number(number) => {
            let output = Numeric::from_json(number)
                .and_then(Numeric::canonical)
                .map_err(|message| evaluator.error(span, message))?;
            evaluator.check_produced_string(output.len(), span)?;
            evaluator.allocate(output.len(), span)?;
            output
        }
        Value::Array(_) | Value::Object(_) => {
            let mut output = String::new();
            canonical_json(value, evaluator, span, 1, &mut output)?;
            output
        }
    };
    evaluator.check_produced_string(output.len(), span)?;
    Ok(RuntimeValue::Owned(Value::String(output)))
}

fn from_json<'context>(
    value: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let Some(Value::String(source)) = value.as_json() else {
        return Err(evaluator.error(span, "from_json(...) requires string"));
    };
    if source.len() > evaluator.budget.limits.max_from_json_bytes {
        return Err(evaluator.limit_error(
            span,
            format!(
                "from_json input is {} bytes; limit is {}",
                source.len(),
                evaluator.budget.limits.max_from_json_bytes
            ),
        ));
    }
    evaluator.spend_fuel(source.len(), span, "parsing JSON")?;
    // serde_json owns its parse tree before it can be walked. Reserve a
    // deliberately conservative per-input-byte allowance before parsing so
    // that construction itself remains inside the cumulative allocation
    // budget; clone_external then charges the exact materialized result.
    evaluator.allocate(source.len().saturating_mul(64), span)?;
    let mut deserializer = serde_json::Deserializer::from_str(source);
    let parsed = Value::deserialize(&mut deserializer)
        .map_err(|error| evaluator.error(span, format!("from_json parse failed: {error}")))?;
    deserializer
        .end()
        .map_err(|error| evaluator.error(span, format!("from_json trailing content: {error}")))?;
    let bounded = evaluator.clone_external(&parsed, span)?;
    Ok(RuntimeValue::Owned(bounded))
}

fn matches_regex<'context>(
    value: &RuntimeValue<'_>,
    pattern: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let Some(Value::String(haystack)) = value.as_json() else {
        return Err(evaluator.error(span, "matches value must be string"));
    };
    let Some(Value::String(pattern)) = pattern.as_json() else {
        return Err(evaluator.error(span, "matches pattern must be string"));
    };
    if pattern.len() > evaluator.budget.limits.max_regex_pattern_bytes {
        return Err(evaluator.limit_error(span, "regex pattern byte limit exceeded"));
    }
    if haystack.len() > evaluator.budget.limits.max_regex_haystack_bytes {
        return Err(evaluator.limit_error(span, "regex haystack byte limit exceeded"));
    }
    evaluator.spend_fuel(
        pattern.len().saturating_add(haystack.len()),
        span,
        "matching regex",
    )?;
    // Keep the regex compiler itself bounded independently of expression input
    // size. The pattern byte cap alone does not bound compiled automata.
    let remaining = evaluator
        .budget
        .limits
        .max_allocation_bytes
        .saturating_sub(evaluator.budget.allocated_bytes());
    // The regex crate's Unicode tables alone can exceed a 1 KiB program for a
    // simple bounded expression. Reserve a practical fixed floor while still
    // scaling with authored pattern size and charging the full bound against
    // the evaluator allocation budget before compilation.
    let compiled_limit = pattern.len().saturating_mul(64).max(256 * 1024);
    let reservation = compiled_limit.saturating_mul(2);
    if reservation > remaining {
        return Err(evaluator.limit_error(span, "regex compilation allocation limit exceeded"));
    }
    evaluator.allocate(reservation, span)?;
    let regex = RegexBuilder::new(pattern)
        .size_limit(compiled_limit)
        .dfa_size_limit(compiled_limit)
        .build()
        .map_err(|error| evaluator.error(span, format!("invalid regex pattern: {error}")))?;
    Ok(RuntimeValue::Owned(Value::Bool(regex.is_match(haystack))))
}

fn number<'context>(
    value: &RuntimeValue<'_>,
    evaluator: &mut Evaluator<'context, '_, '_>,
    span: SourceSpan,
) -> Result<RuntimeValue<'context>, ExpressionError> {
    let numeric = match value.as_json() {
        Some(Value::Number(number)) => {
            Numeric::from_json(number).map_err(|message| evaluator.error(span, message))?
        }
        Some(Value::String(value)) => {
            evaluator.check_scalar(value, span)?;
            evaluator.spend_fuel(value.len(), span, "parsing numeric string")?;
            Numeric::parse_string(value).map_err(|message| evaluator.error(span, message))?
        }
        _ => {
            return Err(evaluator.error(
                span,
                format!(
                    "number(...) requires number or numeric string; received {}",
                    value.type_name()
                ),
            ));
        }
    };
    Ok(RuntimeValue::Owned(
        numeric
            .to_json()
            .map_err(|message| evaluator.error(span, message))?,
    ))
}

pub(crate) fn canonical_json(
    value: &Value,
    evaluator: &mut Evaluator<'_, '_, '_>,
    span: SourceSpan,
    depth: usize,
    output: &mut String,
) -> Result<(), ExpressionError> {
    if depth > evaluator.budget.limits.max_traversal_depth {
        return Err(evaluator.limit_error(span, "JSON serialization depth limit exceeded"));
    }
    evaluator.spend_fuel(1, span, "serializing JSON")?;
    match value {
        Value::Null => push_bounded(output, "null", evaluator, span)?,
        Value::Bool(value) => push_bounded(
            output,
            if *value { "true" } else { "false" },
            evaluator,
            span,
        )?,
        Value::Number(number) => {
            let numeric =
                Numeric::from_json(number).map_err(|message| evaluator.error(span, message))?;
            let rendered = numeric
                .canonical()
                .map_err(|message| evaluator.error(span, message))?;
            evaluator.allocate(rendered.len(), span)?;
            push_bounded(output, &rendered, evaluator, span)?;
        }
        Value::String(value) => {
            evaluator.check_scalar(value, span)?;
            evaluator.spend_fuel(value.len(), span, "serializing string")?;
            push_json_string(value, evaluator, span, output)?;
        }
        Value::Array(values) => {
            push_bounded(output, "[", evaluator, span)?;
            for (index, value) in values.iter().enumerate() {
                evaluator.traverse_element(span)?;
                if index > 0 {
                    push_bounded(output, ",", evaluator, span)?;
                }
                canonical_json(value, evaluator, span, depth + 1, output)?;
            }
            push_bounded(output, "]", evaluator, span)?;
        }
        Value::Object(values) => {
            push_bounded(output, "{", evaluator, span)?;
            evaluator.allocate(
                values
                    .len()
                    .saturating_mul(std::mem::size_of::<(&String, &Value)>()),
                span,
            )?;
            let mut entries: Vec<_> = values.iter().collect();
            charge_elements(entries.len(), evaluator, span)?;
            entries.sort_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    push_bounded(output, ",", evaluator, span)?;
                }
                evaluator.check_scalar(key, span)?;
                evaluator.spend_fuel(key.len(), span, "serializing object key")?;
                push_json_string(key, evaluator, span, output)?;
                push_bounded(output, ":", evaluator, span)?;
                canonical_json(value, evaluator, span, depth + 1, output)?;
            }
            push_bounded(output, "}", evaluator, span)?;
        }
    }
    evaluator.check_produced_string(output.len(), span)
}

fn push_json_string(
    value: &str,
    evaluator: &mut Evaluator<'_, '_, '_>,
    span: SourceSpan,
    output: &mut String,
) -> Result<(), ExpressionError> {
    push_bounded(output, "\"", evaluator, span)?;
    for character in value.chars() {
        match character {
            '"' => push_bounded(output, "\\\"", evaluator, span)?,
            '\\' => push_bounded(output, "\\\\", evaluator, span)?,
            '\u{08}' => push_bounded(output, "\\b", evaluator, span)?,
            '\u{0c}' => push_bounded(output, "\\f", evaluator, span)?,
            '\n' => push_bounded(output, "\\n", evaluator, span)?,
            '\r' => push_bounded(output, "\\r", evaluator, span)?,
            '\t' => push_bounded(output, "\\t", evaluator, span)?,
            character if character <= '\u{1f}' => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let scalar = character as usize;
                let escape = [
                    b'\\',
                    b'u',
                    HEX[(scalar >> 12) & 0xf],
                    HEX[(scalar >> 8) & 0xf],
                    HEX[(scalar >> 4) & 0xf],
                    HEX[scalar & 0xf],
                ];
                push_bounded(
                    output,
                    std::str::from_utf8(&escape).unwrap(),
                    evaluator,
                    span,
                )?;
            }
            character => {
                let mut encoded = [0u8; 4];
                push_bounded(output, character.encode_utf8(&mut encoded), evaluator, span)?;
            }
        }
    }
    push_bounded(output, "\"", evaluator, span)
}

fn push_bounded(
    output: &mut String,
    value: &str,
    evaluator: &mut Evaluator<'_, '_, '_>,
    span: SourceSpan,
) -> Result<(), ExpressionError> {
    evaluator.check_produced_string(output.len().saturating_add(value.len()), span)?;
    evaluator.allocate(value.len(), span)?;
    output.push_str(value);
    Ok(())
}

fn charge_elements(
    count: usize,
    evaluator: &mut Evaluator<'_, '_, '_>,
    span: SourceSpan,
) -> Result<(), ExpressionError> {
    for _ in 0..count {
        evaluator.traverse_element(span)?;
    }
    Ok(())
}
