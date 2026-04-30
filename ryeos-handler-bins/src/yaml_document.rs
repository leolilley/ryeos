use ryeos_handler_protocol::ParseErrKind;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug)]
pub struct ParseError {
    pub kind: ParseErrKind,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YamlDocumentConfig {
    #[serde(default)]
    require_mapping: bool,
}

pub fn validate_config(config: &Value) -> Result<(), String> {
    let _: YamlDocumentConfig =
        serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn parse(config: &Value, content: &str) -> Result<Value, ParseError> {
    let cfg: YamlDocumentConfig = serde_json::from_value(config.clone()).map_err(|e| {
        ParseError {
            kind: ParseErrKind::Internal,
            message: format!("yaml_document config: {e}"),
        }
    })?;

    if content.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }

    let yaml: serde_yaml::Value =
        serde_yaml::from_str(content).map_err(|e| ParseError {
            kind: ParseErrKind::Syntax,
            message: format!("yaml_document parse: {e}"),
        })?;

    if cfg.require_mapping && !matches!(yaml, serde_yaml::Value::Mapping(_)) {
        return Err(ParseError {
            kind: ParseErrKind::Schema,
            message: "yaml_document: require_mapping=true rejects non-mapping root".into(),
        });
    }

    serde_json::to_value(yaml).map_err(|e| ParseError {
        kind: ParseErrKind::Internal,
        message: format!("yaml→json: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_yaml_mapping() {
        let out = parse(&json!({ "require_mapping": true }), "name: foo\nversion: 1\n").unwrap();
        assert_eq!(out["name"], "foo");
    }

    #[test]
    fn require_mapping_rejects_scalar() {
        let err = parse(&json!({ "require_mapping": true }), "42\n").unwrap_err();
        assert!(err.message.contains("require_mapping"));
    }

    #[test]
    fn require_mapping_rejects_sequence() {
        let err = parse(&json!({ "require_mapping": true }), "- a\n- b\n").unwrap_err();
        assert!(err.message.contains("require_mapping"));
    }

    #[test]
    fn validate_config_rejects_unknown_field() {
        let err = validate_config(&json!({ "require_mapping": true, "bogus": 1 })).unwrap_err();
        assert!(
            err.contains("unknown field") || err.contains("bogus"),
            "expected unknown-field rejection, got: {err}"
        );
    }

    #[test]
    fn empty_yields_empty_object() {
        let out = parse(&json!({}), "").unwrap();
        assert!(out.as_object().unwrap().is_empty());
    }
}
