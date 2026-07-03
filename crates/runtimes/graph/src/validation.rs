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

    // A `hooks:` block is parsed but not yet executed by the walker. Reject a
    // non-empty list loudly so an author never believes an authored hook is
    // firing when nothing reads it.
    if cfg.hooks.as_ref().is_some_and(|h| !h.is_empty()) {
        result
            .errors
            .push("graph hooks are not yet wired; remove the block".into());
    }

    // Bundle-event / vault capabilities are runtime authority: a signed manifest
    // mints them; a graph's `permissions:` cannot. Surface the SAME refusals the
    // daemon applies at cap-assembly time — both well-formed reserved grants AND
    // malformed/partial-wildcard forms (e.g. `ryeos.p*.vault.*`) — so the author
    // gets the feedback here, not only at launch. The daemon enforces this
    // authoritatively regardless (this is UX).
    for grant in &def.declared_permissions {
        if let Err(err) = ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(
            std::slice::from_ref(grant),
        ) {
            result
                .errors
                .push(format!("graph permission rejected: {err}"));
        }
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
    if node.node_type == NodeType::Action && node.action.is_none() && name != cfg.start {
        result.errors.push(format!(
            "node '{name}' has no 'action' and no explicit 'node_type' — this is ambiguous"
        ));
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
                result.errors.push(format!(
                    "foreach node '{name}' must declare 'as' for the iteration variable"
                ));
            }
            // `collect` and `as` must differ: the walker writes the
            // collected results under `collect`, then removes the
            // iteration variable `as` from state — if they share a name
            // the collected results are removed too (silent data loss).
            if let (Some(collect), Some(as_var)) = (&node.collect, &node.r#as) {
                if collect == as_var {
                    result.errors.push(format!(
                        "foreach node '{name}' uses '{collect}' for both 'collect' and 'as' — \
                         the collected results would be removed with the iteration variable"
                    ));
                }
            }
        }
        NodeType::Gate => {
            // R-D: gate nodes MUST declare `next` as a sequence of
            // conditions. An unconditional `next` on a gate node means
            // the gate always goes to the same place — that's an action
            // node, not a gate. Reject it.
            match &node.next {
                None | Some(EdgeSpec::Unconditional { .. }) => {
                    result
                        .errors
                        .push(format!("gate node '{name}' must declare conditional 'next' (a sequence of conditions)"));
                }
                Some(EdgeSpec::Conditional { branches: edges }) => {
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
            if node.action.is_none() && name != cfg.start {
                // Already caught above — skip the redundant warning.
            }
        }
    }

    // A follow node suspends the graph and awaits a detached child. It is only
    // meaningful on a single action dispatch, and its result does not exist at
    // suspend time — so reject it on non-action, parallel, and cacheable nodes.
    if node.follow {
        if node.node_type != NodeType::Action {
            result.errors.push(format!(
                "node '{name}' sets 'follow' but is a {:?} node — only action nodes can follow",
                node.node_type
            ));
        }
        if node.parallel {
            result
                .errors
                .push(format!("node '{name}' cannot be both 'follow' and 'parallel'"));
        }
        if node.is_cacheable() {
            result.errors.push(format!(
                "node '{name}' cannot be both 'follow' and cacheable — the child result does \
                 not exist at suspend time"
            ));
        }
        if node.action.is_none() {
            result
                .errors
                .push(format!("follow node '{name}' has no 'action' to launch"));
        }
    }

    // Per-step retry is only meaningful for a dispatching node (a single
    // action or a foreach's per-item dispatch). It is a loud error to combine
    // it with `follow` in v1: retrying a follow means minting a fresh
    // follow_key per attempt plus a waiter-lifecycle design that does not exist
    // yet — route a failed follow with `on_error` instead.
    if let Some(retry) = &node.retry {
        if !matches!(node.node_type, NodeType::Action | NodeType::Foreach) {
            result.errors.push(format!(
                "node '{name}' declares 'retry' but is a {:?} node — retry is only valid on \
                 action nodes",
                node.node_type
            ));
        }
        if node.follow {
            result.errors.push(format!(
                "node '{name}' cannot combine 'retry' and 'follow' — retrying a follow needs a \
                 fresh follow_key and waiter lifecycle per attempt (excluded in v1); route a \
                 failed follow with 'on_error' instead"
            ));
        }
        if !(1..=10).contains(&retry.attempts) {
            result.errors.push(format!(
                "node '{name}' retry.attempts must be between 1 and 10 (got {})",
                retry.attempts
            ));
        }
        if retry.backoff_ms == 0 {
            result
                .errors
                .push(format!("node '{name}' retry.backoff_ms must be greater than 0"));
        }
        if let Some(cap) = retry.max_backoff_ms {
            if cap < retry.backoff_ms {
                result.errors.push(format!(
                    "node '{name}' retry.max_backoff_ms ({cap}) must be >= retry.backoff_ms ({})",
                    retry.backoff_ms
                ));
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
        EdgeSpec::Unconditional { to: target } => {
            if !cfg.nodes.contains_key(target) {
                result.errors.push(format!(
                    "edge target '{target}' from node '{node_name}' does not exist"
                ));
            }
        }
        EdgeSpec::Conditional { branches: edges } => {
            for ce in edges {
                if !cfg.nodes.contains_key(&ce.to) {
                    result.errors.push(format!(
                        "conditional edge target '{}' from node '{node_name}' does not exist",
                        ce.to
                    ));
                }
            }
            // A conditional `next` with no default branch (an edge with
            // no `when`) terminates the graph when no condition matches.
            // That dead-end is usually a mistake — warn so authors add a
            // fallback if they didn't intend it.
            if !edges.is_empty() && edges.iter().all(|ce| ce.when.is_some()) {
                result.warnings.push(format!(
                    "conditional 'next' in node '{node_name}' has no default branch — \
                     if no condition matches, the graph terminates here"
                ));
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
    let mut referenced_state: HashSet<String> = HashSet::new();
    let mut referenced_inputs: HashSet<String> = HashSet::new();

    for node in cfg.nodes.values() {
        for field in [
            node.assign.as_ref(),
            node.action.as_ref(),
            node.output.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            collect_refs(field, "state", &mut referenced_state);
            collect_refs(field, "inputs", &mut referenced_inputs);
        }
        if let Some(ref over) = node.over {
            collect_path_refs(over, "state", &mut referenced_state);
            collect_path_refs(over, "inputs", &mut referenced_inputs);
        }
        if let Some(ref next) = node.next {
            collect_edge_refs(next, "state", &mut referenced_state);
            collect_edge_refs(next, "inputs", &mut referenced_inputs);
        }
        if let Some(Value::Object(map)) = node.assign.as_ref() {
            for k in map.keys() {
                assigned_keys.insert(k.clone());
            }
        }
        if let Some(ref collect_var) = node.collect {
            assigned_keys.insert(collect_var.clone());
        }
    }

    for key in &referenced_state {
        if !assigned_keys.contains(key) {
            result
                .warnings
                .push(format!("state key '{key}' referenced but never assigned"));
        }
    }

    // `${inputs.*}` references must be declared in the graph's
    // `config_schema.properties` (when a schema is declared) — otherwise
    // the input is undocumented and unvalidated.
    if let Some(props) = cfg
        .config_schema
        .as_ref()
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
    {
        for key in &referenced_inputs {
            if !props.contains_key(key) {
                result.warnings.push(format!(
                    "input referenced as ${{inputs.{key}}} but '{key}' is not declared in config.config_schema.properties"
                ));
            }
        }
    }

    result
}

/// Collect `${<prefix>.KEY}` references from any JSON value (recursing
/// through objects and arrays). `prefix` is `state` or `inputs`.
fn collect_refs(value: &serde_json::Value, prefix: &str, refs: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(s) => collect_path_refs(s, prefix, refs),
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_refs(v, prefix, refs);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_refs(v, prefix, refs);
            }
        }
        _ => {}
    }
}

fn collect_path_refs(template: &str, prefix: &str, refs: &mut HashSet<String>) {
    let re = regex::Regex::new(&format!(
        r"\$\{{{}\.([a-zA-Z_][a-zA-Z0-9_]*)",
        regex::escape(prefix)
    ))
    .unwrap();
    for cap in re.captures_iter(template) {
        refs.insert(cap[1].to_string());
    }
}

fn collect_edge_refs(edge: &EdgeSpec, prefix: &str, refs: &mut HashSet<String>) {
    match edge {
        EdgeSpec::Conditional { branches: edges } => {
            for ce in edges {
                if let Some(ref when) = ce.when {
                    collect_refs(when, prefix, refs);
                }
            }
        }
        EdgeSpec::Unconditional { .. } => {}
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
        EdgeSpec::Unconditional { to: t } => vec![t.clone()],
        EdgeSpec::Conditional { branches: edges } => edges.iter().map(|e| e.to.clone()).collect(),
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
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      next:
        type: unconditional
        to: done
    done:
      node_type: return
    orphan:
      node_type: action
      action: {item_id: "tool:test/echo"}
"#;
        let graph = make_graph(yaml);
        let result = analyze_graph(&graph);
        assert!(result.errors.is_empty());
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("orphan") && w.contains("unreachable")));
    }

    #[test]
    fn validate_graph_warns_missing_state_assignments() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      assign: {out: "${state.undef_key}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let result = analyze_graph(&graph);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("undef_key") && w.contains("never assigned")));
    }

    #[test]
    fn validate_graph_rejects_empty_nodes() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes: {}
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("config.nodes is empty")));
    }

    #[test]
    fn validate_graph_rejects_non_empty_hooks_block() {
        // Authored `hooks:` are not executed by the walker; a non-empty list
        // is rejected rather than silently ignored.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - id: obs
      event: graph_completed
      action: {item_id: "tool:test/echo"}
  nodes:
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("hooks are not yet wired")),
            "expected non-empty hooks rejection, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_allows_absent_hooks_block() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.contains("hooks are not yet wired")),
            "absent hooks must not trip the rejection: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_missing_config_start_node() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: nonexistent
  nodes:
    step1:
      action: {item_id: "tool:test/echo"}
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("nonexistent") && e.contains("does not exist")));
    }

    // R-D: foreach nodes MUST declare `as`.

    #[test]
    fn validate_graph_rejects_foreach_missing_as() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      action: {item_id: "tool:test/echo"}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("iterate") && e.contains("must declare 'as'")),
            "expected error for foreach without 'as', got: {:?}",
            result.errors
        );
    }

    // R-D: gate nodes MUST declare conditional `next`.

    #[test]
    fn validate_graph_rejects_gate_with_unconditional_next() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("check") && e.contains("conditional")),
            "expected error for gate with unconditional next, got: {:?}",
            result.errors
        );
    }

    // R-D: nodes with no action and no explicit node_type must be rejected.

    #[test]
    fn validate_graph_rejects_foreach_collect_equals_as() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "results"
      collect: "results"
      action: {item_id: "tool:test/echo"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("iterate") && e.contains("both 'collect' and 'as'")),
            "expected collect==as error, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn analyze_graph_warns_conditional_without_default() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      next:
        type: conditional
        branches:
          - when: {path: state.mode, op: eq, value: fast}
            to: done
    done:
      node_type: return
