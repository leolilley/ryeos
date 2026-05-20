//! Slim-payload projection for compose_context_positions augmentation.
//!
//! Builds a multi-root payload from multiple `ResolutionOutput`s and a
//! position map. Validates that every edge endpoint and every position
//! root exists in `items_by_ref`.
//!
//! The output is a generic JSON structure (not a knowledge-specific
//! type) so the daemon doesn't depend on kind-specific crates.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use ryeos_engine::resolution::ResolutionOutput;
use ryeos_runtime::op_wire::{EdgeKind, GraphEdge, TrustClass, VerifiedItem};

use super::LaunchAugmentationError;

/// Build a compose-context payload for the knowledge runtime's
/// `compose_positions` op.
///
/// Takes multiple resolution outputs (one per unique root ref across
/// all positions) and a position map, and produces a JSON payload
/// containing all items, edges, and position assignments.
///
/// # Invariants validated
/// 1. Every edge endpoint exists in `items_by_ref`.
/// 2. Every position root exists in `items_by_ref`.
pub fn build_compose_context_payload(
    per_root: &BTreeMap<String, ResolutionOutput>,
    positions: &BTreeMap<String, Vec<String>>,
    per_position_budget: &BTreeMap<String, usize>,
) -> Result<Value, LaunchAugmentationError> {
    let mut items_by_ref: BTreeMap<String, VerifiedItem> = BTreeMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();

    for (_root_ref, resolution) in per_root {
        // Root + ancestors.
        for resolved in
            std::iter::once(&resolution.root).chain(resolution.ancestors.iter())
        {
            items_by_ref
                .entry(resolved.resolved_ref.clone())
                .or_insert_with(|| ancestor_to_verified(resolved));
        }
        // Referenced items.
        for resolved in &resolution.referenced_items {
            items_by_ref
                .entry(resolved.resolved_ref.clone())
                .or_insert_with(|| ancestor_to_verified(resolved));
        }
        // Extends edges.
        if !resolution.ancestors.is_empty() {
            let mut from = resolution.root.resolved_ref.clone();
            for (i, anc) in resolution.ancestors.iter().enumerate() {
                edges.push(GraphEdge {
                    from: from.clone(),
                    to: anc.resolved_ref.clone(),
                    kind: EdgeKind::Extends,
                    depth_from_root: Some(i + 1),
                });
                from = anc.resolved_ref.clone();
            }
        }
        // Reference edges.
        for edge in &resolution.references_edges {
            edges.push(GraphEdge {
                from: edge.from_ref.clone(),
                to: edge.to_ref.clone(),
                kind: EdgeKind::References,
                depth_from_root: None,
            });
        }
    }

    // Validate: every edge endpoint is in items_by_ref.
    for edge in &edges {
        if !items_by_ref.contains_key(&edge.from)
            || !items_by_ref.contains_key(&edge.to)
        {
            return Err(LaunchAugmentationError::ProjectionInvariant {
                reason: format!(
                    "edge endpoint missing from items_by_ref: {} -> {}",
                    edge.from, edge.to
                ),
            });
        }
    }

    // Validate: every position root is in items_by_ref.
    for (position, refs) in positions {
        for r in refs {
            if !items_by_ref.contains_key(r) {
                return Err(LaunchAugmentationError::ProjectionInvariant {
                    reason: format!(
                        "position '{position}' root '{r}' missing from items_by_ref"
                    ),
                });
            }
        }
    }

    // Build the payload JSON. This matches the shape of
    // `ComposeContextPayload` from the knowledge runtime's types,
    // but as raw JSON so the daemon doesn't import knowledge types.
    Ok(json!({
        "items_by_ref": serde_json::to_value(&items_by_ref)?,
        "edges": serde_json::to_value(&edges)?,
        "roots_by_position": positions,
        "per_position_budget": per_position_budget,
        "default_budget": 2000,
    }))
}

