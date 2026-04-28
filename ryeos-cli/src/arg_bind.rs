//! Argument binding — parse remaining argv into a JSON parameters object.
//!
//! The CLI does not declare params in verb YAMLs. Instead, it parses the
//! tail of argv as key=value pairs and passes them through as a JSON object.
//! Positional args (anything without `=`) become `_args: [...]`.
//!
//! The daemon validates params against the item's schema.

use serde_json::Value;

use crate::error::CliDispatchError;

/// Bind tail argv tokens into a JSON parameters object.
///
/// `--key value` and `--key=value` become `{ "key": "value" }`.
/// Remaining positional tokens become `{ "_args": [...] }`.
pub fn bind_tail(tail: &[String]) -> Result<Value, CliDispatchError> {
    let mut params = serde_json::Map::new();
    let mut positional = Vec::new();
    let mut i = 0;

    while i < tail.len() {
        let token = &tail[i];
        if let Some(key) = token.strip_prefix("--") {
            if key.contains('=') {
                // --key=value
                let (k, v) = key.split_once('=').unwrap();
                params.insert(k.to_string(), Value::String(v.to_string()));
            } else if i + 1 < tail.len() {
                // --key value
                i += 1;
                params.insert(key.to_string(), Value::String(tail[i].clone()));
            } else {
                // --key (no value) → treat as boolean true
                params.insert(key.to_string(), Value::Bool(true));
            }
        } else {
            positional.push(Value::String(token.clone()));
        }
        i += 1;
    }

    if !positional.is_empty() {
        params.insert("_args".to_string(), Value::Array(positional));
    }

    Ok(Value::Object(params))
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
