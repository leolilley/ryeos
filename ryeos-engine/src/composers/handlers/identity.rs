//! `IdentityComposer` — built-in no-op composer handler.
//!
//! The composer equivalent of an "empty contract": kinds whose items
//! have no multi-document composition (no extends chain, no
//! inheritance) bind to this handler so "no composition" is an
//! explicit, code-backed, tested behavior rather than a missing-
//! registry special case. Always returns `KindComposedView::None`.

use serde_json::Value;

use crate::resolution::{KindComposedView, ResolutionError, ResolvedAncestor};

use super::KindComposer;

/// Native composer handler ID. Kind schemas declare
/// `composer: rye/core/identity` to bind to this no-op handler.
pub const HANDLER_ID: &str = "rye/core/identity";

pub struct IdentityComposer;

impl KindComposer for IdentityComposer {
    /// The identity composer takes no configuration. Reject anything
    /// other than `null` or an empty mapping so a typo in a kind
    /// schema's `composer_config` block fails loud at boot rather
    /// than being silently ignored.
    fn validate_config(&self, config: &Value) -> Result<(), String> {
        match config {
            Value::Null => Ok(()),
            Value::Object(obj) if obj.is_empty() => Ok(()),
            Value::Object(_) => Err(
                "identity composer takes no config: composer_config must be null or {}"
                    .to_string(),
            ),
            other => Err(format!(
                "identity composer takes no config: composer_config must be null or {{}} \
                 (got {})",
                value_type(other)
            )),
        }
    }

    fn compose(
        &self,
        _config: &Value,
        _root: &ResolvedAncestor,
        root_parsed: &Value,
        _ancestors: &[ResolvedAncestor],
        _ancestor_parsed: &[Value],
    ) -> Result<KindComposedView, ResolutionError> {
        Ok(KindComposedView::identity(root_parsed.clone()))
    }
}

fn value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolution::{ResolutionStepName, TrustClass};
    use serde_json::json;
    use std::path::PathBuf;

    fn ancestor(name: &str) -> ResolvedAncestor {
        ResolvedAncestor {
            requested_id: format!("x:{name}"),
            resolved_ref: format!("x:{name}"),
            source_path: PathBuf::from(format!("/{name}")),
            trust_class: TrustClass::TrustedSystem,
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: String::new(),
            raw_content_digest: String::new(),
        }
    }

    #[test]
    fn returns_root_parsed_verbatim() {
        let r = ancestor("r");
        let view = IdentityComposer
            .compose(&Value::Null, &r, &json!({ "k": 1 }), &[], &[])
            .unwrap();
        assert_eq!(view.composed, json!({ "k": 1 }));
        assert!(view.derived.is_empty());
        assert!(view.policy_facts.is_empty());
    }

    #[test]
    fn ignores_ancestors() {
        let r = ancestor("r");
        let p = ancestor("p");
        let view = IdentityComposer
            .compose(
                &Value::Null,
                &r,
                &json!({ "x": 1 }),
                &[p],
                &[json!({ "anything": 1 })],
            )
            .unwrap();
        assert_eq!(view.composed, json!({ "x": 1 }));
    }

    #[test]
    fn validate_config_accepts_null() {
        IdentityComposer.validate_config(&Value::Null).unwrap();
    }

    #[test]
    fn validate_config_accepts_empty_object() {
        IdentityComposer.validate_config(&json!({})).unwrap();
    }

    #[test]
    fn validate_config_rejects_non_empty_object() {
        let err = IdentityComposer
            .validate_config(&json!({ "anything": 1 }))
            .unwrap_err();
        assert!(err.contains("identity composer takes no config"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_array() {
        let err = IdentityComposer
            .validate_config(&json!([1, 2, 3]))
            .unwrap_err();
        assert!(err.contains("array"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_string() {
        let err = IdentityComposer
            .validate_config(&json!("nope"))
            .unwrap_err();
        assert!(err.contains("string"), "got: {err}");
    }
}
