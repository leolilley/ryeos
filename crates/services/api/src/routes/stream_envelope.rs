//! Transport-neutral stream envelope (re-export + SSE framing).
//!
//! Core type lives in `ryeos_app::stream_envelope::RouteStreamEnvelope`.
//! This module adds SSE-specific framing for the HTTP transport layer.

pub use ryeos_app::stream_envelope::RouteStreamEnvelope;

/// Convert a `RouteStreamEnvelope` to an SSE `Event` frame.
pub fn envelope_to_sse(env: &RouteStreamEnvelope) -> axum::response::sse::Event {
    let data = serde_json::to_string(&env.payload).unwrap_or_else(|_| "{}".into());
    let mut builder = axum::response::sse::Event::default().data(data);
    if let Some(ref id) = env.id {
        builder = builder.id(id);
    }
    builder = builder.event(&env.event_type);
    builder
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_to_sse_frames_correctly() {
        let env = RouteStreamEnvelope::with_id("e1", "thread.upsert", json!({"status": "running"}));
        let sse = envelope_to_sse(&env);
        let _s = format!("{sse:?}");
    }
}
