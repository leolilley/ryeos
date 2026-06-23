//! Reachable-subgraph extraction over knowledge edges.
//!
//! Given a verified corpus (`items_by_ref`), a set of `edges`, and a set
//! of `roots`, returns the refs reachable from the roots within `depth`,
//! the edges internal to that set, and any dangling edge endpoints.

use std::collections::{BTreeSet, HashMap, VecDeque};

use ryeos_runtime::method_wire::EdgeKind;

use crate::types::{GraphEdgeOut, GraphOutput, GraphPayload, KnowledgeError};

pub fn graph(payload: &GraphPayload) -> Result<GraphOutput, KnowledgeError> {
    let items = &payload.items_by_ref;
    let mut warnings = Vec::new();
    let mut missing: BTreeSet<String> = BTreeSet::new();

    // Build adjacency and record edge endpoints absent from the corpus.
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &payload.edges {
        adj.entry(e.from.as_str()).or_default().push(e.to.as_str());
        if !items.contains_key(&e.from) {
            missing.insert(e.from.clone());
        }
        if !items.contains_key(&e.to) {
            missing.insert(e.to.clone());
        }
    }

    // Empty roots means "the whole corpus" — every item is a root.
    let roots: Vec<String> = if payload.args.roots.is_empty() {
        items.keys().cloned().collect()
    } else {
        payload.args.roots.clone()
    };

    // BFS bounded by depth (edges traversed, not nodes visited). Only
    // corpus-present refs are ever inserted into `reachable` — a missing
    // root or a dangling edge target is reported via warnings/missing_refs
    // but is NOT a graph node.
    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    for r in &roots {
        if !items.contains_key(r) {
            warnings.push(format!("root not in corpus: {r}"));
            continue;
        }
        if reachable.insert(r.clone()) {
            queue.push_back((r.clone(), 0));
        }
    }
    while let Some((node, depth)) = queue.pop_front() {
        if depth >= payload.args.depth {
            continue;
        }
        if let Some(neighbors) = adj.get(node.as_str()) {
            for &nb in neighbors {
                // Dangling targets are recorded in `missing_refs` (above),
                // never traversed or added as nodes.
                if !items.contains_key(nb) {
                    continue;
                }
                if reachable.insert(nb.to_string()) {
                    queue.push_back((nb.to_string(), depth + 1));
                }
            }
        }
    }

    // Edges output only when BOTH endpoints are reachable corpus nodes —
    // so a dangling edge is never emitted as an internal edge.
    let edges: Vec<GraphEdgeOut> = payload
        .edges
        .iter()
        .filter(|e| reachable.contains(&e.from) && reachable.contains(&e.to))
        .map(|e| GraphEdgeOut {
            from: e.from.clone(),
            to: e.to.clone(),
            kind: edge_kind_str(e.kind),
        })
        .collect();

    Ok(GraphOutput {
        nodes: reachable.into_iter().collect(),
        edges,
        roots,
        missing_refs: missing.into_iter().collect(),
        warnings,
    })
}

fn edge_kind_str(kind: EdgeKind) -> String {
    match kind {
        EdgeKind::Extends => "extends",
        EdgeKind::References => "references",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::method_wire::{GraphEdge, TrustClass, VerifiedItem};
    use std::collections::BTreeMap;

    use crate::types::GraphArgs;

    fn item() -> VerifiedItem {
        VerifiedItem {
            raw_content: "x".into(),
            raw_content_digest: "d".into(),
            metadata: serde_json::json!({}),
            trust_class: TrustClass::TrustedBundle,
        }
    }

    fn edge(from: &str, to: &str) -> GraphEdge {
        GraphEdge {
            from: from.into(),
            to: to.into(),
            kind: EdgeKind::References,
            depth_from_root: None,
        }
    }

    fn payload(refs: &[&str], edges: Vec<GraphEdge>, inputs: GraphArgs) -> GraphPayload {
        let mut map = BTreeMap::new();
        for r in refs {
            map.insert(r.to_string(), item());
        }
        GraphPayload {
            items_by_ref: map,
            edges,
            args: inputs,
        }
    }

    #[test]
    fn reaches_within_depth() {
        let p = payload(
            &["a", "b", "c", "d"],
            vec![edge("a", "b"), edge("b", "c"), edge("c", "d")],
            GraphArgs {
                roots: vec!["a".into()],
                depth: 2,
            },
        );
        let out = graph(&p).unwrap();
        // a -(1)-> b -(2)-> c ; d is at depth 3, excluded.
        assert_eq!(out.nodes, vec!["a", "b", "c"]);
        assert!(!out.nodes.contains(&"d".to_string()));
    }

    #[test]
    fn empty_roots_returns_whole_corpus() {
        let p = payload(
            &["a", "b"],
            vec![edge("a", "b")],
            GraphArgs {
                roots: vec![],
                depth: 5,
            },
        );
        let out = graph(&p).unwrap();
        assert_eq!(out.nodes, vec!["a", "b"]);
        assert_eq!(out.edges.len(), 1);
    }

    #[test]
    fn dangling_edge_endpoint_reported_but_not_a_node_or_edge() {
        let p = payload(
            &["a"],
            vec![edge("a", "ghost")],
            GraphArgs {
                roots: vec!["a".into()],
                depth: 3,
            },
        );
        let out = graph(&p).unwrap();
        assert_eq!(out.missing_refs, vec!["ghost"]);
        // The dangling target must NOT appear as a node...
        assert_eq!(out.nodes, vec!["a"]);
        assert!(!out.nodes.contains(&"ghost".to_string()));
        // ...nor as an emitted (internal) edge.
        assert!(
            out.edges.is_empty(),
            "dangling edge must not be emitted: {:?}",
            out.edges
        );
    }

    #[test]
    fn missing_root_warns_and_is_not_a_node() {
        let p = payload(
            &["a"],
            vec![],
            GraphArgs {
                roots: vec!["nope".into()],
                depth: 1,
            },
        );
        let out = graph(&p).unwrap();
        assert!(out.warnings.iter().any(|w| w.contains("nope")));
        assert!(
            out.nodes.is_empty(),
            "missing root must not be a node: {:?}",
            out.nodes
        );
    }
}
