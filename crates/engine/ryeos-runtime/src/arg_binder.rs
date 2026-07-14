//! Shared argument binder — unified argv → JSON parameters.
//!
//! Used by command dispatchers and direct execute mode.
//! Single source of truth for heuristic flag/value binding.
//!
//! ## Rules
//!
//! - `--key=value` → `{ "key": "value" }`  (hyphens normalised to underscores)
//! - `--key value` → `{ "key": "value" }`  (next token consumed as value)
//! - `--key` (alone or before `--next`) → `{ "key": true }`
//! - `--key -1` → `{ "key": "-1" }` (single-dash values are values, not flags)
//! - `--foo-bar val` → `{ "foo_bar": "val" }` (hyphens in keys → underscores)
//! - Positional tokens → `{ "_args": [...] }`
//! - Repeated keys: accumulate into array (`--scopes a --scopes b` → `["a","b"]`).
//!   Single occurrence remains scalar; handlers wanting `Vec<T>` should
//!   deserialize via [`crate::scalar_or_vec`] to accept both forms.
//! - Empty input → `{}`

use crate::command::{
    CommandArgumentKind, CommandArgumentSlot, CommandDef, InvocationInputContract,
    InvocationInputType,
};

/// Normalise a flag key: strip the `--` prefix (already done by caller)
/// and replace hyphens with underscores so `--public-key` becomes `public_key`.
/// This matches the snake_case field names used by handlers with
/// `#[serde(deny_unknown_fields)]`.
fn normalise_key(key: &str) -> String {
    key.replace('-', "_")
}

/// Bind raw argv tokens into a flat JSON parameters object.
///
/// This is schema-free heuristic binding — no POSIX short flags, no `--`
/// terminator, no type coercion. The daemon validates params against the
/// item's schema downstream.
///
pub fn bind_argv(argv: &[String]) -> serde_json::Value {
    let mut params = serde_json::Map::new();
    let mut positional = Vec::new();
    let mut i = 0;

    while i < argv.len() {
        let token = &argv[i];
        if let Some(key) = token.strip_prefix("--") {
            if key.is_empty() {
                // bare "--" — skip (not currently used, but safe)
                i += 1;
                continue;
            }
            let (k, v) = if key.contains('=') {
                // --key=value
                let (k, v) = key.split_once('=').unwrap();
                (normalise_key(k), serde_json::Value::String(v.to_string()))
            } else if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                // --key value (value may start with single dash, e.g. "-1")
                i += 1;
                (
                    normalise_key(key),
                    serde_json::Value::String(argv[i].clone()),
                )
            } else {
                // --key (bare flag)
                (normalise_key(key), serde_json::Value::Bool(true))
            };
            insert_or_accumulate(&mut params, k, v);
        } else {
            positional.push(serde_json::Value::String(token.clone()));
        }
        i += 1;
    }

    if !positional.is_empty() {
        params.insert("_args".to_string(), serde_json::Value::Array(positional));
    }

    serde_json::Value::Object(params)
}

/// Command-aware binding driven by command metadata. Supports ordered
/// positional forms and fails closed on undeclared positional tails.
pub fn bind_argv_with_command(
    argv: &[String],
    command: Option<&CommandDef>,
) -> Result<serde_json::Value, String> {
    bind_argv_with_command_and_contract_and_overlay(argv, command, None, None)
}

/// Command-aware binding with caller-supplied structured fields. The overlay
/// is merged before form selection and required-slot validation, so data-driven
/// UI arguments and positional argv are equivalent inputs to one binder.
pub fn bind_argv_with_command_and_overlay(
    argv: &[String],
    command: Option<&CommandDef>,
    overlay: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    bind_argv_with_command_and_contract_and_overlay(argv, command, None, Some(overlay))
}

/// Command-aware binding plus optional kind-agnostic invocation contract
/// normalization. Existing descriptor/form binding runs first; the contract
/// then coerces and validates present top-level fields.
pub fn bind_argv_with_command_and_contract(
    argv: &[String],
    command: Option<&CommandDef>,
    contract: Option<&InvocationInputContract>,
) -> Result<serde_json::Value, String> {
    bind_argv_with_command_and_contract_and_overlay(argv, command, contract, None)
}