"#;
        let result = analyze_graph(&make_graph(yaml));
        assert!(
            result.errors.is_empty(),
            "unexpected errors: {:?}",
            result.errors
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("check") && w.contains("no default branch")),
            "expected no-default warning, got: {:?}",
            result.warnings
        );
    }

    #[test]
    fn analyze_graph_no_default_warning_when_default_present() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      next:
        type: conditional
        branches:
          - when: {path: state.mode, op: eq, value: fast}
            to: done
          - to: done
    done:
      node_type: return
"#;
        let result = analyze_graph(&make_graph(yaml));
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.contains("no default branch")),
            "default branch present — should not warn: {:?}",
            result.warnings
        );
    }

    #[test]
    fn analyze_graph_warns_undeclared_input() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  config_schema:
    type: object
    properties:
      declared: {type: string}
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {a: "${inputs.declared}", b: "${inputs.missing}"}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let result = analyze_graph(&make_graph(yaml));
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("missing") && w.contains("config_schema")),
            "expected undeclared-input warning for 'missing', got: {:?}",
            result.warnings
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.contains("inputs.declared")),
            "declared input must not warn: {:?}",
            result.warnings
        );
    }

    #[test]
    fn analyze_graph_no_input_warning_without_config_schema() {
        // No config_schema → can't know declared inputs → don't warn.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {a: "${inputs.anything}"}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let result = analyze_graph(&make_graph(yaml));
        assert!(
            !result.warnings.iter().any(|w| w.contains("config_schema")),
            "no schema → no input warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn analyze_graph_scans_return_output_for_undeclared_input() {
        // `${inputs.*}` in a return node's `output` is also checked.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  config_schema:
    type: object
    properties:
      known: {type: string}
  nodes:
    done:
      node_type: return
      output:
        id: "${inputs.unknown}"
"#;
        let result = analyze_graph(&make_graph(yaml));
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("unknown") && w.contains("config_schema")),
            "expected output ${{inputs.unknown}} to warn, got: {:?}",
            result.warnings
        );
    }

    #[test]
    fn validate_graph_rejects_node_with_no_action_or_type() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      next:
        type: unconditional
        to: done
    done:
      node_type: return
    orphan:
      assign: {foo: bar}
