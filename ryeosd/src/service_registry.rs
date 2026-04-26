//! Typed service handler registry.
//!
//! Service handlers are daemon-owned in-process operations dispatched by
//! `/execute` when the resolved item has `kind: service`. Each handler is
//! identified by an endpoint string declared in the signed service YAML.
//!
//! `ServiceDescriptor` is the single source of truth for all daemon-supported
//! services. Each handler module exports `DESCRIPTOR` carrying the canonical
//! ref, endpoint, availability, and handler. Everything else (registry, self-
//! check, availability lookup, gate tests) derives from `handlers::ALL`.
//!
//! Per-endpoint handler bodies live in `crate::services::handlers::*`,
//! one file per handler.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::service_executor::ServiceAvailability;
use crate::services::handlers;
use crate::state::AppState;

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
/// self-check, and gate tests. Adding a service requires editing:
/// 1. The YAML (and re-signing).
/// 2. The handler module (exporting `DESCRIPTOR`).
/// 3. `handlers::ALL` in `mod.rs`.
pub struct ServiceDescriptor {
    /// Canonical item ref, e.g. "service:bundle/install".
    pub service_ref: &'static str,
    /// Endpoint string the handler dispatches on, e.g. "bundle.install".
    pub endpoint: &'static str,
    /// Live / standalone availability.
    pub availability: ServiceAvailability,
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

impl ServiceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a raw fn-pointer handler. Used by `build_service_registry()`
    /// when wiring `handlers::ALL` into the dispatch table.
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
    #[cfg(test)]
    pub fn endpoints(&self) -> Vec<&str> {
        self.handlers.keys().map(|s| s.as_str()).collect()
    }
}

/// Build the service registry with all 17 service handlers.
///
/// Called once at daemon startup. The registry is immutable after build.
pub fn build_service_registry() -> ServiceRegistry {
    let mut reg = ServiceRegistry::new();
    for desc in handlers::ALL {
        reg.register_raw(desc.endpoint, desc.handler);
    }
    reg
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
/// Cap enforcement happens in the /execute dispatch path by intersecting
/// these with the caller's scopes.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_registry_has_all_descriptors() {
        let reg = build_service_registry();
        for desc in handlers::ALL {
            assert!(
                reg.has(desc.endpoint),
                "missing handler for endpoint '{}'",
                desc.endpoint
            );
        }
    }

    #[test]
    fn service_registry_count() {
        let reg = build_service_registry();
        assert_eq!(reg.endpoints().len(), 17);
    }

    #[test]
    fn no_duplicate_endpoints() {
        let mut seen = std::collections::HashSet::new();
        for desc in handlers::ALL {
            assert!(
                seen.insert(desc.endpoint),
                "duplicate endpoint '{}' in service descriptor",
                desc.endpoint
            );
        }
    }

    #[test]
    fn unknown_endpoint_returns_none() {
        let reg = build_service_registry();
        assert!(reg.get("nonexistent.method").is_none());
    }

    #[test]
    fn extract_endpoint_from_metadata() {
        let mut extra = HashMap::new();
        extra.insert("endpoint".to_string(), serde_json::json!("system.status"));
        extra.insert("required_caps".to_string(), serde_json::json!(["system.read"]));
        assert_eq!(extract_endpoint(&extra).unwrap(), "system.status");
    }

    #[test]
    fn extract_endpoint_missing_fails() {
        let mut extra = HashMap::new();
        extra.insert("required_caps".to_string(), serde_json::json!(["system.read"]));
        assert!(extract_endpoint(&extra).is_err());
    }

    #[test]
    fn extract_required_caps_from_metadata() {
        let mut extra = HashMap::new();
        extra.insert("endpoint".to_string(), serde_json::json!("threads.list"));
        extra.insert("required_caps".to_string(), serde_json::json!(["threads.read", "system.read"]));
        let caps = extract_required_caps(&extra);
        assert_eq!(caps, vec!["threads.read", "system.read"]);
    }

    #[test]
    fn extract_required_caps_empty_when_missing() {
        let extra = HashMap::new();
        let caps = extract_required_caps(&extra);
        assert!(caps.is_empty());
    }
}