fn bind_argv_with_command_and_contract_and_overlay(
    argv: &[String],
    command: Option<&CommandDef>,
    contract: Option<&InvocationInputContract>,
    overlay: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let Some(command) = command else {
        let mut value = bind_argv(argv);
        merge_overlay(&mut value, overlay)?;
        return normalize_params_with_contract(value, contract);
    };

    let mut value = bind_argv(argv);
    merge_overlay(&mut value, overlay)?;
    decode_structured_command_fields(&mut value, command)?;

    let forms = command.forms.clone();
    if forms.is_empty() {
        apply_command_defaults(&mut value, command);
        reject_undeclared_positionals(&value, command)?;
        return normalize_params_with_contract(value, contract);
    }

    let Some(obj) = value.as_object_mut() else {
        return normalize_params_with_contract(value, contract);
    };
    let positionals: Vec<String> = obj
        .remove("_args")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(ToString::to_string))
        .collect();

    if positionals.is_empty() {
        for form in &forms {
            for slot in &form.slots {
                let key = normalise_key(&slot.field);
                if matches!(obj.get(&key), Some(serde_json::Value::Bool(true))) {
                    return Err(format!(
                        "--{} requires a value",
                        slot.field.replace('_', "-")
                    ));
                }
            }
        }
        if forms.iter().any(|form| {
            form.slots.iter().all(|slot| {
                let key = normalise_key(&slot.field);
                obj.contains_key(&key) || command.defaults.contains_key(&key)
            })
        }) {
            apply_command_defaults(&mut value, command);
            return normalize_params_with_contract(value, contract);
        }
        let required = forms
            .first()
            .map(|form| {
                form.slots
                    .iter()
                    .filter(|slot| {
                        let key = normalise_key(&slot.field);
                        !obj.contains_key(&key) && !command.defaults.contains_key(&key)
                    })
                    .map(|slot| format!("<{}>", slot.field))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|required| !required.is_empty())
            .unwrap_or_else(|| "required positional argument".to_string());
        if forms.len() == 1 {
            return Err(format!("command {:?} requires {required}", command.tokens));
        }
        return Err(format!(
            "command {:?} requires one of its positional forms",
            command.tokens
        ));
    }

    for form in &forms {
        let unset_slots: Vec<&CommandArgumentSlot> = form
            .slots
            .iter()
            .filter(|slot| !obj.contains_key(&normalise_key(&slot.field)))
            .collect();
        if unset_slots.len() == 1 {
            let slot = unset_slots[0];
            if command.arguments.iter().any(|arg| {
                arg.name == slot.field
                    && arg.arity == crate::CommandArgumentArity::Variadic
                    && arg.kind == slot.matcher
            }) && positionals
                .iter()
                .all(|arg| positional_matches(slot.matcher, arg))
            {
                obj.insert(
                    normalise_key(&slot.field),
                    serde_json::Value::Array(
                        positionals
                            .iter()
                            .map(|arg| serde_json::Value::String(arg.clone()))
                            .collect(),
                    ),
                );
                apply_command_defaults(&mut value, command);
                return normalize_params_with_contract(value, contract);
            }
        }
        if unset_slots.len() != positionals.len() {
            continue;
        }
        if unset_slots
            .iter()
            .zip(positionals.iter())
            .all(|(slot, arg)| positional_matches(slot.matcher, arg))
        {
            for (slot, arg) in unset_slots.into_iter().zip(positionals.iter()) {
                obj.insert(
                    normalise_key(&slot.field),
                    serde_json::Value::String(arg.clone()),
                );
            }
            apply_command_defaults(&mut value, command);
            return normalize_params_with_contract(value, contract);
        }
    }

    Err(format!(
        "positional arguments {:?} do not match any positional form for alias {:?}",
        positionals, command.tokens
    ))
}

