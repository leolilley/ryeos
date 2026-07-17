//! Tolerant single-item projection for corpus read ops
//! (`scope: corpus` — e.g. knowledge `query`/`graph`/`validate`).
//!
//! Unlike [`run_resolution_pipeline`](super::run_resolution_pipeline), this:
//!   - does NOT compose or walk the extends chain;
//!   - does NOT load reference targets (so the daemon can enumerate the
//!     whole corpus and let dangling edges through to the runtime);
//!   - returns the verified item EVEN IF its frontmatter is malformed (so
//!     a corpus `validate` can report it from `raw_content`).
//!
//! Integrity failures (bad signature, unreadable, missing) are NOT
//! swallowed — they surface as errors so a corpus op can fail loudly
//! instead of silently presenting a clean-looking partial corpus.

use crate::canonical_ref::CanonicalRef;
use crate::item_resolution::ResolutionRoots;
use crate::kind_registry::KindRegistry;
use crate::parsers::ParserDispatcher;
use crate::trust::TrustStore;

use super::alias::AliasResolver;
use super::context::{ensure_canonical, field_as_list, load_item_raw, RawLoadedItem};
use super::decl::ResolutionStepDecl;
use super::types::{ResolutionError, ResolutionStepName, ResolvedAncestor};

/// One enumerated corpus item: the verified item plus the DIRECT reference
/// edges it declares. Edge targets are NOT required to exist — the
/// consumer (runtime `graph`/`validate`) reports dangling endpoints.
pub struct CorpusItemProjection {
    pub item: ResolvedAncestor,
    pub reference_edges: Vec<CorpusReferenceEdge>,
}

pub struct CorpusReferenceEdge {
    pub from_ref: String,
    pub to_ref: String,
}

/// Project a single enumerated corpus item: verified raw load + declared
/// (depth-1) reference edges. See the module docs for the tolerance
/// contract.
pub fn resolve_item_for_corpus(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    roots: &ResolutionRoots,
    trust_store: &TrustStore,
) -> Result<CorpusItemProjection, ResolutionError> {
    // Verified raw load. Errors on integrity failure / missing / unreadable
    // — never silently dropped — but tolerates a malformed body.
    let raw = load_item_raw(
        kinds,
        roots,
        trust_store,
        item,
        "<corpus>",
        ResolutionStepName::PipelineInit,
    )?;

    let ancestor = ResolvedAncestor {
        requested_id: item.to_string(),
        resolved_ref: item.to_string(),
        source_path: raw.source_path.clone(),
        source_space: raw.source_space,
        trust_class: raw.trust_class,
        alias_resolution: None,
        added_by: ResolutionStepName::PipelineInit,
        raw_content: raw.raw_content.clone(),
        source_content_digest: raw.source_content_digest.clone(),
        raw_content_digest: raw.raw_content_digest.clone(),
    };

    // Best-effort edges. A malformed body (or malformed references field)
    // yields no edges — the runtime reports the bad frontmatter from
    // `raw_content` — but the item itself is always returned.
    let reference_edges = extract_reference_edges(item, kinds, parsers, &raw).unwrap_or_default();

    Ok(CorpusItemProjection {
        item: ancestor,
        reference_edges,
    })
}

/// Parse the item and pull its declared DIRECT reference refs,
/// canonicalized exactly as the `resolve_references` step does (alias
/// expansion + default-kind). Returns `None` on any parse/extraction
/// failure so the caller treats it as "no edges" rather than dropping the
/// item.
fn extract_reference_edges(
    item: &CanonicalRef,
    kinds: &KindRegistry,
    parsers: &ParserDispatcher,
    raw: &RawLoadedItem,
) -> Option<Vec<CorpusReferenceEdge>> {
    let kind_schema = kinds.get(&item.kind)?;

    // Which field holds references is schema-declared — no hardcoded name.
    // A kind with no `resolve_references` step has no reference edges.
    let field = kind_schema.resolution.iter().find_map(|d| match d {
        ResolutionStepDecl::ResolveReferences { field, .. } => Some(field.clone()),
        _ => None,
    })?;

    let source_format = kind_schema.resolved_format_for(&raw.matched_ext)?;
    let parsed = parsers
        .dispatch(
            &source_format.parser,
            &raw.content,
            Some(&raw.source_path),
            &source_format.signature,
        )
        .ok()?;

    let declared = field_as_list(&parsed, &field, ResolutionStepName::ResolveReferences).ok()?;

    let aliases = kind_schema
        .execution
        .as_ref()
        .map(|e| AliasResolver::new(e.aliases.clone(), e.alias_max_depth))
        .unwrap_or_else(|| AliasResolver::new(Default::default(), 8));

    let mut edges = Vec::new();
    for ref_id in declared {
        // Canonicalize like the references step: alias-expand, then ensure
        // canonical with the item's kind as default. Emit the edge BEFORE
        // (and without) loading the target — dangling refs are preserved.
        // A single un-canonicalizable ref is skipped (best-effort); the
        // item itself still ships.
        let Ok((after_alias, _hop)) = aliases.resolve(&ref_id, &item.kind) else {
            continue;
        };
        let canonical = ensure_canonical(&after_alias, &item.kind);
        let Ok(to_ref) = CanonicalRef::parse(&canonical) else {
            continue;
        };
        edges.push(CorpusReferenceEdge {
            from_ref: item.to_string(),
            to_ref: to_ref.to_string(),
        });
    }
    Some(edges)
}
