//! Op dispatch: wire op name → handler.
//!
//! The kind schema is the source of truth for the operation vocabulary —
//! the daemon validates/routes/authorizes on it and rejects undeclared ops
//! before this runtime is spawned. This module registers the runtime's
//! handler implementations against that contract, keyed directly on the
//! `BatchOpEnvelope.op` string. There is no enum mirror of the vocabulary
//! and no payload-retagging step.

use serde_json::Value;

use crate::types::{
    ComposeContextPayload, ComposePayload, GraphPayload, KnowledgeError, QueryPayload,
    ValidatePayload,
};
use crate::{compose, graph, query, validate};

type OpHandler = fn(Value) -> Result<Value, KnowledgeError>;

/// Single dispatch point: wire op name → handler.
///
/// SECONDARY guard: the daemon already resolves the requested op against
/// the kind schema and rejects undeclared ops before spawning the runtime,
/// so an unhandled op here means direct invocation or schema/runtime skew.
/// Report it — never silently no-op. Keyed strictly off `op` (the envelope
/// field), so a payload that happens to carry an `"operation"` key cannot
/// influence which handler runs.
pub fn dispatch(op: &str, payload: Value) -> Result<Value, KnowledgeError> {
    let handler: OpHandler = match op {
        "compose" => op_compose,
        "compose_positions" => op_compose_positions,
        "query" => op_query,
        "graph" => op_graph,
        "validate" => op_validate,
        other => {
            return Err(KnowledgeError::InvalidInput {
                op: other.to_string(),
                reason: "no handler registered for this operation".into(),
            })
        }
    };
    handler(payload)
}

/// Deserialize an op payload into its typed shape; a malformed/wrong-shaped
/// payload (including a non-object) surfaces as `InvalidInput`.
fn parse_payload<T: serde::de::DeserializeOwned>(
    op: &'static str,
    payload: Value,
) -> Result<T, KnowledgeError> {
    serde_json::from_value(payload).map_err(|e| KnowledgeError::InvalidInput {
        op: op.into(),
        reason: e.to_string(),
    })
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, KnowledgeError> {
    serde_json::to_value(value).map_err(|e| KnowledgeError::Internal(e.to_string()))
}

fn op_compose(payload: Value) -> Result<Value, KnowledgeError> {
    let p: ComposePayload = parse_payload("compose", payload)?;
    to_json(compose::compose(&p)?)
}

fn op_compose_positions(payload: Value) -> Result<Value, KnowledgeError> {
    let p: ComposeContextPayload = parse_payload("compose_positions", payload)?;
    to_json(compose::compose_positions(&p)?)
}

fn op_query(payload: Value) -> Result<Value, KnowledgeError> {
    let p: QueryPayload = parse_payload("query", payload)?;
    to_json(query::query(&p)?)
}

fn op_graph(payload: Value) -> Result<Value, KnowledgeError> {
    let p: GraphPayload = parse_payload("graph", payload)?;
    to_json(graph::graph(&p)?)
}

fn op_validate(payload: Value) -> Result<Value, KnowledgeError> {
    let p: ValidatePayload = parse_payload("validate", payload)?;
    to_json(validate::validate(&p)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── active-handler smoke tests (routing + real wire shape) ──────────

    #[test]
    fn query_dispatches_and_reads_inputs() {
        let out = dispatch(
            "query",
            json!({"items_by_ref": {}, "edges": [], "inputs": {"query": "needle", "limit": 3}}),
        )
        .expect("query must dispatch");
        assert_eq!(out["query"], "needle");
    }

    #[test]
    fn validate_dispatches_and_reads_nested_roots() {
        // Proves `inputs.roots` still flows (regression for the earlier
        // nested-roots bug) AND that the op routes to validate.
        let out = dispatch(
            "validate",
            json!({"items_by_ref": {}, "edges": [], "inputs": {"roots": ["k/a"]}}),
        )
        .expect("validate must dispatch");
        assert_eq!(out["valid"], false);
        assert!(out["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("root not found in corpus: k/a")));
    }

    #[test]
    fn graph_dispatches_on_empty_corpus() {
        let out = dispatch(
            "graph",
            json!({"items_by_ref": {}, "edges": [], "inputs": {}}),
        )
        .expect("graph must dispatch");
        assert!(out["nodes"].as_array().unwrap().is_empty());
    }

    fn trusted_item(body: &str) -> Value {
        json!({
            "raw_content": body,
            "raw_content_digest": "d",
            "metadata": {},
            "trust_class": "trusted_bundle"
        })
    }

    #[test]
    fn compose_dispatches() {
        let out = dispatch(
            "compose",
            json!({
                "root_ref": "k/a",
                "items_by_ref": {"k/a": trusted_item("---\ntitle: A\n---\nbody")},
                "edges": [],
                "inputs": {"token_budget": 100}
            }),
        )
        .expect("compose must dispatch");
        assert!(out.get("content").is_some());
    }

    #[test]
    fn compose_positions_dispatches_top_level_shape() {
        // Augmentation payload: top-level roots_by_position/budgets, NOT
        // inputs-nested.
        let out = dispatch(
            "compose_positions",
            json!({
                "items_by_ref": {"k/a": trusted_item("---\ntitle: A\n---\nbody")},
                "edges": [],
                "roots_by_position": {"primary": ["k/a"]},
                "per_position_budget": {"primary": 100},
                "default_budget": 100
            }),
        )
        .expect("compose_positions must dispatch");
        assert!(out.get("rendered").is_some());
    }

    // ── unknown ops ─────────────────────────────────────────────────────

    #[test]
    fn undeclared_ops_are_invalid_input() {
        // Any op without a handler — a typo or a name the runtime does not
        // implement — hits the single unknown arm.
        for op in ["bogus", "snapshot", "index"] {
            let err = dispatch(op, json!({})).expect_err("undeclared op must error");
            assert!(matches!(err, KnowledgeError::InvalidInput { .. }), "{op}: {err:?}");
        }
    }

    // ── correctness regressions ─────────────────────────────────────────

    #[test]
    fn envelope_op_wins_over_payload_operation_key() {
        // A payload carrying a top-level "operation" must NOT override the
        // dispatched op. This routes as validate (per `op`), not query.
        let out = dispatch(
            "validate",
            json!({
                "operation": "query",
                "items_by_ref": {},
                "edges": [],
                "inputs": {"roots": ["k/a"]}
            }),
        )
        .expect("must dispatch");
        // validate output shape (has `valid`), not query output (`matches`).
        assert!(out.get("valid").is_some(), "should have run validate: {out}");
        assert!(out.get("matches").is_none(), "should NOT have run query");
    }

    #[test]
    fn non_object_payload_is_invalid_input() {
        let err = dispatch("query", json!("not an object")).expect_err("must error");
        assert!(matches!(err, KnowledgeError::InvalidInput { .. }), "got: {err:?}");
    }
}