fn merge_overlay(
    value: &mut serde_json::Value,
    overlay: Option<&serde_json::Value>,
) -> Result<(), String> {
    let Some(overlay) = overlay else {
        return Ok(());
    };
    if overlay.is_null() {
        return Ok(());
    }
    let fields = overlay
        .as_object()
        .ok_or_else(|| "command argument overlay must be an object".to_string())?;
    let target = value
        .as_object_mut()
        .ok_or_else(|| "command argument binder returned a non-object".to_string())?;
    target.extend(fields.clone());
    Ok(())
}

/// Normalize already-bound JSON parameters using a kind-agnostic input
/// contract. Only present top-level fields are checked here; missing required
/// fields remain the responsibility of the downstream item/handler contract.
pub fn normalize_params_with_contract(
    value: serde_json::Value,
    contract: Option<&InvocationInputContract>,
) -> Result<serde_json::Value, String> {
    let Some(contract) = contract else {
        return Ok(value);
    };
    let mut obj = match value {
        serde_json::Value::Object(obj) => obj,
        other => {
            return Err(format!(
            "invocation parameters must be an object when a schema contract is declared, got {}",
            json_type(&other)
        ))
        }
    };

    for (field, decl) in &contract.fields {
        let Some(raw) = obj.get(field).cloned() else {
            continue;
        };
        let normalized = normalize_field_value(field, raw, decl.ty)?;
        obj.insert(field.clone(), normalized);
    }

    Ok(serde_json::Value::Object(obj))
}

fn normalize_field_value(
    field: &str,
    value: serde_json::Value,
    ty: InvocationInputType,
) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    match ty {
        InvocationInputType::String => match value {
            Value::String(_) => Ok(value),
            other => Err(format!(
                "--{} must be a string, got {}",
                flag_name(field),
                json_type(&other)
            )),
        },
        InvocationInputType::Integer => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Ok(Value::Number(n)),
            Value::String(s) => s
                .parse::<i64>()
                .map(|n| serde_json::json!(n))
                .map_err(|_| format!("--{} must be an integer", flag_name(field))),
            other => Err(format!(
                "--{} must be an integer, got {}",
                flag_name(field),
                json_type(&other)
            )),
        },
        InvocationInputType::Number => match value {
            Value::Number(_) => Ok(value),
            Value::String(s) => s
                .parse::<serde_json::Value>()
                .ok()
                .and_then(|value| match value {
                    Value::Number(_) => Some(value),
                    _ => None,
                })
                .ok_or_else(|| format!("--{} must be a number", flag_name(field))),
            other => Err(format!(
                "--{} must be a number, got {}",
                flag_name(field),
                json_type(&other)
            )),
        },
        InvocationInputType::Boolean => match value {
            Value::Bool(_) => Ok(value),
            Value::String(s) if s == "true" => Ok(Value::Bool(true)),
            Value::String(s) if s == "false" => Ok(Value::Bool(false)),
            Value::String(_) => Err(format!("--{} must be true or false", flag_name(field))),
            other => Err(format!(
                "--{} must be a boolean, got {}",
                flag_name(field),
                json_type(&other)
            )),
        },
        InvocationInputType::Json => Ok(value),
        InvocationInputType::Object => match value {
            Value::Object(_) => Ok(value),
            other => Err(format!(
                "--{} must be an object, got {}",
                flag_name(field),
                json_type(&other)
            )),
        },
        InvocationInputType::Array => match value {
            Value::Array(_) => Ok(value),
            Value::String(s) => Ok(Value::Array(vec![Value::String(s)])),
            other => Err(format!(
                "--{} must be an array, got {}",
                flag_name(field),
                json_type(&other)
            )),
        },
    }
}

fn flag_name(field: &str) -> String {
    field.replace('_', "-")
}

fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn apply_command_defaults(value: &mut serde_json::Value, command: &CommandDef) {
    if command.defaults.is_empty() {
        return;
    }
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for (key, default) in &command.defaults {
        obj.entry(normalise_key(key))
            .or_insert_with(|| default.clone());
    }
}

fn reject_undeclared_positionals(
    value: &serde_json::Value,
    command: &CommandDef,
) -> Result<(), String> {
    let positionals = value
        .get("_args")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or_default();
    if positionals == 0 {
        return Ok(());
    }
    Err(format!(
        "command {:?} does not declare positional forms but received {positionals} positional argument(s)",
        command.tokens
    ))
}

