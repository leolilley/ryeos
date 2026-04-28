//! `resolve_references` — walk the `references` field laterally.
//!
//! Unlike `extends`, references are not hierarchical and cycles are
//! allowed. The step emits a deduplicated list of edges keyed by
//! `(from_source_path, to_source_path)` so the consumer can render the
//! relationship graph or enforce per-edge trust.
//!
//! `max_depth` bounds *how far the lateral walk recurses* (default 3) so
//! a deeply linked reference graph doesn't expand the universe.
//!
//! Per-node we track `best_depth_seen`: only skip recursion when we have
//! previously visited the node at an equal-or-shallower depth. Skipping
//! purely on first-visit drops valid edges when DFS happens to take the
//! long branch first (e.g. `root→A→X→C` visits C at depth 3 and locks it
//! before the shorter `root→B→C` branch can recurse into C's own edges).
//!
//! Phase 2 addition: the step also populates
//! `ResolutionContext.referenced_items` with verified, content-pinned
//! items for each unique reference target. Deduplication is by canonical
//! ref string (first discovery wins; edges preserve full topology).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::canonical_ref::CanonicalRef;

use crate::resolution::context::{ensure_canonical, field_as_list, ResolutionContext};
use crate::resolution::types::{ResolutionEdge, ResolutionError, ResolutionStepName};

pub(crate) fn run(
    ctx: &mut ResolutionContext<'_>,
    field: &str,
    max_depth: usize,
) -> Result<(), ResolutionError> {
    let root_ref = ctx.current_ref.clone();
    let root_path = ctx.root_loaded.source_path().clone();
    let direct = field_as_list(&ctx.root_loaded.parsed, field, ResolutionStepName::ResolveReferences)?;

    // Best (smallest) depth at which we've recursed *into* a given node.
    // Root is treated as visited at depth 0.
    let mut best_depth_seen: HashMap<PathBuf, usize> = HashMap::new();
    best_depth_seen.insert(root_path.clone(), 0);

    // Track which canonical refs have already been added to
    // referenced_items so we dedup by identity (first discovery wins;
    // edges preserve full topology).
    let mut seen_refs: HashSet<String> = HashSet::new();

    let mut edges_added = 0usize;
    for ref_id in direct {
        edges_added += walk(
            ctx,
            &root_path,
            &root_ref,
            &ref_id,
            &root_ref.kind.clone(),
            field,
            1,
            max_depth,
            &mut best_depth_seen,
            &mut seen_refs,
        )?;
    }

    ctx.record_step_output(
        "resolve_references",
        serde_json::json!({
            "field": field,
            "max_depth": max_depth,
            "edges": edges_added,
            "referenced_items": ctx.referenced_items.len(),
        }),
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk(
    ctx: &mut ResolutionContext<'_>,
    from_path: &PathBuf,
    from_ref: &CanonicalRef,
    requested_id: &str,
    parent_kind: &str,
    field: &str,
    depth: usize,
    max_depth: usize,
    best_depth_seen: &mut HashMap<PathBuf, usize>,
    seen_refs: &mut HashSet<String>,
) -> Result<usize, ResolutionError> {
    if depth > max_depth {
        return Ok(0);
    }

    let (after_alias, alias_hop) = ctx.aliases.resolve(requested_id, parent_kind)?;
    let canonical = ensure_canonical(&after_alias, parent_kind);
    let ref_ = CanonicalRef::parse(&canonical).map_err(|e| ResolutionError::StepFailed {
        step: ResolutionStepName::ResolveReferences,
        reason: format!("invalid canonical ref `{canonical}`: {e}"),
    })?;

    let loaded = ctx.load_item(&ref_, requested_id, ResolutionStepName::ResolveReferences)?;
    let to_path = loaded.source_path.clone();
    let trust_class = loaded.trust_class;

    // Extract the parsed value before consuming loaded for the ancestor.
    let parsed_for_recursion = loaded.parsed.clone();

    let edge = ResolutionEdge {
        from_ref: from_ref.to_string(),
        from_source_path: from_path.clone(),
        to_ref: ref_.to_string(),
        to_source_path: to_path.clone(),
        trust_class,
        added_by: ResolutionStepName::ResolveReferences,
    };

    let mut added = if push_dedup(&mut ctx.references_edges, edge) {
        1
    } else {
        0
    };

    // Add to referenced_items if this ref hasn't been seen yet.
    // Dedup by canonical ref string, not by source path, so multiple
    // paths to the same item produce one entry.
    let canonical_str = ref_.to_string();
    let is_new_ref = seen_refs.insert(canonical_str);
    if is_new_ref {
        let ancestor = loaded.into_ancestor(
            requested_id.to_string(),
            ref_.to_string(),
            alias_hop,
            ResolutionStepName::ResolveReferences,
        );
        ctx.referenced_items.push(ancestor);
    }

    // Skip recursion only if we've already entered this node at an equal
    // or shallower depth. A shorter path always strictly subsumes a
    // longer one, so a re-entry at smaller depth must be allowed to
    // expand any edges that were truncated by `max_depth` last time.
    match best_depth_seen.get(&to_path) {
        Some(&prior_depth) if prior_depth <= depth => return Ok(added),
        _ => {
            best_depth_seen.insert(to_path.clone(), depth);
        }
    }

    let next_kind = ref_.kind.clone();
    let next_path = to_path.clone();
    let next_ids = field_as_list(&parsed_for_recursion, field, ResolutionStepName::ResolveReferences)?;
    for next_id in next_ids {
        added += walk(
            ctx,
            &next_path,
            &ref_,
            &next_id,
            &next_kind,
            field,
            depth + 1,
            max_depth,
            best_depth_seen,
            seen_refs,
        )?;
    }
    Ok(added)
}

fn push_dedup(edges: &mut Vec<ResolutionEdge>, edge: ResolutionEdge) -> bool {
    let dup = edges.iter().any(|e| {
        e.from_source_path == edge.from_source_path && e.to_source_path == edge.to_source_path
    });
    if dup {
        false
    } else {
        edges.push(edge);
        true
    }
}
