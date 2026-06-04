//! Composition ordering policies.

use crate::types::ComposeRole;
use ryeos_runtime::op_wire::{EdgeKind, GraphEdge};

/// Item in composition order with its role.
pub struct OrderedItem {
    pub ref_id: String,
    pub role: ComposeRole,
    pub depth: usize,
}

/// Build an ordered item list by walking edges from a root ref.
///
/// Order: ancestors deepest-first → root → references in edge declaration order.
pub fn extends_first(root_ref: &str, edges: &[GraphEdge]) -> Vec<OrderedItem> {
    let mut result = Vec::new();
    let mut visited = std::collections::BTreeSet::new();

    // Collect extends ancestors in post-order (deepest first)
    let mut extends_ancestors = Vec::new();
    collect_extends_post_order(root_ref, edges, &mut extends_ancestors, &mut visited, 0);

    // Add ancestors (skip the root, which is the last entry)
    for (ref_id, depth) in &extends_ancestors {
        if *ref_id != root_ref {
            result.push(OrderedItem {
                ref_id: ref_id.clone(),
                role: ComposeRole::Extends,
                depth: *depth,
            });
        }
    }

    // Add root
    result.push(OrderedItem {
        ref_id: root_ref.to_string(),
        role: ComposeRole::Primary,
        depth: 0,
    });

    // Add references from root in edge declaration order
    for edge in edges {
        if edge.from == root_ref && edge.kind == EdgeKind::References {
            if !result.iter().any(|i| i.ref_id == edge.to) {
                let depth = edge.depth_from_root.unwrap_or(1);
                result.push(OrderedItem {
                    ref_id: edge.to.clone(),
                    role: ComposeRole::Reference,
                    depth,
                });
            }
        }
    }

    result
}

/// Post-order DFS: visit children first, then push self.
fn collect_extends_post_order(
    node: &str,
    edges: &[GraphEdge],
    result: &mut Vec<(String, usize)>,
    visited: &mut std::collections::BTreeSet<String>,
    depth: usize,
) {
    if !visited.insert(node.to_string()) {
        return;
    }
    for edge in edges {
        if edge.from == node && edge.kind == EdgeKind::Extends {
            collect_extends_post_order(&edge.to, edges, result, visited, depth + 1);
        }
    }
    result.push((node.to_string(), depth));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::op_wire::EdgeKind;

    #[test]
    fn single_item_no_edges() {
        let items = extends_first("root", &[]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].ref_id, "root");
        assert_eq!(items[0].role, ComposeRole::Primary);
    }

    #[test]
    fn extends_chain_order() {
        // root extends parent extends grandparent
        let edges = vec![
            GraphEdge {
                from: "root".to_string(),
                to: "parent".to_string(),
                kind: EdgeKind::Extends,
                depth_from_root: Some(1),
            },
            GraphEdge {
                from: "parent".to_string(),
                to: "grandparent".to_string(),
                kind: EdgeKind::Extends,
                depth_from_root: Some(2),
            },
        ];
        let items = extends_first("root", &edges);
        assert_eq!(items.len(), 3);
        // Deepest ancestor first
        assert_eq!(items[0].ref_id, "grandparent");
        assert_eq!(items[0].role, ComposeRole::Extends);
        assert_eq!(items[1].ref_id, "parent");
        assert_eq!(items[1].role, ComposeRole::Extends);
        assert_eq!(items[2].ref_id, "root");
        assert_eq!(items[2].role, ComposeRole::Primary);
    }
}