/// Helper: convert engine `ResolvedAncestor` → wire `VerifiedItem`.
fn ancestor_to_verified(
    a: &ryeos_engine::resolution::ResolvedAncestor,
) -> VerifiedItem {
    VerifiedItem {
        raw_content: a.raw_content.clone(),
        raw_content_digest: a.raw_content_digest.clone(),
        metadata: serde_json::json!({
            "source_path": a.source_path,
            "trust_class": format!("{:?}", a.trust_class),
            "requested_id": a.requested_id,
        }),
        trust_class: match a.trust_class {
            ryeos_engine::resolution::TrustClass::TrustedSystem => TrustClass::TrustedSystem,
            ryeos_engine::resolution::TrustClass::TrustedUser => TrustClass::TrustedUser,
            ryeos_engine::resolution::TrustClass::UntrustedUserSpace => {
                TrustClass::TrustedProject
            }
            ryeos_engine::resolution::TrustClass::Unsigned => TrustClass::Untrusted,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::resolution::{
        ResolvedAncestor, ResolutionEdge, ResolutionOutput, ResolutionStepName,
        TrustClass as EngineTrustClass,
    };
    use ryeos_engine::resolution::KindComposedView;

    fn make_ancestor(ref_str: &str, content: &str, trust: EngineTrustClass) -> ResolvedAncestor {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let digest = format!("{:016x}", hasher.finish());
        ResolvedAncestor {
            requested_id: ref_str.to_string(),
            resolved_ref: ref_str.to_string(),
            source_path: std::path::PathBuf::from(format!("/tmp/{ref_str}")),
            trust_class: trust,
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: content.to_string(),
            raw_content_digest: digest,
        }
    }

    fn make_resolution(root: ResolvedAncestor, ancestors: Vec<ResolvedAncestor>, ref_edges: Vec<ResolutionEdge>) -> ResolutionOutput {
        ResolutionOutput {
            root,
            ancestors,
            references_edges: ref_edges,
            referenced_items: vec![],
            step_outputs: std::collections::HashMap::new(),
            executor_trust_class: EngineTrustClass::TrustedUser,
            composed: KindComposedView::identity(json!({})),
        }
    }

    #[test]
    fn projection_single_position_single_root() {
        let root = make_ancestor("knowledge:doc", "content", EngineTrustClass::TrustedUser);
        let mut per_root = BTreeMap::new();
        per_root.insert("knowledge:doc".to_string(), make_resolution(root, vec![], vec![]));

        let mut positions = BTreeMap::new();
        positions.insert("system".to_string(), vec!["knowledge:doc".to_string()]);

        let payload = build_compose_context_payload(&per_root, &positions, &BTreeMap::new()).unwrap();
        let obj = payload.as_object().unwrap();

        assert_eq!(obj["items_by_ref"].as_object().unwrap().len(), 1);
        assert!(obj["items_by_ref"]["knowledge:doc"].is_object());
        assert_eq!(obj["edges"].as_array().unwrap().len(), 0);
        assert_eq!(
            obj["roots_by_position"]["system"].as_array().unwrap()[0],
            "knowledge:doc"
        );
    }

    #[test]
    fn projection_two_positions_disjoint() {
        let root1 = make_ancestor("knowledge:a", "a", EngineTrustClass::TrustedUser);
        let root2 = make_ancestor("knowledge:b", "b", EngineTrustClass::TrustedSystem);
        let mut per_root = BTreeMap::new();
        per_root.insert("knowledge:a".to_string(), make_resolution(root1, vec![], vec![]));
        per_root.insert("knowledge:b".to_string(), make_resolution(root2, vec![], vec![]));

        let mut positions = BTreeMap::new();
        positions.insert("system".to_string(), vec!["knowledge:a".to_string()]);
        positions.insert("before".to_string(), vec!["knowledge:b".to_string()]);

        let payload = build_compose_context_payload(&per_root, &positions, &BTreeMap::new()).unwrap();
        assert_eq!(payload["items_by_ref"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn projection_shared_ancestor_deduped() {
        let base = make_ancestor("knowledge:base", "base", EngineTrustClass::TrustedSystem);
        let child1 = make_ancestor("knowledge:c1", "c1", EngineTrustClass::TrustedUser);
        let child2 = make_ancestor("knowledge:c2", "c2", EngineTrustClass::TrustedUser);

        let mut per_root = BTreeMap::new();
        per_root.insert(
            "knowledge:c1".to_string(),
            make_resolution(child1, vec![base.clone()], vec![]),
        );
        per_root.insert(
            "knowledge:c2".to_string(),
            make_resolution(child2, vec![base.clone()], vec![]),
        );

        let mut positions = BTreeMap::new();
        positions.insert("pos1".to_string(), vec!["knowledge:c1".to_string()]);
        positions.insert("pos2".to_string(), vec!["knowledge:c2".to_string()]);

        let payload = build_compose_context_payload(&per_root, &positions, &BTreeMap::new()).unwrap();
        // c1, c2, base — base appears once despite being in both resolutions.
        assert_eq!(payload["items_by_ref"].as_object().unwrap().len(), 3);
    }

    #[test]
    fn projection_reference_edges_included() {
        let root = make_ancestor("knowledge:main", "main", EngineTrustClass::TrustedUser);
        let ref_item = make_ancestor("knowledge:other", "other", EngineTrustClass::TrustedSystem);
        let ref_edge = ResolutionEdge {
            from_ref: "knowledge:main".to_string(),
            from_source_path: std::path::PathBuf::from("/tmp/main"),
            to_ref: "knowledge:other".to_string(),
            to_source_path: std::path::PathBuf::from("/tmp/other"),
            trust_class: EngineTrustClass::TrustedSystem,
            added_by: ResolutionStepName::ResolveReferences,
        };
        let mut resolution = make_resolution(root, vec![], vec![ref_edge]);
        resolution.referenced_items.push(ref_item.clone());

        let mut per_root = BTreeMap::new();
        per_root.insert("knowledge:main".to_string(), resolution);

        let mut positions = BTreeMap::new();
        positions.insert("system".to_string(), vec!["knowledge:main".to_string()]);

        let payload = build_compose_context_payload(&per_root, &positions, &BTreeMap::new()).unwrap();
        let edges = payload["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "knowledge:main");
        assert_eq!(edges[0]["to"], "knowledge:other");
        assert_eq!(edges[0]["kind"], "references");
    }

    #[test]
    fn projection_missing_edge_endpoint_errors() {
        let root = make_ancestor("knowledge:main", "main", EngineTrustClass::TrustedUser);
        // Edge references an item NOT in referenced_items (dangling edge).
        let ref_edge = ResolutionEdge {
            from_ref: "knowledge:main".to_string(),
            from_source_path: std::path::PathBuf::from("/tmp/main"),
            to_ref: "knowledge:missing".to_string(),
            to_source_path: std::path::PathBuf::from("/tmp/missing"),
            trust_class: EngineTrustClass::TrustedSystem,
            added_by: ResolutionStepName::ResolveReferences,
        };
        let resolution = make_resolution(root, vec![], vec![ref_edge]);

        let mut per_root = BTreeMap::new();
        per_root.insert("knowledge:main".to_string(), resolution);

        let mut positions = BTreeMap::new();
        positions.insert("system".to_string(), vec!["knowledge:main".to_string()]);

        let err = build_compose_context_payload(&per_root, &positions, &BTreeMap::new())
            .expect_err("dangling edge must error");
        assert!(
            matches!(err, LaunchAugmentationError::ProjectionInvariant { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn projection_missing_position_root_errors() {
        let root = make_ancestor("knowledge:a", "a", EngineTrustClass::TrustedUser);
        let mut per_root = BTreeMap::new();
        per_root.insert("knowledge:a".to_string(), make_resolution(root, vec![], vec![]));

        let mut positions = BTreeMap::new();
        positions.insert("system".to_string(), vec!["knowledge:nonexistent".to_string()]);

        let err = build_compose_context_payload(&per_root, &positions, &BTreeMap::new())
            .expect_err("missing position root must error");
        assert!(
            matches!(err, LaunchAugmentationError::ProjectionInvariant { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn projection_budgets_included() {
        let root = make_ancestor("knowledge:doc", "c", EngineTrustClass::TrustedUser);
        let mut per_root = BTreeMap::new();
        per_root.insert("knowledge:doc".to_string(), make_resolution(root, vec![], vec![]));

        let mut positions = BTreeMap::new();
        positions.insert("system".to_string(), vec!["knowledge:doc".to_string()]);

        let mut budgets = BTreeMap::new();
        budgets.insert("system".to_string(), 4000);

        let payload = build_compose_context_payload(&per_root, &positions, &budgets).unwrap();
        assert_eq!(payload["per_position_budget"]["system"], 4000);
        assert_eq!(payload["default_budget"], 2000);
    }
}
