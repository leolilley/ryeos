//! Live route-table diagnostics snapshot.
//!
//! The compiled route table lives in the API layer's `ApiState`, which
//! service handlers cannot reach (they receive `Arc<AppState>`). The
//! daemon composition root publishes a diagnostic projection of the
//! table into `AppState::extensions` via this type once, at boot (a
//! running node's routes never change), so `service:system/routes` can
//! report what is actually loaded, and which public routes the node left
//! out, without a crate dependency cycle.

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

/// A public route the node left out at boot, and why.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkippedRouteDiagnostic {
    pub id: String,
    pub path: String,
    /// Stable code, e.g. `public_route_not_named`.
    pub code: String,
}

/// Snapshot holder published into `AppState::extensions`.
pub struct RouteDiagnostics {
    fingerprint: ArcSwap<String>,
    entries: ArcSwap<Vec<RouteDiagnosticEntry>>,
    skipped: ArcSwap<Vec<SkippedRouteDiagnostic>>,
}

impl RouteDiagnostics {
    pub fn new() -> Self {
        Self {
            fingerprint: ArcSwap::from_pointee(String::new()),
            entries: ArcSwap::from_pointee(Vec::new()),
            skipped: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// Publish the snapshot. Called by the composition root once the boot
    /// route table is built.
    pub fn publish(
        &self,
        fingerprint: String,
        entries: Vec<RouteDiagnosticEntry>,
        skipped: Vec<SkippedRouteDiagnostic>,
    ) {
        self.fingerprint.store(Arc::new(fingerprint));
        self.entries.store(Arc::new(entries));
        self.skipped.store(Arc::new(skipped));
    }

    pub fn skipped(&self) -> Arc<Vec<SkippedRouteDiagnostic>> {
        self.skipped.load_full()
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
            vec![SkippedRouteDiagnostic {
                id: "public/front".into(),
                path: "/".into(),
                code: "public_route_not_named".into(),
            }],
        );

        assert_eq!(diags.skipped().len(), 1);
        assert_eq!(diags.skipped()[0].code, "public_route_not_named");
        assert_eq!(diags.fingerprint().as_str(), "fp-1");
        assert_eq!(diags.entries().len(), 1);
        assert_eq!(diags.entries()[0].id, "core/execute");
    }
}
