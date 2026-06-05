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

use crate::command::{CommandArgumentKind, CommandArgumentSlot, CommandDef};

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
    let Some(command) = command else {
        return Ok(bind_argv(argv));
    };

    let mut value = bind_argv(argv);
    decode_structured_command_fields(&mut value, command)?;

    let forms = command.forms.clone();
    if forms.is_empty() {
        reject_undeclared_positionals(&value, command)?;
        return Ok(value);
    }

    let Some(obj) = value.as_object_mut() else {
        return Ok(value);
    };
    let positionals: Vec<String> = obj
        .remove("_args")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(ToString::to_string))
        .collect();

    if positionals.is_empty() {
        return Ok(value);
    }

    for form in &forms {
        let unset_slots: Vec<&CommandArgumentSlot> = form
            .slots
            .iter()
            .filter(|slot| !obj.contains_key(&normalise_key(&slot.field)))
            .collect();
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
            return Ok(value);
        }
    }

    Err(format!(
        "positional arguments {:?} do not match any positional form for alias {:?}",
        positionals, command.tokens
    ))
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
            category: "commands".into(),
            section: "commands".into(),
            name: tokens.join("-"),
            tokens,
            description: String::new(),
            aliases: Vec::new(),
            help: None,
            arguments: Vec::new(),
            forms,
            parameter_binding: None,
            project: None,
            dispatch: crate::CommandDispatch::Group,
            source_file: std::path::PathBuf::new(),
            provenance: crate::CommandProvenance::default(),
        }
    }
}
