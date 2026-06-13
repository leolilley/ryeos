//! Talking to the daemon: the HTTP/UDS client and its stream parsing.
//!
//! Everything here is plumbing below the seat: no studio state, no
//! rendering. `daemon` is the signed/session client; `sse` parses the
//! event-stream wire format into typed events.

pub mod daemon;
pub mod sse;
