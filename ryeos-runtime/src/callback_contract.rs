//! Typed contract for `runtime.dispatch_action` callback responses.
//!
//! V5.4 Phase 2 cleanup — pre-cleanup the daemon hand-rolled a
//! `{thread, result, data, status}` envelope; consumers (directive
//! runtime, graph runtime) read undocumented fields off raw JSON. This
//! module makes the shape a typed boundary so:
//!
//! * the daemon writes the same fields the runtimes read,
//! * the runtimes never serialize/deserialize the wrapper noise into
//!   the model's tool-result bytes (only the leaf `result` is
//!   model-visible),
//! * future consumers (graph-runtime continuation chains, mock
//!   provider tests in V5.4 P3b) can pattern-match on a stable type.
//!
//! There is no `data` field, no `status` field. Leaf-dispatcher
//! semantics like continuation IDs live INSIDE `result` — there is no
//! parallel sidechannel.
//!
//! NEVER add a `data` or `status` mirror here without the same change
//! landing daemon-side AND a pin test asserting the byte-stable shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Daemon → runtime response from `runtime.dispatch_action`.
///
/// Mirrors the shape every leaf dispatcher in `ryeosd/src/dispatch.rs`
/// returns at its `Ok(json!({ "thread": ..., "result": ... }))` site:
///
/// * service terminator       (`dispatch_service`)
/// * subprocess terminator    (`dispatch_subprocess` / `dispatch_managed_subprocess`)
///
/// Both return identical-shape unary outcomes; this struct binds
/// that contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackDispatchResponse {
    /// Finalized (or, for detached launches, currently-running)
    /// child-thread snapshot. Shape is whatever the leaf dispatcher
    /// produced for `ThreadDetail` / running-thread JSON; this struct
    /// does not constrain it further.
    pub thread: Value,

    /// Leaf dispatcher's terminal result value. This is the ONLY
    /// model-visible portion when the calling runtime feeds the
    /// response back into an LLM as tool-call output: directive
    /// runtime serializes JUST this field, not the wrapper.
    ///
    /// If a leaf dispatcher signals continuation (a child run that
    /// chained), the continuation ID lives at `result.continuation_id`
    /// — not at any top-level sidechannel.
    pub result: Value,
}

impl CallbackDispatchResponse {
    /// Try to extract a continuation ID from `result.continuation_id`.
    /// Returns `None` for terminal results.
    ///
    /// This is the ONLY place runtime-side code should look for a
    /// continuation ID — there is no `data.continuation_id` fallback.
    pub fn continuation_id(&self) -> Option<&str> {
        self.result
            .get("continuation_id")
            .and_then(|v| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_minimal() {
        let response = CallbackDispatchResponse {
            thread: json!({"id": "T-x", "status": "completed"}),
            result: json!({"output": 42}),
        };
        let serialized = serde_json::to_value(&response).unwrap();
        // Must be exactly `{thread, result}`, no extra keys.
        let map = serialized.as_object().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("thread"));
        assert!(map.contains_key("result"));

        let parsed: CallbackDispatchResponse = serde_json::from_value(serialized).unwrap();
        assert_eq!(parsed.thread, response.thread);
        assert_eq!(parsed.result, response.result);
    }

    #[test]
    fn parses_real_leaf_dispatcher_shape() {
        // Mirrors the json!() literals at ryeosd/src/dispatch.rs:494, :718, :858.
        let raw = json!({
            "thread": {
                "id": "T-child-7",
                "status": "completed",
                "kind": "service_run",
            },
            "result": "ok",
        });
        let parsed: CallbackDispatchResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.continuation_id(), None);
    }

    #[test]
    fn continuation_id_extracts_from_result() {
        let response = CallbackDispatchResponse {
            thread: json!({"id": "T-parent"}),
            result: json!({"continuation_id": "T-successor"}),
        };
        assert_eq!(response.continuation_id(), Some("T-successor"));
    }

    #[test]
    fn rejects_legacy_envelope_with_extra_fields() {
        // Defense in depth: a legacy `{thread, result, data, status}`
        // payload MUST fail to deserialize — the daemon must never
        // emit the old shape, and a legacy emitter must surface
        // loudly rather than silently lose fields.
        let legacy = json!({
            "thread": {"id": "T-x"},
            "result": "ok",
            "data": "ok",
            "status": "ok",
        });
        let err = serde_json::from_value::<CallbackDispatchResponse>(legacy).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("data") || msg.contains("status") || msg.contains("unknown field"),
            "expected deny_unknown_fields error mentioning the legacy field, got: {msg}"
        );
    }
}
