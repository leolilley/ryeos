use std::collections::{HashMap, HashSet};

use ryeos_handler_protocol::{
    ComposeRequest, ComposeSuccess, ResolutionStepNameWire,
};
use serde::Deserialize;
use serde_json::{Map, Value};

pub fn validate_config(config: &Value) -> Result<(), String> {
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
                return Err(format!("extends_chain: duplicate derive_as `{d}`"));
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

pub fn compose(
    config: &Value,
    request: &ComposeRequest,
) -> Result<ComposeSuccess, (ResolutionStepNameWire, String)> {
    let cfg: ExtendsChainConfig = serde_json::from_value(config.clone()).map_err(|e| {
        (ResolutionStepNameWire::PipelineInit, format!("invalid composer_config: {e}"))
    })?;

    let root_parsed = &request.root.parsed;
    let root_ref = &request.root.item.resolved_ref;
    let ancestor_parsed: Vec<&Value> = request.ancestors.iter().map(|a| &a.parsed).collect();

    let root_has_extends = root_parsed
        .get(&cfg.extends_field)
        .map(|v| !v.is_null())
        .unwrap_or(false);

    match (root_has_extends, ancestor_parsed.is_empty()) {
        (true, true) => {
            return Err((
                ResolutionStepNameWire::PipelineInit,
                format!(
                    "root {root_ref} declares `{}` but resolution produced an empty ancestor chain",
                    cfg.extends_field
                ),
            ));
        }
        (false, false) => {
            return Err((
                ResolutionStepNameWire::PipelineInit,
                format!(
                    "root {root_ref} declares no `{}` but resolution produced {} ancestors — \
                     pipeline state is inconsistent",
                    cfg.extends_field,
                    ancestor_parsed.len()
                ),
            ));
        }
        _ => {}
    }

    for rule in &cfg.fields {
        validate_field_shape(rule, root_parsed, root_ref, true)?;
        for (i, parent) in ancestor_parsed.iter().enumerate() {
            let parent_ref = &request.ancestors[i].item.resolved_ref;
            validate_field_shape(rule, parent, parent_ref, false)?;
        }
    }

    let mut composed = root_parsed.clone();
    let mut derived: HashMap<String, Value> = HashMap::new();
    for rule in &cfg.fields {
        apply_strategy(rule, &mut composed, &ancestor_parsed, root_parsed)?;
        if let Some(name) = &rule.derive_as {
            derived.insert(name.clone(), build_derived_value(rule, &composed));
        }
    }

    let mut policy_facts: HashMap<String, Value> = HashMap::new();
    for pf in &cfg.policy_facts {
        policy_facts.insert(pf.name.clone(), extract_policy_fact(&composed, pf));
    }

    Ok(ComposeSuccess {
        composed,
        derived,
        policy_facts,
    })
}

fn validate_field_shape(
    rule: &ComposerFieldRule,
    parsed: &Value,
    ref_label: &str,
    is_root: bool,
) -> Result<(), (ResolutionStepNameWire, String)> {
    let value = parsed.get(&rule.name);
    let present = value.map(|v| !v.is_null()).unwrap_or(false);

    if !present {
        if rule.required && is_root {
            return Err((
                ResolutionStepNameWire::PipelineInit,
                format!(
                    "{ref_label}: parser handler emitted no `{field}` field \
                     but the kind's composer_config marks it as required — \
                     parser handler/declared-schema disagreement",
                    field = rule.name,
                ),
            ));
        }
        return Ok(());
    }

    let value = value.unwrap();
    if let Some(expected) = rule.expect_value_type {
        if !expected.matches(value) {
            return Err((
                ResolutionStepNameWire::PipelineInit,
                format!(
                    "{ref_label}: `{}` of type {actual} but composer_config expects {expected_str} — \
                     parser handler/declared-schema disagreement",
                    rule.name,
                    actual = json_value_type(value),
                    expected_str = expected.as_str(),
                ),
            ));
        }
    }

    if rule.strategy == ComposerStrategy::DictMergeStringSeqRootLast {
        let obj = value.as_object().ok_or_else(|| {
            (
                ResolutionStepNameWire::PipelineInit,
                format!(
                    "{ref_label}: `{}` must be a mapping for dict_merge_string_seq_root_last",
                    rule.name
                ),
            )
        })?;
        for (key, items) in obj {
            let arr = items.as_array().ok_or_else(|| {
                (
                    ResolutionStepNameWire::PipelineInit,
                    format!(
                        "{ref_label}: `{}.{key}` must be an array",
                        rule.name
                    ),
                )
            })?;
            for (i, v) in arr.iter().enumerate() {
                if !v.is_string() {
                    return Err((
                        ResolutionStepNameWire::PipelineInit,
                        format!(
                            "{ref_label}: `{}.{key}[{i}]` must be a string",
                            rule.name
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn apply_strategy(
    rule: &ComposerFieldRule,
    composed: &mut Value,
    ancestor_parsed: &[&Value],
    root_parsed: &Value,
) -> Result<(), (ResolutionStepNameWire, String)> {
    match rule.strategy {
        ComposerStrategy::RootVerbatim => {}
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendsChainConfig {
    extends_field: String,
    fields: Vec<ComposerFieldRule>,
    #[serde(default)]
    policy_facts: Vec<PolicyFactExtractor>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposerFieldRule {
    name: String,
    strategy: ComposerStrategy,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    expect_value_type: Option<ValueType>,
    #[serde(default)]
    derive_as: Option<String>,
    #[serde(default)]
    derived_dict_string_seq: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ComposerStrategy {
    RootVerbatim,
    InheritFromTopmost,
    DictMergeStringSeqRootLast,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ValueType {
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
struct PolicyFactExtractor {
    name: String,
    path: Vec<String>,
    expect: PolicyFactShape,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PolicyFactShape {
    ArrayOfStrings,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_handler_protocol::{
        ComposeInput, ComposeItemContext, TrustClassWire,
    };
    use serde_json::json;

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

    fn root_input(parsed: Value) -> ComposeInput {
        ComposeInput {
            item: ComposeItemContext {
                requested_id: "item:r".into(),
                resolved_ref: "item:r".into(),
                trust_class: TrustClassWire::TrustedSystem,
            },
            parsed,
        }
    }

    fn ancestor_input(name: &str, parsed: Value) -> ComposeInput {
        ComposeInput {
            item: ComposeItemContext {
                requested_id: format!("item:{name}"),
                resolved_ref: format!("item:{name}"),
                trust_class: TrustClassWire::TrustedSystem,
            },
            parsed,
        }
    }

    fn run(
        cfg: Value,
        root: Value,
        ancestors: Vec<ComposeInput>,
    ) -> Result<ComposeSuccess, (ResolutionStepNameWire, String)> {
        compose(
            &cfg,
            &ComposeRequest {
                composer_config: Value::Null,
                root: root_input(root),
                ancestors,
            },
        )
    }

    fn policy_fact_string_seq(view: &ComposeSuccess, name: &str) -> Vec<String> {
        view.policy_facts
            .get(name)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn derived_string(view: &ComposeSuccess, name: &str) -> Option<String> {
        view.derived.get(name).and_then(|v| v.as_str().map(String::from))
    }

    fn derived_string_seq_map(view: &ComposeSuccess, name: &str) -> HashMap<String, Vec<String>> {
        view.derived
            .get(name)
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| {
                        let items = v
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (k.clone(), items)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

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
            r_parsed,
            vec![ancestor_input("parent", p_parsed)],
        )
        .unwrap();
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec!["rye.execute.tool.bash"]
        );
        assert_eq!(derived_string(&view, "body").unwrap(), "body-text");
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
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
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
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        let map = derived_string_seq_map(&view, "composed_context");
        let before = map.get("before").unwrap();
        assert_eq!(
            before,
            &vec!["knowledge:p1".to_string(), "knowledge:c1".to_string()]
        );
    }

    #[test]
    fn extends_declared_but_no_ancestors_fails() {
        let r = json!({ "extends": "parent", "body": "body" });
        let result = run(demo_config(), r, vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn ancestors_without_extends_fails() {
        let r = json!({ "body": "body" });
        let p = json!({ "body": "" });
        let result = run(demo_config(), r, vec![ancestor_input("parent", p)]);
        assert!(result.is_err());
    }

    #[test]
    fn no_extends_no_ancestors_succeeds() {
        let r = json!({ "body": "body" });
        let view = run(demo_config(), r, vec![]).unwrap();
        assert_eq!(derived_string(&view, "body").unwrap(), "body");
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
    }

    #[test]
    fn missing_required_field_returns_error() {
        let r = json!({});
        let result = run(demo_config(), r, vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn validate_config_accepts_demo_config() {
        validate_config(&demo_config()).expect("demo config accepted");
    }

    #[test]
    fn validate_config_rejects_unknown_strategy() {
        let cfg = json!({
            "extends_field": "extends",
            "fields": [{ "name": "x", "strategy": "made_up_strategy" }]
        });
        let err = validate_config(&cfg).unwrap_err();
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
        let err = validate_config(&cfg).unwrap_err();
        assert!(err.contains("duplicate field rule for `a`"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_empty_extends_field() {
        let cfg = json!({
            "extends_field": "",
            "fields": []
        });
        let err = validate_config(&cfg).unwrap_err();
        assert!(err.contains("extends_field"), "got: {err}");
    }

    #[test]
    fn validate_config_rejects_unknown_top_level_field() {
        let cfg = json!({
            "extends_field": "extends",
            "fields": [],
            "junk_extra": true
        });
        let err = validate_config(&cfg).unwrap_err();
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
        let err = validate_config(&cfg).unwrap_err();
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
        let err = validate_config(&cfg).unwrap_err();
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
        let err = validate_config(&cfg).unwrap_err();
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
        let err = validate_config(&cfg).unwrap_err();
        assert!(err.contains("empty path"), "got: {err}");
    }

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
        let view = run(cfg, r, vec![]).unwrap();
        assert_eq!(derived_string(&view, "f").unwrap(), "only-root");
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
        let view = run(cfg, r, vec![ancestor_input("p", p)]).unwrap();
        assert_eq!(
            view.composed.get("f").unwrap(),
            &json!({ "any": "shape" })
        );
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
        let view = run(cfg, r, vec![ancestor_input("p", p)]).unwrap();
        let map = derived_string_seq_map(&view, "ctx");
        let v = map.get("k").unwrap();
        assert_eq!(v, &vec!["p1".to_string(), "c1".to_string()]);
    }

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
        let view = run(cfg, r, vec![]).unwrap();
        assert_eq!(policy_fact_string_seq(&view, "caps"), vec!["a", "b"]);
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
        let view = run(cfg, r, vec![]).unwrap();
        assert!(policy_fact_string_seq(&view, "caps").is_empty());
    }
}
