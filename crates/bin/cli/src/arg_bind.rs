//! Argument binding — parse remaining argv into a JSON parameters object.
//!
//! Delegates to the shared `ryeos_runtime::arg_binder` for consistency
//! between CLI and daemon. The CLI only uses this for the `execute`
//! escape-hatch path (item_ref direct mode). Token mode sends raw
//! tokens to the daemon, which binds them server-side.
//!
//! The `--input` flag provides a JSON-first escape hatch for complex
//! parameters (arrays, nested objects, numbers) that the heuristic
//! binder cannot express.

use serde_json::Value;

use crate::error::CliDispatchError;

/// Bind tail argv tokens into a JSON parameters object.
///
/// Thin wrapper around the shared binder, returning `Result` to match
/// the fallible binding seam used at the call sites.
#[allow(dead_code)]
pub fn bind_tail(tail: &[String]) -> Result<Value, CliDispatchError> {
    Ok(ryeos_runtime::bind_argv(tail))
}

/// Parse `--input` arguments from tail and return the JSON value.
///
/// Handles three forms:
/// - `--input <file>` — reads file as JSON
///
/// Descriptor-declared parameter short-circuits, shared by the daemon
/// and offline dispatch paths so the two cannot drift:
/// - `--input <file|->` is honored only when the command's
///   `parameter_binding` declares an input flag;
/// - a single JSON-object argument binds directly when the command
///   declares `single_json_object_arg`.
///
/// Returns `None` when no declared short-circuit applies and normal
/// positional/flag binding should proceed.
pub fn bind_declared_shortcuts(
    tail: &[String],
    command: &ryeos_runtime::CommandDef,
) -> Result<Option<Value>, CliDispatchError> {
    let binding = command.parameter_binding.as_ref();
    if binding.is_some_and(|binding| binding.input_flag.is_some()) {
        if let Some(input) = parse_input_arg(tail)? {
            let input = if command.project.is_some() {
                merge_project_control_flags(input, tail)?
            } else {
                input
            };
            return Ok(Some(input));
        }
    }
    if binding.is_some_and(|binding| binding.single_json_object_arg) && tail.len() == 1 {
        if let Ok(value) = serde_json::from_str::<Value>(&tail[0]) {
            if value.is_object() {
                return Ok(Some(value));
            }
        }
    }
    Ok(None)
}

fn merge_project_control_flags(
    mut input: Value,
    tail: &[String],
) -> Result<Value, CliDispatchError> {
    let Some(obj) = input.as_object_mut() else {
        return Ok(input);
    };

    let mut i = 0;
    while i < tail.len() {
        let token = &tail[i];
        if token == "--no-project" {
            obj.insert("no_project".to_string(), Value::Bool(true));
            i += 1;
            continue;
        }
        if let Some(path) = token
            .strip_prefix("--project=")
            .or_else(|| token.strip_prefix("-p="))
        {
            obj.insert("project".to_string(), Value::String(path.to_string()));
            i += 1;
            continue;
        }
        if token == "--project" || token == "-p" {
            let Some(path) = tail.get(i + 1) else {
                return Err(CliDispatchError::Config(
                    crate::error::CliConfigError::InvalidExecuteRef {
                        path: "<cli>".into(),
                        item_ref: token.clone(),
                        detail: format!("{token} requires a value (path to the project root)"),
                    },
                ));
            };
            if path == "--no-project" || path == "--input" || path == "-p" || path == "--project" {
                return Err(CliDispatchError::Config(
                    crate::error::CliConfigError::InvalidExecuteRef {
                        path: "<cli>".into(),
                        item_ref: token.clone(),
                        detail: format!("{token} requires a value (path to the project root)"),
                    },
                ));
            }
            obj.insert("project".to_string(), Value::String(path.clone()));
            i += 2;
            continue;
        }
        i += 1;
    }

    Ok(input)
}

/// - `--input -` — reads stdin as JSON
/// - `--input=<file>` — equals-form
///
/// Returns `None` if no `--input` flag is present (caller should fall
/// back to heuristic binding).
pub fn parse_input_arg(tail: &[String]) -> Result<Option<Value>, CliDispatchError> {
    if let Some(input_idx) = tail.iter().position(|t| t == "--input") {
        let input_arg = tail.get(input_idx + 1).ok_or_else(|| {
            CliDispatchError::Config(crate::error::CliConfigError::InvalidExecuteRef {
                path: "<cli>".into(),
                item_ref: "--input".into(),
                detail: "--input requires an argument (file path, inline JSON, or '-' for stdin)"
                    .into(),
            })
        })?;
        let text = read_input_source(input_arg)?;
        Ok(Some(parse_input_value(&text, input_arg)?))
    } else if let Some(arg) = tail.iter().find(|t| t.starts_with("--input=")) {
        let path = &arg["--input=".len()..];
        let text = read_input_source(path)?;
        Ok(Some(parse_input_value(&text, path)?))
    } else {
        Ok(None)
    }
}

