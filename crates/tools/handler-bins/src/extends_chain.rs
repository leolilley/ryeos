use std::collections::{BTreeSet, HashMap, HashSet};

use regex::Regex;
use ryeos_handler_protocol::{
    ComposeRequest, ComposeSuccess, ComposerFieldRequirement, ComposerFieldSemantics,
    ResolutionStepNameWire,
};
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

/// Validate that configured rules preserve each requested field as one atomic
/// value. Strategy names remain private to this handler; the engine asks only
/// for generic semantics through the handler protocol.
pub fn validate_field_requirements(
    config: &Value,
    requirements: &[ComposerFieldRequirement],
) -> Result<(), String> {
    let cfg: ExtendsChainConfig =
        serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    for requirement in requirements {
        if requirement.path.is_empty() || requirement.path.iter().any(String::is_empty) {
            return Err(
                "extends_chain composer field requirement path must not be empty".to_string(),
            );
        }
        let field = &requirement.path[0];
        let rule = cfg
            .fields
            .iter()
            .find(|rule| rule.name == *field)
            .ok_or_else(|| {
                format!(
                    "extends_chain: path `{}` requires {:?} composition semantics but its root has no field rule",
                    requirement.path.join("."), requirement.semantics
                )
            })?;
        let supported = match (requirement.path.len(), requirement.semantics) {
            (1, ComposerFieldSemantics::RootVerbatim) => {
                rule.strategy == ComposerStrategy::RootVerbatim
            }
            (1, ComposerFieldSemantics::InheritOrReplace) => {
                rule.strategy == ComposerStrategy::ReplaceRootLast
            }
            (_, ComposerFieldSemantics::InheritOrReplace) => {
                rule.strategy == ComposerStrategy::DictMergeRootLast
            }
            _ => false,
        };
        if !supported {
            return Err(format!(
                "extends_chain: path `{}` is rooted in strategy {:?}, which cannot provide {:?} composition semantics",
                requirement.path.join("."),
                rule.strategy,
                requirement.semantics
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
        policy_facts.insert(pf.name.clone(), extract_policy_fact(&composed, pf)?);
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
    if let Some(expected) = rule.expect_value_type
        && !expected.matches(value)
    {
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
                    if let Some(v) = parent.get(&rule.name)
                        && !v.is_null()
                        && let Value::Object(obj) = composed
                    {
                        obj.insert(rule.name.clone(), v.clone());
                        break;
                    }
                }
            }
        }
        ComposerStrategy::ReplaceRootLast => {
            if let Some(value) = last_declared_field(ancestor_parsed, root_parsed, &rule.name)
                && let Value::Object(obj) = composed
            {
                obj.insert(rule.name.clone(), value.clone());
            }
        }
        ComposerStrategy::DictMergeRootLast => {
            let mut merged: Map<String, Value> = Map::new();
            let mut declared = false;
            for parent in ancestor_parsed {
                declared |= parent.get(&rule.name).is_some_and(|value| !value.is_null());
                merge_object_root_last(&mut merged, parent.get(&rule.name));
            }
            declared |= root_parsed
                .get(&rule.name)
                .is_some_and(|value| !value.is_null());
            merge_object_root_last(&mut merged, root_parsed.get(&rule.name));
            if declared && let Value::Object(obj) = composed {
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
            let narrowed =
                narrow_mapping_against_effective_parent(&rule.name, ancestor_parsed, root_parsed)?;
            if let Some(narrowed) = narrowed
                && let Value::Object(obj) = composed
            {
                obj.insert(rule.name.clone(), Value::Object(narrowed));
            }
        }
        ComposerStrategy::NarrowRequiresCapabilities => {
            narrow_requires_capabilities(&rule.name, composed, ancestor_parsed, root_parsed)?;
        }
    }
    Ok(())
}

/// Fold deepest ancestor through root. Every declaration is narrowed against
/// the immediately effective parent, so authority discarded by an
/// intermediate document cannot reappear in a grandchild.
fn narrow_mapping_against_effective_parent(
    field: &str,
    ancestor_parsed: &[&Value],
    root_parsed: &Value,
) -> Result<Option<Map<String, Value>>, (ResolutionStepNameWire, String)> {
    let mut effective: Option<Map<String, Value>> = None;
    for source in ancestor_parsed
        .iter()
        .copied()
        .chain(std::iter::once(root_parsed))
    {
        let Some(declared) = source.get(field).filter(|value| !value.is_null()) else {
            continue;
        };
        let child = declared.as_object().ok_or_else(|| {
            (
                ResolutionStepNameWire::PipelineInit,
                format!("composer field `{field}` must be a mapping"),
            )
        })?;
        for (verb, value) in child {
            strict_string_sequence(field, verb, value)?;
        }

        let Some(parent) = effective.as_ref() else {
            effective = Some(child.clone());
            continue;
        };
        let mut narrowed = parent.clone();
        let all_verbs = parent
            .keys()
            .chain(child.keys())
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for verb in all_verbs {
            let Some(child_value) = child.get(verb) else {
                continue;
            };
            let child_caps = strict_string_sequence(field, verb, child_value)?;
            let parent_caps = parent
                .get(verb)
                .map(|value| strict_string_sequence(field, verb, value))
                .transpose()?
                .unwrap_or_default();
            let caps = narrow_capabilities(&child_caps, &parent_caps);
            narrowed.insert(
                verb.to_string(),
                Value::Array(caps.into_iter().map(Value::String).collect()),
            );
        }
        effective = Some(narrowed);
    }
    Ok(effective)
}

fn strict_string_sequence(
    field: &str,
    verb: &str,
    value: &Value,
) -> Result<Vec<String>, (ResolutionStepNameWire, String)> {
    let values = value.as_array().ok_or_else(|| {
        (
            ResolutionStepNameWire::PipelineInit,
            format!("composer field `{field}.{verb}` must be an array of strings"),
        )
    })?;
    values
        .iter()
        .map(|value| {
            value.as_str().map(String::from).ok_or_else(|| {
                (
                    ResolutionStepNameWire::PipelineInit,
                    format!("composer field `{field}.{verb}` must contain only strings"),
                )
            })
        })
        .collect()
}

/// Compose `requires.capabilities` for a child against its ancestors. See
/// [`ComposerStrategy::NarrowRequiresCapabilities`].
fn narrow_requires_capabilities(
    field: &str,
    composed: &mut Value,
    ancestor_parsed: &[&Value],
    root_parsed: &Value,
) -> Result<(), (ResolutionStepNameWire, String)> {
    // Removed authoring fails loudly. Both sub-trees are strict-validated for
    // root + ancestors before composition.
    reject_removed_permissions_field(root_parsed)?;
    validate_requires_shape(root_parsed)?;
    if let Some(m) = manifest_value(root_parsed) {
        validate_manifest_shape(m)?;
    }
    for parent in ancestor_parsed {
        reject_removed_permissions_field(parent)?;
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

/// Reject the removed top-level `permissions:` block. The composed-value
/// contract ignores unowned top-level keys, so explicit rejection prevents an
/// unsupported authority declaration from being silently discarded.
pub(crate) fn reject_removed_permissions_field(
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

/// Validate the `requires` tree's closed key set so typos and removed keys
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

/// Compose the `declared` list by validating every direct inheritance edge in
/// deepest-to-nearest-to-root order. Omission inherits; a declaration must be
/// completely covered by the immediately effective parent and cannot recover
/// authority discarded by an intermediate ancestor.
fn compose_declared(
    root_parsed: &Value,
    ancestor_parsed: &[&Value],
) -> Result<Option<Value>, (ResolutionStepNameWire, String)> {
    let mut effective: Option<Vec<String>> = None;
    for source in ancestor_parsed
        .iter()
        .copied()
        .chain(std::iter::once(root_parsed))
    {
        let Some(declared) = declared_value(source) else {
            continue;
        };
        validate_declared_shape(declared)?;
        let child = string_array(declared);
        if let Some(parent) = effective.as_ref() {
            let covered = narrow_capabilities(&child, parent);
            if covered != child {
                let missing = child
                    .iter()
                    .filter(|capability| !covered.contains(capability))
                    .cloned()
                    .collect::<Vec<_>>();
                return Err((
                    ResolutionStepNameWire::PipelineInit,
                    format!(
                        "requires.capabilities.declared widens its direct parent: {}",
                        missing.join(", ")
                    ),
                ));
            }
        }
        effective = Some(child);
    }
    Ok(effective.map(|caps| Value::Array(caps.into_iter().map(Value::String).collect())))
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
// `RuntimeVaultOperation` / `ProjectSnapshotOperation`). Mirrored here so the
// composer can fail loud at compose time without a dependency on that crate;
// the launch-time parser is still the authoritative gate.
const BUNDLE_EVENT_OPS: &[&str] = &["append", "scan"];
const RUNTIME_VAULT_OPS: &[&str] = &["put", "get", "delete", "list"];
const PROJECT_SNAPSHOT_OPS: &[&str] = &["status", "log", "show", "create"];
const LARGE_CONTENT_OPS: &[&str] = &["ingest", "scrub"];

/// The only key permitted directly under `requires.capabilities.manifest`: the
/// `runtime_authority` family set the daemon mints from the signed manifest.
const MANIFEST_KEYS: &[&str] = &["runtime_authority"];

/// One runtime-authority family and how it narrows. Resource-operation
/// families compare `(id, operation)` pairs, operation-list families compare
/// operations directly, and item authoring compares `(kind, namespace)` with
/// the parent namespace pattern covering the child. Adding a family here
/// teaches both the shape check and narrowing check at once, mirroring the
/// closed family set in `ryeos_bundle::runtime_authority`.
#[derive(Clone, Copy)]
enum ManifestFamilyShape {
    ResourceOperations {
        id_key: &'static str,
        operations: &'static [&'static str],
    },
    OperationList {
        operations: &'static [&'static str],
    },
    ItemAuthoring,
}

struct ManifestFamily {
    key: &'static str,
    shape: ManifestFamilyShape,
}

const RUNTIME_AUTHORITY_FAMILIES: &[ManifestFamily] = &[
    ManifestFamily {
        key: "bundle_events",
        shape: ManifestFamilyShape::ResourceOperations {
            id_key: "event_kind",
            operations: BUNDLE_EVENT_OPS,
        },
    },
    ManifestFamily {
        key: "runtime_vault",
        shape: ManifestFamilyShape::ResourceOperations {
            id_key: "namespace",
            operations: RUNTIME_VAULT_OPS,
        },
    },
    ManifestFamily {
        key: "item_authoring",
        shape: ManifestFamilyShape::ItemAuthoring,
    },
    ManifestFamily {
        key: "project_snapshots",
        shape: ManifestFamilyShape::OperationList {
            operations: PROJECT_SNAPSHOT_OPS,
        },
    },
    ManifestFamily {
        key: "large_content",
        shape: ManifestFamilyShape::OperationList {
            operations: LARGE_CONTENT_OPS,
        },
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
/// non-empty id and a non-empty list of known operations), the pattern-based
/// `item_authoring` list (each entry a `{kind, namespace}` mapping), and the
/// `project_snapshots` operation list. Fails loud rather than deferring
/// malformed authoring to the launch parser.
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
        match family.shape {
            ManifestFamilyShape::ResourceOperations { id_key, operations } => {
                validate_manifest_resources(ra_map.get(family.key), id_key, operations, family.key)?
            }
            ManifestFamilyShape::OperationList { operations } => {
                validate_manifest_operation_list(ra_map.get(family.key), operations, family.key)?
            }
            ManifestFamilyShape::ItemAuthoring => {
                validate_item_authoring_entries(ra_map.get(family.key), family.key)?
            }
        }
    }
    Ok(())
}

/// Shape check for a flat operation-list authority family.
fn validate_manifest_operation_list(
    list: Option<&Value>,
    valid_operations: &[&str],
    tag: &str,
) -> Result<(), (ResolutionStepNameWire, String)> {
    let err = |m: String| (ResolutionStepNameWire::PipelineInit, m);
    let Some(value) = list else {
        return Ok(());
    };
    let operations = value.as_array().ok_or_else(|| {
        err(format!(
            "`requires.capabilities.manifest.runtime_authority.{tag}` must be a list"
        ))
    })?;
    for operation in operations {
        let operation = operation
            .as_str()
            .ok_or_else(|| err(format!("`{tag}` operations must be strings")))?;
        if !valid_operations.contains(&operation) {
            return Err(err(format!(
                "invalid operation `{operation}` for `{tag}` (allowed: {})",
                valid_operations.join(", ")
            )));
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

/// Compose the `manifest` sub-tree while checking every direct inheritance
/// edge. An omitted value inherits the immediately effective value.
fn compose_manifest(
    root_parsed: &Value,
    ancestor_parsed: &[&Value],
) -> Result<Option<Value>, (ResolutionStepNameWire, String)> {
    let mut effective: Option<Map<String, Value>> = None;
    for source in ancestor_parsed
        .iter()
        .copied()
        .chain(std::iter::once(root_parsed))
    {
        let Some(child) = capability_subtree(source, "manifest") else {
            continue;
        };
        if let Some(parent) = effective.as_ref() {
            let missing = manifest_missing(child, parent);
            if missing.is_empty() {
                effective = Some(child.clone());
            } else {
                return Err((
                    ResolutionStepNameWire::PipelineInit,
                    format!(
                        "requires.capabilities.manifest.runtime_authority widens its direct parent: {}",
                        missing.join(", ")
                    ),
                ));
            }
        } else {
            effective = Some(child.clone());
        }
    }
    Ok(effective.map(Value::Object))
}

fn manifest_missing(child: &Map<String, Value>, parent: &Map<String, Value>) -> Vec<String> {
    let mut missing = Vec::new();
    let child_ra = child.get("runtime_authority");
    let parent_ra = parent.get("runtime_authority");
    for family in RUNTIME_AUTHORITY_FAMILIES {
        let child_family = child_ra.and_then(|v| v.get(family.key));
        let parent_family = parent_ra.and_then(|v| v.get(family.key));
        match family.shape {
            ManifestFamilyShape::ResourceOperations { id_key, .. } => {
                collect_missing_resource_requirements(
                    child_family,
                    parent_family,
                    id_key,
                    family.key,
                    &mut missing,
                )
            }
            ManifestFamilyShape::OperationList { .. } => collect_missing_operation_requirements(
                child_family,
                parent_family,
                family.key,
                &mut missing,
            ),
            ManifestFamilyShape::ItemAuthoring => collect_missing_authoring_requirements(
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

/// Collect child operations that are not present in the parent's flat
/// operation list.
fn collect_missing_operation_requirements(
    child: Option<&Value>,
    parent: Option<&Value>,
    tag: &str,
    missing: &mut Vec<String>,
) {
    let Some(child_operations) = child.and_then(Value::as_array) else {
        return;
    };
    let parent_operations: HashSet<&str> = parent
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for operation in child_operations.iter().filter_map(Value::as_str) {
        if !parent_operations.contains(operation) {
            missing.push(format!("{tag}.{operation}"));
        }
    }
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

/// Select the nearest declaration without treating `null` as omission.
///
/// `replace_root_last` is an exact-value strategy: a declared null must ride
/// through to final contract validation instead of silently exposing an older
/// ancestor value. Only an absent key means inherit.
fn last_declared_field<'a>(
    ancestor_parsed: &'a [&'a Value],
    root_parsed: &'a Value,
    field: &str,
) -> Option<&'a Value> {
    ancestor_parsed
        .iter()
        .filter_map(|parent| parent.get(field))
        .chain(root_parsed.get(field))
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

fn extract_policy_fact(
    composed: &Value,
    pf: &PolicyFactExtractor,
) -> Result<Value, (ResolutionStepNameWire, String)> {
    let mut cur = composed;
    for seg in &pf.path {
        match cur.get(seg) {
            Some(v) => cur = v,
            None => return Ok(shape_default(pf.expect)),
        }
    }
    match pf.expect {
        PolicyFactShape::ArrayOfStrings => {
            let arr = cur.as_array().ok_or_else(|| {
                (
                    ResolutionStepNameWire::PipelineInit,
                    format!(
                        "policy fact `{}` path `{}` must resolve to an array of strings",
                        pf.name,
                        pf.path.join(".")
                    ),
                )
            })?;
            if arr.iter().any(|value| !value.is_string()) {
                return Err((
                    ResolutionStepNameWire::PipelineInit,
                    format!(
                        "policy fact `{}` path `{}` contains a non-string entry",
                        pf.name,
                        pf.path.join(".")
                    ),
                ));
            }
            Ok(Value::Array(arr.clone()))
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
    /// - `declared` (self-asserted action authority) narrows by **failing** — a
    ///   nearer document must explicitly stay within its direct parent's
    ///   effective authority and cannot silently request a different program.
    /// - `manifest` (runtime authority) narrows by **failing** — a child that
    ///   widens beyond its parent's `(event_kind, op)` / `(namespace, op)` pairs
    ///   fails compose, because dropping a hard requirement would only defer the
    ///   failure to a callback authz error.
    ///
    /// Also rejects the unsupported top-level `permissions:` field and any
    /// unknown key under `requires.capabilities` so removed authoring cannot be
    /// ignored. The signed bundle manifest remains the final upper bound for
    /// `manifest` at launch.
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
                    "name": "capabilities",
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
                    "path": ["capabilities", "execute"],
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
            err.1.contains("widens its direct parent") && err.1.contains("bundle_events.e.scan"),
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
            err.1.contains("widens its direct parent") && err.1.contains("bundle_events.f.append"),
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

    /// A `requires` block carrying only project-snapshot operations.
    fn snapshot_requires(project_snapshots: Value) -> Value {
        json!({
            "capabilities": { "manifest": { "runtime_authority": {
                "project_snapshots": project_snapshots,
            } } }
        })
    }

    #[test]
    fn requires_project_snapshots_root_kept_verbatim() {
        let child = json!({
            "requires": snapshot_requires(json!(["status", "create"])),
            "body": "b"
        });
        let view = run(requires_config(), child, vec![]).unwrap();
        assert_eq!(
            view.composed
                .pointer("/requires/capabilities/manifest/runtime_authority/project_snapshots"),
            Some(&json!(["status", "create"])),
        );
    }

    #[test]
    fn requires_project_snapshots_child_narrows_parent() {
        let parent = json!({
            "requires": snapshot_requires(json!(["status", "create"])),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": snapshot_requires(json!(["status"])),
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
                .pointer("/requires/capabilities/manifest/runtime_authority/project_snapshots"),
            Some(&json!(["status"])),
        );
    }

    #[test]
    fn requires_project_snapshots_child_cannot_widen_parent() {
        let parent = json!({
            "requires": snapshot_requires(json!(["status"])),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": snapshot_requires(json!(["create"])),
            "body": "b"
        });
        let error = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap_err();
        assert!(
            error.1.contains("widens its direct parent")
                && error.1.contains("project_snapshots.create"),
            "got: {}",
            error.1
        );
    }

    #[test]
    fn requires_project_snapshots_rejects_unknown_operation() {
        let child = json!({
            "requires": snapshot_requires(json!(["restore"])),
            "body": "b"
        });
        let error = run(requires_config(), child, vec![]).unwrap_err();
        assert!(
            error
                .1
                .contains("invalid operation `restore` for `project_snapshots`"),
            "got: {}",
            error.1
        );
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
            err.1.contains("widens its direct parent")
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
        // Resolver order is deepest-first: [grandparent, parent].
        let err = run(
            requires_config(),
            child,
            vec![
                ancestor_input("grandparent", grandparent),
                ancestor_input("parent", parent),
            ],
        )
        .unwrap_err();
        assert!(
            err.1.contains("widens its direct parent") && err.1.contains("bundle_events.e.scan"),
            "got: {}",
            err.1
        );
    }

    // ── declared sub-tree (fail-closed narrowing) ────────────────────

    #[test]
    fn declared_child_widening_is_rejected() {
        let parent =
            json!({ "requires": declared_block(json!(["ryeos.execute.tool.read"])), "body": "" });
        let child = json!({
            "extends": "parent",
            "requires": declared_block(json!(["ryeos.execute.tool.read", "ryeos.execute.tool.bash"])),
            "body": "b"
        });
        let error = run(
            requires_config(),
            child,
            vec![ancestor_input("parent", parent)],
        )
        .unwrap_err();
        assert!(error.1.contains("widens its direct parent"));
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

    // ── removed-field rejection + strict shape ──────────────────────

    #[test]
    fn removed_top_level_permissions_is_rejected() {
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
    fn removed_callbacks_key_is_rejected() {
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
        // `declared` is a flat list of cap strings; a verb-bucketed mapping
        // fails loudly.
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
            "capabilities": { "execute": ["ryeos.execute.tool.bash"] },
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
            "capabilities": { "execute": ["ryeos.execute.tool.read"] },
            "body": "body"
        });
        let p = json!({
            "capabilities": { "execute": ["ryeos.execute.tool.bash"] },
            "body": ""
        });
        let view = run(demo_config(), r, vec![ancestor_input("parent", p)]).unwrap();
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
    }

    #[test]
    fn grandchild_cannot_recover_authority_discarded_by_parent() {
        let root = json!({
            "name": "grandchild",
            "extends": "parent",
            "capabilities": { "execute": ["ryeos.execute.tool.write"] },
            "body": "body"
        });
        let grandparent = json!({
            "name": "grandparent",
            "capabilities": {
                "execute": ["ryeos.execute.tool.read", "ryeos.execute.tool.write"]
            },
            "body": ""
        });
        let parent = json!({
            "name": "parent",
            "extends": "grandparent",
            "capabilities": { "execute": ["ryeos.execute.tool.read"] },
            "body": ""
        });
        let view = run(
            demo_config(),
            root,
            vec![
                ancestor_input("grandparent", grandparent),
                ancestor_input("parent", parent),
            ],
        )
        .unwrap();
        assert!(policy_fact_string_seq(&view, "effective_caps").is_empty());
    }

    #[test]
    fn narrowed_mapping_rejects_non_string_entries() {
        let root = json!({
            "name": "child",
            "capabilities": { "execute": ["ryeos.execute.tool.read", 7] },
            "body": "body"
        });
        let error = run(demo_config(), root, Vec::new()).unwrap_err();
        assert!(error.1.contains("must contain only strings"));
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
    fn exact_field_requires_atomic_strategy() {
        let replace = json!({
            "extends_field": "extends",
            "fields": [{
                "name": "policy",
                "strategy": "replace_root_last",
                "expect_value_type": "mapping"
            }]
        });
        validate_field_requirements(
            &replace,
            &[ComposerFieldRequirement {
                path: vec!["policy".into()],
                semantics: ComposerFieldSemantics::InheritOrReplace,
            }],
        )
        .unwrap();

        let deep_merge = json!({
            "extends_field": "extends",
            "fields": [{
                "name": "policy",
                "strategy": "dict_merge_root_last",
                "expect_value_type": "mapping"
            }]
        });
        let error = validate_field_requirements(
            &deep_merge,
            &[ComposerFieldRequirement {
                path: vec!["policy".into()],
                semantics: ComposerFieldSemantics::InheritOrReplace,
            }],
        )
        .unwrap_err();
        assert!(error.contains("cannot provide InheritOrReplace"));
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
    fn replace_root_last_preserves_declared_null_for_contract_rejection() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "layout", "strategy": "replace_root_last", "expect_value_type": "mapping" }
            ]
        });
        let root = json!({ "ext": "parent", "layout": null });
        let parent = json!({ "layout": { "root": "parent" } });
        let view = run(cfg, root, vec![ancestor_input("parent", parent)]).unwrap();
        assert!(view.composed["layout"].is_null());
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
    fn optional_dict_merge_preserves_true_absence() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "ambient", "strategy": "dict_merge_root_last", "expect_value_type": "mapping" }
            ]
        });
        let view = run(cfg, json!({}), Vec::new()).unwrap();
        assert!(view.composed.get("ambient").is_none());
    }

    #[test]
    fn graph_config_hooks_inherit_clear_and_replace_atomically() {
        let cfg = json!({
            "extends_field": "extends",
            "fields": [
                { "name": "config", "strategy": "dict_merge_root_last", "expect_value_type": "mapping" }
            ]
        });
        let inherited_hooks = json!([
            {"id": "parent", "event": "graph_started", "result": "discard", "action": {}}
        ]);
        let parent = json!({
            "config": {"start": "parent", "nodes": {}, "hooks": inherited_hooks}
        });

        let omitted = run(
            cfg.clone(),
            json!({"extends": "parent", "config": {"start": "child"}}),
            vec![ancestor_input("parent", parent.clone())],
        )
        .unwrap();
        assert_eq!(
            omitted.composed.pointer("/config/hooks"),
            parent.pointer("/config/hooks")
        );

        let cleared = run(
            cfg.clone(),
            json!({"extends": "parent", "config": {"hooks": []}}),
            vec![ancestor_input("parent", parent.clone())],
        )
        .unwrap();
        assert_eq!(cleared.composed.pointer("/config/hooks"), Some(&json!([])));

        let replacement = json!([
            {"id": "child", "event": "graph_completed", "result": "observation", "action": {}}
        ]);
        let replaced = run(
            cfg,
            json!({"extends": "parent", "config": {"hooks": replacement.clone()}}),
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(
            replaced.composed.pointer("/config/hooks"),
            Some(&replacement)
        );
    }

    #[test]
    fn directive_hooks_inherit_clear_and_replace_atomically() {
        let cfg = json!({
            "extends_field": "extends",
            "fields": [
                { "name": "hooks", "strategy": "replace_root_last", "expect_value_type": "sequence" }
            ]
        });
        let inherited = json!([
            {"id": "parent", "event": "after_step", "result": "discard", "action": {}}
        ]);
        let parent = json!({"hooks": inherited});

        let omitted = run(
            cfg.clone(),
            json!({"extends": "parent"}),
            vec![ancestor_input("parent", parent.clone())],
        )
        .unwrap();
        assert_eq!(omitted.composed.get("hooks"), parent.get("hooks"));

        let cleared = run(
            cfg.clone(),
            json!({"extends": "parent", "hooks": []}),
            vec![ancestor_input("parent", parent.clone())],
        )
        .unwrap();
        assert_eq!(cleared.composed.get("hooks"), Some(&json!([])));

        let replacement = json!([
            {"id": "child", "event": "continuation", "result": "control", "action": {}}
        ]);
        let replaced = run(
            cfg,
            json!({"extends": "parent", "hooks": replacement.clone()}),
            vec![ancestor_input("parent", parent)],
        )
        .unwrap();
        assert_eq!(replaced.composed.get("hooks"), Some(&replacement));
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

    #[test]
    fn policy_fact_present_path_rejects_non_string_entries() {
        let cfg = json!({
            "extends_field": "ext",
            "fields": [
                { "name": "perms", "strategy": "inherit_from_topmost" }
            ],
            "policy_facts": [
                { "name": "caps", "path": ["perms", "execute"], "expect": "array_of_strings" }
            ]
        });
        let error = run(
            cfg,
            json!({"perms": {"execute": ["allowed", 7]}}),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.1.contains("contains a non-string entry"));
    }

    #[test]
    fn declared_three_level_chain_cannot_recover_discarded_authority() {
        let grandparent = json!({
            "requires": declared_block(json!([
                "ryeos.execute.tool.a",
                "ryeos.execute.tool.b"
            ])),
            "body": ""
        });
        let parent = json!({
            "extends": "grandparent",
            "requires": declared_block(json!(["ryeos.execute.tool.a"])),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": declared_block(json!(["ryeos.execute.tool.b"])),
            "body": "child"
        });

        let error = run(
            requires_config(),
            child,
            vec![
                ancestor_input("grandparent", grandparent),
                ancestor_input("parent", parent),
            ],
        )
        .unwrap_err();
        assert!(error.1.contains("widens its direct parent"));
        assert!(error.1.contains("ryeos.execute.tool.b"));
    }

    #[test]
    fn declared_three_level_chain_accepts_direct_narrowing_at_every_edge() {
        let grandparent = json!({
            "requires": declared_block(json!([
                "ryeos.execute.tool.a",
                "ryeos.execute.tool.b"
            ])),
            "body": ""
        });
        let parent = json!({
            "extends": "grandparent",
            "requires": declared_block(json!(["ryeos.execute.tool.a"])),
            "body": ""
        });
        let child = json!({
            "extends": "parent",
            "requires": declared_block(json!(["ryeos.execute.tool.a"])),
            "body": "child"
        });

        let view = run(
            requires_config(),
            child,
            vec![
                ancestor_input("grandparent", grandparent),
                ancestor_input("parent", parent),
            ],
        )
        .unwrap();
        assert_eq!(declared_execute(&view), vec!["ryeos.execute.tool.a"]);
    }

    #[test]
    fn declared_omission_inherits_through_each_deepest_first_edge() {
        let grandparent = json!({
            "requires": declared_block(json!([
                "ryeos.execute.tool.a",
                "ryeos.execute.tool.b"
            ])),
            "body": ""
        });
        let parent = json!({"extends": "grandparent", "body": ""});
        let child = json!({"extends": "parent", "body": "child"});

        let view = run(
            requires_config(),
            child,
            vec![
                ancestor_input("grandparent", grandparent),
                ancestor_input("parent", parent),
            ],
        )
        .unwrap();
        assert_eq!(
            declared_execute(&view),
            vec!["ryeos.execute.tool.a", "ryeos.execute.tool.b"]
        );
    }
}
