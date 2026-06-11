//! Live route-table diagnostics snapshot.
//!
//! The compiled route table lives in the API layer's `ApiState`, which
//! service handlers cannot reach (they receive `Arc<AppState>`). The
//! daemon composition root publishes a diagnostic projection of the
//! table into `AppState::extensions` via this type — once at boot and
//! again on every route reload — so `service:system/routes` can report
//! what is actually loaded without a crate dependency cycle.

use std::sync::Arc;

use arc_swap::ArcSwap;

/// One loaded route, projected for operator diagnostics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RouteDiagnosticEntry {
    pub id: String,
    pub methods: Vec<String>,
    pub path: String,
    pub source_file: String,
    pub response_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_source: Option<String>,
}

/// Snapshot holder published into `AppState::extensions`.
pub struct RouteDiagnostics {
    fingerprint: ArcSwap<String>,
    entries: ArcSwap<Vec<RouteDiagnosticEntry>>,
}

impl RouteDiagnostics {
    pub fn new() -> Self {
        Self {
            fingerprint: ArcSwap::from_pointee(String::new()),
            entries: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// Replace the published snapshot. Called by the composition root
    /// after every successful route-table build.
    pub fn publish(&self, fingerprint: String, entries: Vec<RouteDiagnosticEntry>) {
        self.fingerprint.store(Arc::new(fingerprint));
        self.entries.store(Arc::new(entries));
    }

    pub fn fingerprint(&self) -> Arc<String> {
        self.fingerprint.load_full()
    }

    pub fn entries(&self) -> Arc<Vec<RouteDiagnosticEntry>> {
        self.entries.load_full()
    }
}

impl Default for RouteDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_replaces_snapshot() {
        let diags = RouteDiagnostics::new();
        assert!(diags.entries().is_empty());

        diags.publish(
            "fp-1".into(),
            vec![RouteDiagnosticEntry {
                id: "core/execute".into(),
                methods: vec!["POST".into()],
                path: "/execute".into(),
                source_file: "/x/execute.yaml".into(),
                response_mode: "execute".into(),
                response_source: None,
            }],
        );

        assert_eq!(diags.fingerprint().as_str(), "fp-1");
        assert_eq!(diags.entries().len(), 1);
        assert_eq!(diags.entries()[0].id, "core/execute");
    }
}
