use std::collections::{HashMap, HashSet};

use regex::Regex;
use ryeos_handler_protocol::{ComposeRequest, ComposeSuccess, ResolutionStepNameWire};
use serde::Deserialize;
use serde_json::{Map, Value};

mod permissions;

use permissions::narrow_capabilities;

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
        if rule.strategy == ComposerStrategy::KeyedSeqMergeRootLast {
            match rule.key.as_deref() {
                Some(key) if !key.is_empty() => {}
                Some(_) => {
                    return Err(format!(
                        "extends_chain: field `{}` has empty key for keyed_seq_merge_root_last",
                        rule.name
                    ));
                }
                None => {
                    return Err(format!(
                        "extends_chain: field `{}` uses keyed_seq_merge_root_last but has no key",
                        rule.name
                    ));
                }
            }
        } else if rule.key.is_some() {
            return Err(format!(
                "extends_chain: field `{}` sets `key` but strategy is not `keyed_seq_merge_root_last`",
                rule.name
            ));
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
        (
            ResolutionStepNameWire::PipelineInit,
            format!("invalid composer_config: {e}"),
        )
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

    if rule.strategy == ComposerStrategy::DictMergeRootLast && !value.is_object() {
        return Err((
            ResolutionStepNameWire::PipelineInit,
            format!(
                "{ref_label}: `{}` must be a mapping for dict_merge_root_last",
                rule.name
            ),
        ));
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
                    format!("{ref_label}: `{}.{key}` must be an array", rule.name),
                )
            })?;
            for (i, v) in arr.iter().enumerate() {
                if !v.is_string() {
                    return Err((
                        ResolutionStepNameWire::PipelineInit,
                        format!("{ref_label}: `{}.{key}[{i}]` must be a string", rule.name),
                    ));
                }
            }
        }
    }
    if rule.strategy == ComposerStrategy::KeyedSeqMergeRootLast {
        let key = rule.key.as_deref().unwrap_or("id");
        let arr = value.as_array().ok_or_else(|| {
            (
                ResolutionStepNameWire::PipelineInit,
                format!(
                    "{ref_label}: `{}` must be an array for keyed_seq_merge_root_last",
                    rule.name
                ),
            )
        })?;
        for (i, item) in arr.iter().enumerate() {
            let obj = item.as_object().ok_or_else(|| {
                (
                    ResolutionStepNameWire::PipelineInit,
                    format!("{ref_label}: `{}[{i}]` must be an object", rule.name),
                )
            })?;
            match obj.get(key).and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => {}
                _ => {
                    return Err((
                        ResolutionStepNameWire::PipelineInit,
                        format!(
                            "{ref_label}: `{}[{i}].{key}` must be a non-empty string",
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
        ComposerStrategy::ReplaceRootLast => {
            if let Some(value) = last_non_null_field(ancestor_parsed, root_parsed, &rule.name) {
                if let Value::Object(obj) = composed {
                    obj.insert(rule.name.clone(), value.clone());
                }
            }
        }
        ComposerStrategy::DictMergeRootLast => {
            let mut merged: Map<String, Value> = Map::new();
            for parent in ancestor_parsed {
                merge_object_root_last(&mut merged, parent.get(&rule.name));
            }
            merge_object_root_last(&mut merged, root_parsed.get(&rule.name));
            if let Value::Object(obj) = composed {
                obj.insert(rule.name.clone(), Value::Object(merged));
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
        ComposerStrategy::KeyedSeqMergeRootLast => {
            let merged = merge_keyed_seq_root_last(
                ancestor_parsed,
                root_parsed.get(&rule.name),
                &rule.name,
                rule.key.as_deref().unwrap_or("id"),
            );
            if let Value::Object(obj) = composed {
                obj.insert(rule.name.clone(), Value::Array(merged));
            }
        }
        ComposerStrategy::NarrowAgainstParentEffective => {
            let child_has = root_parsed
                .get(&rule.name)
                .map(|v| !v.is_null())
                .unwrap_or(false);

            if !child_has {
                // Child omitted the field — inherit from first ancestor that has it.
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
            } else {
                // Child declared the field — narrow each verb against parent effective.
                let child_val = root_parsed.get(&rule.name).unwrap();
                let parent_val = ancestor_parsed
                    .iter()
                    .find_map(|p| p.get(&rule.name).filter(|v| !v.is_null()));

                let narrowed = match (
                    child_val.as_object(),
                    parent_val.and_then(|v| v.as_object()),
                ) {
                    (Some(child_map), Some(parent_map)) => {
                        let mut result = Map::new();
                        let all_verbs: HashSet<&str> = child_map
                            .keys()
                            .chain(parent_map.keys())
                            .map(|s| s.as_str())
                            .collect();

                        for verb in all_verbs {
                            let child_verb_caps = child_map
                                .get(verb)
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(String::from))
                                        .collect::<Vec<String>>()
                                })
                                .unwrap_or_default();

                            let parent_verb_caps = parent_map
                                .get(verb)
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(String::from))
                                        .collect::<Vec<String>>()
                                })
                                .unwrap_or_default();

                            if child_map.contains_key(verb) {
                                // Child declared this verb — narrow against parent
                                let narrowed_caps =
                                    narrow_capabilities(&child_verb_caps, &parent_verb_caps);
                                result.insert(
                                    verb.to_string(),
                                    Value::Array(
                                        narrowed_caps.into_iter().map(Value::String).collect(),
                                    ),
                                );
                            } else {
                                // Child omitted this verb — inherit parent's caps
                                result.insert(
                                    verb.to_string(),
                                    parent_map.get(verb).cloned().unwrap_or(Value::Null),
                                );
                            }
                        }
                        Value::Object(result)
                    }
                    _ => child_val.clone(), // Not a mapping — verbatim (no narrowing possible)
                };

                if let Value::Object(obj) = composed {
                    obj.insert(rule.name.clone(), narrowed);
                }
            }
        }
        ComposerStrategy::NarrowRequiresCapabilities => {
            narrow_requires_capabilities(&rule.name, composed, ancestor_parsed, root_parsed)?;
        }
    }
    Ok(())
}

