//! Composition algorithms: single-root `compose` and multi-root `compose_positions`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use ryeos_runtime::op_wire::{EdgeKind, GraphEdge, SingleRootPayload, VerifiedItem};

use crate::budget::estimate_tokens;
use crate::frontmatter::strip_frontmatter;
use crate::ordering::{extends_first, OrderedItem};
use crate::render::render_item;
use crate::types::{
    ComposeContextPayload, ComposeEdge, ComposeEdgeKind, ComposeInputs, ComposeItem, ComposeMeta,
    ComposeOutput, ComposePayload, KnowledgeError, OmittedItem, OmissionReason,
    RenderedContexts, RenderedPosition,
};

/// Single-root compose. Walks the graph from root_ref, strips frontmatter,
/// renders items in extends-first order within the token budget.
pub fn compose(payload: &ComposePayload) -> Result<ComposeOutput, KnowledgeError> {
    let root_ref = &payload.root.root_ref;
    let items_by_ref = &payload.root.items_by_ref;
    let edges = &payload.root.edges;
    let inputs = &payload.inputs;
    let budget = inputs.token_budget;

    // Build ordered item list
    let ordered = build_ordered_items(root_ref, edges);

    // Apply exclude filter
    let exclude_set: BTreeSet<&str> = inputs.exclude_refs.iter().map(|s| s.as_str()).collect();

    let mut content_parts = Vec::new();
    let mut resolved_items = Vec::new();
    let mut items_omitted = Vec::new();
    let mut compose_edges = Vec::new();
    let mut tokens_used = 0usize;

    // Convert edges to compose edges
    for edge in edges {
        compose_edges.push(ComposeEdge {
            from: edge.from.clone(),
            to: edge.to.clone(),
            kind: match edge.kind {
                EdgeKind::Extends => ComposeEdgeKind::Extends,
                EdgeKind::References => ComposeEdgeKind::References,
            },
        });
    }

    for item in ordered {
        // Check exclusion
        if exclude_set.contains(item.ref_id.as_str()) {
            items_omitted.push(OmittedItem {
                item_id: item.ref_id.clone(),
                reason: OmissionReason::Excluded,
            });
            continue;
        }

        // Look up verified item
        let verified = items_by_ref.get(&item.ref_id).ok_or_else(|| {
            KnowledgeError::Internal(format!(
                "edge endpoint missing from items_by_ref: {}",
                item.ref_id
            ))
        })?;

        // Strip frontmatter
        let body = strip_frontmatter(&verified.raw_content, &item.ref_id)?;

        // Render
        let rendered = render_item(item.role, &item.ref_id, &body);
        let item_tokens = estimate_tokens(&rendered);

        // Budget check
        if tokens_used + item_tokens > budget {
            items_omitted.push(OmittedItem {
                item_id: item.ref_id.clone(),
                reason: OmissionReason::OverBudget,
            });
            continue;
        }

        tokens_used += item_tokens;
        content_parts.push(rendered);
        resolved_items.push(ComposeItem {
            item_id: item.ref_id.clone(),
            role: item.role,
            depth: item.depth,
            tokens: item_tokens,
        });
    }

    let content = content_parts.join("");
    let tokens_remaining = budget.saturating_sub(tokens_used);

    Ok(ComposeOutput {
        content,
        composition: ComposeMeta {
            resolved_items,
            items_omitted,
            edges: compose_edges,
        },
        tokens_used,
        token_budget: budget,
        tokens_remaining,
    })
}

