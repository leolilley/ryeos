//! `ExtendsChainComposer` — generic composer for kinds whose
//! composition rule is "walk the resolved extends chain and merge
//! per-field according to a per-field strategy".
//!
//! Engine code never hardcodes field names. The kind's
//! `composer_config` block names:
//!   * `extends_field` — the field on the parsed root that signals
//!     "this item extends another"; used solely for the
//!     extends-invariant check (declared XOR ancestor chain non-empty).
//!   * `fields` — per-field merge rules (one per field name the
//!     composer should compose). Each rule names a strategy and may
//!     declare:
//!       - `required`  — when true, the field MUST be present
//!         and non-null on the root (composer-level handler-vs-schema
//!         disagreement guard).
//!       - `expect_value_type` — when set, the field's value type is
//!         asserted against the named primitive (`string`, `mapping`,
//!         `sequence`); shape mismatch is hard-failed at compose time.
//!       - `derive_as` — when set, the field's COMPOSED value (after
//!         the strategy is applied) is also exposed in the view's
//!         `derived` map under this name (e.g. `derive_as: body` on
//!         the body field, `derive_as: composed_context` on the
//!         context field). Engine never hardcodes the names; consumers
//!         and the kind schema agree on the convention.
//!       - `derived_dict_string_seq` — when true and the strategy is
//!         `dict_merge_string_seq_root_last`, the derived value is
//!         expressed as a flat `Map<String, Vec<String>>` (consumer
//!         convenience) rather than the raw object-of-arrays.
//!     Strategies:
//!       - `root_verbatim` — the value is the root's value (no
//!         inheritance, no merge); used for body-style fields.
//!       - `inherit_from_topmost` — child wins; otherwise inherit the
//!         field verbatim from the deepest ancestor that declared it.
//!       - `dict_merge_string_seq_root_last` — merge `Map<String,
//!         Vec<String>>` shape: ancestors first (deepest →
//!         child-side), root last; per-key vectors are concatenated.
//!   * `policy_facts` — list of `{ name, path, expect }` extractors
//!     that pull launcher-facing policy values out of the composed
//!     payload. The launcher and other policy consumers read
//!     `view.policy_facts.get(name)` — the engine never names a
//!     policy fact key in algorithm code.
//!
//! All field-name string literals in this module live ONLY inside
//! `#[cfg(test)]` scaffolding; the algorithm itself reads names from
//! `ExtendsChainConfig`.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::resolution::{
    KindComposedView, ResolutionError, ResolutionStepName, ResolvedAncestor,
};

use super::KindComposer;

