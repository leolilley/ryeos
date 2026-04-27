use std::collections::{HashMap, HashSet};

use crate::model::{EdgeSpec, GraphConfig, GraphDefinition, GraphNode, NodeType};

#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn validate_graph(def: &GraphDefinition) -> ValidationResult {
    let mut result = ValidationResult {
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    let cfg = &def.config;

    // C3: config.start must exist and be non-empty
    if cfg.start.is_empty() {
        result
            .errors
            .push("config.start must be a non-empty string".into());
    } else if !cfg.nodes.contains_key(&cfg.start) {
        result
            .errors
            .push(format!("start node '{}' does not exist", cfg.start));
    }

    let has_return = cfg.nodes.values().any(|n| n.node_type == NodeType::Return);
    if !has_return {
        result
            .warnings
            .push("graph has no return node — will terminate on max_steps".into());
    }

    // C3: config.nodes must not be empty
    if cfg.nodes.is_empty() {
        result
            .errors
            .push("config.nodes is empty — at least one node is required".into());
    }

    for (name, node) in &cfg.nodes {
        validate_node(name, node, cfg, &mut result);
    }

    result
}

fn validate_node(name: &str, node: &GraphNode, cfg: &GraphConfig, result: &mut ValidationResult) {
    // R-D: nodes that declare neither `action` nor an explicit
    // `node_type` (other than default Action) MUST be rejected.
    // The parser defaults to NodeType::Action, so a bare `{name: {}}`
    // becomes an action node with no action — a no-op that masks
    // typos. Reject it.
    if node.node_type == NodeType::Action && node.action.is_none() && name != &cfg.start {
        result
            .errors
            .push(format!("node '{name}' has no 'action' and no explicit 'node_type' — this is ambiguous"));
    }

    match node.node_type {
        NodeType::Foreach => {
            if node.over.is_none() {
                result
                    .errors
                    .push(format!("foreach node '{name}' missing 'over' expression"));
            }
            if node.action.is_none() {
                result
                    .errors
                    .push(format!("foreach node '{name}' missing 'action'"));
            }
            // R-D: foreach nodes MUST declare `as` for the iteration
            // variable. Without it, the variable defaults to "item"
            // silently — a typo in the key goes unnoticed.
            if node.r#as.is_none() {
                result
                    .errors
                    .push(format!("foreach node '{name}' must declare 'as' for the iteration variable"));
            }
        }
        NodeType::Gate => {
            // R-D: gate nodes MUST declare `next` as a sequence of
            // conditions. An unconditional `next` on a gate node means
            // the gate always goes to the same place — that's an action
            // node, not a gate. Reject it.
            match &node.next {
                None | Some(EdgeSpec::Unconditional(_)) => {
                    result
                        .errors
                        .push(format!("gate node '{name}' must declare conditional 'next' (a sequence of conditions)"));
                }
                Some(EdgeSpec::Conditional(edges)) => {
                    if edges.is_empty() {
                        result
                            .errors
                            .push(format!("gate node '{name}' has empty conditional edges"));
                    }
                }
            }
        }
        NodeType::Return => {}
        NodeType::Action => {
            if node.action.is_none() && name != &cfg.start {
                // Already caught above — skip the redundant warning.
            }
        }
    }

    if let Some(ref next) = node.next {
        validate_edges(name, next, cfg, result);
    }

    if let Some(ref on_err) = node.on_error {
        if !cfg.nodes.contains_key(on_err) {
            result.errors.push(format!(
                "on_error target '{on_err}' in node '{name}' does not exist"
            ));
        }
    }
}

fn validate_edges(
    node_name: &str,
    edge: &EdgeSpec,
    cfg: &GraphConfig,
    result: &mut ValidationResult,
) {
    match edge {
        EdgeSpec::Unconditional(target) => {
            if !cfg.nodes.contains_key(target) {
                result.errors.push(format!(
                    "edge target '{target}' from node '{node_name}' does not exist"
                ));
            }
        }
        EdgeSpec::Conditional(edges) => {
            for ce in edges {
                if !cfg.nodes.contains_key(&ce.to) {
                    result.errors.push(format!(
                        "conditional edge target '{}' from node '{node_name}' does not exist",
                        ce.to
                    ));
                }
            }
        }
    }
}

pub fn analyze_graph(def: &GraphDefinition) -> ValidationResult {
    let mut result = validate_graph(def);
    let cfg = &def.config;

    let reachable = bfs_reachable(&cfg.start, &cfg.nodes);
    for name in cfg.nodes.keys() {
        if !reachable.contains(name) {
            result
                .warnings
                .push(format!("node '{name}' is unreachable from start"));
        }
    }

    let mut assigned_keys: HashSet<String> = HashSet::new();
    let mut referenced_keys: HashSet<String> = HashSet::new();

    for (name, node) in &cfg.nodes {
        if let Some(ref assign) = node.assign {
            collect_state_refs(assign, &mut referenced_keys);
            if let Value::Object(map) = assign {
                for k in map.keys() {
                    assigned_keys.insert(k.clone());
                }
            }
        }
        if let Some(ref over) = node.over {
            collect_path_refs(over, &mut referenced_keys);
        }
        if let Some(ref action) = node.action {
            collect_state_refs(action, &mut referenced_keys);
        }
        if let Some(ref next) = node.next {
            collect_edge_refs(next, &mut referenced_keys);
        }
        if let Some(ref collect_var) = node.collect {
            assigned_keys.insert(collect_var.clone());
        }
        let _ = name;
    }

    for key in &referenced_keys {
        if !assigned_keys.contains(key) {
            result
                .warnings
                .push(format!("state key '{key}' referenced but never assigned"));
        }
    }

    result
}

