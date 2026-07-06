//! SSE frame type — one Server-Sent Event off a daemon stream, untouched.
//!
//! The client does NOT interpret runtime event kinds: frames go to the
//! shared reducer (`RyeOsEvent::ThreadTail`) or the hint listener as raw
//! `(event_type, data)`, and `ryeos-client-base` applies the semantics —
//! the same path the web `EventSource` uses. Keeping this transport-shaped
//! is the ryeos boundary: clients move bytes, base owns meaning.

/// One SSE frame: the `event:` field (or `"message"` when absent) and the
/// joined `data:` payload, verbatim.
#[derive(Debug, Clone)]
pub struct SseFrame {
    pub event_type: String,
    pub data: String,
}
