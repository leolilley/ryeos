use std::collections::{HashMap, HashSet};

use ryeos_runtime::ReferenceSegment;
use serde_json::Value;

use crate::model::{
    EdgeSpec, GraphConfig, GraphDefinition, GraphNode, NodeType, MAX_GRAPH_SEGMENT_STEPS,
    MAX_GRAPH_STEPS, MAX_RETRY_BACKOFF_MS,
};

const MAX_NODE_CONCURRENCY: usize = 256;

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

    if !(1..=MAX_GRAPH_STEPS).contains(&cfg.max_steps) {
        result.errors.push(format!(
            "config.max_steps must be between 1 and {MAX_GRAPH_STEPS} (got {})",
            cfg.max_steps
        ));
    }
    if let Some(segment_steps) = cfg.segment_steps {
        if !(1..=MAX_GRAPH_SEGMENT_STEPS).contains(&segment_steps) {
            result.errors.push(format!(
                "config.segment_steps must be between 1 and {MAX_GRAPH_SEGMENT_STEPS} \
                 (got {segment_steps})"
            ));
        }
        if segment_steps > cfg.max_steps {
            result.errors.push(format!(
                "config.segment_steps ({segment_steps}) must not exceed config.max_steps ({})",
                cfg.max_steps
            ));
        }
    }

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
    let assign_keys = node
        .assign
        .as_ref()
        .and_then(Value::as_object)
        .map(|assign| assign.keys().map(String::as_str).collect::<HashSet<_>>())
        .unwrap_or_default();
    let follow_fanout = node.node_type == NodeType::Action && node.follow && node.over.is_some();

    if node.parallel && node.assign.is_some() && !follow_fanout {
        result.errors.push(format!(
            "node '{name}' cannot combine 'parallel: true' with 'assign'; \
             collect ordered results and derive aggregate state in a later node"
        ));
    }

    if node.assign.is_some() && !matches!(node.node_type, NodeType::Action | NodeType::Foreach) {
        result.errors.push(format!(
            "node '{name}' declares 'assign' on a {:?} node; assignment is only valid on action and foreach nodes",
            node.node_type
        ));
    }
    if node.output.is_some() && node.node_type != NodeType::Return {
        result.errors.push(format!(
            "node '{name}' declares 'output' but is not a return node"
        ));
    }
    if node.action.is_some() && matches!(node.node_type, NodeType::Gate | NodeType::Return) {
        result.errors.push(format!(
            "node '{name}' declares an action on a {:?} node",
            node.node_type
        ));
    }
    let iterates = node.node_type == NodeType::Foreach || follow_fanout;
    if !iterates {
        for (field, present) in [
            ("over", node.over.is_some()),
            ("as", node.r#as.is_some()),
            ("collect", node.collect.is_some()),
            ("parallel", node.parallel),
        ] {
            if present {
                result.errors.push(format!(
                    "node '{name}' declares '{field}' but is not a foreach or follow-fanout node"
                ));
            }
        }
    }

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
            if node.is_cacheable() {
                result.errors.push(format!(
                    "foreach node '{name}' cannot declare 'cache_result'; foreach \
                     dispatches and aggregation are not cacheable as one action"
                ));
            }
            if !node.env_requires.is_empty() {
                result.errors.push(format!(
                    "foreach node '{name}' cannot declare 'env_requires'; per-node environment \
                     preflight is only supported on action nodes"
                ));
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
            if let Some(as_var) = &node.r#as {
                if assign_keys.contains(as_var.as_str()) {
                    result.errors.push(format!(
                        "foreach node '{name}' uses '{as_var}' as both its iteration variable and an assign key"
                    ));
                }
            }
            if let Some(collect) = &node.collect {
                if assign_keys.contains(collect.as_str()) {
                    result.errors.push(format!(
                        "foreach node '{name}' uses '{collect}' as both its collect key and an assign key"
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
        NodeType::Return => {
            if node.next.is_some() {
                result.errors.push(format!(
                    "return node '{name}' cannot declare 'next'; return is terminal"
                ));
            }
        }
        NodeType::Action => {
            if node.action.is_none() && name != cfg.start {
                // Already caught above — skip the redundant warning.
            }
        }
    }

    // A follow node suspends the graph and awaits a detached child. It is only
    // meaningful on an action dispatch, and its result does not exist at
    // suspend time. `over` opts an action into the cohort-follow state machine.
    if node.follow {
        if node.node_type != NodeType::Action {
            result.errors.push(format!(
                "node '{name}' sets 'follow' but is a {:?} node — only action nodes can follow",
                node.node_type
            ));
        }
        if node.parallel && node.over.is_none() {
            result.errors.push(format!(
                "node '{name}' cannot be both 'follow' and 'parallel'"
            ));
        }
        if node.over.is_some() {
            if node.assign.is_some() {
                result.errors.push(format!(
                    "follow fanout node '{name}' cannot declare 'assign'; collect ordered results \
                     and derive aggregate state in a later node"
                ));
            }
            if node.r#as.is_none() {
                result.errors.push(format!(
                    "follow fanout node '{name}' must declare 'as' for the iteration variable"
                ));
            }
            if !node.parallel {
                result.errors.push(format!(
                    "follow fanout node '{name}' must set 'parallel: true'"
                ));
            }
            if let (Some(collect), Some(as_var)) = (&node.collect, &node.r#as) {
                if collect == as_var {
                    result.errors.push(format!(
                        "follow fanout node '{name}' uses '{collect}' for both 'collect' and 'as'"
                    ));
                }
            }
            if let Some(as_var) = &node.r#as {
                if assign_keys.contains(as_var.as_str()) {
                    result.errors.push(format!(
                        "follow fanout node '{name}' uses '{as_var}' as both its iteration variable and an assign key"
                    ));
                }
            }
            if let Some(collect) = &node.collect {
                if assign_keys.contains(collect.as_str()) {
                    result.errors.push(format!(
                        "follow fanout node '{name}' uses '{collect}' as both its collect key and an assign key"
                    ));
                }
            }
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

    // A `detach: true` node launches a lineage-linked, cohort-tagged child that
    // runs concurrently — the native fanout primitive (`foreach → launch`). Like
    // follow it dispatches (needs an action) and has no result at launch time (so
    // it cannot cache), but unlike follow the parent never suspends — so it is
    // valid on a foreach's per-item dispatch, not only a single action. `detach`
    // and `follow` are mutually exclusive: a node either suspends for its child
    // (follow) or walks on past it (detach), never both.
    if node.detach {
        if !matches!(node.node_type, NodeType::Action | NodeType::Foreach) {
            result.errors.push(format!(
                "node '{name}' sets 'detach' but is a {:?} node — only action and foreach nodes \
                 can launch a detached child",
                node.node_type
            ));
        }
        if node.follow {
            result.errors.push(format!(
                "node '{name}' cannot be both 'detach' and 'follow'"
            ));
        }
        if node.is_cacheable() {
            result.errors.push(format!(
                "node '{name}' cannot be both 'detach' and cacheable — a detached launch has no \
                 result at dispatch time"
            ));
        }
        if node.action.is_none() {
            result
                .errors
                .push(format!("detach node '{name}' has no 'action' to launch"));
        }
    }

    // Cohort `facets:` are stamped by the daemon only on a detached child launch;
    // on any other node they are silently inert, so reject them as a loud
    // authoring error rather than let a mistagged cohort pass unnoticed.
    if node.facets.is_some() && !(node.detach || (node.follow && node.over.is_some())) {
        result.errors.push(format!(
            "node '{name}' declares 'facets' without 'detach' or follow fanout — cohort facets are only stamped \
             on a detached child launch or follow fanout child launch"
        ));
    }

    if let Some(width) = node.max_concurrency {
        if width == 0 || width > MAX_NODE_CONCURRENCY {
            result.errors.push(format!(
                "node '{name}' max_concurrency must be between 1 and {MAX_NODE_CONCURRENCY}"
            ));
        }
        let used_by_parallel_foreach = node.node_type == NodeType::Foreach && node.parallel;
        let used_by_detach_foreach = node.node_type == NodeType::Foreach && node.detach;
        let used_by_follow_fanout = follow_fanout;
        if !(used_by_parallel_foreach || used_by_detach_foreach || used_by_follow_fanout) {
            result.errors.push(format!(
                "node '{name}' declares 'max_concurrency' where it has no effect; it is only \
                 valid on parallel foreach, detach foreach launch windows, and follow fanout"
            ));
        }
    }

    // Per-step retry is only meaningful for a dispatching node (a single
    // action or a foreach's per-item dispatch). It is a loud error to combine
    // it with `follow`: retrying a follow means minting a fresh
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
                 fresh follow_key and waiter lifecycle per attempt; route a \
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
            result.errors.push(format!(
                "node '{name}' retry.backoff_ms must be greater than 0"
            ));
        }
        if retry.backoff_ms > MAX_RETRY_BACKOFF_MS {
            result.errors.push(format!(
                "node '{name}' retry.backoff_ms ({}) exceeds the runtime maximum ({MAX_RETRY_BACKOFF_MS})",
                retry.backoff_ms
            ));
        }
        if let Some(cap) = retry.max_backoff_ms {
            if cap < retry.backoff_ms {
                result.errors.push(format!(
                    "node '{name}' retry.max_backoff_ms ({cap}) must be >= retry.backoff_ms ({})",
                    retry.backoff_ms
                ));
            }
            if cap > MAX_RETRY_BACKOFF_MS {
                result.errors.push(format!(
                    "node '{name}' retry.max_backoff_ms ({cap}) exceeds the runtime maximum ({MAX_RETRY_BACKOFF_MS})"
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
            if !edges.is_empty() && edges.iter().all(|ce| !ce.when.is_absent()) {
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
        if let Some(Value::Object(map)) = node.assign.as_ref() {
            for k in map.keys() {
                assigned_keys.insert(k.clone());
            }
        }
        if let Some(ref collect_var) = node.collect {
            assigned_keys.insert(collect_var.clone());
        }
    }

    // Reference analysis comes from the same ASTs execution uses. This sees
    // arithmetic, functions, ternaries, bracket access, and nested templates
    // without a second regex grammar drifting away from rye-expr/1.
    for reference in def.compiled.references() {
        let Some(ReferenceSegment::Key(key)) = reference.segments().first() else {
            continue;
        };
        match reference.root() {
            "state" => {
                referenced_state.insert(key.clone());
            }
            "inputs" => {
                referenced_inputs.insert(key.clone());
            }
            _ => {}
        }
    }

    for key in &referenced_state {
        if !assigned_keys.contains(key) {
            result
                .warnings
                .push(format!("state key '{key}' referenced but never assigned"));
        }
    }

    // `CompiledGraph::compile` rejects undeclared static `inputs.*`
    // references whenever config_schema.properties is present. Keep this set
    // for analysis parity and future reporting, but do not downgrade a load
    // error into the former warning-only contract.
    let _ = referenced_inputs;

    result
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
      action: {item_id: "tool:test/echo", ref_bindings: {}}
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
    fn graph_load_rejects_inert_top_level_max_concurrency() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  max_concurrency: 4
  nodes:
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("max_concurrency"),
            "the removed top-level field must fail strict parsing: {error:#}"
        );
    }

    #[test]
    fn validate_graph_rejects_out_of_range_step_budgets() {
        let base = r#"
version: "1.0.0"
category: test
config:
  start: done
__LIMITS__
  nodes:
    done: {node_type: return}
"#;
        let cases = [
            (
                "  max_steps: 0",
                "config.max_steps must be between 1 and 500",
            ),
            (
                "  max_steps: 501",
                "config.max_steps must be between 1 and 500",
            ),
            (
                "  segment_steps: 0",
                "config.segment_steps must be between 1 and 500",
            ),
            (
                "  max_steps: 500\n  segment_steps: 501",
                "config.segment_steps must be between 1 and 500",
            ),
            (
                "  max_steps: 10\n  segment_steps: 11",
                "config.segment_steps (11) must not exceed config.max_steps (10)",
            ),
        ];

        for (limits, expected) in cases {
            let result = validate_graph(&make_graph(&base.replace("__LIMITS__", limits)));
            assert!(
                result.errors.iter().any(|error| error.contains(expected)),
                "missing {expected:?} for {limits:?}: {:?}",
                result.errors
            );
        }
    }

    #[test]
    fn validate_graph_accepts_step_budget_boundaries() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  max_steps: 500
  segment_steps: 500
  nodes:
    done: {node_type: return}
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result.errors.is_empty(),
            "unexpected errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_accepts_typed_hooks_on_graph_event() {
        // A well-formed hook targeting a real graph event parses and validates
        // cleanly (no error, no unrecognized-event warning).
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - id: obs
      event: graph_completed
      action: {item_id: "tool:test/echo", ref_bindings: {}, params: {}}
  nodes:
    done:
      node_type: return
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result.errors.is_empty(),
            "typed hook must validate: {:?}",
            result.errors
        );
        assert!(
            !result.warnings.iter().any(|w| w.contains("never fire")),
            "a graph_completed hook must not warn: {:?}",
            result.warnings
        );
    }

    #[test]
    fn graph_load_rejects_hook_on_unrecognized_event() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - id: typo
      event: graph_finishd
      action: {item_id: "tool:test/echo", ref_bindings: {}}
  nodes:
    done:
      node_type: return
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("event has no HookContextSchema"),
            "expected an unknown hook event compilation error, got: {error:#}"
        );
    }

    #[test]
    fn graph_load_rejects_hook_root_outside_event_schema() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - id: invalid-root
      event: graph_started
      action:
        item_id: "tool:test/echo"
        params: {value: "${result.value}"}
  nodes:
    done:
      node_type: return
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("references undeclared root `result`"),
            "expected the graph_started schema to reject result, got: {error:#}"
        );
    }

    #[test]
    fn validate_graph_rejects_hook_with_unknown_field() {
        // deny_unknown_fields on HookDefinition: the killed untyped form (a hook
        // carrying arbitrary keys the walker silently ignored) fails the parse.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - id: bad
      event: graph_completed
      when: something
      action: {item_id: "tool:test/echo", ref_bindings: {}}
  nodes:
    done:
      node_type: return
"#;
        assert!(
            GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err(),
            "an untyped hook field must fail the strict parse"
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
      action: {item_id: "tool:test/echo", ref_bindings: {}}
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
      action: {item_id: "tool:test/echo", ref_bindings: {}}
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

    #[test]
    fn validate_graph_rejects_foreach_cache_fields() {
        let base = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: item
      action: {item_id: "tool:test/echo"}
__CACHE__
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#;
        let field = "      cache_result: true";
        let result = validate_graph(&make_graph(&base.replace("__CACHE__", field)));
        assert!(
            result.errors.iter().any(|error| {
                error.contains("foreach node 'iterate'") && error.contains("not cacheable")
            }),
            "{field} must be rejected on foreach: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_inert_foreach_env_requires() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: item
      env_requires: [EXAMPLE_TOKEN]
      action: {item_id: "tool:test/echo"}
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result.errors.iter().any(|error| {
                error.contains("foreach node 'iterate'") && error.contains("env_requires")
            }),
            "inert foreach env_requires must be rejected: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_next_on_return() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      next: {type: unconditional, to: unreachable}
    unreachable: {node_type: return}
"#;
        let result = validate_graph(&make_graph(yaml));
        assert!(
            result.errors.iter().any(|error| {
                error.contains("return node 'done'") && error.contains("cannot declare 'next'")
            }),
            "terminal return next must be rejected: {:?}",
            result.errors
        );
    }

    #[test]
    fn validate_graph_rejects_max_concurrency_without_a_consumer() {
        let base = r#"
version: "1.0.0"
category: test
config:
  start: work
  nodes:
    work:
__NODE__
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#;
        let cases = [
            concat!(
                "      node_type: action\n",
                "      action: {item_id: \"tool:test/echo\"}\n",
                "      max_concurrency: 2"
            ),
            concat!(
                "      node_type: action\n",
                "      action: {item_id: \"directive:test/child\"}\n",
                "      follow: true\n",
                "      max_concurrency: 2"
            ),
            concat!(
                "      node_type: foreach\n",
                "      over: \"${state.items}\"\n",
                "      as: item\n",
                "      action: {item_id: \"tool:test/echo\"}\n",
                "      max_concurrency: 2"
            ),
        ];
        for node in cases {
            let result = validate_graph(&make_graph(&base.replace("__NODE__", node)));
            assert!(
                result.errors.iter().any(|error| {
                    error.contains("max_concurrency") && error.contains("where it has no effect")
                }),
                "unused max_concurrency must be rejected for {node:?}: {:?}",
                result.errors
            );
        }
    }

    #[test]
    fn validate_graph_accepts_max_concurrency_with_each_consumer() {
        let base = r#"
version: "1.0.0"
category: test
config:
  start: work
  nodes:
    work:
__NODE__
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#;
        let cases = [
            concat!(
                "      node_type: foreach\n",
                "      over: \"${state.items}\"\n",
                "      as: item\n",
                "      parallel: true\n",
                "      action: {item_id: \"tool:test/echo\"}\n",
                "      max_concurrency: 2"
            ),
            concat!(
                "      node_type: foreach\n",
                "      over: \"${state.items}\"\n",
                "      as: item\n",
                "      detach: true\n",
                "      action: {item_id: \"directive:test/child\"}\n",
                "      max_concurrency: 2"
            ),
            concat!(
                "      node_type: action\n",
                "      follow: true\n",
                "      over: \"${state.items}\"\n",
                "      as: item\n",
                "      parallel: true\n",
                "      action: {item_id: \"directive:test/child\"}\n",
                "      max_concurrency: 2"
            ),
        ];
        for node in cases {
            let result = validate_graph(&make_graph(&base.replace("__NODE__", node)));
            assert!(
                !result
                    .errors
                    .iter()
                    .any(|error| error.contains("max_concurrency")),
                "used max_concurrency must be accepted for {node:?}: {:?}",
                result.errors
            );
        }
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

    #[test]
    fn graph_definition_rejects_non_expression_conditions() {
        let cases = [
            (
                "structured",
                "          - when: {path: state.ready, op: eq, value: true}\n",
            ),
            ("explicit null", "          - when: null\n"),
            ("empty", "          - when: ''\n"),
        ];
        let base = r#"
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
__BRANCH__            to: done
    done:
      node_type: return
"#;

        for (label, branch) in cases {
            let yaml = base.replace("__BRANCH__", branch);
            assert!(
                GraphDefinition::from_yaml(&yaml, Some("test.yaml")).is_err(),
                "{label} condition must be rejected"
            );
        }
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
      action: {item_id: "tool:test/echo", ref_bindings: {}}
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
    fn validate_follow_fanout_shape_matrix() {
        let base = r#"
version: "1.0.0"
category: test
config:
  start: fan
  nodes:
    fan:
      follow: true
      over: "${state.items}"
      as: item
      parallel: true
      collect: results
      max_concurrency: 3
      action: {item_id: "directive:child", ref_bindings: {}, params: {value: "${item}"}}
    done: {node_type: return}
"#;
        assert!(validate_graph(&make_graph(base)).errors.is_empty());
        let cases = [
            ("      as: item\n", "", "must declare 'as'"),
            (
                "      parallel: true\n",
                "      parallel: false\n",
                "parallel: true",
            ),
            (
                "      collect: results\n",
                "      collect: item\n",
                "both 'collect' and 'as'",
            ),
            (
                "      max_concurrency: 3\n",
                "      max_concurrency: 0\n",
                "between 1 and 256",
            ),
            (
                "      max_concurrency: 3\n",
                "      max_concurrency: 257\n",
                "between 1 and 256",
            ),
            (
                "      action: {item_id: \"directive:child\", ref_bindings: {}, params: {value: \"${item}\"}}\n",
                "      retry: {attempts: 2, backoff_ms: 0}\n      action: {item_id: \"directive:child\", ref_bindings: {}}\n",
                "cannot combine 'retry' and 'follow'",
            ),
        ];
        for (from, to, expected) in cases {
            let mut yaml = base.replacen(from, to, 1);
            if expected == "must declare 'as'" {
                yaml = yaml.replace("${item}", "1");
            }
            let graph = make_graph(&yaml);
            let errors = validate_graph(&graph).errors;
            assert!(
                errors.iter().any(|e| e.contains(expected)),
                "missing {expected:?} in {errors:?}"
            );
        }
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
          - when: 'state.mode == "fast"'
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
    fn analyze_graph_allows_same_node_conditional_on_assigned_state() {
        // Assignment and branch selection share the node's candidate state, so
        // a same-node `state.found` reference is deliberate and must not be
        // diagnosed as a stale read.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: recall
  nodes:
    recall:
      action: {item_id: "tool:recall", ref_bindings: {}}
      assign:
        found: "${result.found}"
      next:
        type: conditional
        branches:
          - when: 'state.found == "yes"'
            to: warm
          - to: study
    warm:
      node_type: return
    study:
      node_type: return
"#;
        let result = analyze_graph(&make_graph(yaml));
        assert!(
            result.errors.is_empty(),
            "unexpected errors: {:?}",
            result.errors
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| warning.contains("before this node's assign")),
            "same-node branches read candidate state: {:?}",
            result.warnings
        );
    }

    #[test]
    fn analyze_graph_quiet_on_result_paths_and_cross_node_state() {
        // `recall` branches on its own outcome via `result.found` (the correct
        // idiom); `gate2` reads `state.found` but does NOT assign it — a
        // legitimate read of a prior node's committed state. Neither warns.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: recall
  nodes:
    recall:
      action: {item_id: "tool:recall", ref_bindings: {}}
      assign:
        found: "${result.found}"
      next:
        type: conditional
        branches:
          - when: 'result.found == "yes"'
            to: gate2
          - to: gate2
    gate2:
      node_type: gate
      next:
        type: conditional
        branches:
          - when: 'state.found == "yes"'
            to: warm
          - to: study
    warm:
      node_type: return
    study:
      node_type: return
"#;
        let result = analyze_graph(&make_graph(yaml));
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.contains("before this node's assign")),
            "must not warn for result.* or cross-node state.*: {:?}",
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
          - when: 'state.mode == "fast"'
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
    fn graph_load_rejects_undeclared_input() {
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
      action: {item_id: "tool:test/echo", ref_bindings: {}, params: {a: "${inputs.declared}", b: "${inputs.missing}"}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
        let error = format!(
            "{:#}",
            GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err()
        );
        assert!(error.contains("missing"), "unexpected error: {error}");
        assert!(
            error.contains("config_schema.properties"),
            "unexpected error: {error}"
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
      action: {item_id: "tool:test/echo", ref_bindings: {}, params: {a: "${inputs.anything}"}}
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
    fn graph_load_rejects_undeclared_input_in_return_output() {
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
        let error = format!(
            "{:#}",
            GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err()
        );
        assert!(error.contains("unknown"), "unexpected error: {error}");
        assert!(
            error.contains("config_schema.properties"),
            "unexpected error: {error}"
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
      action: {item_id: "tool:x", ref_bindings: {}}
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
      action: {item_id: "tool:x", ref_bindings: {}}
      follow: true
      cache_result: true
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
      action: {item_id: "tool:x", ref_bindings: {}}
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
            "      action: {item_id: \"tool:x\", ref_bindings: {}}\n      retry: {attempts: 3, backoff_ms: 1000, max_backoff_ms: 30000}\n      next: {type: unconditional, to: done}",
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
            "      action: {item_id: \"tool:x\", ref_bindings: {}}\n      retry: {attempts: 11, backoff_ms: 100}\n      next: {type: unconditional, to: done}",
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
            "      action: {item_id: \"tool:x\", ref_bindings: {}}\n      retry: {attempts: 0, backoff_ms: 100}\n      next: {type: unconditional, to: done}",
        ));
        assert!(zero
            .errors
            .iter()
            .any(|e| e.contains("retry.attempts must be between 1 and 10")));
    }

    #[test]
    fn validate_graph_rejects_retry_zero_backoff() {
        let result = validate_graph(&retry_graph(
            "      action: {item_id: \"tool:x\", ref_bindings: {}}\n      retry: {attempts: 3, backoff_ms: 0}\n      next: {type: unconditional, to: done}",
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
    fn validate_graph_rejects_retry_delays_above_runtime_maximum() {
        let too_large = MAX_RETRY_BACKOFF_MS + 1;
        let backoff = validate_graph(&retry_graph(&format!(
            "      action: {{item_id: \"tool:x\"}}\n      retry: {{attempts: 2, backoff_ms: {too_large}}}\n      next: {{type: unconditional, to: done}}"
        )));
        assert!(backoff.errors.iter().any(|error| {
            error.contains("retry.backoff_ms") && error.contains("runtime maximum")
        }));

        let cap = validate_graph(&retry_graph(&format!(
            "      action: {{item_id: \"tool:x\"}}\n      retry: {{attempts: 2, backoff_ms: 1, max_backoff_ms: {too_large}}}\n      next: {{type: unconditional, to: done}}"
        )));
        assert!(cap.errors.iter().any(|error| {
            error.contains("retry.max_backoff_ms") && error.contains("runtime maximum")
        }));
    }

    #[test]
    fn validate_graph_rejects_retry_max_below_backoff() {
        let result = validate_graph(&retry_graph(
            "      action: {item_id: \"tool:x\", ref_bindings: {}}\n      retry: {attempts: 3, backoff_ms: 1000, max_backoff_ms: 500}\n      next: {type: unconditional, to: done}",
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
            "      action: {item_id: \"tool:x\", ref_bindings: {}}\n      follow: true\n      retry: {attempts: 3, backoff_ms: 100}\n      next: {type: unconditional, to: done}",
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
            "      node_type: foreach\n      over: \"${state.items}\"\n      as: item\n      action: {item_id: \"tool:x\", ref_bindings: {}}\n      retry: {attempts: 2, backoff_ms: 50}\n      next: {type: unconditional, to: done}",
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
      action: {item_id: "tool:test/echo", ref_bindings: {}}
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

    #[test]
    fn parallel_foreach_rejects_assignment() {
        let graph = make_graph(
            r#"
version: "1"
category: test
config:
  start: fan
  nodes:
    fan:
      node_type: foreach
      over: "${state.items}"
      as: item
      parallel: true
      collect: results
      action: {item_id: "tool:test/noop"}
      assign: {count: "${state.count + 1}"}
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#,
        );
        let result = validate_graph(&graph);
        assert!(result
            .errors
            .iter()
            .any(|error| { error.contains("parallel: true") && error.contains("with 'assign'") }));
    }

    #[test]
    fn foreach_state_keys_must_be_pairwise_disjoint() {
        for (collect, assign) in [
            ("item", "other"),
            ("results", "results"),
            ("results", "item"),
        ] {
            let graph = make_graph(&format!(
                r#"
version: "1"
category: test
config:
  start: each
  nodes:
    each:
      node_type: foreach
      over: "${{state.items}}"
      as: item
      collect: {collect}
      action: {{item_id: "tool:test/noop"}}
      assign: {{{assign}: true}}
      next: {{type: unconditional, to: done}}
    done: {{node_type: return}}
"#
            ));
            let result = validate_graph(&graph);
            assert!(
                !result.errors.is_empty(),
                "expected collision error for collect={collect}, assign={assign}"
            );
        }
    }

    #[test]
    fn graph_load_enforces_action_result_root_contract() {
        let invalid = r#"
version: "1"
category: test
config:
  start: call
  nodes:
    call:
      action:
        item_id: "tool:test/noop"
        params: {value: "${result.value}"}
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(invalid, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("node call.action: expression root `result`"),
            "action fields must not observe a result that does not exist yet: {error:#}"
        );

        let valid = r#"
version: "1"
category: test
config:
  start: call
  nodes:
    call:
      action: {item_id: "tool:test/noop"}
      assign: {saved: "${result.value}"}
      next:
        type: conditional
        branches:
          - when: "state.saved == result.value"
            to: done
          - to: done
    done: {node_type: return}
"#;
        GraphDefinition::from_yaml(valid, Some("test.yaml")).unwrap();
    }

    #[test]
    fn graph_load_rejects_iteration_root_before_binding_or_after_collection() {
        let over_uses_item = r#"
version: "1"
category: test
config:
  start: each
  nodes:
    each:
      node_type: foreach
      over: "${item.values}"
      as: item
      action: {item_id: "tool:test/noop"}
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(over_uses_item, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("node each.over: expression root `item`"),
            "the iteration variable is not bound while evaluating over: {error:#}"
        );

        let branch_uses_item = r#"
version: "1"
category: test
config:
  start: fan
  nodes:
    fan:
      follow: true
      parallel: true
      over: "${state.items}"
      as: item
      action: {item_id: "directive:test/child"}
      next:
        type: conditional
        branches:
          - when: "item.ready"
            to: done
          - to: done
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(branch_uses_item, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("expression root `item` is not available here"),
            "a fanout branch runs after the temporary item binding is gone: {error:#}"
        );
    }

    #[test]
    fn graph_load_rejects_result_when_action_node_has_no_action() {
        let yaml = r#"
version: "1"
category: test
config:
  start: route
  nodes:
    route:
      next:
        type: conditional
        branches:
          - when: "result.ready"
            to: done
          - to: done
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("expression root `result` is not available here"),
            "an action-shaped routing node without an action has no result: {error:#}"
        );
    }

    #[test]
    fn graph_load_rejects_result_root_in_gate_condition() {
        let yaml = r#"
version: "1"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      next:
        type: conditional
        branches:
          - when: "result.ready"
            to: done
          - to: done
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("expression root `result` is not available here"),
            "gate conditions have no action result: {error:#}"
        );
    }

    #[test]
    fn graph_load_enforces_return_output_root_contract() {
        let valid = r#"
version: "1"
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
        state: "${state.value}"
        input: "${inputs.known}"
        execution: "${_execution.depth}"
        run: "${_run.graph_run_id}"
"#;
        GraphDefinition::from_yaml(valid, Some("test.yaml")).unwrap();

        let invalid = valid.replace("${state.value}", "${result.value}");
        let error = GraphDefinition::from_yaml(&invalid, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("node done.output: expression root `result`"),
            "return output has no action result: {error:#}"
        );
    }

    #[test]
    fn graph_load_validates_hook_input_paths_against_config_schema() {
        let yaml = r#"
version: "1"
category: test
config:
  start: done
  config_schema:
    type: object
    properties:
      known: {type: string}
  hooks:
    - id: observe-start
      event: graph_started
      action:
        item_id: "tool:test/noop"
        params: {value: "${inputs.missing}"}
  nodes:
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("input `missing` is not declared"),
            "hook expressions obey the graph input schema: {error:#}"
        );
    }

    #[test]
    fn graph_load_rejects_unknown_root_but_allows_dynamic_input_index() {
        let unknown = r#"
version: "1"
category: test
config:
  start: call
  nodes:
    call:
      action:
        item_id: "tool:test/noop"
        params: {value: "${ambient.value}"}
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(unknown, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("expression root `ambient` is not available here"),
            "unknown roots must fail graph loading: {error:#}"
        );

        let dynamic_input = r#"
version: "1"
category: test
config:
  start: call
  config_schema:
    type: object
    properties:
      known: {type: string}
  nodes:
    call:
      action:
        item_id: "tool:test/noop"
        params: {value: "${inputs[state.input_name]}"}
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#;
        GraphDefinition::from_yaml(dynamic_input, Some("test.yaml")).unwrap();
    }

    #[test]
    fn graph_load_rejects_duplicate_default_branch() {
        let yaml = r#"
version: "1"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      next:
        type: conditional
        branches:
          - to: left
          - to: right
    left: {node_type: return}
    right: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        assert!(
            format!("{error:#}").contains("more than one default branch"),
            "duplicate defaults must fail graph loading: {error:#}"
        );
    }

    #[test]
    fn follow_fanout_rejects_assignment_and_state_key_collisions() {
        let base = r#"
version: "1"
category: test
config:
  start: fan
  nodes:
    fan:
      follow: true
      over: "${state.items}"
      as: item
      parallel: true
      collect: results
      action: {item_id: "directive:test/child", params: {value: "${item}"}}
__ASSIGN__
    done: {node_type: return}
"#;

        for (assign, collision) in [
            ("      assign: {}", None),
            ("      assign: {count: 1}", None),
            (
                "      assign: {item: 1}",
                Some("both its iteration variable and an assign key"),
            ),
            (
                "      assign: {results: 1}",
                Some("both its collect key and an assign key"),
            ),
        ] {
            let graph = make_graph(&base.replace("__ASSIGN__", assign));
            let errors = validate_graph(&graph).errors;
            assert!(
                errors.iter().any(|error| {
                    error.contains("follow fanout") && error.contains("cannot declare 'assign'")
                }),
                "follow fanout assignment must be rejected: {errors:?}"
            );
            if let Some(collision) = collision {
                assert!(
                    errors.iter().any(|error| error.contains(collision)),
                    "missing collision diagnostic {collision:?}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn graph_load_rejects_invalid_and_reserved_iteration_variables() {
        for (variable, expected) in [
            ("bad-name", "must match [A-Za-z_][A-Za-z0-9_]*"),
            ("9item", "must match [A-Za-z_][A-Za-z0-9_]*"),
            ("state", "reserved by rye-expr/1"),
            ("result", "reserved by rye-expr/1"),
            ("_run", "reserved by rye-expr/1"),
        ] {
            let yaml = format!(
                r#"
version: "1"
category: test
config:
  start: each
  nodes:
    each:
      node_type: foreach
      over: "${{state.items}}"
      as: "{variable}"
      collect: results
      action: {{item_id: "tool:test/noop"}}
      next: {{type: unconditional, to: done}}
    done: {{node_type: return}}
"#
            );
            let error = GraphDefinition::from_yaml(&yaml, Some("test.yaml")).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "iteration variable {variable:?} should fail with {expected:?}: {error:#}"
            );
        }
    }
}