"#;
        let graph = make_graph(yaml);
        let result = validate_graph(&graph);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("orphan") && e.contains("ambiguous")),
            "expected error for node with no action/type, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_follow_on_non_action() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:x"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
      follow: true
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("follow") && e.contains("only action nodes")),
            "expected non-action follow rejection, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_cacheable_follow() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:x"}
      follow: true
      cache: true
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("follow") && e.contains("cacheable")),
            "expected cacheable-follow rejection, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_parallel_follow() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:x"}
      follow: true
      parallel: true
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("follow") && e.contains("parallel")),
            "expected parallel-follow rejection, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_follow_without_action() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      node_type: action
      follow: true
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("follow node") && e.contains("no 'action'")),
            "expected follow-without-action rejection, got: {:?}",
            result.errors
        );
    }

    // ── §A per-step retry validation ─────────────────────────────────

    fn retry_graph(node_body: &str) -> GraphDefinition {
        let yaml = format!(
            r#"
version: "1.0.0"
category: test
config:
  start: n
  nodes:
    n:
{node_body}
    done:
      node_type: return
"#
        );
        make_graph(&yaml)
    }

    #[test]
    fn validate_graph_accepts_valid_retry_on_action() {
        let result = validate_graph(&retry_graph(
            "      action: {item_id: \"tool:x\"}\n      retry: {attempts: 3, backoff_ms: 1000, max_backoff_ms: 30000}\n      next: {type: unconditional, to: done}",
        ));
        assert!(
            result.errors.is_empty(),
            "valid retry must not error: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_retry_attempts_out_of_range() {
        let too_many = validate_graph(&retry_graph(
            "      action: {item_id: \"tool:x\"}\n      retry: {attempts: 11, backoff_ms: 100}\n      next: {type: unconditional, to: done}",
        ));
        assert!(
            too_many
                .errors
                .iter()
                .any(|e| e.contains("retry.attempts must be between 1 and 10")),
            "expected attempts range rejection, got: {:?}",
            too_many.errors
        );
        let zero = validate_graph(&retry_graph(
            "      action: {item_id: \"tool:x\"}\n      retry: {attempts: 0, backoff_ms: 100}\n      next: {type: unconditional, to: done}",
        ));
        assert!(zero
            .errors
            .iter()
            .any(|e| e.contains("retry.attempts must be between 1 and 10")));
    }

    #[test]
    fn validate_graph_rejects_retry_zero_backoff() {
        let result = validate_graph(&retry_graph(
            "      action: {item_id: \"tool:x\"}\n      retry: {attempts: 3, backoff_ms: 0}\n      next: {type: unconditional, to: done}",
        ));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("retry.backoff_ms must be greater than 0")),
            "expected backoff>0 rejection, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_retry_max_below_backoff() {
        let result = validate_graph(&retry_graph(
            "      action: {item_id: \"tool:x\"}\n      retry: {attempts: 3, backoff_ms: 1000, max_backoff_ms: 500}\n      next: {type: unconditional, to: done}",
        ));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("retry.max_backoff_ms") && e.contains(">= retry.backoff_ms")),
            "expected max<backoff rejection, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_retry_on_gate_node() {
        let result = validate_graph(&retry_graph(
            "      node_type: gate\n      retry: {attempts: 3, backoff_ms: 100}\n      next: {type: conditional, branches: [{to: done}]}",
        ));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("retry is only valid on")),
            "expected non-action retry rejection, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_retry_plus_follow() {
        let result = validate_graph(&retry_graph(
            "      action: {item_id: \"tool:x\"}\n      follow: true\n      retry: {attempts: 3, backoff_ms: 100}\n      next: {type: unconditional, to: done}",
        ));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("cannot combine 'retry' and 'follow'")),
            "expected retry+follow rejection, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_accepts_retry_on_foreach() {
        let result = validate_graph(&retry_graph(
            "      node_type: foreach\n      over: \"${state.items}\"\n      as: item\n      action: {item_id: \"tool:x\"}\n      retry: {attempts: 2, backoff_ms: 50}\n      next: {type: unconditional, to: done}",
        ));
        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.contains("retry is only valid on")),
            "foreach is a valid retry target: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_runtime_authority_permission() {
        // A graph cannot self-grant bundle-event/vault runtime authority via
        // `declared` — that authority comes only from a signed manifest.
        let yaml = r#"