fn decode_structured_command_fields(
    value: &mut serde_json::Value,
    command: &CommandDef,
) -> Result<(), String> {
    let Some(obj) = value.as_object_mut() else {
        return Ok(());
    };

    for argument in &command.arguments {
        if argument.kind != CommandArgumentKind::Json {
            continue;
        }
        let key = normalise_key(&argument.name);
        let Some(raw) = obj.get(&key).cloned() else {
            continue;
        };
        let Some(raw_str) = raw.as_str() else {
            continue;
        };
        let decoded: serde_json::Value = serde_json::from_str(raw_str)
            .map_err(|e| format!("--{} must be valid JSON: {e}", argument.name))?;
        obj.insert(key, decoded);
    }

    if let Some(raw) = obj.get("parameters").cloned() {
        let Some(raw_str) = raw.as_str() else {
            return Ok(());
        };
        let decoded: serde_json::Value = serde_json::from_str(raw_str)
            .map_err(|e| format!("--parameters must be valid JSON object: {e}"))?;
        if !decoded.is_object() {
            return Err("--parameters must decode to a JSON object".into());
        }
        obj.insert("parameters".to_string(), decoded);
    }

    Ok(())
}

fn positional_matches(matcher: CommandArgumentKind, arg: &str) -> bool {
    match matcher {
        CommandArgumentKind::String | CommandArgumentKind::Path | CommandArgumentKind::Json => true,
        CommandArgumentKind::CanonicalRef => {
            ryeos_engine::canonical_ref::CanonicalRef::parse(arg).is_ok()
        }
    }
}

