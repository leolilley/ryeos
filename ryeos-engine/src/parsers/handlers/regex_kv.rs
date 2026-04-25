//! `parser_regex_kv` — capture key/value pairs from text via regexes.
//!
//! Config:
//! ```yaml
//! patterns:
//!   - regex: "^__(\\w+)__\\s*=\\s*\"([^\"]+)\""
//!     key_group: 1
//!     value_group: 2
//! ```
//!
//! Each pattern is run against the input; every match contributes one
//! key/value pair to the output `Object`. Later matches overwrite
//! earlier ones, mirroring how Python's `__dunder__` extraction worked
//! in the legacy engine.

use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::EngineError;

use super::{NativeParserHandler, ParseInput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegexKvConfig {
    pub patterns: Vec<RegexKvPattern>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegexKvPattern {
    pub regex: String,
    pub key_group: usize,
    pub value_group: usize,
}

pub struct RegexKvHandler;

impl NativeParserHandler for RegexKvHandler {
    fn validate_config(&self, config: &Value) -> Result<(), String> {
        let cfg: RegexKvConfig =
            serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        if cfg.patterns.is_empty() {
            return Err("regex_kv: patterns list is empty".into());
        }
        for (i, p) in cfg.patterns.iter().enumerate() {
            let re = Regex::new(&p.regex).map_err(|e| format!("patterns[{i}] regex: {e}"))?;
            // `captures_len()` includes the implicit whole-match group
            // at index 0, so the maximum *valid* group index is
            // `captures_len() - 1`.
            let max_group = re.captures_len();
            if p.key_group >= max_group {
                return Err(format!(
                    "patterns[{i}] key_group {} out of range (regex has {} group(s) including group 0)",
                    p.key_group, max_group
                ));
            }
            if p.value_group >= max_group {
                return Err(format!(
                    "patterns[{i}] value_group {} out of range (regex has {} group(s) including group 0)",
                    p.value_group, max_group
                ));
            }
        }
        Ok(())
    }

    fn parse(&self, config: &Value, input: ParseInput<'_>) -> Result<Value, EngineError> {
        let cfg: RegexKvConfig = serde_json::from_value(config.clone())
            .map_err(|e| EngineError::Internal(format!("regex_kv config: {e}")))?;

        let mut out = Map::new();
        for p in &cfg.patterns {
            let re = Regex::new(&p.regex)
                .map_err(|e| EngineError::Internal(format!("regex_kv: {e}")))?;
            for cap in re.captures_iter(input.content) {
                let key = cap
                    .get(p.key_group)
                    .ok_or_else(|| {
                        EngineError::Internal(format!(
                            "regex_kv: key_group {} missing from match",
                            p.key_group
                        ))
                    })?
                    .as_str()
                    .to_string();
                let value = cap
                    .get(p.value_group)
                    .ok_or_else(|| {
                        EngineError::Internal(format!(
                            "regex_kv: value_group {} missing from match",
                            p.value_group
                        ))
                    })?
                    .as_str()
                    .to_string();
                out.insert(key, Value::String(value));
            }
        }
        Ok(Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_python_dunders() {
        let h = RegexKvHandler;
        let cfg = json!({
            "patterns": [
                {
                    "regex": r#"(?m)^__(\w+)__\s*=\s*"([^"]+)""#,
                    "key_group": 1,
                    "value_group": 2
                }
            ]
        });
        let out = h
            .parse(
                &cfg,
                ParseInput {
                    content: "__version__ = \"1.0\"\n__executor_id__ = \"native:foo\"\n",
                    path: None,
                },
            )
            .unwrap();
        assert_eq!(out["version"], "1.0");
        assert_eq!(out["executor_id"], "native:foo");
    }

    #[test]
    fn multiple_patterns_all_apply() {
        let h = RegexKvHandler;
        let cfg = json!({
            "patterns": [
                { "regex": r#"(?m)^__(\w+)__\s*=\s*"([^"]+)""#,
                  "key_group": 1, "value_group": 2 },
                { "regex": r#"(?m)^#\s*([A-Z]+):\s*(.+)$"#,
                  "key_group": 1, "value_group": 2 }
            ]
        });
        let out = h
            .parse(
                &cfg,
                ParseInput {
                    content: "__version__ = \"1.0\"\n# TAG: alpha\n",
                    path: None,
                },
            )
            .unwrap();
        assert_eq!(out["version"], "1.0");
        assert_eq!(out["TAG"], "alpha");
    }

    #[test]
    fn capture_group_out_of_range_rejected_at_boot() {
        let h = RegexKvHandler;
        // regex has one capture group (plus implicit group 0) → valid
        // groups are {0, 1}. Ask for group 5 to be out of range.
        let cfg = json!({
            "patterns": [
                { "regex": r#"(?m)^__(\w+)__"#,
                  "key_group": 1, "value_group": 5 }
            ]
        });
        let err = h.validate_config(&cfg).unwrap_err();
        assert!(
            err.contains("value_group") && err.contains("out of range"),
            "expected boot-time out-of-range rejection, got: {err}"
        );
    }

    #[test]
    fn capture_group_key_out_of_range_rejected_at_boot() {
        let h = RegexKvHandler;
        // No capture groups at all → captures_len() == 1 (just group 0)
        // → key_group == 1 is out of range.
        let cfg = json!({
            "patterns": [
                { "regex": r#"(?m)^literal$"#,
                  "key_group": 1, "value_group": 0 }
            ]
        });
        let err = h.validate_config(&cfg).unwrap_err();
        assert!(
            err.contains("key_group") && err.contains("out of range"),
            "expected boot-time out-of-range rejection, got: {err}"
        );
    }

    #[test]
    fn validate_config_catches_bad_regex() {
        let h = RegexKvHandler;
        let err = h
            .validate_config(&json!({
                "patterns": [{ "regex": "[", "key_group": 1, "value_group": 2 }]
            }))
            .unwrap_err();
        assert!(err.contains("regex"));
    }
}
