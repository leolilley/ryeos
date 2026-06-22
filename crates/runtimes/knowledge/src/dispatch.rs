//! Method dispatch: wire method name → handler.
//!
//! The kind schema is the source of truth for generically dispatchable
//! methods: the daemon validates/routes/authorizes on it and rejects
//! undeclared generic requests before this runtime is spawned. This module
//! registers runtime handler implementations keyed directly on the
//! `MethodCallEnvelope.method` string. It also contains augmentation-private
//! handlers, currently `compose_positions`, invoked only by daemon launch
//! augmentations with a bespoke payload. There is no enum mirror of either
//! vocabulary and no payload-retagging step.

use serde_json::Value;

use crate::types::{
    ComposeContextPayload, ComposePayload, GraphPayload, KnowledgeError, QueryPayload,
    ValidatePayload,
};
use crate::{compose, graph, query, validate};

type MethodHandler = fn(Value) -> Result<Value, KnowledgeError>;

/// Single dispatch point: wire method name → handler.
///
/// SECONDARY guard: the daemon already resolves generic requests against
/// the kind schema before spawning the runtime, and launch augmentations own
/// their private target method names, so an unhandled method here means direct
/// invocation or schema/runtime skew. Report it — never silently no-op.
/// Keyed strictly off `method` (the envelope field), so a payload that happens
/// to carry a `"method"` key cannot influence which handler runs.
pub fn dispatch(method: &str, payload: Value) -> Result<Value, KnowledgeError> {
    let handler: MethodHandler = match method {
        "compose" => method_compose,
        "compose_positions" => method_compose_positions,
        "query" => method_query,
        "graph" => method_graph,
        "validate" => method_validate,
        other => {
            return Err(KnowledgeError::InvalidArg {
                method: other.to_string(),
                reason: "no handler registered for this method".into(),
            })
        }
    };
    handler(payload)
}

/// Deserialize a method payload into its typed shape; a malformed/wrong-shaped
/// payload (including a non-object) surfaces as `InvalidArg`.
fn parse_payload<T: serde::de::DeserializeOwned>(
    method: &'static str,
    payload: Value,
) -> Result<T, KnowledgeError> {
    serde_json::from_value(payload).map_err(|e| KnowledgeError::InvalidArg {
        method: method.into(),
        reason: e.to_string(),
    })
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, KnowledgeError> {
    serde_json::to_value(value).map_err(|e| KnowledgeError::Internal(e.to_string()))
}

fn method_compose(payload: Value) -> Result<Value, KnowledgeError> {
    let p: ComposePayload = parse_payload("compose", payload)?;
    to_json(compose::compose(&p)?)
}

fn method_compose_positions(payload: Value) -> Result<Value, KnowledgeError> {
    let p: ComposeContextPayload = parse_payload("compose_positions", payload)?;
    to_json(compose::compose_positions(&p)?)
}

fn method_query(payload: Value) -> Result<Value, KnowledgeError> {
    let p: QueryPayload = parse_payload("query", payload)?;
    to_json(query::query(&p)?)
}

fn method_graph(payload: Value) -> Result<Value, KnowledgeError> {
    let p: GraphPayload = parse_payload("graph", payload)?;
    to_json(graph::graph(&p)?)
}

fn method_validate(payload: Value) -> Result<Value, KnowledgeError> {
    let p: ValidatePayload = parse_payload("validate", payload)?;
    to_json(validate::validate(&p)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── active-handler smoke tests (routing + real wire shape) ──────────

    #[test]
    fn query_dispatches_and_reads_args() {
        let out = dispatch(
            "query",
            json!({"items_by_ref": {}, "edges": [], "args": {"query": "needle", "limit": 3}}),
        )
        .expect("query must dispatch");
        assert_eq!(out["query"], "needle");
    }

    #[test]
    fn validate_dispatches_and_reads_nested_roots() {
        // Proves `args.roots` still flows (regression for the earlier
        // nested-roots bug) AND that the method routes to validate.
        let out = dispatch(
            "validate",
            json!({"items_by_ref": {}, "edges": [], "args": {"roots": ["k/a"]}}),
        )
        .expect("validate must dispatch");
        assert_eq!(out["valid"], false);
        assert!(out["errors"].as_array().unwrap().iter().any(|e| e
            .as_str()
            .unwrap()
            .contains("root not found in corpus: k/a")));
    }

    #[test]
    fn graph_dispatches_on_empty_corpus() {
        let out = dispatch(
            "graph",
            json!({"items_by_ref": {}, "edges": [], "args": {}}),
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
                "args": {"token_budget": 100}
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

    // ── unknown methods ─────────────────────────────────────────────────

    #[test]
    fn undeclared_methods_are_invalid_arg() {
        // Any method without a handler — a typo or a name the runtime does
        // not implement — hits the single unknown arm.
        for method in ["bogus", "snapshot", "index"] {
            let err = dispatch(method, json!({})).expect_err("undeclared method must error");
            assert!(
                matches!(err, KnowledgeError::InvalidArg { .. }),
                "{method}: {err:?}"
            );
        }
    }

    // ── correctness regressions ─────────────────────────────────────────

    #[test]
    fn envelope_method_wins_over_payload_method_key() {
        // A payload carrying a top-level "method" must NOT override the
        // dispatched method. This routes as validate (per the `method`
        // argument), not query.
        let out = dispatch(
            "validate",
            json!({
                "method": "query",
                "items_by_ref": {},
                "edges": [],
                "args": {"roots": ["k/a"]}
            }),
        )
        .expect("must dispatch");
        // validate output shape (has `valid`), not query output (`matches`).
        assert!(
            out.get("valid").is_some(),
            "should have run validate: {out}"
        );
        assert!(out.get("matches").is_none(), "should NOT have run query");
    }

    #[test]
    fn non_object_payload_is_invalid_arg() {
        let err = dispatch("query", json!("not an object")).expect_err("must error");
        assert!(
            matches!(err, KnowledgeError::InvalidArg { .. }),
            "got: {err:?}"
        );
    }
}
