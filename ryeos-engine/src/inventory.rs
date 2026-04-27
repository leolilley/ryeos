//! Inventory builder — daemon-side, kind-driven discovery of items
//! the LAUNCHING kind asked the engine to resolve on its behalf.
//!
//! Single source of truth for "what tools / knowledge / graph nodes
//! does this directive get to see". The runtime is a pure consumer
//! via `LaunchEnvelope.inventory[<kind>]`; no extension switching or
//! parser dispatch lives anywhere downstream of this module.
//!
//! ## Contract
//!
//! For each launching item (e.g. `directive:my/agent`), the daemon
//! reads the launching kind schema's `inventory_kinds:` list. For
//! each entry (e.g. `tool`), this module:
//!
//! 1. Reads the **target kind's** schema (`directory`, `extensions`,
//!    `parser`, `signature_envelope`, `extraction_rules`,
//!    `inventory_schema_keys`).
//! 2. Recursively enumerates every reachable item via
//!    `item_resolution::enumerate_kind_refs`, honouring the same
//!    project → user → system precedence the resolver itself uses.
//! 3. Resolves each ref to a concrete file path via
//!    `item_resolution::resolve_item_full` (so shadowing diagnostics
//!    are consistent with `Engine::resolve`).
//! 4. Parses the file body via the supplied `ParserDispatcher`. The
//!    dispatcher MUST be the **same** effective dispatcher the daemon
//!    used elsewhere in this launch (parser-overlay snapshot
//!    consistency).
//! 5. Applies the kind schema's existing `metadata.rules` to the
//!    parsed body to populate an `ItemMetadata`.
//! 6. Projects metadata + parsed body into an `ItemDescriptor`,
//!    pulling `schema` from the first non-null hit in
//!    `inventory_schema_keys` and lifting unknown metadata fields
//!    into `extra` for runtime consumption.
//!
//! Schema-driven all the way down — adding a new format = editing a
//! kind YAML, never a Rust file.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::item_resolution::{enumerate_kind_refs, resolve_item_full, ResolutionRoots};
use crate::kind_registry::{apply_extraction_rules, KindRegistry, KindSchema};
use crate::parsers::ParserDispatcher;

/// One inventoried item, fully resolved by the daemon's engine. The
/// runtime serialises this directly into its kind-specific typed view
/// (provider tool list, knowledge frame, graph-node manifest, …) —
/// it never re-parses the underlying source file.
///
/// Field semantics:
/// - `name`: API-safe flattened identifier intended for downstream
///   consumption (LLM tool name, knowledge alias, …). Derived from
///   the canonical bare-id (`rye/core/read` → `rye_core_read`) via
///   [`flatten_bare_id`] so nested layouts don't collide.
/// - `item_id`: full canonical ref (e.g. `tool:rye/core/read`) the
///   runtime hands back to `runtime.dispatch_action` for execution.
/// - `description`: extracted via the kind schema's `metadata.rules`
///   `description:` rule. `None` when the source declares no
///   description.
/// - `schema`: first non-null value found at the keys declared in
///   the kind schema's `inventory_schema_keys`. `None` when the kind
///   declares no schema keys, or none are present in the parsed body.
/// - `extra`: every metadata.rules-emitted field other than the
///   typed slots already surfaced. Lets each runtime read
///   kind-specific metadata it cares about without forcing every
///   field into the typed surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ItemDescriptor {
    pub name: String,
    pub item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, Value>,
}

/// Per-launch inventory result, keyed by inventoried kind name. The
/// daemon embeds this into `LaunchEnvelope.inventory`.
pub type Inventory = HashMap<String, Vec<ItemDescriptor>>;

/// Build the full inventory the launching kind asked for. Returns an
/// empty map when the kind declares no `inventory_kinds`.
///
/// `launching_kind_schema` is the schema of the kind whose item is
/// being executed (e.g. the `directive` schema when running
/// `directive:my/agent`). `kinds` is the full kind registry — used to
/// look up each inventoried kind's schema. `roots` and `parsers` MUST
/// be the **same** instances the launcher used elsewhere in this
/// request (per the snapshot-consistency contract; see
/// `Engine::effective_parser_dispatcher`).
///
/// Per-item failures are NOT swallowed: a malformed parser response
/// or a verification error inside an inventoried kind is a hard error
/// that aborts inventory construction. The launcher is expected to
/// surface this as a 4xx/5xx — silent partial inventories are exactly
/// the class of bug this module exists to prevent.
pub fn build_inventory_for_launching_kind(
    launching_kind_schema: &KindSchema,
    kinds: &KindRegistry,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
) -> Result<Inventory, EngineError> {
    let mut out: Inventory = HashMap::new();
    for inventoried_kind in &launching_kind_schema.inventory_kinds {
        let target_schema = kinds.get(inventoried_kind).ok_or_else(|| {
            EngineError::SchemaLoaderError {
                reason: format!(
                    "build_inventory: launching kind declares `inventory_kinds: \
                     [{inventoried_kind}]` but no kind by that name is registered \
                     (typo? missing bundle?)"
                ),
            }
        })?;
        let descriptors = build_inventory_for_kind(
            inventoried_kind,
            target_schema,
            roots,
            parsers,
        )?;
        out.insert(inventoried_kind.clone(), descriptors);
    }
    Ok(out)
}