/// Parse `--input` content. YAML is a strict superset of JSON, so a
/// single YAML parse accepts both forms — no format guessing, no
/// silent JSON→YAML fallback. Accepting YAML lets operators feed a spec
/// straight from a `.yaml` file (e.g. `scheduler register --input
/// spec.yaml`) instead of hand-quoting JSON on the shell.
///
/// `--input` is a structured params/spec value: the result must be an
/// object or array. A bare scalar (the shape malformed JSON degrades to
/// under YAML, e.g. `not valid {{{` → the string `"not valid {{{"`) is
/// rejected loudly rather than slipping through.
fn parse_input_value(text: &str, source: &str) -> Result<Value, CliDispatchError> {
    let invalid = |detail: String| {
        CliDispatchError::Config(crate::error::CliConfigError::InvalidExecuteRef {
            path: source.to_string(),
            item_ref: "--input".into(),
            detail,
        })
    };

    let value = serde_yaml::from_str::<Value>(text)
        .map_err(|e| invalid(format!("failed to parse --input as JSON or YAML: {e}")))?;

    if !value.is_object() && !value.is_array() {
        return Err(invalid(
            "--input must be a JSON or YAML object or array, not a scalar".into(),
        ));
    }
    Ok(value)
}

fn read_input_source(source: &str) -> Result<String, CliDispatchError> {
    if source == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).map_err(|e| {
            CliDispatchError::Config(crate::error::CliConfigError::InvalidExecuteRef {
                path: "<stdin>".into(),
                item_ref: "--input".into(),
                detail: format!("failed to read stdin: {e}"),
            })
        })?;
        Ok(buf)
    } else if is_inline_json(source) {
        // Inline JSON/array literal — `--input '{"x":1}'`. Return it
        // verbatim for `parse_input_value` to parse, rather than treating
        // it as a file path. Only an unambiguous `{`/`[` prefix counts,
        // so file paths and `key: value` YAML stay file-only (covered by
        // the `@file`/stdin/positional paths).
        Ok(source.to_string())
    } else {
        std::fs::read_to_string(source).map_err(|e| {
            CliDispatchError::Config(crate::error::CliConfigError::InvalidExecuteRef {
                path: source.into(),
                item_ref: "--input".into(),
                detail: format!("failed to read --input file '{}': {e}", source),
            })
        })
    }
}