/// Multi-root, multi-position compose. Used by the `compose_positions` op.
///
/// Iterates `roots_by_position` in deterministic order (system → before → after,
/// then remaining in BTreeMap order). Cross-position deduplication: refs already
/// emitted in an earlier position are excluded from later positions.
pub fn compose_positions(payload: &ComposeContextPayload) -> Result<RenderedContexts, KnowledgeError> {
    let position_order = determine_position_order(&payload.roots_by_position);
    let mut already_emitted: BTreeSet<String> = BTreeSet::new();
    let mut rendered = BTreeMap::new();

    for position in position_order {
        let roots = payload.roots_by_position.get(&position);
        let budget = payload
            .per_position_budget
            .get(&position)
            .copied()
            .unwrap_or(payload.default_budget);

        let mut combined_content = String::new();
        let mut combined_items = Vec::new();
        let mut combined_omitted = Vec::new();
        let mut combined_edges = Vec::new();
        let mut budget_remaining = budget;

        if let Some(root_refs) = roots {
            for root_ref in root_refs {
                // If this root was already emitted in an earlier position,
                // skip it entirely and report it as excluded.
                if already_emitted.contains(root_ref) {
                    combined_omitted.push(OmittedItem {
                        item_id: root_ref.clone(),
                        reason: OmissionReason::Excluded,
                    });
                    continue;
                }

                // Build a SingleRootPayload for this root by extracting
                // its subgraph from the global maps.
                let sub_edges: Vec<_> = payload
                    .edges
                    .iter()
                    .filter(|e| {
                        is_reachable_from(root_ref, &e.from, &payload.edges)
                            || e.from == *root_ref
                    })
                    .cloned()
                    .collect();

                let sub_items: BTreeMap<String, VerifiedItem> = payload
                    .items_by_ref
                    .iter()
                    .filter(|(k, _)| {
                        *k == root_ref
                            || sub_edges.iter().any(|e| e.from == **k || e.to == **k)
                            || is_reachable_from(root_ref, k, &payload.edges)
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                let single_root = SingleRootPayload {
                    root_ref: root_ref.clone(),
                    items_by_ref: sub_items,
                    edges: sub_edges,
                };

                // Exclude refs already emitted in earlier positions (but not this root)
                let exclude: Vec<String> = already_emitted
                    .iter()
                    .filter(|r| *r != root_ref)
                    .cloned()
                    .collect();

                let compose_payload = ComposePayload {
                    root: single_root,
                    inputs: ComposeInputs {
                        token_budget: budget_remaining,
                        exclude_refs: exclude,
                        position: Some(position.clone()),
                    },
                };

                let result = compose(&compose_payload)?;

                // Add newly emitted refs to tracking
                for item in &result.composition.resolved_items {
                    already_emitted.insert(item.item_id.clone());
                }

                combined_content.push_str(&result.content);
                budget_remaining = budget_remaining.saturating_sub(result.tokens_used);

                combined_items.extend(result.composition.resolved_items);
                combined_omitted.extend(result.composition.items_omitted);
                combined_edges.extend(result.composition.edges);
            }
        }

        rendered.insert(
            position,
            RenderedPosition {
                content: combined_content,
                composition: ComposeMeta {
                    resolved_items: combined_items,
                    items_omitted: combined_omitted,
                    edges: combined_edges,
                },
                tokens_used: budget.saturating_sub(budget_remaining),
                token_budget: budget,
            },
        );
    }

    Ok(RenderedContexts { rendered })
}

/// Deterministic position order: system → before → after, then remaining.
fn determine_position_order(roots_by_position: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut order = Vec::new();
    let priority = ["system", "before", "after"];

    for p in &priority {
        if roots_by_position.contains_key(*p) {
            order.push(p.to_string());
        }
    }

    // Remaining positions in BTreeMap order
    for pos in roots_by_position.keys() {
        if !priority.contains(&pos.as_str()) {
            order.push(pos.clone());
        }
    }

    order
}

/// Check if `from` can reach `target` via extends edges.
fn is_reachable_from(root: &str, target: &str, edges: &[GraphEdge]) -> bool {
    if root == target {
        return true;
    }
    let mut visited = BTreeSet::new();
    let mut stack = vec![root.to_string()];
    while let Some(current) = stack.pop() {
        if current == target {
            return true;
        }
        if !visited.insert(current.clone()) {
            continue;
        }
        for edge in edges {
            if edge.from == current {
                stack.push(edge.to.clone());
            }
        }
    }
    false
}

/// Build ordered items from root + edges (extends-first order).
fn build_ordered_items(root_ref: &str, edges: &[GraphEdge]) -> Vec<OrderedItem> {
    extends_first(root_ref, edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ComposeInputs, ComposePayload, ComposeRole};
    use ryeos_runtime::op_wire::{EdgeKind, TrustClass};

    fn make_item(ref_id: &str, content: &str) -> (String, VerifiedItem) {
        (
            ref_id.to_string(),
            VerifiedItem {
                raw_content: content.to_string(),
                raw_content_digest: format!("sha256:{ref_id}"),
                metadata: serde_json::json!({}),
                trust_class: TrustClass::TrustedSystem,
            },
        )
    }

    fn make_extends_edge(from: &str, to: &str) -> GraphEdge {
        GraphEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind: EdgeKind::Extends,
            depth_from_root: Some(1),
        }
    }

    fn make_refs_edge(from: &str, to: &str) -> GraphEdge {
        GraphEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind: EdgeKind::References,
            depth_from_root: Some(1),
        }
    }

    #[test]
    fn compose_single_root_no_edges() {
        let mut items = BTreeMap::new();
        items.insert("root".to_string(), make_item("root", "Root body").1);

        let payload = ComposePayload {
            root: SingleRootPayload {
                root_ref: "root".to_string(),
                items_by_ref: items,
                edges: vec![],
            },
            inputs: ComposeInputs {
                token_budget: 1000,
                exclude_refs: vec![],
                position: None,
            },
        };

        let result = compose(&payload).unwrap();
        assert!(result.content.contains("Root body"));
        assert_eq!(result.composition.resolved_items.len(), 1);
        assert_eq!(result.composition.resolved_items[0].role, ComposeRole::Primary);
    }

    #[test]
    fn compose_with_extends_chain() {
        let mut items = BTreeMap::new();
        items.insert("root".to_string(), make_item("root", "Root body").1);
        items.insert("parent".to_string(), make_item("parent", "Parent body").1);

        let payload = ComposePayload {
            root: SingleRootPayload {
                root_ref: "root".to_string(),
                items_by_ref: items,
                edges: vec![make_extends_edge("root", "parent")],
            },
            inputs: ComposeInputs {
                token_budget: 1000,
                exclude_refs: vec![],
                position: None,
            },
        };

        let result = compose(&payload).unwrap();
        assert!(result.content.contains("Parent body"));
        assert!(result.content.contains("Root body"));
        // Parent (extends) should appear before root (primary)
        let parent_pos = result.content.find("Parent").unwrap();
        let root_pos = result.content.find("Root body").unwrap();
        assert!(parent_pos < root_pos);
    }

    #[test]
    fn compose_with_references() {
        let mut items = BTreeMap::new();
        items.insert("root".to_string(), make_item("root", "Root body").1);
        items.insert("ref1".to_string(), make_item("ref1", "Ref body").1);

        let payload = ComposePayload {
            root: SingleRootPayload {
                root_ref: "root".to_string(),
                items_by_ref: items,
                edges: vec![make_refs_edge("root", "ref1")],
            },
            inputs: ComposeInputs {
                token_budget: 1000,
                exclude_refs: vec![],
                position: None,
            },
        };

        let result = compose(&payload).unwrap();
        assert!(result.content.contains("Root body"));
        assert!(result.content.contains("Ref body"));
    }

    #[test]
    fn compose_exclude_refs() {
        let mut items = BTreeMap::new();
        items.insert("root".to_string(), make_item("root", "Root body").1);
        items.insert("ref1".to_string(), make_item("ref1", "Ref body").1);

        let payload = ComposePayload {
            root: SingleRootPayload {
                root_ref: "root".to_string(),
                items_by_ref: items,
                edges: vec![make_refs_edge("root", "ref1")],
            },
            inputs: ComposeInputs {
                token_budget: 1000,
                exclude_refs: vec!["ref1".to_string()],
                position: None,
            },
        };

        let result = compose(&payload).unwrap();
        assert!(!result.content.contains("Ref body"));
        assert_eq!(result.composition.items_omitted.len(), 1);
        assert_eq!(result.composition.items_omitted[0].reason, OmissionReason::Excluded);
    }

    #[test]
    fn compose_over_budget() {
        let mut items = BTreeMap::new();
        items.insert("root".to_string(), make_item("root", "Root body").1);
        items.insert("big".to_string(), make_item("big", &"X".repeat(500)).1);

        let payload = ComposePayload {
            root: SingleRootPayload {
                root_ref: "root".to_string(),
                items_by_ref: items,
                edges: vec![make_refs_edge("root", "big")],
            },
            inputs: ComposeInputs {
                token_budget: 5, // very tight
                exclude_refs: vec![],
                position: None,
            },
        };

        let result = compose(&payload).unwrap();
        // The root might fit but the big reference won't
        assert!(result.composition.items_omitted.len() >= 1);
    }

    #[test]
    fn compose_positions_deterministic_order() {
        let order = determine_position_order(&BTreeMap::from([
            ("after".to_string(), vec![]),
            ("system".to_string(), vec![]),
            ("before".to_string(), vec![]),
        ]));
        assert_eq!(order, vec!["system", "before", "after"]);
    }

    #[test]
    fn compose_positions_extra_positions_after_priority() {
        let order = determine_position_order(&BTreeMap::from([
            ("zebra".to_string(), vec![]),
            ("system".to_string(), vec![]),
            ("alpha".to_string(), vec![]),
        ]));
        assert_eq!(order[0], "system");
        // alpha and zebra in BTreeMap order (alphabetical)
        assert!(order[1] == "alpha");
        assert!(order[2] == "zebra");
    }

    #[test]
    fn compose_positions_cross_position_dedupe() {
        let mut items = BTreeMap::new();
        items.insert("shared".to_string(), make_item("shared", "Shared content").1);
        items.insert("extra".to_string(), make_item("extra", "Extra content").1);

        let payload = ComposeContextPayload {
            items_by_ref: items,
            edges: vec![],
            roots_by_position: BTreeMap::from([
                ("system".to_string(), vec!["shared".to_string()]),
                ("before".to_string(), vec!["shared".to_string(), "extra".to_string()]),
            ]),
            per_position_budget: BTreeMap::from([
                ("system".to_string(), 1000),
                ("before".to_string(), 1000),
            ]),
            default_budget: 1000,
        };

        let result = compose_positions(&payload).unwrap();

        // "shared" should appear in system
        let system = result.rendered.get("system").unwrap();
        assert!(system.content.contains("Shared content"));

        // "shared" should be excluded from before, "extra" should appear
        let before = result.rendered.get("before").unwrap();
        assert!(before.content.contains("Extra content"));
        // "shared" should be in omitted due to cross-position dedupe
        assert!(before
            .composition
            .items_omitted
            .iter()
            .any(|o| o.item_id == "shared" && o.reason == OmissionReason::Excluded));
    }
}
