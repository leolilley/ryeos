use std::collections::{BTreeMap, HashSet};

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_runtime::ReferenceSegment;
use serde_json::Value;

use crate::model::{EdgeSpec, GraphDefinition, GraphNode, NodeType};

#[derive(Debug, Clone)]
pub struct ValidationResult {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Runtime-side analysis is deliberately non-authoritative. Every structural,
/// expression, and capability rule has already run in
/// `ryeos-graph-definition::prepare_effective_graph` before launch authority
/// exists. This pass reports only observations that cannot reject the admitted
/// program.
pub fn validate_graph(def: &GraphDefinition) -> ValidationResult {
    let mut warnings = Vec::new();
    let cfg = &def.config;
    if !cfg
        .nodes
        .values()
        .any(|node| node.node_type == NodeType::Return)
    {
        warnings.push("graph has no return node — will terminate on max_steps".to_string());
    }
    for (name, node) in &cfg.nodes {
        if let Some(EdgeSpec::Conditional { branches }) = &node.next
            && !branches.is_empty()
            && branches.iter().all(|branch| !branch.when.is_absent())
        {
            warnings.push(format!(
                "conditional 'next' in node '{name}' has no default branch — \
                 if no condition matches, the graph terminates here"
            ));
        }
    }
    ValidationResult {
        errors: Vec::new(),
        warnings,
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
    let mut referenced_state: HashSet<String> = HashSet::new();
    if let Some(Value::Object(map)) = cfg.state.as_ref() {
        assigned_keys.extend(map.keys().cloned());
    }
    for node in cfg.nodes.values() {
        if let Some(Value::Object(map)) = node.assign.as_ref() {
            assigned_keys.extend(map.keys().cloned());
        }
        if let Some(collect_var) = &node.collect {
            assigned_keys.insert(collect_var.clone());
        }
    }

    // This analysis consumes the exact ASTs prepared by the kind compiler; it
    // never reparses templates or expressions.
    for reference in def.compiled.references() {
        let Some(ReferenceSegment::Key(key)) = reference.segments().first() else {
            continue;
        };
        if reference.root() == "state" {
            referenced_state.insert(key.clone());
        }
    }
    for key in &referenced_state {
        if !assigned_keys.contains(key) {
            result
                .warnings
                .push(format!("state key '{key}' referenced but never assigned"));
        }
    }
    result
}

/// Environment-dependent availability check for child lifecycles. This is
/// intentionally separate from graph-language admission because the set of
/// managed kinds belongs to the active node registry.
pub fn validate_managed_child_kinds(
    def: &GraphDefinition,
    managed_kinds: &HashSet<String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, node) in &def.config.nodes {
        let lifecycle = if node.follow {
            "follow"
        } else if node.detach {
            "detach"
        } else {
            continue;
        };
        let Some(action) = node.action.as_ref() else {
            continue;
        };
        let Some(item_id) = action.get("item_id").and_then(Value::as_str) else {
            errors.push(format!(
                "{lifecycle} node '{name}' must declare a literal canonical action.item_id \
                 so its managed runtime can be validated"
            ));
            continue;
        };
        let kind = if item_id.contains("${") {
            let Some((kind, bare_id)) = item_id.split_once(':') else {
                errors.push(format!(
                    "{lifecycle} node '{name}' action.item_id has a dynamic child kind — the \
                     managed runtime must be provable during graph validation"
                ));
                continue;
            };
            if kind.contains("${") || bare_id.is_empty() {
                errors.push(format!(
                    "{lifecycle} node '{name}' action.item_id has a dynamic or empty child kind — \
                     the managed runtime must be provable during graph validation"
                ));
                continue;
            }
            match CanonicalRef::parse(&format!("{kind}:managed-child")) {
                Ok(item_ref) => item_ref.kind,
                Err(error) => {
                    errors.push(format!(
                        "{lifecycle} node '{name}' action.item_id '{item_id}' has an invalid \
                         child kind: {error}"
                    ));
                    continue;
                }
            }
        } else {
            match CanonicalRef::parse(item_id) {
                Ok(item_ref) => item_ref.kind,
                Err(error) => {
                    errors.push(format!(
                        "{lifecycle} node '{name}' action.item_id '{item_id}' is not a canonical \
                         item ref: {error}"
                    ));
                    continue;
                }
            }
        };
        if !managed_kinds.contains(&kind) {
            errors.push(format!(
                "{lifecycle} node '{name}' targets child kind '{}' with no managed runtime — \
                 a {lifecycle} child must be a managed runtime execution",
                kind
            ));
        }
    }
    errors
}

fn bfs_reachable(start: &str, nodes: &BTreeMap<String, GraphNode>) -> HashSet<String> {
    let mut visited = HashSet::new();
    let mut queue = vec![start.to_string()];
    while let Some(current) = queue.pop() {
        if !visited.insert(current.clone()) {
            continue;
        }
        if let Some(node) = nodes.get(&current) {
            if let Some(next) = &node.next {
                for target in edge_targets(next) {
                    if !visited.contains(&target) {
                        queue.push(target);
                    }
                }
            }
            if let Some(on_error) = &node.on_error
                && !visited.contains(on_error)
            {
                queue.push(on_error.clone());
            }
        }
    }
    visited
}

pub fn edge_targets(edge: &EdgeSpec) -> Vec<String> {
    match edge {
        EdgeSpec::Unconditional { to } => vec![to.clone()],
        EdgeSpec::Conditional { branches } => {
            branches.iter().map(|branch| branch.to.clone()).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(yaml: &str) -> GraphDefinition {
        GraphDefinition::from_yaml(yaml, Some("analysis.yaml")).unwrap()
    }

    #[test]
    fn analysis_uses_admitted_ast_for_warnings_only() {
        let graph = graph(
            r#"
version: "1.0.0"
category: test
config:
  start: choose
  nodes:
    choose:
      node_type: gate
      next:
        type: conditional
        branches:
          - {when: "state.missing == true", to: done}
    dead: {node_type: return, output: dead}
    done: {node_type: return, output: done}
"#,
        );
        let result = analyze_graph(&graph);
        assert!(result.errors.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("no default"))
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("unreachable"))
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("never assigned"))
        );
    }

    #[test]
    fn managed_child_check_uses_the_active_registry() {
        let graph = graph(
            r#"
version: "1.0.0"
category: test
config:
  start: launch
  nodes:
    launch:
      action: {item_id: "directive:test/${inputs.child}"}
      follow: true
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#,
        );
        assert_eq!(
            validate_managed_child_kinds(&graph, &HashSet::new()).len(),
            1
        );
        assert!(
            validate_managed_child_kinds(&graph, &HashSet::from(["directive".to_string()]))
                .is_empty()
        );
    }
}