/// Compose `requires.capabilities` for a child against its ancestors. See
/// [`ComposerStrategy::NarrowRequiresCapabilities`].
fn narrow_requires_capabilities(
    field: &str,
    composed: &mut Value,
    ancestor_parsed: &[&Value],
    root_parsed: &Value,
) -> Result<(), (ResolutionStepNameWire, String)> {
    // Legacy authoring fails loudly — no silent ignore, no back-compat. Both
    // sub-trees are strict-validated for root + ancestors before composition.
    reject_legacy_permissions(root_parsed)?;
    validate_requires_shape(root_parsed)?;
    if let Some(m) = manifest_value(root_parsed) {
        validate_manifest_shape(m)?;
    }
    for parent in ancestor_parsed {
        reject_legacy_permissions(parent)?;
        validate_requires_shape(parent)?;
        if let Some(m) = manifest_value(parent) {
            validate_manifest_shape(m)?;
        }
    }

    // `declared` and `manifest` inherit/narrow independently, so a child that
    // changes one subtree never drops the other.
    let declared = compose_declared(root_parsed, ancestor_parsed)?;
    let manifest = compose_manifest(root_parsed, ancestor_parsed)?;

    let mut capabilities = Map::new();
    if let Some(d) = declared {
        capabilities.insert("declared".to_string(), d);
    }
    if let Some(m) = manifest {
        capabilities.insert("manifest".to_string(), m);
    }
    if let Value::Object(obj) = composed {
        if capabilities.is_empty() {
            obj.remove(field);
        } else {
            let mut requires = Map::new();
            requires.insert("capabilities".to_string(), Value::Object(capabilities));
            obj.insert(field.to_string(), Value::Object(requires));
        }
    }
    Ok(())
}

/// Reject a legacy top-level `permissions:` block. It is removed from the
/// schema, but `composed_value_contract` ignores extra top-level keys, so an
/// old item would otherwise silently lose its authority — fail it instead.
pub(crate) fn reject_legacy_permissions(
    parsed: &Value,
) -> Result<(), (ResolutionStepNameWire, String)> {
    if parsed
        .get("permissions")
        .map(|v| !v.is_null())
        .unwrap_or(false)
    {
        return Err((
            ResolutionStepNameWire::PipelineInit,
            "top-level `permissions:` is removed — declare action authority as a flat list \
             under `requires.capabilities.declared`"
                .to_string(),
        ));
    }
    Ok(())
}

/// Validate the `requires` tree's known keys so typos and dropped legacy keys
/// (e.g. `callbacks`) fail loudly at compose time rather than minting nothing.
pub(crate) fn validate_requires_shape(
    parsed: &Value,
) -> Result<(), (ResolutionStepNameWire, String)> {
    let err = |m: String| (ResolutionStepNameWire::PipelineInit, m);
    let Some(requires) = parsed.get("requires").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let req_map = requires
        .as_object()
        .ok_or_else(|| err("`requires` must be a mapping".to_string()))?;
    for key in req_map.keys() {
        if key != "capabilities" {
            return Err(err(format!(
                "unknown key `requires.{key}` (only `capabilities` is allowed)"
            )));
        }
    }
    let Some(caps) = req_map.get("capabilities").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let caps_map = caps
        .as_object()
        .ok_or_else(|| err("`requires.capabilities` must be a mapping".to_string()))?;
    for key in caps_map.keys() {
        if key != "declared" && key != "manifest" {
            return Err(err(format!(
                "unknown key `requires.capabilities.{key}` \
                 (only `declared` and `manifest` are allowed)"
            )));
        }
    }
    Ok(())
}

/// `requires.capabilities.<sub>` mapping for `parsed`, if present.
fn capability_subtree<'a>(parsed: &'a Value, sub: &str) -> Option<&'a Map<String, Value>> {
    parsed
        .get("requires")?
        .get("capabilities")?
        .get(sub)?
        .as_object()
}

/// `requires.capabilities.declared` value (a list of cap strings), if present.
pub(crate) fn declared_value(parsed: &Value) -> Option<&Value> {
    parsed
        .get("requires")?
        .get("capabilities")?
        .get("declared")
        .filter(|v| !v.is_null())
}

/// Compose the `declared` list: child caps narrowed against the nearest
/// ancestor declaring `declared` (drop semantics — an allowance, safe to trim);
/// inherited verbatim when the child omits it.
fn compose_declared(
    root_parsed: &Value,
    ancestor_parsed: &[&Value],
) -> Result<Option<Value>, (ResolutionStepNameWire, String)> {
    let child = declared_value(root_parsed);
    let parent = ancestor_parsed.iter().find_map(|p| declared_value(p));
    match (child, parent) {
        (None, None) => Ok(None),
        (None, Some(p)) => {
            validate_declared_shape(p)?;
            Ok(Some(p.clone()))
        }
        (Some(c), None) => {
            validate_declared_shape(c)?;
            Ok(Some(c.clone()))
        }
        (Some(c), Some(p)) => {
            validate_declared_shape(c)?;
            validate_declared_shape(p)?;
            let narrowed = narrow_capabilities(&string_array(c), &string_array(p));
            Ok(Some(Value::Array(
                narrowed.into_iter().map(Value::String).collect(),
            )))
        }
    }
}

/// Strict shape check for `declared`: a list of cap strings (the cap encodes
/// its own verb). Fails (does not filter) so malformed authority is caught at
/// compose rather than silently turned into deny-all / partial caps.
pub(crate) fn validate_declared_shape(
    declared: &Value,
) -> Result<(), (ResolutionStepNameWire, String)> {
    let err = |m: &str| (ResolutionStepNameWire::PipelineInit, m.to_string());
    let arr = declared
        .as_array()
        .ok_or_else(|| err("`requires.capabilities.declared` must be a list of cap strings"))?;
    if arr.iter().any(|v| !v.is_string()) {
        return Err(err(
            "`requires.capabilities.declared` must contain only strings",
        ));
    }
    Ok(())
}

fn string_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// The manifest-backed runtime-authority vocabulary lives in
// `ryeos_bundle::runtime_authority` (`RuntimeAuthorityDecls` /
// `RuntimeAuthorityRequirements`, `BundleEventOperation` /
// `RuntimeVaultOperation`). Mirrored here so the composer can fail loud at
// compose time without a dependency on that crate; the launch-time parser is
// still the authoritative gate.
const BUNDLE_EVENT_OPS: &[&str] = &["append", "scan"];
const RUNTIME_VAULT_OPS: &[&str] = &["put", "get", "delete", "list"];

/// The only key permitted directly under `requires.capabilities.manifest`: the
/// `runtime_authority` family set the daemon mints from the signed manifest.
const MANIFEST_KEYS: &[&str] = &["runtime_authority"];

/// One runtime-authority family and how it narrows. Operation-based families
/// (`ops = Some`) compare `(id, operation)` pairs; the pattern-based
/// item-authoring family (`ops = None`) compares `(kind, namespace)` with the
/// parent namespace pattern covering the child. Adding a family here teaches
/// both the shape check and the narrowing check at once, mirroring the closed
/// family set in `ryeos_bundle::runtime_authority`.
struct ManifestFamily {
    key: &'static str,
    id_key: &'static str,
    ops: Option<&'static [&'static str]>,
}

