use regex::Regex;
use ryeos_handler_protocol::ParseErrKind;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug)]
pub struct ParseError {
    pub kind: ParseErrKind,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegexKvConfig {
    patterns: Vec<RegexKvPattern>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegexKvPattern {
    regex: String,
    key_group: usize,
    value_group: usize,
}

pub fn validate_config(config: &Value) -> Result<(), String> {
    let cfg: RegexKvConfig =
        serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    if cfg.patterns.is_empty() {
        return Err("regex_kv: patterns list is empty".into());
    }
    for (i, p) in cfg.patterns.iter().enumerate() {
        let re = Regex::new(&p.regex).map_err(|e| format!("patterns[{i}] regex: {e}"))?;
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

pub fn parse(config: &Value, content: &str) -> Result<Value, ParseError> {
    let cfg: RegexKvConfig = serde_json::from_value(config.clone()).map_err(|e| ParseError {
        kind: ParseErrKind::Internal,
        message: format!("regex_kv config: {e}"),
    })?;

    let mut out = Map::new();
    for p in &cfg.patterns {
        let re = Regex::new(&p.regex).map_err(|e| ParseError {
            kind: ParseErrKind::Internal,
            message: format!("regex_kv: {e}"),
        })?;
        for cap in re.captures_iter(content) {
            let key = cap
                .get(p.key_group)
                .ok_or_else(|| ParseError {
                    kind: ParseErrKind::Internal,
                    message: format!("regex_kv: key_group {} missing from match", p.key_group),
                })?
                .as_str()
                .to_string();
            let value = cap
                .get(p.value_group)
                .ok_or_else(|| ParseError {
                    kind: ParseErrKind::Internal,
                    message: format!("regex_kv: value_group {} missing from match", p.value_group),
                })?
                .as_str()
                .to_string();
            out.insert(key, Value::String(value));
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_python_dunders() {
        let cfg = json!({
            "patterns": [
                {
                    "regex": r#"(?m)^__(\w+)__\s*=\s*"([^"]+)""#,
                    "key_group": 1,
                    "value_group": 2
                }
            ]
        });
        let out = parse(
            &cfg,
            "__version__ = \"1.0\"\n__executor_id__ = \"native:foo\"\n",
        )
        .unwrap();
        assert_eq!(out["version"], "1.0");
        assert_eq!(out["executor_id"], "native:foo");
    }

    #[test]
    fn multiple_patterns_all_apply() {
        let cfg = json!({
            "patterns": [
                { "regex": r#"(?m)^__(\w+)__\s*=\s*"([^"]+)""#,
                  "key_group": 1, "value_group": 2 },
                { "regex": r#"(?m)^#\s*([A-Z]+):\s*(.+)$"#,
                  "key_group": 1, "value_group": 2 }
            ]
        });
        let out = parse(&cfg, "__version__ = \"1.0\"\n# TAG: alpha\n").unwrap();
        assert_eq!(out["version"], "1.0");
        assert_eq!(out["TAG"], "alpha");
    }

    #[test]
    fn capture_group_out_of_range_rejected_at_boot() {
        let cfg = json!({
            "patterns": [
                { "regex": r#"(?m)^__(\w+)__"#,
                  "key_group": 1, "value_group": 5 }
            ]
        });
        let err = validate_config(&cfg).unwrap_err();
        assert!(
            err.contains("value_group") && err.contains("out of range"),
            "expected boot-time out-of-range rejection, got: {err}"
        );
    }

    #[test]
    fn capture_group_key_out_of_range_rejected_at_boot() {
        let cfg = json!({
            "patterns": [
                { "regex": r#"(?m)^literal$"#,
                  "key_group": 1, "value_group": 0 }
            ]
        });
        let err = validate_config(&cfg).unwrap_err();
        assert!(
            err.contains("key_group") && err.contains("out of range"),
            "expected boot-time out-of-range rejection, got: {err}"
        );
    }

    #[test]
    fn validate_config_catches_bad_regex() {
        let err = validate_config(&json!({
            "patterns": [{ "regex": "[", "key_group": 1, "value_group": 2 }]
        }))
        .unwrap_err();
        assert!(err.contains("regex"));
    }
}