version: "1.0.0"
category: test
requires:
  capabilities:
    declared:
      - ryeos.append.bundle-events.foo/evt
      - ryeos.execute.tool.echo
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("bundle-events.foo/evt") && e.contains("reserved")),
            "expected runtime-authority reject, got: {:?}",
            result.errors
        );
        // The ordinary execute permission must NOT be flagged.
        assert!(
            !result.errors.iter().any(|e| e.contains("tool.echo")),
            "ordinary execute permission must not be flagged: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_wildcard_runtime_authority_permission() {
        // A wildcard that intrudes on the runtime-authority namespace is flagged
        // too, not only an exact cap.
        let yaml = r#"
version: "1.0.0"
category: test
requires:
  capabilities:
    declared:
      - ryeos.scan.bundle-events.*
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("scan.bundle-events.*") && e.contains("reserved")),
            "expected wildcard intrusion to be rejected, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_malformed_partial_wildcard_permission() {
        // A partial-wildcard grant the scope grammar forbids (`ryeos.p*.vault.*`)
        // must be flagged here too — the same refusal launch applies — not only
        // at cap assembly.
        let yaml = r#"
version: "1.0.0"
category: test
requires:
  capabilities:
    declared:
      - ryeos.p*.vault.*
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("ryeos.p*.vault.*") && e.contains("not a canonical")),
            "expected malformed grant to be rejected, got: {:?}",
            result.errors
        );
    }
}
