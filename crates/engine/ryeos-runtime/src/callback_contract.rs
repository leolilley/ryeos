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

/// Runtime-neutral provenance for one callback-dispatched action.
///
/// The daemon owns this statement. Kind runtimes may project it into their
/// own receipts or expression roots, but authored result bytes are never
/// mutated to carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDispatchEvidence {
    pub source: RuntimeDispatchSource,
    pub effect_class: RuntimeDispatchEffectClass,
    pub action_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_identity: Option<String>,
    pub publication: RuntimeDispatchPublication,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_from: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeDispatchSource {
    Executed,
    EffectRecord,
    ExecutionCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeDispatchEffectClass {
    Live,
    Recorded,
    Sealed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeDispatchPublication {
    NotApplicable,
    Inserted,
    Folded,
}

impl RuntimeDispatchEvidence {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (field, value) in [
            ("dispatch action digest", Some(self.action_digest.as_str())),
            ("dispatch effect identity", self.effect_identity.as_deref()),
            ("dispatch record hash", self.record_hash.as_deref()),
            ("dispatch replay source", self.replayed_from.as_deref()),
        ] {
            if let Some(value) = value
                && !lillux::valid_hash(value)
            {
                anyhow::bail!("{field} is not a canonical digest");
            }
        }
        match (self.source, self.effect_class, self.publication) {
            (
                RuntimeDispatchSource::Executed,
                RuntimeDispatchEffectClass::Live,
                RuntimeDispatchPublication::NotApplicable,
            ) if self.effect_identity.is_none()
                && self.record_hash.is_none()
                && self.replayed_from.is_none() => {}
            (
                RuntimeDispatchSource::Executed,
                RuntimeDispatchEffectClass::Recorded | RuntimeDispatchEffectClass::Sealed,
                RuntimeDispatchPublication::Inserted | RuntimeDispatchPublication::Folded,
            ) if self.effect_identity.is_some()
                && self.record_hash.is_some()
                && self.replayed_from.is_none() => {}
            (
                RuntimeDispatchSource::EffectRecord,
                RuntimeDispatchEffectClass::Recorded | RuntimeDispatchEffectClass::Sealed,
                RuntimeDispatchPublication::NotApplicable,
            ) if self.effect_identity.is_some()
                && self.record_hash.is_some()
                && self.record_hash == self.replayed_from => {}
            (
                RuntimeDispatchSource::ExecutionCache,
                RuntimeDispatchEffectClass::Live,
                RuntimeDispatchPublication::NotApplicable,
            ) if self.effect_identity.is_none()
                && self.record_hash.is_none()
                && self.replayed_from.is_none() => {}
            _ => anyhow::bail!("dispatch evidence fields are mutually inconsistent"),
        }
        Ok(())
    }
}

/// Daemon → runtime response from `runtime.dispatch_action`.
///
/// Mirrors the shape every leaf dispatcher in `crates/bin/daemon/src/dispatch.rs`
/// returns at its
/// `Ok(json!({ "thread": ..., "result": ..., "dispatch": ... }))` site:
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

    /// Daemon-owned action provenance, separate from authored result bytes.
    pub dispatch: RuntimeDispatchEvidence,
}

impl CallbackDispatchResponse {
    /// Try to extract a continuation ID from `result.continuation_id`.
    /// Returns `None` for terminal results.
    ///
    /// This is the ONLY place runtime-side code should look for a
    /// continuation ID — there is no `data.continuation_id` fallback.
    pub fn continuation_id(&self) -> Option<&str> {
        self.result.get("continuation_id").and_then(|v| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live_dispatch() -> RuntimeDispatchEvidence {
        RuntimeDispatchEvidence {
            source: RuntimeDispatchSource::Executed,
            effect_class: RuntimeDispatchEffectClass::Live,
            action_digest: "ab".repeat(32),
            effect_identity: None,
            publication: RuntimeDispatchPublication::NotApplicable,
            record_hash: None,
            replayed_from: None,
        }
    }

    #[test]
    fn round_trip_minimal() {
        let response = CallbackDispatchResponse {
            thread: json!({"id": "T-x", "status": "completed"}),
            result: json!({"output": 42}),
            dispatch: live_dispatch(),
        };
        let serialized = serde_json::to_value(&response).unwrap();
        // Must be exactly `{thread, result, dispatch}`, no wrapper noise.
        let map = serialized.as_object().unwrap();
        assert_eq!(map.len(), 3);
        assert!(map.contains_key("thread"));
        assert!(map.contains_key("result"));
        assert!(map.contains_key("dispatch"));

        let parsed: CallbackDispatchResponse = serde_json::from_value(serialized).unwrap();
        assert_eq!(parsed.thread, response.thread);
        assert_eq!(parsed.result, response.result);
        assert_eq!(parsed.dispatch, response.dispatch);
    }

    #[test]
    fn parses_real_leaf_dispatcher_shape() {
        // Mirrors the json!() literals at crates/bin/daemon/src/dispatch.rs:494, :718, :858.
        let raw = json!({
            "thread": {
                "id": "T-child-7",
                "status": "completed",
                "kind": "service_run",
            },
            "result": "ok",
            "dispatch": live_dispatch(),
        });
        let parsed: CallbackDispatchResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.continuation_id(), None);
    }

    #[test]
    fn continuation_id_extracts_from_result() {
        let response = CallbackDispatchResponse {
            thread: json!({"id": "T-parent"}),
            result: json!({"continuation_id": "T-successor"}),
            dispatch: live_dispatch(),
        };
        assert_eq!(response.continuation_id(), Some("T-successor"));
    }

    #[test]
    fn rejects_old_envelope_with_extra_fields() {
        // Defense in depth: an old `{thread, result, data, status}`
        // payload MUST fail to deserialize — the daemon must never
        // emit the old shape, and an old emitter must surface
        // loudly rather than silently lose fields.
        let old_shape = json!({
            "thread": {"id": "T-x"},
            "result": "ok",
            "data": "ok",
            "status": "ok",
        });
        let err = serde_json::from_value::<CallbackDispatchResponse>(old_shape).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("data") || msg.contains("status") || msg.contains("unknown field"),
            "expected deny_unknown_fields error mentioning the old field, got: {msg}"
        );
    }
}
