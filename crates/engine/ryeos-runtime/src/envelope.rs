//! Launch envelope types — re-exported from `ryeos_engine::launch_envelope_types`.
//!
//! Single source of truth lives in the engine crate; the daemon mints
//! the envelope and runtimes deserialise the same struct.
//!
//! `EnvelopeTarget` is gone — the runtime gets the root path / digest /
//! kind / id from `LaunchEnvelope.resolution.root` directly.

pub use ryeos_engine::launch_envelope_types::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, HardLimits, ItemDescriptor,
    LaunchEnvelope, LaunchEnvelopeBuilder, RuntimeCost, RuntimeResult, COST_BASIS_ROLLUP,
};

/// Canonical success classification for daemon/runtime result envelopes. Graph
/// dispatch and follow joins must agree: native envelopes use `success`, while
/// subprocess envelopes use the terminator's `outcome_code` discriminator.
pub fn envelope_succeeded(value: &serde_json::Value) -> bool {
    let Some(obj) = value.as_object() else { return true };
    if obj.contains_key("result") && obj.contains_key("success") && obj.contains_key("status")
        && (obj.contains_key("outputs") || obj.contains_key("warnings"))
    {
        return obj.get("success").and_then(serde_json::Value::as_bool) == Some(true);
    }
    if obj.contains_key("result") && obj.contains_key("outcome_code") {
        return matches!(obj.get("outcome_code").and_then(serde_json::Value::as_str), Some("completed" | "success"));
    }
    true
}