/// An `--input` argument is inline content (not a path) when it begins
/// with a JSON object/array marker. Whitespace is tolerated so a quoted
/// `--input ' {"x":1}'` still counts.
fn is_inline_json(source: &str) -> bool {
    matches!(source.trim_start().chars().next(), Some('{') | Some('['))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_value_accepts_json_and_yaml() {
        // One YAML parse handles both — JSON object syntax is valid YAML.
        let json = parse_input_value(r#"{"schedule_id":"s","enabled":true}"#, "x").unwrap();
        assert_eq!(json["schedule_id"], "s");
        assert_eq!(json["enabled"], true);

        // Block YAML — the shell-quoting-free path.
        let yaml = parse_input_value("schedule_id: s\nenabled: true\n", "x").unwrap();
        assert_eq!(yaml["schedule_id"], "s");
        assert_eq!(yaml["enabled"], true);

        // Malformed input fails with a JSON-or-YAML message.
        let err = parse_input_value("{ this: is: not valid", "x").unwrap_err();
        assert!(format!("{err:?}").contains("JSON or YAML"));
    }

    #[test]
    fn parse_input_value_rejects_bare_scalar() {
        // A bare scalar is valid YAML but not a params/spec value.
        let err = parse_input_value("just a string", "x").unwrap_err();
        assert!(format!("{err:?}").contains("object or array"));
    }

    #[test]
    fn is_inline_json_detects_object_and_array() {
        assert!(is_inline_json(r#"{"x":1}"#));
        assert!(is_inline_json("  [1, 2]"));
        // Paths and bare YAML are NOT inline.
        assert!(!is_inline_json("params.json"));
        assert!(!is_inline_json("/tmp/spec.yaml"));
        assert!(!is_inline_json("key: value"));
        assert!(!is_inline_json("-"));
    }

    #[test]
    fn parse_input_arg_accepts_inline_json() {
        let tail = vec![
            "--input".into(),
            r#"{"game_id":"g","max_actions":60}"#.into(),
        ];
        let value = parse_input_arg(&tail).unwrap().unwrap();
        assert_eq!(value["game_id"], "g");
        assert_eq!(value["max_actions"], 60);
    }

    #[test]
    fn parse_input_arg_equals_form_accepts_inline_json() {
        let tail = vec![r#"--input={"k":[1,2]}"#.into()];
        let value = parse_input_arg(&tail).unwrap().unwrap();
        assert_eq!(value["k"], serde_json::json!([1, 2]));
    }

    #[test]
    fn bind_key_value() {
        let tail = vec!["--name".into(), "foo".into(), "--verbose".into()];
        let result = bind_tail(&tail).unwrap();
        assert_eq!(result.get("name").unwrap(), "foo");
        assert_eq!(result.get("verbose").unwrap(), true);
    }

    #[test]
    fn bind_equals_syntax() {
        let tail = vec!["--seed=119".into()];
        let result = bind_tail(&tail).unwrap();
        assert_eq!(result.get("seed").unwrap(), "119");
    }

    #[test]
    fn bind_positional() {
        let tail = vec!["./bundles/standard".into()];
        let result = bind_tail(&tail).unwrap();
        assert_eq!(result.get("_args").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn bind_mixed() {
        let tail = vec!["--seed".into(), "119".into(), "./bundles/standard".into()];
        let result = bind_tail(&tail).unwrap();
        assert_eq!(result.get("seed").unwrap(), "119");
        assert_eq!(result.get("_args").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn bind_empty() {
        let tail: Vec<String> = vec![];
        let result = bind_tail(&tail).unwrap();
        assert_eq!(result.as_object().unwrap().len(), 0);
    }

    #[test]
    fn bind_flag_no_value() {
        let tail = vec!["--verify".into()];
        let result = bind_tail(&tail).unwrap();
        assert_eq!(result.get("verify").unwrap(), true);
    }

    // ── --input parsing tests ──

    #[test]
    fn input_none_when_absent() {
        let tail = vec!["--name".into(), "foo".into()];
        assert!(parse_input_arg(&tail).unwrap().is_none());
    }

    #[test]
    fn input_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("params.json");
        std::fs::write(
            &file_path,
            r#"{"public_key":"ed25519:abc","scopes":["a","b"]}"#,
        )
        .unwrap();

        let tail = vec!["--input".into(), file_path.to_string_lossy().into_owned()];
        let result = parse_input_arg(&tail).unwrap().unwrap();
        assert_eq!(result["public_key"], "ed25519:abc");
        assert_eq!(result["scopes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn input_reads_file_equals_form() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("params.json");
        std::fs::write(&file_path, r#"{"limit":10,"nested":{"key":"val"}}"#).unwrap();

        let tail = vec![format!("--input={}", file_path.display())];
        let result = parse_input_arg(&tail).unwrap().unwrap();
        assert_eq!(result["limit"], 10);
        assert_eq!(result["nested"]["key"], "val");
    }

    #[test]
    fn input_missing_arg_is_error() {
        let tail = vec!["--input".into()];
        let result = parse_input_arg(&tail);
        assert!(result.is_err());
    }

    #[test]
    fn input_bad_json_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("bad.json");
        std::fs::write(&file_path, "not valid json {{{").unwrap();

        let tail = vec!["--input".into(), file_path.to_string_lossy().into_owned()];
        let result = parse_input_arg(&tail);
        assert!(result.is_err());
    }

    #[test]
    fn input_missing_file_is_error() {
        let tail = vec!["--input".into(), "/nonexistent/file.json".into()];
        let result = parse_input_arg(&tail);
        assert!(result.is_err());
    }

    #[test]
    fn input_accepts_array_rejects_scalar() {
        // Contract: `--input` accepts an object or array; a bare scalar
        // is rejected. (Callers validate the deeper params shape.)
        let dir = tempfile::tempdir().unwrap();

        let array_path = dir.path().join("array.json");
        std::fs::write(&array_path, r#"[1,2,3]"#).unwrap();
        let array = parse_input_arg(&["--input".into(), array_path.to_string_lossy().into_owned()])
            .unwrap()
            .unwrap();
        assert!(array.is_array());

        let scalar_path = dir.path().join("scalar.json");
        std::fs::write(&scalar_path, r#""just a string""#).unwrap();
        let err = parse_input_arg(&["--input".into(), scalar_path.to_string_lossy().into_owned()])
            .unwrap_err();
        assert!(format!("{err:?}").contains("object or array"));
    }

    #[test]
    fn input_duplicate_last_wins() {
        // Two --input flags: first one parses, second is ignored because
        // position() finds the first match.
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.json");
        let file_b = dir.path().join("b.json");
        std::fs::write(&file_a, r#"{"source":"a"}"#).unwrap();
        std::fs::write(&file_b, r#"{"source":"b"}"#).unwrap();

        let tail = vec![
            "--input".into(),
            file_a.to_string_lossy().into_owned(),
            "--input".into(),
            file_b.to_string_lossy().into_owned(),
        ];
        let result = parse_input_arg(&tail).unwrap().unwrap();
        assert_eq!(result["source"], "a", "first --input wins");
    }

    #[test]
    fn input_with_equals_and_space_both_parse() {
        // Both forms in the same tail — position() finds the space form first.
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.json");
        let file_b = dir.path().join("b.json");
        std::fs::write(&file_a, r#"{"form":"space"}"#).unwrap();
        std::fs::write(&file_b, r#"{"form":"equals"}"#).unwrap();

        let tail = vec![
            "--input".into(),
            file_a.to_string_lossy().into_owned(),
            format!("--input={}", file_b.display()),
        ];
        let result = parse_input_arg(&tail).unwrap().unwrap();
        assert_eq!(result["form"], "space", "space form takes priority");
    }
}