/// Insert a key/value into the params map; on repeat, accumulate into
/// an array. The first repeat promotes the existing scalar to a
/// one-element array, then appends the new value. Subsequent repeats
/// just append. Booleans (bare flags) are treated as scalars and
/// follow the same promotion path.
fn insert_or_accumulate(
    params: &mut serde_json::Map<String, serde_json::Value>,
    key: String,
    value: serde_json::Value,
) {
    use serde_json::Value;
    match params.remove(&key) {
        None => {
            params.insert(key, value);
        }
        Some(Value::Array(mut arr)) => {
            arr.push(value);
            params.insert(key, Value::Array(arr));
        }
        Some(existing) => {
            params.insert(key, Value::Array(vec![existing, value]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        let result = bind_argv(&[]);
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn flag_with_value() {
        let result = bind_argv(&["--name".into(), "alice".into()]);
        assert_eq!(result["name"], "alice");
    }

    #[test]
    fn flag_with_equals() {
        let result = bind_argv(&["--seed=119".into()]);
        assert_eq!(result["seed"], "119");
    }

    #[test]
    fn bare_key_value_stays_positional() {
        let result = bind_argv(&["remote=railway".into(), "foo=".into(), "=bar".into()]);
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args, &["remote=railway", "foo=", "=bar"]);
    }

    #[test]
    fn bare_flag_is_true() {
        let result = bind_argv(&["--verbose".into()]);
        assert_eq!(result["verbose"], true);
    }

    #[test]
    fn bare_flag_before_next_flag() {
        let result = bind_argv(&["--verbose".into(), "--count".into(), "5".into()]);
        assert_eq!(result["verbose"], true);
        assert_eq!(result["count"], "5");
    }

    #[test]
    fn negative_number_as_value() {
        // --seed -1: -1 is NOT a flag (only -- starts a flag)
        let result = bind_argv(&["--seed".into(), "-1".into()]);
        assert_eq!(result["seed"], "-1");
    }

    #[test]
    fn positional_args() {
        let result = bind_argv(&["cmd".into(), "--verbose".into(), "arg".into()]);
        // "arg" follows --verbose and doesn't start with "--", so it's consumed as value
        assert_eq!(result["verbose"], "arg");
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "cmd");
    }

    #[test]
    fn positional_only() {
        let result = bind_argv(&["./bundles/standard".into()]);
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "./bundles/standard");
    }

    #[test]
    fn repeated_flag_promotes_to_array() {
        // Two occurrences → array.
        let result = bind_argv(&[
            "--name".into(),
            "alice".into(),
            "--name".into(),
            "bob".into(),
        ]);
        let names = result["name"].as_array().unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "alice");
        assert_eq!(names[1], "bob");
    }

    #[test]
    fn repeated_flag_three_times_accumulates() {
        // Three occurrences → 3-element array (no nesting).
        let result = bind_argv(&[
            "--scope".into(),
            "a".into(),
            "--scope".into(),
            "b".into(),
            "--scope".into(),
            "c".into(),
        ]);
        let scopes = result["scope"].as_array().unwrap();
        assert_eq!(scopes.len(), 3);
        assert_eq!(scopes[0], "a");
        assert_eq!(scopes[1], "b");
        assert_eq!(scopes[2], "c");
    }

    #[test]
    fn single_flag_stays_scalar() {
        let result = bind_argv(&["--name".into(), "alice".into()]);
        assert_eq!(result["name"], "alice");
        assert!(!result["name"].is_array());
    }

    #[test]
    fn repeated_mixed_scalar_and_equals_form() {
        // Combining `--key=a` then `--key b` accumulates.
        let result = bind_argv(&["--key=a".into(), "--key".into(), "b".into()]);
        let arr = result["key"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "a");
        assert_eq!(arr[1], "b");
    }

    #[test]
    fn mixed_flags_positionals_equals() {
        let result = bind_argv(&[
            "--seed".into(),
            "119".into(),
            "./bundles/standard".into(),
            "--force".into(),
        ]);
        assert_eq!(result["seed"], "119");
        assert_eq!(result["force"], true);
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "./bundles/standard");
    }

    #[test]
    fn flag_consumes_non_dash_value() {
        // --force followed by a non-dash token: consumed as value
        let result = bind_argv(&["--force".into(), "some-arg".into()]);
        assert_eq!(result["force"], "some-arg");
        assert!(result.get("_args").is_none());
    }

    #[test]
    fn bare_double_dash_skipped() {
        let result = bind_argv(&["--".into(), "positional".into()]);
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "positional");
    }

    // ── Hyphen-to-underscore normalisation ──

    #[test]
    fn hyphenated_key_normalised_to_underscore() {
        let result = bind_argv(&["--public-key".into(), "abc123".into()]);
        assert_eq!(result["public_key"], "abc123");
        assert!(result.get("public-key").is_none());
    }

    #[test]
    fn hyphenated_equals_key_normalised() {
        let result = bind_argv(&["--some-key=value".into()]);
        assert_eq!(result["some_key"], "value");
        assert!(result.get("some-key").is_none());
    }

    #[test]
    fn hyphenated_bare_flag_normalised() {
        let result = bind_argv(&["--dry-run".into()]);
        assert_eq!(result["dry_run"], true);
        assert!(result.get("dry-run").is_none());
    }

    #[test]
    fn mixed_hyphenated_and_plain_keys() {
        let result = bind_argv(&[
            "--public-key".into(),
            "ed25519:abc".into(),
            "--label".into(),
            "test-key".into(),
            "--dry-run".into(),
        ]);
        assert_eq!(result["public_key"], "ed25519:abc");
        assert_eq!(result["label"], "test-key");
        assert_eq!(result["dry_run"], true);
    }

    #[test]
    fn command_decodes_parameters_json_object() {
        let command = test_command(vec!["remote".into(), "run".into()], Vec::new());

        let result = bind_argv_with_command(
            &[
                "--parameters".into(),
                r#"{"smoke_only":true,"limit":1}"#.into(),
            ],
            Some(&command),
        )
        .unwrap();

        assert_eq!(result["parameters"]["smoke_only"], true);
        assert_eq!(result["parameters"]["limit"], 1);
    }

    #[test]
    fn command_defaults_fill_missing_fields() {
        let mut command = test_command(vec!["web".into(), "base".into()], Vec::new());
        command.defaults.insert(
            "surface".into(),
            serde_json::Value::String("surface:ryeos/ui/base".into()),
        );

        let result = bind_argv_with_command(&[], Some(&command)).unwrap();

        assert_eq!(result["surface"], "surface:ryeos/ui/base");
    }

    #[test]
    fn command_positionals_override_defaults() {
        let mut command = test_command(
            vec!["web".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "surface".into(),
                    matcher: crate::CommandArgumentKind::String,
                }],
            }],
        );
        command.defaults.insert(
            "surface".into(),
            serde_json::Value::String("surface:ryeos/ui/atlas".into()),
        );

        let result =
            bind_argv_with_command(&["surface:custom/ryeos-ui/debug".into()], Some(&command))
                .unwrap();

        assert_eq!(result["surface"], "surface:custom/ryeos-ui/debug");
    }

    #[test]
    fn command_rejects_parameters_json_array() {
        let command = test_command(vec!["remote".into(), "run".into()], Vec::new());

        let result = bind_argv_with_command(
            &["--parameters".into(), r#"["not","object"]"#.into()],
            Some(&command),
        );

        assert!(result.unwrap_err().contains("JSON object"));
    }

    #[test]
    fn command_decodes_declared_json_argument() {
        let mut command = test_command(vec!["scheduler".into(), "register".into()], Vec::new());
        command.arguments.push(crate::CommandArgumentDef {
            name: "params".into(),
            kind: crate::CommandArgumentKind::Json,
            positional: 0,
            required: false,
            arity: crate::CommandArgumentArity::One,
            description: None,
        });

        let result = bind_argv_with_command(
            &[
                "--schedule-id".into(),
                "daily".into(),
                "--params".into(),
                r#"{"skip_shows":true,"limit":5000}"#.into(),
            ],
            Some(&command),
        )
        .unwrap();

        assert_eq!(result["schedule_id"], "daily");
        assert_eq!(result["params"]["skip_shows"], true);
        assert_eq!(result["params"]["limit"], 5000);
    }

    #[test]
    fn command_contract_coerces_integer_flag_value() {
        let command = test_command(vec!["thread".into(), "list".into()], Vec::new());
        let contract = crate::InvocationInputContract::from_lightweight_schema_value(
            &serde_json::json!({ "limit": "integer?" }),
        )
        .unwrap()
        .unwrap();

        let result = bind_argv_with_command_and_contract(
            &["--limit".into(), "5".into()],
            Some(&command),
            Some(&contract),
        )
        .unwrap();

        assert_eq!(result["limit"], serde_json::json!(5));
    }

    #[test]
    fn command_contract_coerces_integer_equals_value() {
        let command = test_command(vec!["thread".into(), "list".into()], Vec::new());
        let contract = crate::InvocationInputContract::from_lightweight_schema_value(
            &serde_json::json!({ "limit": "integer?" }),
        )
        .unwrap()
        .unwrap();

        let result = bind_argv_with_command_and_contract(
            &["--limit=5".into()],
            Some(&command),
            Some(&contract),
        )
        .unwrap();

        assert_eq!(result["limit"], serde_json::json!(5));
    }

    #[test]
    fn command_contract_rejects_invalid_integer_value() {
        let command = test_command(vec!["thread".into(), "list".into()], Vec::new());
        let contract = crate::InvocationInputContract::from_lightweight_schema_value(
            &serde_json::json!({ "limit": "integer?" }),
        )
        .unwrap()
        .unwrap();

        let err = bind_argv_with_command_and_contract(
            &["--limit".into(), "five".into()],
            Some(&command),
            Some(&contract),
        )
        .unwrap_err();

        assert!(err.contains("--limit must be an integer"));
    }

    #[test]
    fn command_contract_coerces_boolean_value() {
        let command = test_command(vec!["scheduler".into(), "list".into()], Vec::new());
        let contract = crate::InvocationInputContract::from_lightweight_schema_value(
            &serde_json::json!({ "enabled": "boolean?" }),
        )
        .unwrap()
        .unwrap();

        let result = bind_argv_with_command_and_contract(
            &["--enabled".into(), "false".into()],
            Some(&command),
            Some(&contract),
        )
        .unwrap();

        assert_eq!(result["enabled"], serde_json::json!(false));
    }

    #[test]
    fn command_contract_keeps_declared_string_numeric_text() {
        let command = test_command(vec!["demo".into()], Vec::new());
        let contract = crate::InvocationInputContract::from_lightweight_schema_value(
            &serde_json::json!({ "name": "string" }),
        )
        .unwrap()
        .unwrap();

        let result = bind_argv_with_command_and_contract(
            &["--name".into(), "5".into()],
            Some(&command),
            Some(&contract),
        )
        .unwrap();

        assert_eq!(result["name"], serde_json::json!("5"));
    }

    #[test]
    fn command_contract_accepts_repeated_array_values() {
        let command = test_command(vec!["remote".into(), "authorize".into()], Vec::new());
        let contract = crate::InvocationInputContract::from_lightweight_schema_value(
            &serde_json::json!({ "scopes": "string[]?" }),
        )
        .unwrap()
        .unwrap();

        let result = bind_argv_with_command_and_contract(
            &["--scopes".into(), "a".into(), "--scopes".into(), "b".into()],
            Some(&command),
            Some(&contract),
        )
        .unwrap();

        assert_eq!(result["scopes"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn command_contract_promotes_single_array_value() {
        let command = test_command(vec!["remote".into(), "authorize".into()], Vec::new());
        let contract = crate::InvocationInputContract::from_lightweight_schema_value(
            &serde_json::json!({ "scopes": "string[]?" }),
        )
        .unwrap()
        .unwrap();

        let result = bind_argv_with_command_and_contract(
            &["--scopes".into(), "a".into()],
            Some(&command),
            Some(&contract),
        )
        .unwrap();

        assert_eq!(result["scopes"], serde_json::json!(["a"]));
    }

    #[test]
    fn command_contract_rejects_non_object_parameters() {
        let contract = crate::InvocationInputContract::from_lightweight_schema_value(
            &serde_json::json!({ "limit": "integer?" }),
        )
        .unwrap()
        .unwrap();

        let err = normalize_params_with_contract(
            serde_json::json!(["not", "an", "object"]),
            Some(&contract),
        )
        .unwrap_err();

        assert!(err.contains("invocation parameters must be an object"));
    }

    #[test]
    fn command_string_form_accepts_canonical_ref_glob() {
        let command = test_command(
            vec!["sign".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "item_ref".into(),
                    matcher: crate::CommandArgumentKind::String,
                }],
            }],
        );

        let result = bind_argv_with_command(&["tool:agent-kiwi/*".into()], Some(&command)).unwrap();
        assert_eq!(result["item_ref"], "tool:agent-kiwi/*");
    }

    #[test]
    fn command_variadic_form_binds_multiple_positionals_to_array() {
        let mut command = test_command(
            vec!["sign".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "item_refs".into(),
                    matcher: crate::CommandArgumentKind::String,
                }],
            }],
        );
        command.arguments.push(crate::CommandArgumentDef {
            name: "item_refs".into(),
            kind: crate::CommandArgumentKind::String,
            positional: 1,
            required: true,
            arity: crate::CommandArgumentArity::Variadic,
            description: None,
        });

        let result = bind_argv_with_command(
            &[".ai/directives/a.md".into(), ".ai/directives/b.md".into()],
            Some(&command),
        )
        .unwrap();

        assert_eq!(
            result["item_refs"],
            serde_json::json!([".ai/directives/a.md", ".ai/directives/b.md"])
        );
    }

    #[test]
    fn command_canonical_ref_form_rejects_canonical_ref_glob() {
        let command = test_command(
            vec!["sign".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "item_ref".into(),
                    matcher: crate::CommandArgumentKind::CanonicalRef,
                }],
            }],
        );

        let result = bind_argv_with_command(&["tool:agent-kiwi/*".into()], Some(&command));
        assert!(result
            .unwrap_err()
            .contains("do not match any positional form"));
    }

    #[test]
    fn command_path_form_binds_bundle_source_positional() {
        let command = test_command(
            vec!["bundle".into(), "verify".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "source".into(),
                    matcher: crate::CommandArgumentKind::Path,
                }],
            }],
        );

        let result = bind_argv_with_command(
            &[
                "bundles/core".into(),
                "--registry-root".into(),
                "bundles/core".into(),
            ],
            Some(&command),
        )
        .unwrap();

        assert_eq!(result["source"], "bundles/core");
        assert_eq!(result["registry_root"], "bundles/core");
        assert!(result.get("_args").is_none());
    }

    #[test]
    fn command_path_form_accepts_flagged_source_without_positional() {
        let command = test_command(
            vec!["bundle".into(), "verify".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "source".into(),
                    matcher: crate::CommandArgumentKind::Path,
                }],
            }],
        );

        let result =
            bind_argv_with_command(&["--source".into(), "bundles/core".into()], Some(&command))
                .unwrap();

        assert_eq!(result["source"], "bundles/core");
    }

    #[test]
    fn command_path_form_rejects_missing_source() {
        let command = test_command(
            vec!["bundle".into(), "verify".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "source".into(),
                    matcher: crate::CommandArgumentKind::Path,
                }],
            }],
        );

        let err = bind_argv_with_command(&[], Some(&command)).unwrap_err();
        assert!(err.contains("requires <source>"), "got: {err}");
    }

    #[test]
    fn command_path_form_rejects_source_without_value() {
        let command = test_command(
            vec!["bundle".into(), "verify".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "source".into(),
                    matcher: crate::CommandArgumentKind::Path,
                }],
            }],
        );

        let err = bind_argv_with_command(&["--source".into()], Some(&command)).unwrap_err();
        assert!(err.contains("--source requires a value"), "got: {err}");
    }

    #[test]
    fn command_form_binds_bundle_install_positionals() {
        let command = test_command(
            vec!["bundle".into(), "install".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![
                    crate::CommandArgumentSlot {
                        field: "name".into(),
                        matcher: crate::CommandArgumentKind::String,
                    },
                    crate::CommandArgumentSlot {
                        field: "source_path".into(),
                        matcher: crate::CommandArgumentKind::Path,
                    },
                ],
            }],
        );

        let result = bind_argv_with_command(
            &["agent-kiwi".into(), "bundles/agent-kiwi".into()],
            Some(&command),
        )
        .unwrap();

        assert_eq!(result["name"], "agent-kiwi");
        assert_eq!(result["source_path"], "bundles/agent-kiwi");
        assert!(result.get("_args").is_none());
    }

    #[test]
    fn command_form_binds_bundle_remove_positional() {
        let command = test_command(
            vec!["bundle".into(), "remove".into()],
            vec![crate::CommandArgumentForm {
                slots: vec![crate::CommandArgumentSlot {
                    field: "name".into(),
                    matcher: crate::CommandArgumentKind::String,
                }],
            }],
        );

        let result = bind_argv_with_command(&["agent-kiwi".into()], Some(&command)).unwrap();

        assert_eq!(result["name"], "agent-kiwi");
        assert!(result.get("_args").is_none());
    }

    #[test]
    fn command_rejects_invalid_declared_json_argument() {
        let mut command = test_command(vec!["scheduler".into(), "register".into()], Vec::new());
        command.arguments.push(crate::CommandArgumentDef {
            name: "params".into(),
            kind: crate::CommandArgumentKind::Json,
            positional: 0,
            required: false,
            arity: crate::CommandArgumentArity::One,
            description: None,
        });

        let result =
            bind_argv_with_command(&["--params".into(), "{not-json}".into()], Some(&command));

        assert!(result.unwrap_err().contains("--params must be valid JSON"));
    }

    fn test_command(
        tokens: Vec<String>,
        forms: Vec<crate::CommandArgumentForm>,
    ) -> crate::CommandDef {
        crate::CommandDef {
            name: tokens.join("-"),
            tokens,
            description: String::new(),
            aliases: Vec::new(),
            help: None,
            arguments: Vec::new(),
            forms,
            defaults: Default::default(),
            parameter_binding: None,
            control_flags: Vec::new(),
            project: None,
            dispatch: crate::CommandDispatch::Group,
            source_file: std::path::PathBuf::new(),
            provenance: crate::CommandProvenance::default(),
        }
    }
}
