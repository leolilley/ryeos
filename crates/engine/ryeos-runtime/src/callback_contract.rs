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

pub use ryeos_effect_contract::{DispatchResultProjection, RetainedEffectResult};

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
    /// Selects the owner of the callback-visible result contract. This is
    /// daemon evidence, never inferred from the returned JSON or item kind.
    pub result_projection: DispatchResultProjection,
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
        self.result_projection.validate()?;
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
        if matches!(
            &self.result_projection,
            DispatchResultProjection::RetainedEffect { .. }
        ) && !matches!(
            (self.source, self.effect_class),
            (
                RuntimeDispatchSource::Executed | RuntimeDispatchSource::EffectRecord,
                RuntimeDispatchEffectClass::Recorded | RuntimeDispatchEffectClass::Sealed,
            )
        ) {
            anyhow::bail!(
                "a retained-effect result projection requires durable executed or replay evidence"
            );
        }
        Ok(())
    }

    /// Validate and project the daemon-owned retained answer, when selected.
    ///
    /// The envelope remains the existing bounded subprocess-shaped leaf wire;
    /// this authority changes who owns its result, not its transport. The
    /// accepted value must hash to the retained CAS object before a runtime can
    /// expose it to authored control flow.
    pub fn retained_effect_result(
        &self,
        value: &Value,
    ) -> anyhow::Result<Option<ValidatedRetainedEffectResult>> {
        self.validate()?;
        let DispatchResultProjection::RetainedEffect { retained_result } = &self.result_projection
        else {
            return Ok(None);
        };
        let envelope: RetainedEffectEnvelope = serde_json::from_value(value.clone())
            .map_err(|error| anyhow::anyhow!("invalid retained-effect result envelope: {error}"))?;
        if envelope.outcome_code.0.is_some() {
            anyhow::bail!("retained-effect result envelope must carry null outcome_code");
        }
        if !envelope.error.is_null() {
            anyhow::bail!("retained-effect result envelope must carry null error");
        }
        if !envelope.artifacts.is_empty() {
            anyhow::bail!("retained-effect result envelope must not carry artifacts");
        }
        if envelope.replayed_from != self.replayed_from {
            anyhow::bail!("retained-effect result replay provenance contradicts dispatch evidence");
        }
        let result_digest = ryeos_effect_contract::canonical_value_digest(&envelope.result)?;
        if result_digest != retained_result.object_hash() {
            anyhow::bail!("retained-effect result does not match its admitted object hash");
        }
        Ok(Some(ValidatedRetainedEffectResult {
            result: envelope.result,
            replayed_from: envelope.replayed_from,
        }))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedRetainedEffectResult {
    pub result: Value,
    pub replayed_from: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedEffectEnvelope {
    outcome_code: RequiredNullableString,
    result: Value,
    error: Value,
    artifacts: Vec<Value>,
    #[serde(default)]
    replayed_from: Option<String>,
}

#[derive(Debug)]
struct RequiredNullableString(Option<String>);

impl<'de> Deserialize<'de> for RequiredNullableString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Classify execution success using the existing terminator contract, not
    /// an item kind or payload keys guessed by each ingress. Service results
    /// are domain values even when they contain `status`/`success`; the
    /// daemon-authored `thread.recorded` discriminator identifies that case.
    /// A replay may carry a null thread and a retained runtime envelope.
    pub fn execution_succeeded(&self) -> bool {
        if self.dispatch.validate().is_err() {
            return false;
        }
        if matches!(self.thread.get("recorded"), Some(Value::Bool(_))) {
            return true;
        }
        if !self.thread.is_null() {
            let status = self
                .thread
                .get("status")
                .and_then(Value::as_str)
                .and_then(ryeos_state::objects::ThreadStatus::from_str_lossy);
            if status != Some(ryeos_state::objects::ThreadStatus::Completed) {
                return false;
            }
        }
        crate::envelope::envelope_succeeded(&self.result)
    }

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
            result_projection: DispatchResultProjection::DispatchedSubject,
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
    fn workload_success_uses_dispatch_contract_not_domain_payload() {
        let mut response = CallbackDispatchResponse {
            thread: json!({"status":"completed"}),
            result: json!({"success":false}),
            dispatch: live_dispatch(),
        };
        assert!(!response.execution_succeeded());
        response.result = json!({"success":true});
        // A status-looking domain fragment is not a terminal envelope. Reuse
        // the same complete contract that graph/follow dispatch validates.
        assert!(!response.execution_succeeded());
        response.result = json!({
            "success": true,
            "status": "completed",
            "result": {},
            "outputs": {},
            "warnings": [],
            "cost": null
        });
        assert!(response.execution_succeeded());
        for status in ["failed", "cancelled", "running", "unknown"] {
            response.thread = json!({"status":status});
            assert!(!response.execution_succeeded(), "{status}");
        }
        response.thread = Value::Null;
        assert!(response.execution_succeeded());
        response.result = json!({"success":false,"status":"failed"});
        assert!(!response.execution_succeeded());
        // Service results can contain domain-level failure/status values.
        response.thread = json!({"recorded":true});
        assert!(response.execution_succeeded());
        response.dispatch.action_digest = "not-a-digest".to_owned();
        assert!(!response.execution_succeeded());
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

    fn retained_dispatch(replayed: bool, object_hash: String) -> RuntimeDispatchEvidence {
        let record_hash = "cd".repeat(32);
        RuntimeDispatchEvidence {
            source: if replayed {
                RuntimeDispatchSource::EffectRecord
            } else {
                RuntimeDispatchSource::Executed
            },
            effect_class: RuntimeDispatchEffectClass::Recorded,
            action_digest: "ab".repeat(32),
            effect_identity: Some("bc".repeat(32)),
            publication: if replayed {
                RuntimeDispatchPublication::NotApplicable
            } else {
                RuntimeDispatchPublication::Inserted
            },
            record_hash: Some(record_hash.clone()),
            replayed_from: replayed.then_some(record_hash),
            result_projection: DispatchResultProjection::RetainedEffect {
                retained_result: RetainedEffectResult::ProductBuildAcceptedResult { object_hash },
            },
        }
    }

    #[test]
    fn retained_effect_projection_requires_exact_value_and_replay_evidence() {
        let result = json!({"products": [{"name": "runtime"}]});
        let object_hash = ryeos_effect_contract::canonical_value_digest(&result).unwrap();
        for replayed in [false, true] {
            let evidence = retained_dispatch(replayed, object_hash.clone());
            let envelope = json!({
                "outcome_code": null,
                "result": result,
                "error": null,
                "artifacts": [],
                "replayed_from": replayed.then(|| "cd".repeat(32)),
            });
            let projected = evidence
                .retained_effect_result(&envelope)
                .unwrap()
                .expect("retained projection");
            assert_eq!(projected.result, result);
            assert_eq!(projected.replayed_from, evidence.replayed_from);

            let mut changed = envelope.clone();
            changed["result"]["products"][0]["name"] = json!("other");
            assert!(evidence.retained_effect_result(&changed).is_err());
        }
    }

    #[test]
    fn retained_effect_projection_refuses_live_or_unbound_transport() {
        let result = json!({"products": []});
        let object_hash = ryeos_effect_contract::canonical_value_digest(&result).unwrap();
        let mut evidence = retained_dispatch(false, object_hash);
        evidence.effect_class = RuntimeDispatchEffectClass::Live;
        evidence.effect_identity = None;
        evidence.record_hash = None;
        evidence.publication = RuntimeDispatchPublication::NotApplicable;
        assert!(evidence.validate().is_err());

        let evidence = retained_dispatch(false, "ab".repeat(32));
        assert!(
            evidence
                .retained_effect_result(&json!({
                    "outcome_code": null,
                    "result": {"products": []},
                    "error": null,
                    "artifacts": [],
                    "unexpected": true,
                }))
                .is_err()
        );
    }
}
