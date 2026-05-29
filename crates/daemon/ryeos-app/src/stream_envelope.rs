//! Transport-neutral stream envelope.
//!
//! `RouteStreamEnvelope` is the single unit of streaming data that all
//! event producers emit and all transport modes (SSE, future WebSocket)
//! frame for the wire. Producers emit envelopes; transports serialize.
//!
//! ## Design
//!
//! - `id`: optional event ID for `Last-Event-ID` / replay.
//! - `event_type`: domain event category (e.g., `thread.upsert`).
//! - `payload`: arbitrary JSON — the actual event data.
//!
//! The envelope is the contract boundary between producers and transports.
//! Adding a field here affects every stream; changing it is a version bump.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Transport-neutral streaming event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteStreamEnvelope {
    /// Optional event ID for replay / deduplication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Domain event type (e.g. `thread.upsert`, `session.heartbeat`).
    pub event_type: String,
    /// Event payload — arbitrary JSON.
    pub payload: Value,
}

impl RouteStreamEnvelope {
    /// Create a new envelope with no ID.
    pub fn new(event_type: impl Into<String>, payload: Value) -> Self {
        Self {
            id: None,
            event_type: event_type.into(),
            payload,
        }
    }

    /// Create a new envelope with an explicit ID.
    pub fn with_id(id: impl Into<String>, event_type: impl Into<String>, payload: Value) -> Self {
        Self {
            id: Some(id.into()),
            event_type: event_type.into(),
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_creates_envelope_without_id() {
        let env = RouteStreamEnvelope::new("thread.upsert", json!({"id": "t1"}));
        assert_eq!(env.event_type, "thread.upsert");
        assert_eq!(env.payload, json!({"id": "t1"}));
        assert!(env.id.is_none());
    }

    #[test]
    fn with_id_creates_envelope_with_id() {
        let env = RouteStreamEnvelope::with_id("evt-1", "thread.upsert", json!({"id": "t1"}));
        assert_eq!(env.id, Some("evt-1".to_string()));
    }

    #[test]
    fn serialize_roundtrip() {
        let env = RouteStreamEnvelope::with_id("e1", "test.event", json!({"key": "val"}));
        let json_str = serde_json::to_string(&env).unwrap();
        let parsed: RouteStreamEnvelope = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, env);
    }

    #[test]
    fn no_id_serializes_compact() {
        let env = RouteStreamEnvelope::new("heartbeat", json!({}));
        let json_str = serde_json::to_string(&env).unwrap();
        assert!(
            !json_str.contains("\"id\""),
            "id should be omitted when None"
        );
    }
}
