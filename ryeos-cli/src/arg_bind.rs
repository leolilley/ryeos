//! Argument binding — parse remaining argv into a JSON parameters object.
//!
//! Delegates to the shared `ryeos_runtime::arg_binder` for consistency
//! between CLI and daemon. The CLI only uses this for the `execute`
//! escape-hatch path (item_ref direct mode). Token mode sends raw
//! tokens to the daemon, which binds them server-side.

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
        let tail = vec![
            "--seed".into(),
            "119".into(),
            "./bundles/standard".into(),
        ];
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
}
