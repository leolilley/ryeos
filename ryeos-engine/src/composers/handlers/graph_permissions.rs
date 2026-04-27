//! `GraphPermissionsComposer` — projects a graph item's `permissions:`
//! list into the composed view's `policy_facts["effective_caps"]` so the
//! daemon-side callback cap enforcement (V5.5 P2) gates graph action
//! callbacks the same way it gates directive tool callbacks.
//!
//! The identity composer was a no-op for policy facts: it returned an
//! empty `policy_facts` map, which meant `derive_effective_caps` in the
//! launcher returned `[]`, and the callback token was minted with
//! deny-all caps. Every action callback the walker attempted was then
//! denied at the daemon boundary — even though the graph's own
//! `permissions:` list permitted them. This was a split-brain: the
//! walker self-policed via `graph.permissions` while the daemon
//! enforced a different (empty) set.
//!
//! This composer closes the gap by lifting `permissions:` into the
//! standard policy fact pathway. After switching, the walker stops
//! self-policing — it consumes `envelope.policy.effective_caps` like
//! any other runtime. One source of truth, one gate (the daemon).

use serde_json::Value;

use crate::resolution::{KindComposedView, ResolutionError, ResolvedAncestor};

use super::KindComposer;

/// Native composer handler ID. Graph kind schemas declare
/// `composer: rye/core/graph_permissions` to bind to this handler.
pub const HANDLER_ID: &str = "rye/core/graph_permissions";

pub struct GraphPermissionsComposer;

impl KindComposer for GraphPermissionsComposer {
    /// The graph permissions composer takes no configuration.
    /// Reject anything other than `null` or an empty mapping so a typo
    /// in a kind schema's `composer_config` block fails loud at boot.
    fn validate_config(&self, config: &Value) -> Result<(), String> {
        match config {
            Value::Null => Ok(()),
            Value::Object(obj) if obj.is_empty() => Ok(()),
            Value::Object(_) => Err(
                "graph_permissions composer takes no config: composer_config must be null or {}"
                    .to_string(),
            ),
            other => Err(format!(
                "graph_permissions composer takes no config: composer_config must be null or {{}} \
                 (got {})",
                value_type(other)
            )),
        }
    }

    /// Compose: copy root parsed verbatim into `composed`, then lift
    /// `permissions:` (top-level sequence) into
    /// `policy_facts["effective_caps"]` so the launcher's
    /// `derive_effective_caps` reads it.
    ///
    /// When `permissions` is absent or not an array, `effective_caps`
    /// is set to an empty array — deny-all, which is the correct
    /// posture for a graph with no declared permissions.
    fn compose(
        &self,
        _config: &Value,
        _root: &ResolvedAncestor,
        root_parsed: &Value,
        _ancestors: &[ResolvedAncestor],
        _ancestor_parsed: &[Value],
    ) -> Result<KindComposedView, ResolutionError> {
        let mut policy_facts = std::collections::HashMap::new();

        let caps = root_parsed
            .get("permissions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        policy_facts.insert(
            "effective_caps".to_string(),
            Value::Array(
                caps.into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );

        Ok(KindComposedView {
            composed: root_parsed.clone(),
            derived: std::collections::HashMap::new(),
            policy_facts,
        })
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
    fn copies_root_parsed_verbatim() {
        let r = ancestor("r");
        let view = GraphPermissionsComposer
            .compose(&Value::Null, &r, &json!({ "k": 1 }), &[], &[])
            .unwrap();
        assert_eq!(view.composed, json!({ "k": 1 }));
        assert!(view.derived.is_empty());
    }

    #[test]
    fn lifts_permissions_into_effective_caps() {
        let r = ancestor("r");
        let view = GraphPermissionsComposer
            .compose(
                &Value::Null,
                &r,
                &json!({
                    "permissions": ["rye.execute.tool.echo", "rye.execute.tool.read"],
                    "config": { "start": "done" }
                }),
                &[],
                &[],
            )
            .unwrap();
        let caps = view.policy_fact_string_seq("effective_caps");
        assert_eq!(
            caps,
            vec!["rye.execute.tool.echo", "rye.execute.tool.read"]
        );
    }

    #[test]
    fn empty_permissions_yields_empty_caps() {
        let r = ancestor("r");
        let view = GraphPermissionsComposer
            .compose(
                &Value::Null,
                &r,
                &json!({ "permissions": [], "config": {} }),
                &[],
                &[],
            )
            .unwrap();
        let caps = view.policy_fact_string_seq("effective_caps");
        assert!(caps.is_empty(), "expected empty caps, got: {caps:?}");
    }

    #[test]
    fn missing_permissions_yields_empty_caps() {
        let r = ancestor("r");
        let view = GraphPermissionsComposer
            .compose(&Value::Null, &r, &json!({ "config": {} }), &[], &[])
            .unwrap();
        let caps = view.policy_fact_string_seq("effective_caps");
        assert!(caps.is_empty(), "expected empty caps, got: {caps:?}");
    }

    #[test]
    fn non_string_permissions_are_skipped() {
        let r = ancestor("r");
        let view = GraphPermissionsComposer
            .compose(
                &Value::Null,
                &r,
                &json!({
                    "permissions": ["rye.execute.tool.echo", 42, null, true],
                }),
                &[],
                &[],
            )
            .unwrap();
        let caps = view.policy_fact_string_seq("effective_caps");
        assert_eq!(caps, vec!["rye.execute.tool.echo"]);
    }

    #[test]
    fn validate_config_accepts_null() {
        GraphPermissionsComposer.validate_config(&Value::Null).unwrap();
    }

    #[test]
    fn validate_config_accepts_empty_object() {
        GraphPermissionsComposer.validate_config(&json!({})).unwrap();
    }

    #[test]
    fn validate_config_rejects_non_empty_object() {
        let err = GraphPermissionsComposer
            .validate_config(&json!({ "permissions_path": "custom" }))
            .unwrap_err();
        assert!(
            err.contains("graph_permissions composer takes no config"),
            "got: {err}"
        );
    }
}