/// Build descriptors for a single inventoried kind. Public for tests
/// and for callers that want a single-kind inventory (e.g. CLI
/// listing).
pub fn build_inventory_for_kind(
    inventoried_kind: &str,
    target_schema: &KindSchema,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
) -> Result<Vec<ItemDescriptor>, EngineError> {
    let refs = enumerate_kind_refs(roots, target_schema, inventoried_kind);
    let mut out: Vec<ItemDescriptor> = Vec::with_capacity(refs.len());
    // Track which `item_id` first produced each flattened name so a
    // collision can name both sides in the diagnostic. Silent
    // shadowing in this map would let a runtime tool dispatcher
    // overwrite one tool with another.
    let mut seen_names: HashMap<String, String> = HashMap::with_capacity(refs.len());
    for ref_ in &refs {
        let descriptor = build_descriptor_for_ref(ref_, target_schema, roots, parsers)
            .map_err(|e| EngineError::InventoryItemFailed {
                kind: inventoried_kind.to_owned(),
                bare_id: ref_.bare_id.clone(),
                source: Box::new(e),
            })?;
        if let Some(prev_id) = seen_names.get(&descriptor.name) {
            return Err(EngineError::DuplicateInventoryName {
                kind: inventoried_kind.to_owned(),
                flattened: descriptor.name.clone(),
                first_item_id: prev_id.clone(),
                second_item_id: descriptor.item_id.clone(),
            });
        }
        seen_names.insert(descriptor.name.clone(), descriptor.item_id.clone());
        out.push(descriptor);
    }
    Ok(out)
}

fn build_descriptor_for_ref(
    ref_: &CanonicalRef,
    target_schema: &KindSchema,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
) -> Result<ItemDescriptor, EngineError> {
    let resolution = resolve_item_full(roots, target_schema, ref_)?;

    let content = std::fs::read_to_string(&resolution.winner_path).map_err(|e| {
        EngineError::Internal(format!(
            "build_inventory: read {}: {e}",
            resolution.winner_path.display()
        ))
    })?;

    let source_format = target_schema
        .resolved_format_for(&resolution.matched_ext)
        .ok_or_else(|| EngineError::Internal(format!(
            "build_inventory: matched extension {} has no source format in schema",
            resolution.matched_ext
        )))?;

    let parsed = parsers.dispatch(
        &source_format.parser,
        &content,
        Some(&resolution.winner_path),
        &source_format.signature,
    )?;

    let metadata = apply_extraction_rules(
        &parsed,
        &target_schema.extraction_rules,
        &resolution.winner_path,
    );

    let description = metadata.description.clone();

    let schema = pick_schema(&parsed, &target_schema.inventory_schema_keys);

    // `extra` carries every metadata field that doesn't have a typed
    // slot on `ItemDescriptor`. We deliberately drop the fields we
    // surface separately (description, name) so a runtime reading
    // `extra` doesn't see duplicates of `descriptor.description`.
    let mut extra: HashMap<String, Value> = metadata.extra.clone();
    if let Some(ref v) = metadata.executor_id {
        extra.insert("executor_id".to_owned(), Value::String(v.clone()));
    }
    if let Some(ref v) = metadata.version {
        extra.insert("version".to_owned(), Value::String(v.clone()));
    }
    if let Some(ref v) = metadata.category {
        extra.insert("category".to_owned(), Value::String(v.clone()));
    }
    if !metadata.required_secrets.is_empty() {
        extra.insert(
            "required_secrets".to_owned(),
            Value::Array(
                metadata
                    .required_secrets
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }

    let name = flatten_bare_id(&ref_.bare_id);
    let item_id = format!("{}:{}", ref_.kind, ref_.bare_id);

    Ok(ItemDescriptor {
        name,
        item_id,
        description,
        schema,
        extra,
    })
}

/// Read the parsed body and return the first non-null value at any
/// of `keys`. `None` when the list is empty (kind opts out of schema)
/// or no candidate is present.
fn pick_schema(parsed: &Value, keys: &[String]) -> Option<Value> {
    for key in keys {
        if let Some(v) = parsed.get(key) {
            if !v.is_null() {
                return Some(v.clone());
            }
        }
    }
    None
}

/// Convert `rye/core/read` (or `rye/file-system/ls`) into an
/// API-safe flat name (`rye_core_read`, `rye_file_system_ls`). The
/// LLM tool surface and many other consumers don't tolerate `/` or
/// `-`; this is the canonical projection.
pub fn flatten_bare_id(bare_id: &str) -> String {
    let mut out = String::with_capacity(bare_id.len());
    for ch in bare_id.chars() {
        match ch {
            '/' | '-' => out.push('_'),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_strips_slashes_and_dashes() {
        assert_eq!(flatten_bare_id("rye/core/read"), "rye_core_read");
        assert_eq!(flatten_bare_id("rye/file-system/ls"), "rye_file_system_ls");
        assert_eq!(flatten_bare_id("echo"), "echo");
    }

    #[test]
    fn pick_schema_returns_first_non_null() {
        let parsed = serde_json::json!({
            "input_schema": null,
            "parameters": [{"name": "p"}],
            "config_schema": {"type": "object"},
        });
        let keys = vec![
            "input_schema".to_owned(),
            "parameters".to_owned(),
            "config_schema".to_owned(),
        ];
        let schema = pick_schema(&parsed, &keys).unwrap();
        assert_eq!(schema, serde_json::json!([{"name": "p"}]));
    }

    #[test]
    fn flatten_collision_demonstrates_why_duplicate_check_exists() {
        // Underscore is preserved verbatim, so an item named `a/b_c`
        // and a sibling `a/b-c` both flatten to `a_b_c`. The
        // duplicate-name guard in `build_inventory_for_kind` exists
        // precisely to refuse this case loudly instead of silently
        // dropping one of them in downstream tool dispatch.
        assert_eq!(flatten_bare_id("a/b-c"), flatten_bare_id("a/b_c"));
    }

    #[test]
    fn pick_schema_returns_none_when_no_keys() {
        let parsed = serde_json::json!({"x": 1});
        assert!(pick_schema(&parsed, &[]).is_none());
    }
}