const RUNTIME_AUTHORITY_FAMILIES: &[ManifestFamily] = &[
    ManifestFamily {
        key: "bundle_events",
        id_key: "event_kind",
        ops: Some(BUNDLE_EVENT_OPS),
    },
    ManifestFamily {
        key: "runtime_vault",
        id_key: "namespace",
        ops: Some(RUNTIME_VAULT_OPS),
    },
    ManifestFamily {
        key: "item_authoring",
        id_key: "kind",
        ops: None,
    },
];

/// The `runtime_authority` sub-object of a `manifest` value, if present.
fn runtime_authority_value(manifest: &Value) -> Option<&Value> {
    manifest.get("runtime_authority").filter(|v| !v.is_null())
}

/// `requires.capabilities.manifest` value, if present and non-null.
pub(crate) fn manifest_value(parsed: &Value) -> Option<&Value> {
    parsed
        .get("requires")?
        .get("capabilities")?
        .get("manifest")
        .filter(|v| !v.is_null())
}

/// Strict shape check for the `manifest` sub-tree at compose time: the only key
/// is `runtime_authority`, whose families are the operation-based
/// `bundle_events` / `runtime_vault` resource lists (each entry a mapping with a
/// non-empty id and a non-empty list of known operations) and the pattern-based
/// `item_authoring` list (each entry a `{kind, namespace}` mapping). Fails loud
/// rather than deferring malformed authoring to the launch parser.
pub(crate) fn validate_manifest_shape(
    manifest: &Value,
) -> Result<(), (ResolutionStepNameWire, String)> {
    let err = |m: String| (ResolutionStepNameWire::PipelineInit, m);
    let map = manifest
        .as_object()
        .ok_or_else(|| err("`requires.capabilities.manifest` must be a mapping".to_string()))?;
    for key in map.keys() {
        if !MANIFEST_KEYS.contains(&key.as_str()) {
            return Err(err(format!(
                "unknown key `requires.capabilities.manifest.{key}` \
                 (only `runtime_authority` is allowed)"
            )));
        }
    }
    let Some(runtime_authority) = runtime_authority_value(manifest) else {
        return Ok(());
    };
    let ra_map = runtime_authority.as_object().ok_or_else(|| {
        err("`requires.capabilities.manifest.runtime_authority` must be a mapping".to_string())
    })?;
    for key in ra_map.keys() {
        if !RUNTIME_AUTHORITY_FAMILIES
            .iter()
            .any(|family| family.key == key)
        {
            return Err(err(format!(
                "unknown key `requires.capabilities.manifest.runtime_authority.{key}` (allowed: {})",
                RUNTIME_AUTHORITY_FAMILIES
                    .iter()
                    .map(|family| family.key)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    for family in RUNTIME_AUTHORITY_FAMILIES {
        match family.ops {
            Some(ops) => {
                validate_manifest_resources(ra_map.get(family.key), family.id_key, ops, family.key)?
            }
            None => validate_item_authoring_entries(ra_map.get(family.key), family.key)?,
        }
    }
    Ok(())
}

/// Shape check for a pattern-based `item_authoring` family: each entry is a
/// mapping with exactly a non-empty string `kind` and a non-empty string
/// `namespace`.
fn validate_item_authoring_entries(
    list: Option<&Value>,
    tag: &str,
) -> Result<(), (ResolutionStepNameWire, String)> {
    let err = |m: String| (ResolutionStepNameWire::PipelineInit, m);
    let Some(value) = list else {
        return Ok(());
    };
    let arr = value.as_array().ok_or_else(|| {
        err(format!(
            "`requires.capabilities.manifest.runtime_authority.{tag}` must be a list"
        ))
    })?;
    for entry in arr {
        let obj = entry
            .as_object()
            .ok_or_else(|| err(format!("each `{tag}` entry must be a mapping")))?;
        for k in obj.keys() {
            if k != "kind" && k != "namespace" {
                return Err(err(format!(
                    "unknown key `{k}` in an `{tag}` entry (allowed: `kind`, `namespace`)"
                )));
            }
        }
        for req_key in ["kind", "namespace"] {
            let value = obj
                .get(req_key)
                .and_then(|v| v.as_str())
                .ok_or_else(|| err(format!("a `{tag}` entry is missing a string `{req_key}`")))?;
            if value.trim().is_empty() {
                return Err(err(format!("a `{tag}` entry has an empty `{req_key}`")));
            }
        }
    }
    Ok(())
}

fn validate_manifest_resources(
    list: Option<&Value>,
    id_key: &str,
    valid_ops: &[&str],
    tag: &str,
) -> Result<(), (ResolutionStepNameWire, String)> {
    let err = |m: String| (ResolutionStepNameWire::PipelineInit, m);
    let Some(value) = list else {
        return Ok(());
    };
    let arr = value.as_array().ok_or_else(|| {
        err(format!(
            "`requires.capabilities.manifest.runtime_authority.{tag}` must be a list"
        ))
    })?;
    for entry in arr {
        let obj = entry
            .as_object()
            .ok_or_else(|| err(format!("each `{tag}` entry must be a mapping")))?;
        for k in obj.keys() {
            if k != id_key && k != "operations" {
                return Err(err(format!(
                    "unknown key `{k}` in a `{tag}` entry (allowed: `{id_key}`, `operations`)"
                )));
            }
        }
        let id = obj
            .get(id_key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| err(format!("a `{tag}` entry is missing a string `{id_key}`")))?;
        if id.trim().is_empty() {
            return Err(err(format!("a `{tag}` entry has an empty `{id_key}`")));
        }
        let ops = obj
            .get("operations")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                err(format!(
                    "`{tag}` entry `{id}` must list `operations` as an array"
                ))
            })?;
        if ops.is_empty() {
            return Err(err(format!(
                "`{tag}` entry `{id}` must list at least one operation"
            )));
        }
        for op in ops {
            let op_str = op
                .as_str()
                .ok_or_else(|| err(format!("`{tag}` operations must be strings")))?;
            if !valid_ops.contains(&op_str) {
                return Err(err(format!(
                    "invalid operation `{op_str}` for `{tag}` (allowed: {})",
                    valid_ops.join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// Compose the `manifest` sub-tree: child must be a subset of the nearest
/// ancestor declaring `manifest` (fail-on-widen — a hard requirement);
/// inherited verbatim when the child omits it.
fn compose_manifest(
    root_parsed: &Value,
    ancestor_parsed: &[&Value],
) -> Result<Option<Value>, (ResolutionStepNameWire, String)> {
    let child = capability_subtree(root_parsed, "manifest");
    let parent = ancestor_parsed
        .iter()
        .find_map(|p| capability_subtree(p, "manifest"));
    match (child, parent) {
        (None, None) => Ok(None),
        (None, Some(p)) => Ok(Some(Value::Object(p.clone()))),
        (Some(c), None) => Ok(Some(Value::Object(c.clone()))),
        (Some(c), Some(p)) => {
            let missing = manifest_missing(c, p);
            if missing.is_empty() {
                Ok(Some(Value::Object(c.clone())))
            } else {
                Err((
                    ResolutionStepNameWire::PipelineInit,
                    format!(
                        "requires.capabilities.manifest.runtime_authority widens parent requirement: {}",
                        missing.join(", ")
                    ),
                ))
            }
        }
    }
}

fn manifest_missing(child: &Map<String, Value>, parent: &Map<String, Value>) -> Vec<String> {
    let mut missing = Vec::new();
    let child_ra = child.get("runtime_authority");
    let parent_ra = parent.get("runtime_authority");
    for family in RUNTIME_AUTHORITY_FAMILIES {
        let child_family = child_ra.and_then(|v| v.get(family.key));
        let parent_family = parent_ra.and_then(|v| v.get(family.key));
        match family.ops {
            Some(_) => collect_missing_resource_requirements(
                child_family,
                parent_family,
                family.id_key,
                family.key,
                &mut missing,
            ),
            None => collect_missing_authoring_requirements(
                child_family,
                parent_family,
                family.key,
                &mut missing,
            ),
        }
    }
    missing.sort();
    missing
}

/// Collect child item-authoring `(kind, namespace)` entries the parent does not
/// cover. A parent entry covers a child entry when the `kind` matches exactly
/// and the parent `namespace` pattern covers the child namespace (so a child may
/// narrow `runtime-authored/*` to `runtime-authored/foo`, but never widen).
fn collect_missing_authoring_requirements(
    child: Option<&Value>,
    parent: Option<&Value>,
    tag: &str,
    missing: &mut Vec<String>,
) {
    let Some(child_arr) = child.and_then(|v| v.as_array()) else {
        return;
    };
    let parent_arr = parent.and_then(|v| v.as_array());
    for entry in child_arr {
        let (Some(kind), Some(namespace)) = (
            entry.get("kind").and_then(|v| v.as_str()),
            entry.get("namespace").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let covered = parent_arr
            .map(|arr| {
                arr.iter().any(|parent_entry| {
                    parent_entry.get("kind").and_then(|v| v.as_str()) == Some(kind)
                        && parent_entry
                            .get("namespace")
                            .and_then(|v| v.as_str())
                            .map(|parent_ns| authoring_namespace_covers(parent_ns, namespace))
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if !covered {
            missing.push(format!("{tag}.{kind}.{namespace}"));
        }
    }
}

/// True when a parent item-authoring namespace pattern covers a child. Mirrors
/// the mint-time rule in `ryeos_bundle::runtime_authority::manifest_backs_requested_cap`:
/// a concrete child (no `*`/`?`) is covered when the parent glob matches it, but
/// a child that itself carries a wildcard is covered ONLY by an identical parent
/// pattern. Glob-vs-glob matching would widen authority (parent `foo?` would
/// "match" child `foo*`, which authorizes names `foo?` never grants), so wildcard
/// children fail closed. The signed-manifest check at mint time is authoritative;
/// this is the compose-time narrowing gate that rejects a child widening its parent.
fn authoring_namespace_covers(parent: &str, child: &str) -> bool {
    if parent == child {
        return true;
    }
    // A wildcard child is only ever covered by an identical parent (handled
    // above) — never by glob-vs-glob matching.
    if child.contains('*') || child.contains('?') {
        return false;
    }
    let mut pattern = String::from("^");
    for ch in parent.chars() {
        match ch {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            other => pattern.push_str(&regex::escape(&other.to_string())),
        }
    }
    pattern.push('$');
    Regex::new(&pattern)
        .map(|re| re.is_match(child))
        .unwrap_or(false)
}

/// Collect child resource/operation pairs that are not covered by the parent.
fn collect_missing_resource_requirements(
    child: Option<&Value>,
    parent: Option<&Value>,
    id_key: &str,
    tag: &str,
    missing: &mut Vec<String>,
) {
    let Some(child_arr) = child.and_then(|v| v.as_array()) else {
        return;
    };

    let parent_index: HashMap<String, HashSet<String>> = parent
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let id = entry.get(id_key)?.as_str()?.to_string();
                    let ops = operation_set(entry);
                    Some((id, ops))
                })
                .collect()
        })
        .unwrap_or_default();

    for entry in child_arr {
        let Some(id) = entry.get(id_key).and_then(|v| v.as_str()) else {
            continue;
        };
        let child_ops = operation_set(entry);
        match parent_index.get(id) {
            Some(parent_ops) => {
                for op in child_ops {
                    if !parent_ops.contains(&op) {
                        missing.push(format!("{tag}.{id}.{op}"));
                    }
                }
            }
            None => {
                for op in child_ops {
                    missing.push(format!("{tag}.{id}.{op}"));
                }
            }
        }
    }
}

/// Collect the string `operations` of a requirement entry into a set.
fn operation_set(entry: &Value) -> HashSet<String> {
    entry
        .get("operations")
        .and_then(|o| o.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn last_non_null_field<'a>(
    ancestor_parsed: &'a [&'a Value],
    root_parsed: &'a Value,
    field: &str,
) -> Option<&'a Value> {
    ancestor_parsed
        .iter()
        .filter_map(|parent| parent.get(field).filter(|v| !v.is_null()))
        .chain(root_parsed.get(field).filter(|v| !v.is_null()))
        .last()
}

fn merge_keyed_seq_root_last(
    ancestor_parsed: &[&Value],
    root_value: Option<&Value>,
    field: &str,
    key: &str,
) -> Vec<Value> {
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, Value> = HashMap::new();

    for source in ancestor_parsed
        .iter()
        .filter_map(|parent| parent.get(field))
        .chain(root_value)
    {
        let Some(arr) = source.as_array() else {
            continue;
        };
        for item in arr {
            let Some(item_key) = item
                .as_object()
                .and_then(|obj| obj.get(key))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if !by_key.contains_key(item_key) {
                order.push(item_key.to_string());
            }
            by_key.insert(item_key.to_string(), item.clone());
        }
    }

    order
        .into_iter()
        .filter_map(|item_key| by_key.remove(&item_key))
        .collect()
}

fn merge_object_root_last(into: &mut Map<String, Value>, source: Option<&Value>) {
    let Some(Value::Object(obj)) = source else {
        return;
    };
    for (key, value) in obj {
        into.insert(key.clone(), value.clone());
    }
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
    key: Option<String>,
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
    ReplaceRootLast,
    DictMergeRootLast,
    DictMergeStringSeqRootLast,
    KeyedSeqMergeRootLast,
    NarrowAgainstParentEffective,
    /// Compose the unified `requires.capabilities` tree, narrowing each sub-tree
    /// against the nearest ancestor that declares it, independently:
    ///
    /// - `declared` (self-asserted action authority) narrows by **dropping** —
    ///   a child keeps only the caps its parent also grants (an allowance, safe
    ///   to trim).
    /// - `manifest` (runtime authority) narrows by **failing** — a child that
    ///   widens beyond its parent's `(event_kind, op)` / `(namespace, op)` pairs
    ///   fails compose, because dropping a hard requirement would only defer the
    ///   failure to a callback authz error.
    ///
    /// Also rejects legacy top-level `permissions:` and any unknown key under
    /// `requires.capabilities` so old authoring fails loudly. The signed bundle
    /// manifest remains the final upper bound for `manifest` at launch.
    NarrowRequiresCapabilities,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ValueType {
    String,
    Mapping,
    #[serde(alias = "array")]
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
    use ryeos_handler_protocol::{ComposeInput, ComposeItemContext, TrustClassWire};
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
                    "strategy": "narrow_against_parent_effective",
                    "expect_value_type": "mapping"
                },
                {
                    "name": "context",
                    "strategy": "dict_merge_string_seq_root_last",
                    "expect_value_type": "mapping",
                    "derive_as": "composed_context",
                    "derived_dict_string_seq": true
                },
                {
                    "name": "model",
                    "strategy": "replace_root_last",
                    "expect_value_type": "mapping"
                },
                {
                    "name": "limits",
                    "strategy": "dict_merge_root_last",
                    "expect_value_type": "mapping"
                },
                {
                    "name": "inputs",
                    "strategy": "keyed_seq_merge_root_last",
                    "key": "name",
                    "expect_value_type": "sequence"
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
                trust_class: TrustClassWire::TrustedBundle,
            },
            parsed,
        }
    }

    fn ancestor_input(name: &str, parsed: Value) -> ComposeInput {
        ComposeInput {
            item: ComposeItemContext {
                requested_id: format!("item:{name}"),
                resolved_ref: format!("item:{name}"),
                trust_class: TrustClassWire::TrustedBundle,
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
        view.derived
            .get(name)
            .and_then(|v| v.as_str().map(String::from))
    }

    // ── runtime-requires narrowing ───────────────────────────────────

    fn requires_config() -> Value {
        json!({
            "extends_field": "extends",
            "fields": [
                {
                    "name": "body",
                    "strategy": "root_verbatim",
                    "required": true,
                    "expect_value_type": "string"
                },
                {
                    "name": "requires",
                    "strategy": "narrow_requires_capabilities",
                    "expect_value_type": "mapping"
                }
            ]
        })
    }

    /// Flatten composed `requires` into sorted `tag:id:op` tokens for assertions.
    fn requires_pairs(view: &ComposeSuccess) -> Vec<String> {
        let mut out = Vec::new();
        let cb = view
            .composed
            .get("requires")
            .and_then(|r| r.get("capabilities"))
            .and_then(|c| c.get("manifest"))
            .and_then(|m| m.get("runtime_authority"));
        if let Some(cb) = cb {
            for (list, id_key, tag) in [
                ("bundle_events", "event_kind", "be"),
                ("runtime_vault", "namespace", "rv"),
            ] {
                if let Some(arr) = cb.get(list).and_then(|v| v.as_array()) {
                    for e in arr {
                        let id = e.get(id_key).and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(ops) = e.get("operations").and_then(|v| v.as_array()) {
                            for op in ops {
                                out.push(format!("{tag}:{id}:{}", op.as_str().unwrap_or("")));
                            }
                        }
                    }
                }
            }
        }
        out.sort();
        out
    }

    fn requires_block(bundle_events: Value, runtime_vault: Value) -> Value {
        json!({
            "capabilities": { "manifest": { "runtime_authority": {
                "bundle_events": bundle_events,
                "runtime_vault": runtime_vault,
            } } }
        })
    }

    /// A `requires` block carrying a `declared` list.
    fn declared_block(caps: Value) -> Value {
        json!({ "capabilities": { "declared": caps } })
    }

    fn declared_execute(view: &ComposeSuccess) -> Vec<String> {
        view.composed
            .get("requires")
            .and_then(|r| r.get("capabilities"))
            .and_then(|c| c.get("declared"))
            .map(string_array)
            .unwrap_or_default()
    }

    #[test]
    fn requires_child_subset_kept_verbatim() {
        let parent = json!({
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append", "scan"] }]),
                json!([]),
            ),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append"] }]),
                json!([]),
            ),
            "body": "b"
        });
        let view = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(requires_pairs(&view), vec!["be:e:append".to_string()]);
    }

    #[test]
    fn requires_child_operation_absent_from_parent_fails() {
        let parent = json!({
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append"] }]),
                json!([]),
            ),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append", "scan"] }]),
                json!([]),
            ),
            "body": "b"
        });
        let err = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap_err();
        assert!(matches!(err.0, ResolutionStepNameWire::PipelineInit));
        assert!(
            err.1.contains("widens parent requirement") && err.1.contains("bundle_events.e.scan"),
            "got: {}",
            err.1
        );
    }

    #[test]
    fn requires_child_omits_inherits_parent() {
        let parent = json!({
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append"] }]),
                json!([{ "namespace": "oauth", "operations": ["get"] }]),
            ),
            "body": ""
        });
        let child = json!({ "extends": "parent", "body": "b" });
        let view = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(
            requires_pairs(&view),
            vec!["be:e:append".to_string(), "rv:oauth:get".to_string()]
        );
    }

    #[test]
    fn requires_child_resource_absent_from_parent_fails() {
        let parent = json!({
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append"] }]),
                json!([]),
            ),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": requires_block(
                json!([
                    { "event_kind": "e", "operations": ["append"] },
                    { "event_kind": "f", "operations": ["append"] }
                ]),
                json!([]),
            ),
            "body": "b"
        });
        let err = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap_err();
        assert!(
            err.1.contains("widens parent requirement") && err.1.contains("bundle_events.f.append"),
            "got: {}",
            err.1
        );
    }

    #[test]
    fn requires_vault_and_events_widening_fails_independently() {
        let parent = json!({
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append"] }]),
                json!([{ "namespace": "oauth", "operations": ["get"] }]),
            ),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append", "scan"] }]),
                json!([{ "namespace": "oauth", "operations": ["get", "put"] }]),
            ),
            "body": "b"
        });
        let err = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap_err();
        assert!(
            err.1.contains("bundle_events.e.scan") && err.1.contains("runtime_vault.oauth.put"),
            "got: {}",
            err.1
        );
    }

    /// A `requires` block carrying only an `item_authoring` family.
    fn authoring_requires(item_authoring: Value) -> Value {
        json!({
            "capabilities": { "manifest": { "runtime_authority": {
                "item_authoring": item_authoring,
            } } }
        })
    }

    #[test]
    fn requires_item_authoring_concrete_child_narrows_parent_wildcard() {
        // Parent grants `runtime-authored/*`; child narrows to a concrete
        // `runtime-authored/foo`, which the parent glob covers.
        let parent = json!({
            "requires": authoring_requires(json!([
                { "kind": "knowledge", "namespace": "runtime-authored/*" }
            ])),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": authoring_requires(json!([
                { "kind": "knowledge", "namespace": "runtime-authored/foo" }
            ])),
            "body": "b"
        });
        let view = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(
            view.composed
                .pointer(
                    "/requires/capabilities/manifest/runtime_authority/item_authoring/0/namespace"
                )
                .and_then(|v| v.as_str()),
            Some("runtime-authored/foo"),
        );
    }

    #[test]
    fn requires_item_authoring_wildcard_child_widening_parent_fails() {
        // Parent grants `runtime-authored/foo?`; child requests the wildcard
        // `runtime-authored/foo*`. Glob-vs-glob would spuriously accept it, but a
        // wildcard child is only covered by an identical parent — fail closed.
        let parent = json!({
            "requires": authoring_requires(json!([
                { "kind": "knowledge", "namespace": "runtime-authored/foo?" }
            ])),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": authoring_requires(json!([
                { "kind": "knowledge", "namespace": "runtime-authored/foo*" }
            ])),
            "body": "b"
        });
        let err = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap_err();
        assert!(
            err.1.contains("widens parent requirement")
                && err
                    .1
                    .contains("item_authoring.knowledge.runtime-authored/foo*"),
            "got: {}",
            err.1
        );
    }

    #[test]
    fn requires_root_level_no_parent_kept_verbatim() {
        // A root directive (no ancestors) keeps its requires verbatim — the
        // signed manifest is the ceiling at launch, not a parent.
        let child = json!({
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append", "scan"] }]),
                json!([]),
            ),
            "body": "b"
        });
        let view = run(requires_config(), child, vec![]).unwrap();
        assert_eq!(
            requires_pairs(&view),
            vec!["be:e:append".to_string(), "be:e:scan".to_string()]
        );
    }

    #[test]
    fn requires_child_checked_against_grandparent_when_parent_omits() {
        let grandparent = json!({
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append"] }]),
                json!([]),
            ),
            "body": ""
        });
        let parent = json!({ "extends": "grandparent", "body": "" });
        let child = json!({
            "extends": "parent",
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append", "scan"] }]),
                json!([]),
            ),
            "body": "b"
        });
        // Ancestors nearest-first: [parent, grandparent].
        let err = run(
            requires_config(),
            child,
            vec![
                ancestor_input("parent", parent),
                ancestor_input("grandparent", grandparent),
            ],
        )
        .unwrap_err();
        assert!(
            err.1.contains("widens parent requirement") && err.1.contains("bundle_events.e.scan"),
            "got: {}",
            err.1
        );
    }

    // ── declared sub-tree (drop semantics) ───────────────────────────

    #[test]
    fn declared_child_narrowed_against_parent_silently_drops() {
        // `declared` is an allowance, not a hard requirement — a child that
        // names a cap the parent lacks is narrowed (dropped), NOT failed.
        let parent =
            json!({ "requires": declared_block(json!(["ryeos.execute.tool.read"])), "body": "" });
        let child = json!({
            "extends": "parent",
            "requires": declared_block(json!(["ryeos.execute.tool.read", "ryeos.execute.tool.bash"])),
            "body": "b"
        });
        let view = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(
            declared_execute(&view),
            vec!["ryeos.execute.tool.read".to_string()]
        );
    }

    #[test]
    fn declared_child_omits_inherits_parent() {
        let parent =
            json!({ "requires": declared_block(json!(["ryeos.execute.tool.read"])), "body": "" });
        let child = json!({ "extends": "parent", "body": "b" });
        let view = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(
            declared_execute(&view),
            vec!["ryeos.execute.tool.read".to_string()]
        );
    }

    #[test]
    fn declared_root_level_kept_verbatim() {
        let child =
            json!({ "requires": declared_block(json!(["ryeos.execute.tool.read"])), "body": "b" });
        let view = run(requires_config(), child, vec![]).unwrap();
        assert_eq!(
            declared_execute(&view),
            vec!["ryeos.execute.tool.read".to_string()]
        );
    }

    // ── mixed-subtree inheritance (declared and manifest independent) ─

    #[test]
    fn changing_declared_preserves_inherited_manifest() {
        let parent = json!({
            "requires": json!({ "capabilities": {
                "declared": ["ryeos.execute.tool.read"],
                "manifest": { "runtime_authority": { "bundle_events": [{ "event_kind": "e", "operations": ["append"] }] } }
            } }),
            "body": ""
        });
        // Child re-declares only `declared` (a subset); parent `manifest` must survive.
        let child = json!({
            "extends": "parent",
            "requires": declared_block(json!(["ryeos.execute.tool.read"])),
            "body": "b"
        });
        let view = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(
            declared_execute(&view),
            vec!["ryeos.execute.tool.read".to_string()]
        );
        assert_eq!(requires_pairs(&view), vec!["be:e:append".to_string()]);
    }

    #[test]
    fn changing_manifest_preserves_inherited_declared() {
        let parent = json!({
            "requires": json!({ "capabilities": {
                "declared": ["ryeos.execute.tool.read"],
                "manifest": { "runtime_authority": { "bundle_events": [{ "event_kind": "e", "operations": ["append"] }] } }
            } }),
            "body": ""
        });
        // Child re-states only `manifest` (a subset); parent `declared` must survive.
        let child = json!({
            "extends": "parent",
            "requires": requires_block(
                json!([{ "event_kind": "e", "operations": ["append"] }]),
                json!([]),
            ),
            "body": "b"
        });
        let view = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(
            declared_execute(&view),
            vec!["ryeos.execute.tool.read".to_string()]
        );
        assert_eq!(requires_pairs(&view), vec!["be:e:append".to_string()]);
    }

    // ── legacy rejection + strict shape (fail loud, no back-compat) ───

    #[test]
    fn legacy_top_level_permissions_rejected() {
        let child = json!({ "permissions": ["ryeos.execute.tool.read"], "body": "b" });
        let err = run(requires_config(), child, vec![]).unwrap_err();
        assert!(
            err.1.contains("`permissions:` is removed")
                && err.1.contains("requires.capabilities.declared"),
            "got: {}",
            err.1
        );
    }

    #[test]
    fn legacy_callbacks_key_rejected() {
        let child = json!({
            "requires": { "capabilities": { "callbacks": {
                "bundle_events": [{ "event_kind": "e", "operations": ["append"] }]
            } } },
            "body": "b"
        });
        let err = run(requires_config(), child, vec![]).unwrap_err();
        assert!(
            err.1
                .contains("unknown key `requires.capabilities.callbacks`"),
            "got: {}",
            err.1
        );
    }

    #[test]
    fn declared_as_map_rejected() {
        // `declared` is a flat list of cap strings — a verb-bucketed mapping
        // (the legacy shape) fails loudly.
        let child = json!({
            "requires": { "capabilities": { "declared": { "execute": ["x"] } } },
            "body": "b"
        });
        let err = run(requires_config(), child, vec![]).unwrap_err();
        assert!(err.1.contains("must be a list"), "got: {}", err.1);
    }

    #[test]
    fn declared_non_string_cap_rejected() {
        let child = json!({
            "requires": { "capabilities": { "declared": [42] } },
            "body": "b"
        });
        let err = run(requires_config(), child, vec![]).unwrap_err();
        assert!(err.1.contains("only strings"), "got: {}", err.1);
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
            "permissions": { "execute": ["ryeos.execute.tool.bash"] },
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
            vec!["ryeos.execute.tool.bash"]
        );
        assert_eq!(derived_string(&view, "body").unwrap(), "body-text");
    }

    #[test]
    fn child_field_narrowed_against_parent() {
        // With narrow_against_parent_effective, child's caps must be
        // covered by parent. Parent has bash, child requests read —
        // bash doesn't cover read, so narrowed to empty.
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.read"] },
            "body": "body"
        });
        let p = json!({
            "permissions": { "execute": ["ryeos.execute.tool.bash"] },
            "body": ""
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
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
    fn directive_model_replaces_root_last() {
        let r = json!({
            "name": "child",
            "extends": "parent",
            "model": {
                "provider": "openrouter",
                "name": "anthropic/claude-sonnet",
                "context_window": 200000
            },
            "body": "child body"
        });
        let p = json!({
            "name": "parent",
            "model": {
                "provider": "openrouter",
                "name": "deepseek/deepseek-v4-pro",
                "context_window": 128000
            },
            "body": "parent body"
        });

        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();

        assert_eq!(view.composed["model"]["name"], "anthropic/claude-sonnet");
        assert_eq!(view.composed["model"]["context_window"], 200000);
        assert_eq!(derived_string(&view, "body").unwrap(), "child body");
    }

    #[test]
    fn directive_model_is_inherited_when_child_omits_it() {
        let r = json!({
            "name": "child",
            "extends": "parent",
            "body": "child body"
        });
        let p = json!({
            "name": "parent",
            "model": {
                "provider": "openrouter",
                "name": "deepseek/deepseek-v4-pro",
                "context_window": 128000
            },
            "body": "parent body"
        });

        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();

        assert_eq!(view.composed["model"]["name"], "deepseek/deepseek-v4-pro");
        assert_eq!(derived_string(&view, "body").unwrap(), "child body");
    }

    #[test]
    fn directive_limits_merge_root_last() {
        let r = json!({
            "name": "child",
            "extends": "parent",
            "limits": { "spend_usd": 0.2 },
            "body": "child body"
        });
        let p = json!({
            "name": "parent",
            "limits": {
                "turns": 8,
                "tokens": 65536,
                "spend_usd": 0.1,
                "duration_seconds": 60
            },
            "body": "parent body"
        });

        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();

        assert_eq!(view.composed["limits"]["turns"], 8);
        assert_eq!(view.composed["limits"]["tokens"], 65536);
        assert_eq!(view.composed["limits"]["spend_usd"], 0.2);
        assert_eq!(view.composed["limits"]["duration_seconds"], 60);
    }

    #[test]
    fn directive_inputs_merge_by_name_root_last() {
        let r = json!({
            "name": "child",
            "extends": "parent",
            "inputs": [
                { "name": "history", "type": "string", "required": true },
                { "name": "workspace_state", "type": "string", "required": false }
            ],
            "body": "child body"
        });
        let p = json!({
            "name": "parent",
            "inputs": [
                { "name": "message", "type": "string", "required": true },
                { "name": "history", "type": "string", "required": false }
            ],
            "body": "parent body"
        });

        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        let inputs = view.composed["inputs"].as_array().unwrap();

        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0]["name"], "message");
        assert_eq!(inputs[1]["name"], "history");
        assert_eq!(inputs[1]["required"], true);
        assert_eq!(inputs[2]["name"], "workspace_state");
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
        assert_eq!(view.composed.get("f").unwrap(), &json!({ "any": "shape" }));
    }

    #[test]
    fn replace_root_last_uses_root_when_present() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "layout", "strategy": "replace_root_last", "expect_value_type": "mapping" }
            ]
        });
        let r = json!({ "ext": "p", "layout": { "root": "child" } });
        let p = json!({ "layout": { "root": "parent" } });
        let view = run(cfg, r, vec![ancestor_input("p", p)]).unwrap();
        assert_eq!(
            view.composed.get("layout").unwrap(),
            &json!({ "root": "child" })
        );
    }

    #[test]
    fn replace_root_last_uses_nearest_parent_when_root_omits_field() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "layout", "strategy": "replace_root_last", "expect_value_type": "mapping" }
            ]
        });
        let r = json!({ "ext": "mid" });
        let base = json!({ "layout": { "root": "base" } });
        let mid = json!({ "layout": { "root": "mid" } });
        let view = run(
            cfg,
            r,
            vec![ancestor_input("base", base), ancestor_input("mid", mid)],
        )
        .unwrap();
        assert_eq!(
            view.composed.get("layout").unwrap(),
            &json!({ "root": "mid" })
        );
    }

    #[test]
    fn dict_merge_root_last_shallow_merges_with_root_override() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "ambient", "strategy": "dict_merge_root_last", "expect_value_type": "mapping" }
            ]
        });
        let r = json!({ "ext": "p", "ambient": { "theme": "dark", "child": true } });
        let p = json!({ "ambient": { "theme": "light", "parent": true } });
        let view = run(cfg, r, vec![ancestor_input("p", p)]).unwrap();
        assert_eq!(
            view.composed.get("ambient").unwrap(),
            &json!({ "theme": "dark", "parent": true, "child": true })
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
    fn keyed_seq_merge_root_last_replaces_by_key_and_preserves_order() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                {
                    "name": "commands",
                    "strategy": "keyed_seq_merge_root_last",
                    "key": "id",
                    "expect_value_type": "array"
                }
            ]
        });
        let r = json!({
            "ext": "p",
            "commands": [
                { "id": "view.graph", "label": "Graph Override" },
                { "id": "view.events", "label": "Events" }
            ]
        });
        let p = json!({
            "commands": [
                { "id": "view.graph", "label": "Graph" },
                { "id": "view.trust", "label": "Trust" }
            ]
        });
        let view = run(cfg, r, vec![ancestor_input("p", p)]).unwrap();
        assert_eq!(
            view.composed.get("commands").unwrap(),
            &json!([
                { "id": "view.graph", "label": "Graph Override" },
                { "id": "view.trust", "label": "Trust" },
                { "id": "view.events", "label": "Events" }
            ])
        );
    }

    #[test]
    fn validate_config_rejects_keyed_seq_without_key() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "commands", "strategy": "keyed_seq_merge_root_last" }
            ]
        });
        let err = validate_config(&cfg).unwrap_err();
        assert!(err.contains("has no key"), "got: {err}");
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

    // ── NarrowAgainstParentEffective tests ─────────────────────────

    #[test]
    fn child_cannot_exceed_parent() {
        // Parent allows only read
        let p = json!({
            "name": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.ryeos.file-system.read"] },
            "body": ""
        });
        // Child requests write (not covered by parent)
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.ryeos.file-system.write"] },
            "body": "body"
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        // Narrowed to empty — parent doesn't cover write
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
    }

    #[test]
    fn child_subset_passes_through() {
        // Parent allows wildcard
        let p = json!({
            "name": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.ryeos.file-system.*"] },
            "body": ""
        });
        // Child requests specific tool within wildcard
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.ryeos.file-system.write"] },
            "body": "body"
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec!["ryeos.execute.tool.ryeos.file-system.write"]
        );
    }

    #[test]
    fn child_omits_permissions_inherits_parent() {
        // Parent allows read
        let p = json!({
            "name": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.ryeos.file-system.read"] },
            "body": ""
        });
        // Child does not declare permissions
        let r = json!({
            "name": "child",
            "extends": "parent",
            "body": "body"
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec!["ryeos.execute.tool.ryeos.file-system.read"]
        );
    }

    #[test]
    fn child_omits_verb_inherits_parent_verb() {
        // Parent has execute and fetch
        let p = json!({
            "name": "parent",
            "permissions": {
                "execute": ["ryeos.execute.tool.x"],
                "fetch": ["ryeos.fetch.tool.x"]
            },
            "body": ""
        });
        // Child declares only execute
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.x"] },
            "body": "body"
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        // Child keeps its execute
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec!["ryeos.execute.tool.x"]
        );
        // Child inherits parent's fetch
        let fetch_caps = view
            .composed
            .get("permissions")
            .and_then(|p| p.get("fetch"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(fetch_caps, vec!["ryeos.fetch.tool.x"]);
    }

    #[test]
    fn wildcard_parent_covers_child_wildcard() {
        let p = json!({
            "name": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.*"] },
            "body": ""
        });
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.ryeos.*"] },
            "body": "body"
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec!["ryeos.execute.tool.ryeos.*"]
        );
    }

    #[test]
    fn empty_child_caps_stay_empty() {
        let p = json!({
            "name": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.*"] },
            "body": ""
        });
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": [] },
            "body": "body"
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
    }

    #[test]
    fn global_wildcard_covers_everything() {
        let p = json!({
            "name": "parent",
            "permissions": { "execute": ["*"] },
            "body": ""
        });
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": [
                "ryeos.execute.tool.ryeos.file-system.write",
                "ryeos.execute.service.bundle/install"
            ]},
            "body": "body"
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec![
                "ryeos.execute.tool.ryeos.file-system.write",
                "ryeos.execute.service.bundle/install"
            ]
        );
    }

    // ── 3-level (multilevel) narrowing tests ────────────────────────

    #[test]
    fn three_level_chain_narrows_against_immediate_parent() {
        // Grandparent: broad (ryeos.*)
        let gp = json!({
            "name": "grandparent",
            "permissions": { "execute": ["ryeos.*"] },
            "body": ""
        });
        // Parent: narrowed (ryeos.execute.tool.*)
        let p = json!({
            "name": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.*"] },
            "body": ""
        });
        // Child: requests a specific tool — covered by parent's wildcard
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.ryeos.file-system.read"] },
            "body": "body"
        });
        // ancestors are nearest-first: [parent, grandparent]
        let view = run(
            demo_config(),
            r,
            vec![
                ancestor_input("parent", p),
                ancestor_input("grandparent", gp),
            ],
        )
        .unwrap();
        // Child narrows against immediate parent (ryeos.execute.tool.*),
        // which covers ryeos.execute.tool.ryeos.file-system.read — passes.
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec!["ryeos.execute.tool.ryeos.file-system.read"]
        );
    }

    #[test]
    fn three_level_chain_child_omits_inherits_immediate_parent() {
        // Grandparent: broad
        let gp = json!({
            "name": "grandparent",
            "permissions": { "execute": ["ryeos.*"] },
            "body": ""
        });
        // Parent: narrowed
        let p = json!({
            "name": "parent",
            "permissions": { "execute": ["ryeos.execute.tool.read"] },
            "body": ""
        });
        // Child: omits permissions — should inherit from immediate parent
        let r = json!({
            "name": "child",
            "extends": "parent",
            "body": "body"
        });
        // ancestors are nearest-first: [parent, grandparent]
        let view = run(
            demo_config(),
            r,
            vec![
                ancestor_input("parent", p),
                ancestor_input("grandparent", gp),
            ],
        )
        .unwrap();
        // Child inherits from immediate parent (parent), not grandparent
        assert_eq!(
            policy_fact_string_seq(&view, "effective_caps"),
            vec!["ryeos.execute.tool.read"]
        );
    }

    #[test]
    fn three_level_chain_grandparent_only_child_narrows_against_grandparent() {
        // Grandparent: narrow
        let gp = json!({
            "name": "grandparent",
            "permissions": { "execute": ["ryeos.execute.tool.read"] },
            "body": ""
        });
        // Parent: omits permissions (inherits from grandparent)
        let p = json!({
            "name": "parent",
            "body": ""
        });
        // Child: requests broad — should narrow against parent's effective
        // (which was inherited from grandparent: ryeos.execute.tool.read)
        let r = json!({
            "name": "child",
            "extends": "parent",
            "permissions": { "execute": ["ryeos.*"] },
            "body": "body"
        });
        // ancestors are nearest-first: [parent, grandparent]
        let view = run(
            demo_config(),
            r,
            vec![
                ancestor_input("parent", p),
                ancestor_input("grandparent", gp),
            ],
        )
        .unwrap();
        // Child's ryeos.* narrowed against parent's effective (inherited from gp: ryeos.execute.tool.read)
        // ryeos.* is NOT covered by ryeos.execute.tool.read → empty
        assert!(
            policy_fact_string_seq(&view, "effective_caps").is_empty(),
            "child broad cap should be narrowed to empty against grandparent's narrow cap inherited by parent"
        );
    }
}
