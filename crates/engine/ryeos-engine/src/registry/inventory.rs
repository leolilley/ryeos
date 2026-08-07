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
#[cfg(feature = "latency-profiling")]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::item_resolution::{ResolutionRoots, enumerate_kind_refs, resolve_item_full};
use crate::kind_registry::{KindRegistry, KindSchema, apply_extraction_rules};
use crate::parsers::ParserDispatcher;

/// One inventoried item, fully resolved by the daemon's engine. The
/// runtime serialises this directly into its kind-specific typed view
/// (provider tool list, knowledge frame, graph-node manifest, …) —
/// it never re-parses the underlying source file.
///
/// Field semantics:
/// - `name`: API-safe flattened identifier intended for downstream
///   consumption (LLM tool name, knowledge alias, …). Derived from
///   the canonical bare-id (`ryeos/core/read` → `ryeos_core_read`) via
///   [`flatten_bare_id`] so nested layouts don't collide.
/// - `item_id`: full canonical ref (e.g. `tool:ryeos/core/read`) the
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
#[serde(deny_unknown_fields)]
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
    build_inventory_for_launching_kind_filtered(
        launching_kind_schema,
        kinds,
        roots,
        parsers,
        |_| true,
    )
}

/// Build an inventory while refusing excluded canonical refs before their
/// source files are read or parsed.
///
/// The caller owns the filtering policy. This lets a launcher with an already
/// sealed authorization context avoid preparing descriptors that context
/// cannot expose, without moving capability policy into the engine.
pub fn build_inventory_for_launching_kind_filtered(
    launching_kind_schema: &KindSchema,
    kinds: &KindRegistry,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
    mut include: impl FnMut(&CanonicalRef) -> bool,
) -> Result<Inventory, EngineError> {
    let mut out: Inventory = HashMap::new();
    for inventoried_kind in &launching_kind_schema.inventory_kinds {
        let target_schema =
            kinds
                .get(inventoried_kind)
                .ok_or_else(|| EngineError::SchemaLoaderError {
                    reason: format!(
                        "build_inventory: launching kind declares `inventory_kinds: \
                     [{inventoried_kind}]` but no kind by that name is registered \
                     (typo? missing bundle?)"
                    ),
                })?;
        let descriptors = build_inventory_for_kind_filtered(
            inventoried_kind,
            target_schema,
            roots,
            parsers,
            &mut include,
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
    build_inventory_for_kind_filtered(inventoried_kind, target_schema, roots, parsers, &mut |_| {
        true
    })
}

fn build_inventory_for_kind_filtered(
    inventoried_kind: &str,
    target_schema: &KindSchema,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
    include: &mut impl FnMut(&CanonicalRef) -> bool,
) -> Result<Vec<ItemDescriptor>, EngineError> {
    let mut profile = InventoryBuildProfile::new();
    let enumeration_started = profile.start_phase();
    let refs = enumerate_kind_refs(roots, target_schema, inventoried_kind);
    profile.finish_enumeration(enumeration_started);
    let mut out: Vec<ItemDescriptor> = Vec::with_capacity(refs.len());
    // Track which `item_id` first produced each flattened name so a
    // collision can name both sides in the diagnostic. Silent
    // shadowing in this map would let a runtime tool dispatcher
    // overwrite one tool with another.
    let mut seen_names: HashMap<String, String> = HashMap::with_capacity(refs.len());
    for ref_ in &refs {
        if !include(ref_) {
            continue;
        }
        profile.record_authorized();
        let descriptor =
            build_descriptor_for_ref(ref_, target_schema, roots, parsers, &mut profile).map_err(
                |e| EngineError::InventoryItemFailed {
                    kind: inventoried_kind.to_owned(),
                    bare_id: ref_.bare_id.clone(),
                    source: Box::new(e),
                },
            )?;
        // Inventoried-side schema policy declares which extracted metadata a
        // descriptor must carry to be exposed. This keeps direct-invocation
        // semantics out of the engine implementation.
        if !target_schema
            .inventory_policy
            .required_metadata
            .iter()
            .all(|field| descriptor.extra.contains_key(field))
        {
            profile.record_omitted();
            continue;
        }
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
    profile.emit(inventoried_kind, refs.len(), out.len());
    Ok(out)
}

fn build_descriptor_for_ref(
    ref_: &CanonicalRef,
    target_schema: &KindSchema,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
    profile: &mut InventoryBuildProfile,
) -> Result<ItemDescriptor, EngineError> {
    let resolution_started = profile.start_phase();
    let resolution = resolve_item_full(roots, target_schema, ref_)?;
    profile.finish_resolution(resolution_started);

    let read_started = profile.start_phase();
    let content = std::fs::read_to_string(&resolution.winner_path).map_err(|e| {
        EngineError::Internal(format!(
            "build_inventory: read {}: {e}",
            resolution.winner_path.display()
        ))
    })?;
    profile.finish_read(read_started, content.len());

    let source_format = target_schema
        .resolved_format_for(&resolution.matched_ext)
        .ok_or_else(|| {
            EngineError::Internal(format!(
                "build_inventory: matched extension {} has no source format in schema",
                resolution.matched_ext
            ))
        })?;

    let parse_started = profile.start_phase();
    let parsed = parsers.dispatch(
        &source_format.parser,
        &content,
        Some(&resolution.winner_path),
        &source_format.signature,
    )?;
    profile.finish_parse(parse_started);

    let projection_started = profile.start_phase();
    let metadata = apply_extraction_rules(
        &parsed,
        &target_schema.extraction_rules,
        &resolution.winner_path,
        &target_schema.directory,
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

    let descriptor = ItemDescriptor {
        name,
        item_id,
        description,
        schema,
        extra,
    };
    profile.finish_projection(projection_started);
    Ok(descriptor)
}

/// Zero-sized and inlined away in ordinary release builds. Keeping the
/// profiler behind this private abstraction avoids threading feature checks or
/// kind knowledge through inventory semantics.
struct InventoryBuildProfile {
    #[cfg(feature = "latency-profiling")]
    build_started: Instant,
    #[cfg(feature = "latency-profiling")]
    authorized_count: usize,
    #[cfg(feature = "latency-profiling")]
    omitted_count: usize,
    #[cfg(feature = "latency-profiling")]
    source_bytes: u64,
    #[cfg(feature = "latency-profiling")]
    enumeration_us: u64,
    #[cfg(feature = "latency-profiling")]
    resolution_us: u64,
    #[cfg(feature = "latency-profiling")]
    read_us: u64,
    #[cfg(feature = "latency-profiling")]
    parse_us: u64,
    #[cfg(feature = "latency-profiling")]
    projection_us: u64,
}

struct InventoryPhase {
    #[cfg(feature = "latency-profiling")]
    started: Instant,
}

impl InventoryBuildProfile {
    #[inline(always)]
    fn new() -> Self {
        Self {
            #[cfg(feature = "latency-profiling")]
            build_started: Instant::now(),
            #[cfg(feature = "latency-profiling")]
            authorized_count: 0,
            #[cfg(feature = "latency-profiling")]
            omitted_count: 0,
            #[cfg(feature = "latency-profiling")]
            source_bytes: 0,
            #[cfg(feature = "latency-profiling")]
            enumeration_us: 0,
            #[cfg(feature = "latency-profiling")]
            resolution_us: 0,
            #[cfg(feature = "latency-profiling")]
            read_us: 0,
            #[cfg(feature = "latency-profiling")]
            parse_us: 0,
            #[cfg(feature = "latency-profiling")]
            projection_us: 0,
        }
    }

    #[inline(always)]
    fn start_phase(&self) -> InventoryPhase {
        InventoryPhase {
            #[cfg(feature = "latency-profiling")]
            started: Instant::now(),
        }
    }

    #[inline(always)]
    fn finish_enumeration(&mut self, _phase: InventoryPhase) {
        #[cfg(feature = "latency-profiling")]
        {
            self.enumeration_us = elapsed_us(_phase.started);
        }
    }

    #[inline(always)]
    fn record_authorized(&mut self) {
        #[cfg(feature = "latency-profiling")]
        {
            self.authorized_count = self.authorized_count.saturating_add(1);
        }
    }

    #[inline(always)]
    fn record_omitted(&mut self) {
        #[cfg(feature = "latency-profiling")]
        {
            self.omitted_count = self.omitted_count.saturating_add(1);
        }
    }

    #[inline(always)]
    fn finish_resolution(&mut self, _phase: InventoryPhase) {
        #[cfg(feature = "latency-profiling")]
        {
            self.resolution_us = self
                .resolution_us
                .saturating_add(elapsed_us(_phase.started));
        }
    }

    #[inline(always)]
    fn finish_read(&mut self, _phase: InventoryPhase, _source_bytes: usize) {
        #[cfg(feature = "latency-profiling")]
        {
            self.read_us = self.read_us.saturating_add(elapsed_us(_phase.started));
            self.source_bytes = self
                .source_bytes
                .saturating_add(u64::try_from(_source_bytes).unwrap_or(u64::MAX));
        }
    }

    #[inline(always)]
    fn finish_parse(&mut self, _phase: InventoryPhase) {
        #[cfg(feature = "latency-profiling")]
        {
            self.parse_us = self.parse_us.saturating_add(elapsed_us(_phase.started));
        }
    }

    #[inline(always)]
    fn finish_projection(&mut self, _phase: InventoryPhase) {
        #[cfg(feature = "latency-profiling")]
        {
            self.projection_us = self
                .projection_us
                .saturating_add(elapsed_us(_phase.started));
        }
    }

    #[inline(always)]
    fn emit(&self, _inventoried_kind: &str, _discovered_count: usize, _descriptor_count: usize) {
        #[cfg(feature = "latency-profiling")]
        tracing::info!(
            target: "ryeos.metrics",
            event = "inventory_kind_build_timing",
            schema_version = 1_u32,
            inventoried_kind = _inventoried_kind,
            discovered_count = _discovered_count,
            authorized_count = self.authorized_count,
            descriptor_count = _descriptor_count,
            omitted_count = self.omitted_count,
            source_bytes = self.source_bytes,
            enumeration_us = self.enumeration_us,
            resolution_us = self.resolution_us,
            read_us = self.read_us,
            parse_us = self.parse_us,
            projection_us = self.projection_us,
            total_us = elapsed_us(self.build_started),
            "inventory kind build timing"
        );
    }
}

#[cfg(feature = "latency-profiling")]
fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Read the parsed body and return the first non-null value at any
/// of `keys`. `None` when the list is empty (kind opts out of schema)
/// or no candidate is present.
fn pick_schema(parsed: &Value, keys: &[String]) -> Option<Value> {
    for key in keys {
        if let Some(v) = parsed.get(key)
            && !v.is_null()
        {
            return Some(v.clone());
        }
    }
    None
}

/// Convert `ryeos/core/read` (or `ryeos/file-system/ls`) into an
/// API-safe flat name (`ryeos_core_read`, `ryeos_file_system_ls`). The
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
        assert_eq!(flatten_bare_id("ryeos/core/read"), "ryeos_core_read");
        assert_eq!(
            flatten_bare_id("ryeos/file-system/ls"),
            "ryeos_file_system_ls"
        );
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

    #[test]
    fn required_inventory_metadata_controls_descriptor_admission() {
        let mut descriptor = ItemDescriptor::default();
        let required = ["dispatch_identity".to_string()];
        assert!(
            !required
                .iter()
                .all(|field| descriptor.extra.contains_key(field))
        );

        descriptor.extra.insert(
            "dispatch_identity".to_owned(),
            Value::String("@subprocess".to_owned()),
        );
        assert!(
            required
                .iter()
                .all(|field| descriptor.extra.contains_key(field))
        );
    }
}