/// Native composer handler ID. Kind schemas declare
/// `composer: rye/core/extends_chain` to bind to this handler.
pub const HANDLER_ID: &str = "rye/core/extends_chain";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtendsChainConfig {
    pub extends_field: String,
    pub fields: Vec<ComposerFieldRule>,
    #[serde(default)]
    pub policy_facts: Vec<PolicyFactExtractor>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposerFieldRule {
    pub name: String,
    pub strategy: ComposerStrategy,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub expect_value_type: Option<ValueType>,
    #[serde(default)]
    pub derive_as: Option<String>,
    /// When true and the strategy is `DictMergeStringSeqRootLast`,
    /// the value exposed under `derive_as` is the flat
    /// `Map<String, Vec<String>>` rather than the raw object-of-arrays
    /// stored in `composed`. Ignored for other strategies.
    #[serde(default)]
    pub derived_dict_string_seq: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ComposerStrategy {
    /// Value comes verbatim from the root parser output.
    RootVerbatim,
    /// Child wins; inherit verbatim from deepest ancestor with a
    /// non-null value otherwise.
    InheritFromTopmost,
    /// Merge `Map<String, Vec<String>>` from ancestors (deepest →
    /// child-side) with root contributions appended last. Per-key
    /// vectors are concatenated.
    DictMergeStringSeqRootLast,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueType {
    String,
    Mapping,
    Sequence,
    Boolean,
    Number,
}

impl ValueType {
    fn matches(self, v: &Value) -> bool {
        match self {
            ValueType::String => v.is_string(),
            ValueType::Mapping => v.is_object(),
            ValueType::Sequence => v.is_array(),
            ValueType::Boolean => v.is_boolean(),
            ValueType::Number => v.is_number(),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            ValueType::String => "string",
            ValueType::Mapping => "mapping",
            ValueType::Sequence => "sequence",
            ValueType::Boolean => "boolean",
            ValueType::Number => "number",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFactExtractor {
    pub name: String,
    /// Path of object keys to walk into the composed payload.
    pub path: Vec<String>,
    pub expect: PolicyFactShape,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PolicyFactShape {
    /// Array of strings. Missing path → empty array (not an error;
    /// optional facts are policy-allowed).
    ArrayOfStrings,
}

pub struct ExtendsChainComposer;

impl KindComposer for ExtendsChainComposer {
    fn validate_config(&self, config: &Value) -> Result<(), String> {
        let cfg: ExtendsChainConfig =
            serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        if cfg.extends_field.is_empty() {
            return Err("extends_chain: extends_field must not be empty".into());
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let mut derive_seen: HashSet<&str> = HashSet::new();
        for rule in &cfg.fields {
            if rule.name.is_empty() {
                return Err("extends_chain: field rule name must not be empty".into());
            }
            if !seen.insert(rule.name.as_str()) {
                return Err(format!(
                    "extends_chain: duplicate field rule for `{}`",
                    rule.name
                ));
            }
            if rule.name == cfg.extends_field {
                return Err(format!(
                    "extends_chain: field rule `{}` collides with extends_field",
                    rule.name
                ));
            }
            if let Some(d) = &rule.derive_as {
                if d.is_empty() {
                    return Err(format!(
                        "extends_chain: field `{}` has empty derive_as",
                        rule.name
                    ));
                }
                if !derive_seen.insert(d.as_str()) {
                    return Err(format!(
                        "extends_chain: duplicate derive_as `{d}`"
                    ));
                }
            }
            if rule.derived_dict_string_seq
                && rule.strategy != ComposerStrategy::DictMergeStringSeqRootLast
            {
                return Err(format!(
                    "extends_chain: field `{}` sets `derived_dict_string_seq` but \
                     strategy is not `dict_merge_string_seq_root_last`",
                    rule.name
                ));
            }
        }
        let mut pf_seen: HashSet<&str> = HashSet::new();
        for pf in &cfg.policy_facts {
            if pf.name.is_empty() {
                return Err("extends_chain: policy_fact name must not be empty".into());
            }
            if !pf_seen.insert(pf.name.as_str()) {
                return Err(format!(
                    "extends_chain: duplicate policy_fact `{}`",
                    pf.name
                ));
            }
            if pf.path.is_empty() {
                return Err(format!(
                    "extends_chain: policy_fact `{}` has empty path",
                    pf.name
                ));
            }
            if pf.path.iter().any(|s| s.is_empty()) {
                return Err(format!(
                    "extends_chain: policy_fact `{}` has empty path segment",
                    pf.name
                ));
            }
        }
        Ok(())
    }

    fn compose(
        &self,
        config: &Value,
        root: &ResolvedAncestor,
        root_parsed: &Value,
        ancestors: &[ResolvedAncestor],
        ancestor_parsed: &[Value],
    ) -> Result<KindComposedView, ResolutionError> {
        let cfg: ExtendsChainConfig = serde_json::from_value(config.clone()).map_err(|e| {
            compose_err(format!(
                "invalid composer_config (boot validation should have caught this): {e}"
            ))
        })?;

        if ancestors.len() != ancestor_parsed.len() {
            return Err(compose_err(format!(
                "ancestors ({}) / ancestor_parsed ({}) length mismatch — \
                 caller must keep them parallel",
                ancestors.len(),
                ancestor_parsed.len()
            )));
        }

        let root_has_extends = root_parsed
            .get(&cfg.extends_field)
            .map(|v| !v.is_null())
            .unwrap_or(false);

        match (root_has_extends, ancestors.is_empty()) {
            (true, true) => {
                return Err(compose_err(format!(
                    "root {} declares `{}` but resolution produced an empty ancestor chain",
                    root.resolved_ref, cfg.extends_field
                )));
            }
            (false, false) => {
                return Err(compose_err(format!(
                    "root {} declares no `{}` but resolution produced {} ancestors — \
                     pipeline state is inconsistent",
                    root.resolved_ref,
                    cfg.extends_field,
                    ancestors.len()
                )));
            }
            _ => {}
        }

        // Per-field shape validation across root and every ancestor.
        for rule in &cfg.fields {
            validate_field_shape(rule, root_parsed, &root.resolved_ref, /* is_root */ true)?;
            for (i, parent) in ancestor_parsed.iter().enumerate() {
                validate_field_shape(rule, parent, &ancestors[i].resolved_ref, false)?;
            }
        }

        let mut composed = root_parsed.clone();
        let mut derived: HashMap<String, Value> = HashMap::new();
        for rule in &cfg.fields {
            apply_strategy(rule, &mut composed, ancestor_parsed, root_parsed)?;
            if let Some(name) = &rule.derive_as {
                derived.insert(name.clone(), build_derived_value(rule, &composed));
            }
        }

        let mut policy_facts: HashMap<String, Value> = HashMap::new();
        for pf in &cfg.policy_facts {
            policy_facts.insert(pf.name.clone(), extract_policy_fact(&composed, pf));
        }

        Ok(KindComposedView {
            composed,
            derived,
            policy_facts,
        })
    }
}

fn compose_err(reason: String) -> ResolutionError {
    ResolutionError::StepFailed {
        step: ResolutionStepName::PipelineInit,
        reason: format!("extends_chain composer: {reason}"),
    }
}

fn json_value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validate the shape of one field across one parsed value, per the
/// rule's declared `expect_value_type` + `required` + strategy.
fn validate_field_shape(
    rule: &ComposerFieldRule,
    parsed: &Value,
    ref_label: &str,
    is_root: bool,
) -> Result<(), ResolutionError> {
    let value = parsed.get(&rule.name);
    let present = value.map(|v| !v.is_null()).unwrap_or(false);

    if !present {
        if rule.required && is_root {
            return Err(compose_err(format!(
                "{ref_label}: parser handler emitted no `{field}` field \
                 but the kind's composer_config marks it as required — \
                 parser handler/declared-schema disagreement",
                field = rule.name,
            )));
        }
        return Ok(());
    }

    let value = value.unwrap();
    if let Some(expected) = rule.expect_value_type {
        if !expected.matches(value) {
            return Err(compose_err(format!(
                "{ref_label}: `{}` of type {actual} but composer_config expects {expected_str} — \
                 parser handler/declared-schema disagreement",
                rule.name,
                actual = json_value_type(value),
                expected_str = expected.as_str(),
            )));
        }
    }

    // Strategy-implied additional shape checks.
    if rule.strategy == ComposerStrategy::DictMergeStringSeqRootLast {
        let obj = value.as_object().ok_or_else(|| {
            compose_err(format!(
                "{ref_label}: `{}` must be a mapping for dict_merge_string_seq_root_last",
                rule.name
            ))
        })?;
        for (key, items) in obj {
            let arr = items.as_array().ok_or_else(|| {
                compose_err(format!(
                    "{ref_label}: `{}.{key}` must be an array",
                    rule.name
                ))
            })?;
            for (i, v) in arr.iter().enumerate() {
                if !v.is_string() {
                    return Err(compose_err(format!(
                        "{ref_label}: `{}.{key}[{i}]` must be a string",
                        rule.name
                    )));
                }
            }
        }
    }
    Ok(())
}

fn apply_strategy(
    rule: &ComposerFieldRule,
    composed: &mut Value,
    ancestor_parsed: &[Value],
    root_parsed: &Value,
) -> Result<(), ResolutionError> {
    match rule.strategy {
        ComposerStrategy::RootVerbatim => {
            // No-op: composed already started from root_parsed.clone().
            // Kept explicit so the strategy is a first-class declared
            // intent rather than relying on default behaviour.
        }
        ComposerStrategy::InheritFromTopmost => {
            let child_has = root_parsed
                .get(&rule.name)
                .map(|v| !v.is_null())
                .unwrap_or(false);
            if !child_has {
                for parent in ancestor_parsed {
                    if let Some(v) = parent.get(&rule.name) {
                        if !v.is_null() {
                            if let Value::Object(obj) = composed {
                                obj.insert(rule.name.clone(), v.clone());
                            }
                            break;
                        }
                    }
                }
            }
        }
        ComposerStrategy::DictMergeStringSeqRootLast => {
            let mut merged: Map<String, Value> = Map::new();
            for parent in ancestor_parsed {
                merge_string_seq_dict(&mut merged, parent.get(&rule.name));
            }
            merge_string_seq_dict(&mut merged, root_parsed.get(&rule.name));
            if let Value::Object(obj) = composed {
                obj.insert(rule.name.clone(), Value::Object(merged));
            }
        }
    }
    Ok(())
}

fn merge_string_seq_dict(into: &mut Map<String, Value>, source: Option<&Value>) {
    let Some(Value::Object(obj)) = source else {
        return;
    };
    for (key, items) in obj {
        if let Some(arr) = items.as_array() {
            let entry = into
                .entry(key.clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(target) = entry {
                for item in arr {
                    if item.is_string() {
                        target.push(item.clone());
                    }
                }
            }
        }
    }
}

fn build_derived_value(rule: &ComposerFieldRule, composed: &Value) -> Value {
    let raw = composed.get(&rule.name).cloned().unwrap_or(Value::Null);
    if rule.derived_dict_string_seq {
        // Already shaped as object-of-string-arrays. Return as-is.
        // Kept for symmetry/extension — derived map exposes the
        // strategy output verbatim under a consumer-friendly name.
        return raw;
    }
    raw
}

fn extract_policy_fact(composed: &Value, pf: &PolicyFactExtractor) -> Value {
    let mut cur = composed;
    for seg in &pf.path {
        match cur.get(seg) {
            Some(v) => cur = v,
            None => return shape_default(pf.expect),
        }
    }
    match pf.expect {
        PolicyFactShape::ArrayOfStrings => {
            let arr = cur.as_array().cloned().unwrap_or_default();
            let filtered: Vec<Value> = arr.into_iter().filter(|v| v.is_string()).collect();
            Value::Array(filtered)
        }
    }
}

fn shape_default(shape: PolicyFactShape) -> Value {
    match shape {
        PolicyFactShape::ArrayOfStrings => Value::Array(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolution::{ResolutionStepName, TrustClass};
    use serde_json::json;
    use std::path::PathBuf;

    fn demo_config() -> Value {
        json!({
            "extends_field": "extends",
            "fields": [
                {
                    "name": "body",
                    "strategy": "root_verbatim",
                    "required": true,
                    "expect_value_type": "string",
                    "derive_as": "body"
                },
                {
                    "name": "permissions",
                    "strategy": "inherit_from_topmost",
                    "expect_value_type": "mapping"
                },
                {
                    "name": "context",
                    "strategy": "dict_merge_string_seq_root_last",
                    "expect_value_type": "mapping",
                    "derive_as": "composed_context",
                    "derived_dict_string_seq": true
                }
            ],
            "policy_facts": [
                {
                    "name": "effective_caps",
                    "path": ["permissions", "execute"],
                    "expect": "array_of_strings"
                }
            ]
        })
    }

    fn root() -> ResolvedAncestor {
        ResolvedAncestor {
            requested_id: "item:r".to_string(),
            resolved_ref: "item:r".to_string(),
            source_path: PathBuf::from("/r.item.md"),
            trust_class: TrustClass::TrustedSystem,
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: String::new(),
            raw_content_digest: String::new(),
        }
    }

    fn ancestor(name: &str) -> ResolvedAncestor {
        ResolvedAncestor {
            requested_id: format!("item:{name}"),
            resolved_ref: format!("item:{name}"),
            source_path: PathBuf::from(format!("/{name}.item.md")),
            trust_class: TrustClass::TrustedSystem,
            alias_resolution: None,
            added_by: ResolutionStepName::ResolveExtendsChain,
            raw_content: String::new(),
            raw_content_digest: String::new(),
        }
    }

    fn run(
        cfg: Value,
        r: &Value,
        a: &[ResolvedAncestor],
        ap: &[Value],
    ) -> Result<KindComposedView, ResolutionError> {
        ExtendsChainComposer.compose(&cfg, &root(), r, a, ap)
    }

    // ── Behavioral tests under a representative composer_config ─────

    #[test]
    fn child_inherits_field_from_parent() {
        let r_parsed = json!({
            "name": "child",
            "extends": "parent",
            "body": "body-text"
        });
        let p_parsed = json!({
            "name": "parent",
            "permissions": { "execute": ["rye.execute.tool.bash"] },
            "body": ""
        });
        let view = run(
            demo_config(),
            &r_parsed,
            &[ancestor("parent")],
            &[p_parsed],
        )
        .unwrap();
        assert_eq!(
            view.policy_fact_string_seq("effective_caps"),
            vec!["rye.execute.tool.bash"]
        );
        assert_eq!(view.derived_string("body").unwrap(), "body-text");
    }

    #[test]
    fn child_field_wins_over_parent() {
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": ["rye.execute.tool.read"] },
            "body": "body"
        });
        let p = json!({
            "permissions": { "execute": ["rye.execute.tool.bash"] },
            "body": ""
        });
        let view = run(demo_config(), &r, &[ancestor("parent")], &[p]).unwrap();
        assert_eq!(
            view.policy_fact_string_seq("effective_caps"),
            vec!["rye.execute.tool.read"]
        );
    }

    #[test]
    fn dict_merge_parents_first_then_root() {
        let r = json!({
            "extends": "parent",
            "context": { "before": ["knowledge:c1"] },
            "body": "body"
        });
        let p = json!({
            "context": { "before": ["knowledge:p1"] },
            "body": ""
        });
        let view = run(demo_config(), &r, &[ancestor("parent")], &[p]).unwrap();
        let map = view.derived_string_seq_map("composed_context");
        let before = map.get("before").unwrap();
        assert_eq!(
            before,
            &vec!["knowledge:p1".to_string(), "knowledge:c1".to_string()]
        );
    }

    #[test]
    fn extends_declared_but_no_ancestors_fails() {
        let r = json!({ "extends": "parent", "body": "body" });
        let err = run(demo_config(), &r, &[], &[]).unwrap_err();
        assert!(format!("{err}").contains("empty ancestor chain"));
    }

    #[test]
    fn ancestors_without_extends_fails() {
        let r = json!({ "body": "body" });
        let p = json!({ "body": "" });
        let err = run(demo_config(), &r, &[ancestor("parent")], &[p]).unwrap_err();
        assert!(format!("{err}").contains("declares no `extends`"));
    }

    #[test]
    fn no_extends_no_ancestors_succeeds() {
        let r = json!({ "body": "body" });
        let view = run(demo_config(), &r, &[], &[]).unwrap();
        assert_eq!(view.derived_string("body").unwrap(), "body");
        assert!(view.policy_fact_string_seq("effective_caps").is_empty());
    }

    #[test]
    fn missing_required_field_returns_structured_error_not_panic() {
        let r = json!({});
        let err = run(demo_config(), &r, &[], &[]).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("no `body` field") && msg.contains("item:r"),
            "expected structured handler/schema disagreement error, got: {msg}"
        );
    }

    #[test]
    fn wrong_typed_required_field_returns_structured_error_not_panic() {
        let r = json!({ "body": 42 });
        let err = run(demo_config(), &r, &[], &[]).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("`body`")
                && msg.contains("number")
                && msg.contains("item:r"),
            "expected structured type-disagreement error, got: {msg}"
        );
    }

    #[test]
    fn malformed_inherit_field_fails() {
        // permissions is declared mapping; pass an array.
        let r = json!({ "permissions": ["execute"], "body": "x" });
        let err = run(demo_config(), &r, &[], &[]).unwrap_err();
        assert!(format!("{err}").contains("permissions"));
    }

    #[test]
    fn dict_merge_field_not_object_fails() {
        let r = json!({
            "context": ["before"],
            "body": "x"
        });
        let err = run(demo_config(), &r, &[], &[]).unwrap_err();
        assert!(format!("{err}").contains("context"));
    }

    #[test]
    fn dict_merge_entry_not_string_fails() {
        let r = json!({
            "context": { "before": [1, 2] },
            "body": "x"
        });
        let err = run(demo_config(), &r, &[], &[]).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("context.before[0]") && msg.contains("must be a string"),
            "got: {msg}"
        );
    }

    #[test]
    fn dict_merge_value_not_array_fails() {
        let r = json!({
            "context": { "before": "knowledge:x" },
            "body": "x"
        });
        let err = run(demo_config(), &r, &[], &[]).unwrap_err();
        assert!(format!("{err}").contains("context"));
    }

    // ── Strategies in isolation ─────────────────────────────────────

    #[test]
    fn root_verbatim_isolated() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                {
                    "name": "f",
                    "strategy": "root_verbatim",
                    "required": true,
                    "expect_value_type": "string",
                    "derive_as": "f"
                }
            ]
        });
        let r = json!({ "f": "only-root" });
        let view = run(cfg, &r, &[], &[]).unwrap();
        assert_eq!(view.derived_string("f").unwrap(), "only-root");
    }

    #[test]
    fn inherit_from_topmost_isolated() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "f", "strategy": "inherit_from_topmost" }
            ]
        });
        let r = json!({ "ext": "p" });
        let p = json!({ "f": { "any": "shape" } });
        let view = run(cfg, &r, &[ancestor("p")], &[p]).unwrap();
        assert_eq!(
            view.composed.get("f").unwrap(),
            &json!({ "any": "shape" })
        );
    }

    #[test]
    fn inherit_from_topmost_accepts_arbitrary_object_shape() {
        // Regression: validator must NOT require an `execute` child.
        let cfg = json!({
            "extends_field": "ext",
            "fields": [{ "name": "f", "strategy": "inherit_from_topmost" }]
        });
        let r = json!({ "ext": "p" });
        let p = json!({ "f": { "totally_unrelated_key": [1, 2, 3] } });
        let view = run(cfg, &r, &[ancestor("p")], &[p]).unwrap();
        assert!(view.composed.get("f").is_some());
    }

    #[test]
    fn dict_merge_string_seq_root_last_isolated() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                {
                    "name": "ctx",
                    "strategy": "dict_merge_string_seq_root_last",
                    "derive_as": "ctx",
                    "derived_dict_string_seq": true
                }
            ]
        });
        let r = json!({ "ext": "p", "ctx": { "k": ["c1"] } });
        let p = json!({ "ctx": { "k": ["p1"] } });
        let view = run(cfg, &r, &[ancestor("p")], &[p]).unwrap();
        let map = view.derived_string_seq_map("ctx");
        let v = map.get("k").unwrap();
        assert_eq!(v, &vec!["p1".to_string(), "c1".to_string()]);
    }

    // ── Policy facts ────────────────────────────────────────────────

    #[test]
    fn policy_fact_path_extracts_array_of_strings() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "perms", "strategy": "inherit_from_topmost" }
            ],
            "policy_facts": [
                { "name": "caps", "path": ["perms", "execute"], "expect": "array_of_strings" }
            ]
        });
        let r = json!({ "perms": { "execute": ["a", "b"] } });
        let view = run(cfg, &r, &[], &[]).unwrap();
        assert_eq!(view.policy_fact_string_seq("caps"), vec!["a", "b"]);
    }

    #[test]
    fn policy_fact_missing_path_returns_empty() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [],
            "policy_facts": [
                { "name": "caps", "path": ["perms", "execute"], "expect": "array_of_strings" }
            ]
        });
        let r = json!({});
        let view = run(cfg, &r, &[], &[]).unwrap();
        assert!(view.policy_fact_string_seq("caps").is_empty());
    }

    // ── validate_config tests ───────────────────────────────────────

    #[test]
    fn validate_config_accepts_demo_config() {
        ExtendsChainComposer
            .validate_config(&demo_config())
            .expect("demo config accepted");
    }

    #[test]
    fn validate_config_rejects_unknown_strategy() {
        let cfg = json!({
            "extends_field": "extends",
            "fields": [{ "name": "x", "strategy": "made_up_strategy" }]
        });
        let err = ExtendsChainComposer.validate_config(&cfg).unwrap_err();
        assert!(err.contains("made_up_strategy") || err.contains("unknown variant"));
    }

    #[test]
    fn validate_config_rejects_duplicate_field_rules() {
        let cfg = json!({
            "extends_field": "extends",
            "fields": [
                { "name": "a", "strategy": "inherit_from_topmost" },
                { "name": "a", "strategy": "dict_merge_string_seq_root_last" }
            ]
        });
        let err = ExtendsChainComposer.validate_config(&cfg).unwrap_err();
        assert!(err.contains("duplicate field rule for `a`"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_empty_extends_field() {
        let cfg = json!({
            "extends_field": "",
            "fields": []
        });
        let err = ExtendsChainComposer.validate_config(&cfg).unwrap_err();
        assert!(err.contains("extends_field"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_unknown_top_level_field() {
        let cfg = json!({
            "extends_field": "extends",
            "fields": [],
            "junk_extra": true
        });
        let err = ExtendsChainComposer.validate_config(&cfg).unwrap_err();
        assert!(
            err.contains("unknown field") || err.contains("junk_extra"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_config_rejects_field_rule_colliding_with_extends_field() {
        let cfg = json!({
            "extends_field": "extends",
            "fields": [{ "name": "extends", "strategy": "inherit_from_topmost" }]
        });
        let err = ExtendsChainComposer.validate_config(&cfg).unwrap_err();
        assert!(err.contains("collides with extends_field"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_duplicate_derive_as() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "a", "strategy": "root_verbatim", "derive_as": "x" },
                { "name": "b", "strategy": "root_verbatim", "derive_as": "x" }
            ]
        });
        let err = ExtendsChainComposer.validate_config(&cfg).unwrap_err();
        assert!(err.contains("duplicate derive_as"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_duplicate_policy_fact() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [],
            "policy_facts": [
                { "name": "caps", "path": ["a"], "expect": "array_of_strings" },
                { "name": "caps", "path": ["b"], "expect": "array_of_strings" }
            ]
        });
        let err = ExtendsChainComposer.validate_config(&cfg).unwrap_err();
        assert!(err.contains("duplicate policy_fact"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_empty_policy_fact_path() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [],
            "policy_facts": [
                { "name": "caps", "path": [], "expect": "array_of_strings" }
            ]
        });
        let err = ExtendsChainComposer.validate_config(&cfg).unwrap_err();
        assert!(err.contains("empty path"), "got: {err}");
    }
}