fn collect_state_refs(value: &serde_json::Value, refs: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(s) => collect_path_refs(s, refs),
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_state_refs(v, refs);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_state_refs(v, refs);
            }
        }
        _ => {}
    }
}

fn collect_path_refs(template: &str, refs: &mut HashSet<String>) {
    let re = regex::Regex::new(r"\$\{state\.([a-zA-Z_][a-zA-Z0-9_]*)").unwrap();
    for cap in re.captures_iter(template) {
        refs.insert(cap[1].to_string());
    }
}

fn collect_edge_refs(edge: &EdgeSpec, refs: &mut HashSet<String>) {
    match edge {
        EdgeSpec::Conditional(edges) => {
            for ce in edges {
                if let Some(ref when) = ce.when {
                    collect_state_refs(when, refs);
                }
            }
        }
        EdgeSpec::Unconditional(_) => {}
    }
}

fn bfs_reachable(start: &str, nodes: &HashMap<String, GraphNode>) -> HashSet<String> {
    let mut visited = HashSet::new();
    let mut queue = vec![start.to_string()];

    while let Some(current) = queue.pop() {
        if visited.contains(&current) {
            continue;
        }
        visited.insert(current.clone());

        if let Some(node) = nodes.get(&current) {
            if let Some(ref next) = node.next {
                let targets = edge_targets(next);
                for t in targets {
                    if !visited.contains(&t) {
                        queue.push(t);
                    }
                }
            }
            if let Some(ref on_err) = node.on_error {
                if !visited.contains(on_err) {
                    queue.push(on_err.clone());
                }
            }
        }
    }

    visited
}

pub fn edge_targets(edge: &EdgeSpec) -> Vec<String> {
    match edge {
        EdgeSpec::Unconditional(t) => vec![t.clone()],
        EdgeSpec::Conditional(edges) => edges.iter().map(|e| e.to.clone()).collect(),
    }
}

use serde_json::Value;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_graph(yaml: &str) -> GraphDefinition {
        GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap()
    }

    #[test]
    fn validate_graph_warns_unreachable_nodes() {
        let yaml = r#"
category: test
config:
  start: step1
  nodes:
    step1:
      next: done
    done:
      node_type: return
    orphan:
      node_type: action
      action: {item_id: "tool:test/echo"}
"#;
        let graph = make_graph(yaml);
        let result = analyze_graph(&graph);
        assert!(result.errors.is_empty());
        assert!(result.warnings.iter().any(|w| w.contains("orphan") && w.contains("unreachable")));
    }

    #[test]
    fn validate_graph_warns_missing_state_assignments() {
        let yaml = r#"
category: test
config:
  start: step1
  nodes:
    step1:
      assign: {out: "${state.undef_key}"}
      next: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let result = analyze_graph(&graph);
        assert!(result.warnings.iter().any(|w| w.contains("undef_key") && w.contains("never assigned")));
    }

    #[test]
    fn validate_graph_rejects_empty_nodes() {
        let yaml = r#"
category: test
config:
  start: step1
  nodes: {}
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(result.errors.iter().any(|e| e.contains("config.nodes is empty")));
    }

    #[test]
    fn validate_graph_rejects_missing_config_start_node() {
        let yaml = r#"
category: test
config:
  start: nonexistent
  nodes:
    step1:
      action: {item_id: "tool:test/echo"}
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(result.errors.iter().any(|e| e.contains("nonexistent") && e.contains("does not exist")));
    }

    // R-D: foreach nodes MUST declare `as`.

    #[test]
    fn validate_graph_rejects_foreach_missing_as() {
        let yaml = r#"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      action: {item_id: "tool:test/echo"}
      collect: "results"
      next: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(
            result.errors.iter().any(|e| e.contains("iterate") && e.contains("must declare 'as'")),
            "expected error for foreach without 'as', got: {:?}",
            result.errors
        );
    }

    // R-D: gate nodes MUST declare conditional `next`.

    #[test]
    fn validate_graph_rejects_gate_with_unconditional_next() {
        let yaml = r#"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      next: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(
            result.errors.iter().any(|e| e.contains("check") && e.contains("conditional")),
            "expected error for gate with unconditional next, got: {:?}",
            result.errors
        );
    }

    // R-D: nodes with no action and no explicit node_type must be rejected.

    #[test]
    fn validate_graph_rejects_node_with_no_action_or_type() {
        let yaml = r#"
category: test
config:
  start: step1
  nodes:
    step1:
      next: done
    done:
      node_type: return
    orphan:
      assign: {foo: bar}
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(
            result.errors.iter().any(|e| e.contains("orphan") && e.contains("ambiguous")),
            "expected error for node with no action/type, got: {:?}",
            result.errors
        );
    }
}
