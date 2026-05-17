//! Typed service handler registry — data layer.
//!
//! Handler bodies and the build wiring (`build_service_registry`) stay in
//! `ryeosd::service_registry` because they reference per-endpoint handler
//! modules that live in the daemon. The shared types defined here are
//! the surface `AppState` needs to carry without pulling daemon-side
//! handler code into `ryeos-app`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::state::AppState;

/// Whether the service is available in live mode, standalone mode, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAvailability {
    /// Available via `/execute` when daemon is up AND via `run-service` when down.
    Both,
    /// Available only via `/execute` (needs running daemon to receive).
    DaemonOnly,
    /// Available only via `run-service` (daemon must be down; e.g. bundle.install
    /// needs engine reload, rebuild needs exclusive state access).
    OfflineOnly,
}

/// Type-erased async service handler function.
///
/// Each handler deserializes `params` into its own request struct,
/// executes, and serializes the response into a JSON value.
type HandlerFn = Arc<
    dyn Fn(Value, Arc<AppState>) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync,
>;

/// Raw fn-pointer form used by per-handler `DESCRIPTOR` constants.
///
/// Plain `fn` (not `Arc<dyn Fn>`) so each handler module can declare a
/// `pub const DESCRIPTOR: ServiceDescriptor` at module scope.
pub type RawHandlerFn =
    fn(Value, Arc<AppState>) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>>;

/// Single source of truth for a daemon-supported service.
///
/// Carries all metadata needed for registry build, availability check,
/// self-check, and gate tests.
pub struct ServiceDescriptor {
    /// Canonical item ref, e.g. "service:bundle/install".
    pub service_ref: &'static str,
    /// Endpoint string the handler dispatches on, e.g. "bundle.install".
    pub endpoint: &'static str,
    /// Live / standalone availability.
    pub availability: ServiceAvailability,
    /// Capability scopes required to invoke this service.
    /// Empty `&[]` for unrestricted services; non-empty for protected ones.
    pub required_caps: &'static [&'static str],
    /// Type-erased async handler.
    pub handler: RawHandlerFn,
}

/// Registry mapping endpoint strings to service handlers.
///
/// Built at daemon startup. Dispatch is O(1) via HashMap lookup.
#[derive(Clone)]
pub struct ServiceRegistry {
    handlers: HashMap<String, HandlerFn>,
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a raw fn-pointer handler. Used by daemon's
    /// `build_service_registry()` when wiring `handlers::ALL` into the
    /// dispatch table.
    pub fn register_raw(&mut self, endpoint: &str, handler: RawHandlerFn) {
        self.handlers
            .insert(endpoint.to_string(), Arc::new(handler));
    }

    /// Look up a handler by endpoint. Returns `None` for unregistered
    /// endpoints.
    pub fn get(&self, endpoint: &str) -> Option<&HandlerFn> {
        self.handlers.get(endpoint)
    }

    /// Check whether a handler is registered for the given endpoint.
    pub fn has(&self, endpoint: &str) -> bool {
        self.handlers.contains_key(endpoint)
    }

    /// Return all registered endpoint names.
    pub fn endpoints(&self) -> Vec<&str> {
        self.handlers.keys().map(|s| s.as_str()).collect()
    }
}

/// Extract `endpoint` from a verified service item's metadata.
///
/// The kind schema's extraction rules put `endpoint` into
/// `ItemMetadata.extra["endpoint"]` as a JSON string.
pub fn extract_endpoint(metadata_extra: &HashMap<String, Value>) -> Result<String> {
    metadata_extra
        .get("endpoint")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("service YAML missing required field 'endpoint'"))
}

/// Extract `required_caps` from a verified service item's metadata.
///
/// The kind schema's extraction rules put `required_caps` into
/// `ItemMetadata.extra["required_caps"]` as a JSON array of strings.
pub fn extract_required_caps(metadata_extra: &HashMap<String, Value>) -> Vec<String> {
    metadata_extra
        .get("required_caps")
        .and_then(|v| v.as_array())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}
