//! `parser_yaml_document` — parse the body as a single YAML document.
//!
//! Config:
//! ```yaml
//! require_mapping: true   # reject documents whose root is not a map
//! ```

use serde::Deserialize;
use serde_json::Value;

use crate::error::EngineError;

use super::{NativeParserHandler, ParseInput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YamlDocumentConfig {
    #[serde(default)]
    pub require_mapping: bool,
}

pub struct YamlDocumentHandler;

impl NativeParserHandler for YamlDocumentHandler {
    fn validate_config(&self, config: &Value) -> Result<(), String> {
        let _: YamlDocumentConfig =
            serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn parse(&self, config: &Value, input: ParseInput<'_>) -> Result<Value, EngineError> {
        let cfg: YamlDocumentConfig = serde_json::from_value(config.clone())
            .map_err(|e| EngineError::Internal(format!("yaml_document config: {e}")))?;

        // Empty file → empty mapping (lets directives without a header
        // still parse cleanly when the kind format permits it).
        if input.content.trim().is_empty() {
            return Ok(Value::Object(serde_json::Map::new()));
        }

        let yaml: serde_yaml::Value = serde_yaml::from_str(input.content)
            .map_err(|e| EngineError::Internal(format!("yaml_document parse: {e}")))?;

        if cfg.require_mapping && !matches!(yaml, serde_yaml::Value::Mapping(_)) {
            return Err(EngineError::Internal(
                "yaml_document: require_mapping=true rejects non-mapping root".into(),
            ));
        }

        serde_json::to_value(yaml)
            .map_err(|e| EngineError::Internal(format!("yaml→json: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_yaml_mapping() {
        let h = YamlDocumentHandler;
        let cfg = json!({ "require_mapping": true });
        let out = h
            .parse(
                &cfg,
                ParseInput {
                    content: "name: foo\nversion: 1\n",
                    path: None,
                },
            )
            .unwrap();
        assert_eq!(out["name"], "foo");
    }

    #[test]
    fn require_mapping_rejects_scalar() {
        let h = YamlDocumentHandler;
        let cfg = json!({ "require_mapping": true });
        let err = h
            .parse(
                &cfg,
                ParseInput {
                    content: "42\n",
                    path: None,
                },
            )
            .unwrap_err();
        assert!(format!("{err}").contains("require_mapping"));
    }

    #[test]
    fn require_mapping_rejects_sequence() {
        let h = YamlDocumentHandler;
        let cfg = json!({ "require_mapping": true });
        let err = h
            .parse(
                &cfg,
                ParseInput {
                    content: "- a\n- b\n",
                    path: None,
                },
            )
            .unwrap_err();
        assert!(format!("{err}").contains("require_mapping"));
    }

    #[test]
    fn validate_config_rejects_unknown_field() {
        let h = YamlDocumentHandler;
        let err = h
            .validate_config(&json!({ "require_mapping": true, "bogus": 1 }))
            .unwrap_err();
        assert!(
            err.contains("unknown field") || err.contains("bogus"),
            "expected unknown-field rejection, got: {err}"
        );
    }

    #[test]
    fn empty_yields_empty_object() {
        let h = YamlDocumentHandler;
        let out = h
            .parse(
                &json!({}),
                ParseInput {
                    content: "",
                    path: None,
                },
            )
            .unwrap();
        assert!(out.as_object().unwrap().is_empty());
    }
}
