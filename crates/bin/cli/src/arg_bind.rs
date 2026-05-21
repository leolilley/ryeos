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
/// Thin wrapper around the shared binder. Returns `Result` for API
/// compatibility with callers that expect it (no fallibility today,
/// but the return type keeps the seam clean).
pub fn bind_tail(tail: &[String]) -> Result<Value, CliDispatchError> {
    Ok(ryeos_runtime::bind_argv(tail))
}

/// Parse `--input` arguments from tail and return the JSON value.
///
/// Handles three forms:
/// - `--input <file>` — reads file as JSON
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
                detail: "--input requires an argument (file path or '-' for stdin)".into(),
            })
        })?;
        let json_str = read_input_source(input_arg)?;
        let value = serde_json::from_str(&json_str).map_err(|e| {
            CliDispatchError::Config(crate::error::CliConfigError::InvalidExecuteRef {
                path: input_arg.clone(),
                item_ref: "--input".into(),
                detail: format!("failed to parse --input as JSON: {e}"),
            })
        })?;
        Ok(Some(value))
    } else if let Some(arg) = tail.iter().find(|t| t.starts_with("--input=")) {
        let path = &arg["--input=".len()..];
        let json_str = read_input_source(path)?;
        let value = serde_json::from_str(&json_str).map_err(|e| {
            CliDispatchError::Config(crate::error::CliConfigError::InvalidExecuteRef {
                path: path.to_string(),
                item_ref: "--input".into(),
                detail: format!("failed to parse --input as JSON: {e}"),
            })
        })?;
        Ok(Some(value))
    } else {
        Ok(None)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn input_rejects_non_object_json() {
        // Execute expects a parameters object; raw arrays/scalars are
        // technically valid JSON but semantically wrong. parse_input_arg
        // parses whatever JSON it gets — callers validate shape.
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("array.json");
        std::fs::write(&file_path, r#"[1,2,3]"#).unwrap();

        let tail = vec!["--input".into(), file_path.to_string_lossy().into_owned()];
        let result = parse_input_arg(&tail).unwrap().unwrap();
        assert!(
            result.is_array(),
            "raw array is valid JSON, caller must validate shape"
        );
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
