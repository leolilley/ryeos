//! Daemon-side service registry wiring.
//!
//! The data types — `ServiceRegistry`, `ServiceDescriptor`, `ServiceAvailability`,
//! `RawHandlerFn`, `extract_endpoint`, `extract_required_caps` — live in
//! `ryeos_app::service_registry`. Per-endpoint handler bodies live in
//! `crate::handlers::*`, one file per handler. This module is the
//! seam: it walks `handlers::ALL` and registers each descriptor's handler
//! function into a freshly-built registry.

use crate::handlers;

pub use ryeos_app::service_registry::{
    extract_endpoint, extract_required_caps, RawHandlerFn, ServiceAvailability,
    ServiceDescriptor, ServiceRegistry,
};

/// Build the service registry with all service handlers declared in
/// `crate::handlers::ALL`.
///
/// Called once at daemon startup. The registry is immutable after build.
pub fn build_service_registry() -> ServiceRegistry {
    let mut reg = ServiceRegistry::new();
    for desc in handlers::ALL {
        reg.register_raw(desc.endpoint, desc.handler);
    }
    reg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
        assert_eq!(reg.endpoints().len(), 32);
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
        extra.insert(
            "required_caps".to_string(),
            serde_json::json!(["threads.read", "system.read"]),
        );
        let caps = extract_required_caps(&extra);
        assert_eq!(caps, vec!["threads.read", "system.read"]);
    }

    #[test]
    fn extract_required_caps_empty_when_missing() {
        let extra = HashMap::new();
        let caps = extract_required_caps(&extra);
        assert!(caps.is_empty());
    }

    // ── Availability-table tests ─────────────────────────────────
    //
    // `ryeos_executor::executor::availability_for_endpoint` takes a
    // descriptor slice; these tests anchor it against the daemon's
    // canonical `handlers::ALL`. Lives in daemon because that table is
    // daemon-owned.
    use ryeos_executor::executor::{availability_for_endpoint, ExecutionMode};

    #[test]
    fn availability_matches_descriptor_table() {
        for desc in handlers::ALL {
            assert_eq!(
                availability_for_endpoint(handlers::ALL, desc.endpoint).unwrap(),
                desc.availability,
                "availability_for_endpoint disagreed with descriptor for `{}`",
                desc.endpoint
            );
        }
    }

    #[test]
    fn mode_mismatch_standalone_daemon_only_errors() {
        let avail = availability_for_endpoint(handlers::ALL, "commands.submit").unwrap();
        assert_eq!(avail, ServiceAvailability::DaemonOnly);
        match (ExecutionMode::Standalone, avail) {
            (ExecutionMode::Standalone, ServiceAvailability::DaemonOnly) => {}
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn mode_mismatch_live_offline_only_errors() {
        let avail = availability_for_endpoint(handlers::ALL, "bundle.install").unwrap();
        assert_eq!(avail, ServiceAvailability::OfflineOnly);
        match (ExecutionMode::Live, avail) {
            (ExecutionMode::Live, ServiceAvailability::OfflineOnly) => {}
            other => panic!("unexpected: {:?}", other),
        }
    }
}
