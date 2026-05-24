//! UI session substrate contracts.
//!
//! Concrete browser-session and session-bus implementations live in
//! `ryeos-ui`. The application state keeps trait objects here so the
//! core app crate does not depend on browser/UI implementation code.

use std::time::Instant;

use tokio::sync::broadcast;

use crate::stream_envelope::RouteStreamEnvelope;

/// Context provided by a client launcher when minting a browser session.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub surface_ref: String,
    pub project_path: Option<String>,
    pub read_only: bool,
    pub granted_caps: Vec<String>,
}

/// Server-side browser session record exposed to handlers and verifiers.
#[derive(Debug, Clone)]
pub struct BrowserSession {
    pub session_id: String,
    pub created_at: Instant,
    pub expires_at: Instant,
    pub granted_caps: Vec<String>,
    pub project_root: Option<String>,
    pub surface_ref: String,
    pub read_only: bool,
}

pub trait BrowserSessionStoreApi: Send + Sync {
    fn mint_token(&self, ctx: LaunchContext) -> (String, String);
    fn consume_launch_token(&self, token: &str) -> Option<String>;
    fn get_session(&self, session_id: &str) -> Option<BrowserSession>;
    fn evict_expired(&self);
}

pub trait SessionBusApi: Send + Sync {
    fn subscribe(&self, session_id: &str) -> broadcast::Receiver<RouteStreamEnvelope>;
    fn publish(&self, session_id: &str, event_type: &str, payload: serde_json::Value);
    fn replay_after(&self, session_id: &str, last_id: &str) -> Option<Vec<RouteStreamEnvelope>>;
    fn snapshot_required_envelope(&self) -> RouteStreamEnvelope;
}

pub fn snapshot_required_envelope() -> RouteStreamEnvelope {
    RouteStreamEnvelope::new(
        "snapshot_required",
        serde_json::json!({"reason": "event gap exceeds replay ring; re-bootstrap required"}),
    )
}

#[derive(Default)]
pub struct NoopBrowserSessionStore;

impl BrowserSessionStoreApi for NoopBrowserSessionStore {
    fn mint_token(&self, _ctx: LaunchContext) -> (String, String) {
        (String::new(), String::new())
    }

    fn consume_launch_token(&self, _token: &str) -> Option<String> {
        None
    }

    fn get_session(&self, _session_id: &str) -> Option<BrowserSession> {
        None
    }

    fn evict_expired(&self) {}
}

#[derive(Default)]
pub struct NoopSessionBus;

impl SessionBusApi for NoopSessionBus {
    fn subscribe(&self, _session_id: &str) -> broadcast::Receiver<RouteStreamEnvelope> {
        let (_tx, rx) = broadcast::channel(1);
        rx
    }

    fn publish(&self, _session_id: &str, _event_type: &str, _payload: serde_json::Value) {}

    fn replay_after(&self, _session_id: &str, _last_id: &str) -> Option<Vec<RouteStreamEnvelope>> {
        None
    }

    fn snapshot_required_envelope(&self) -> RouteStreamEnvelope {
        snapshot_required_envelope()
    }
}
