//! `resolve_extends_chain` — walk an item's `extends` field and emit a
//! topologically ordered, bottom-up `Vec<ResolvedAncestor>`.
//!
//! v1 cut: `extends` is a single string (parent_id) or absent. The walk
//! is therefore a linear chain in the absence of cycles, and we keep the
//! recursive shape so the multi-parent advanced path can drop in later
//! without restructuring.
//!
//! Cycle detection uses a 3-state DFS over canonical `source_path`:
//!
//!   * `visiting_extends` — paths on the current recursion stack. A path
//!     about to be re-entered while in this set is a true cycle (the only
//!     way single-parent extends produces a cycle is `A extends A`-style
//!     loops, but the protection is structurally identical for any DAG).
//!   * `done_extends`     — paths whose subtree has been fully resolved.
//!     Re-encountering a done node is a no-op (already pushed once).
//!
//! Aliases compose recursively via `AliasResolver` before any cycle
//! check, so `@alias` and the canonical ref it expands to are recognised
//! as the same node.

use serde_json::json;

use crate::canonical_ref::CanonicalRef;

use crate::resolution::context::{ensure_canonical, field_as_string, ResolutionContext};
use crate::resolution::types::{ResolutionError, ResolutionStepName, ResolvedAncestor};

pub(crate) fn run(
    ctx: &mut ResolutionContext<'_>,
    field: &str,
    max_depth: usize,
) -> Result<(), ResolutionError> {
    // Root is already loaded by `run_resolution_pipeline`; we never push
    // it as an ancestor — `ancestors` is the *ancestor* chain, the root
    // is the caller and ships separately as `ResolutionOutput.root`.
    let root_ref = ctx.current_ref.clone();
    let root_path = ctx.root_loaded.source_path().clone();
    let parent_kind = root_ref.kind.clone();
    let parent_id_opt = field_as_string(&ctx.root_loaded.parsed, field)?;

    // Mark the root as being currently visited so any ancestor that tries
    // to climb back to it is detected as a cycle.
    ctx.visiting_extends.insert(root_path.clone());

    if let Some(parent_id) = parent_id_opt {
        walk(ctx, &parent_id, &parent_kind, field, 1, max_depth)?;
    }

    // Pop root off the visiting stack — symmetrical with `walk`.
    ctx.visiting_extends.remove(&root_path);

    let depth_reached = ctx.ancestors.len();
    ctx.record_step_output(
        "resolve_extends_chain",
        json!({
            "field": field,
            "max_depth": max_depth,
            "depth_reached": depth_reached,
        }),
    );

    Ok(())
}

/// Recursive DFS, post-order push (deepest ancestor lands first
/// → bottom-up topological order).
fn walk(
    ctx: &mut ResolutionContext<'_>,
    requested_id: &str,
    parent_kind: &str,
    field: &str,
    depth: usize,
    max_depth: usize,
) -> Result<(), ResolutionError> {
    if depth > max_depth {
        return Err(ResolutionError::MaxDepthExceeded {
            step: ResolutionStepName::ResolveExtendsChain,
            depth,
        });
    }

    // Expand alias (recursive, source_path-equivalent cycle protection
    // happens further down once we know the on-disk path).
    let (after_alias, alias_hop) = ctx.aliases.resolve(requested_id, parent_kind)?;

    // Promote bare IDs to canonical refs using the parent's kind.
    let canonical = ensure_canonical(&after_alias, parent_kind);
    let ref_ = CanonicalRef::parse(&canonical).map_err(|e| ResolutionError::StepFailed {
        step: ResolutionStepName::ResolveExtendsChain,
        reason: format!("invalid canonical ref `{canonical}`: {e}"),
    })?;

    let loaded = ctx.load_item(&ref_, requested_id, ResolutionStepName::ResolveExtendsChain)?;
    let source_path = loaded.source_path.clone();

    // True cycle: this node is currently on the recursion stack.
    if ctx.visiting_extends.contains(&source_path) {
        let cycle = vec![ResolvedAncestor {
            requested_id: requested_id.to_string(),
            resolved_ref: ref_.to_string(),
            source_path: source_path.clone(),
            trust_class: loaded.trust_class,
            alias_resolution: alias_hop,
            added_by: ResolutionStepName::ResolveExtendsChain,
            raw_content: loaded.raw_content,
            raw_content_digest: loaded.raw_content_digest,
        }];
        return Err(ResolutionError::CycleDetected {
            chain: cycle,
            edge_type: "extends".to_string(),
        });
    }

    // Already fully resolved on a sibling branch — no-op (single-parent
    // can't reach this in v1, but the dedup guard is structurally cheap
    // and future-proofs the multi-parent path).
    if ctx.done_extends.contains(&source_path) {
        return Ok(());
    }

    // Push onto the on-stack set before recursing so a deeper hop
    // pointing back to us is caught as a cycle.
    ctx.visiting_extends.insert(source_path.clone());

    // Recurse into this hop's own extends *before* pushing self → post-order
    // ⇒ bottom-up topological order in `ancestors`.
    let grandparent_kind = ref_.kind.clone();
    let grandparent_opt = field_as_string(&loaded.parsed, field)?;

    // Clone the parsed value out before `into_ancestor` consumes the
    // rest of `loaded`. Composers iterate `ancestors` and
    // `ancestor_parsed` as parallel slices, so we have to push them in
    // lockstep at the same recursion frame.
    let parsed_for_ancestor = loaded.parsed.clone();
    let ancestor = loaded.into_ancestor(
        requested_id.to_string(),
        ref_.to_string(),
        alias_hop,
        ResolutionStepName::ResolveExtendsChain,
    );

    if let Some(gp_id) = grandparent_opt {
        walk(ctx, &gp_id, &grandparent_kind, field, depth + 1, max_depth)?;
    }

    // Pop from on-stack and mark done. The ancestor inlines its own
    // verified bytes — there is no separate payload map to keep in sync.
    ctx.visiting_extends.remove(&source_path);
    ctx.done_extends.insert(source_path);
    ctx.ancestors.push(ancestor);
    ctx.ancestor_parsed.push(parsed_for_ancestor);
    Ok(())
}
