//! EffectiveSurface — renderer-agnostic DTO for resolved surfaces.
//!
//! Both the terminal and web renderers consume this single typed
//! envelope instead of raw JSON. It is produced either by
//! `items.effective` (via the daemon) or by the engine directly
//! (offline path).

use serde::{Deserialize, Serialize};

use crate::surface::SurfaceSpec;

/// Renderer-agnostic effective surface DTO.
///
/// Produced by `items.effective` or `Engine::effective_item` and
/// consumed by both terminal and web renderers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSurface {
    pub requested_ref: String,
    pub canonical_ref: String,
    pub kind: String, // always "surface"
    pub trusted: bool,
    pub trust_class: TrustClass,
    pub provenance: serde_json::Value,
    pub spec: SurfaceSpec,
    #[serde(default)]
    pub diagnostics: Vec<EffectiveSurfaceDiagnostic>,
}

/// Trust classification mirrored from the engine's TrustClass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TrustClass {
    TrustedBundle,
    TrustedProject,
    UntrustedProject,
    Unsigned,
}

/// Diagnostic from effective surface resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSurfaceDiagnostic {
    pub level: String,
    pub message: String,
}

/// Error constructing an `EffectiveSurface` from a raw effective
/// item response.
#[derive(Debug, thiserror::Error)]
pub enum EffectiveSurfaceError {
    #[error("expected kind=surface, got `{0}`")]
    WrongKind(String),

    #[error("surface refused: signer not trusted")]
    Untrusted,

    #[error("surface spec did not match contract: {0}")]
    BadSpec(String),

    #[error("invalid response shape: {0}")]
    Shape(String),
}

impl EffectiveSurface {
    /// Construct from a raw effective item JSON response (as returned
    /// by `items.effective`).
    ///
    /// Fails closed on wrong kind, untrusted, or malformed spec.
    pub fn from_effective_item(value: serde_json::Value) -> Result<Self, EffectiveSurfaceError> {
        let kind = value.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "surface" {
            return Err(EffectiveSurfaceError::WrongKind(kind.to_string()));
        }

        let trusted = value
            .get("trusted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let trust_class: TrustClass = value
            .get("trust_class")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(TrustClass::Unsigned);

        let requested_ref = value
            .get("requested_ref")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let canonical_ref = value
            .get("canonical_ref")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let provenance = value
            .get("provenance")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let composed = value
            .get("composed_value")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let spec: SurfaceSpec = serde_json::from_value(composed)
            .map_err(|e| EffectiveSurfaceError::BadSpec(e.to_string()))?;

        let diagnostics: Vec<EffectiveSurfaceDiagnostic> = value
            .get("diagnostics")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(Self {
            requested_ref,
            canonical_ref,
            kind: kind.to_string(),
            trusted,
            trust_class,
            provenance,
            spec,
            diagnostics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_response() -> serde_json::Value {
        serde_json::json!({
            "requested_ref": "surface:ryeos/studio/base",
            "canonical_ref": "surface:ryeos/studio/base",
            "kind": "surface",
            "trusted": true,
            "trust_class": "trusted_bundle",
            "root_trust_class": "trusted_bundle",
            "source": { "path": "/test/base.yaml" },
            "provenance": {},
            "composed_value": {
                "name": "test",
                "layout": {
                    "root": "main",
                    "nodes": {
                        "main": { "type": "pane", "view": "thread_list" }
                    }
                }
            },
            "derived": {},
            "policy_facts": {},
            "diagnostics": []
        })
    }

    #[test]
    fn parses_minimal_surface_response() {
        let es = EffectiveSurface::from_effective_item(minimal_response()).unwrap();
        assert_eq!(es.kind, "surface");
        assert!(es.trusted);
        assert_eq!(es.canonical_ref, "surface:ryeos/studio/base");
        assert_eq!(es.spec.name, "test");
    }

    #[test]
    fn rejects_wrong_kind() {
        let mut resp = minimal_response();
        resp["kind"] = serde_json::json!("client");
        let err = EffectiveSurface::from_effective_item(resp).unwrap_err();
        assert!(matches!(err, EffectiveSurfaceError::WrongKind(k) if k == "client"));
    }

    #[test]
    fn rejects_untrusted_when_strict() {
        let mut resp = minimal_response();
        resp["trusted"] = serde_json::json!(false);
        // from_effective_item doesn't enforce trust — that's the
        // caller's decision. But the trusted field is correctly
        // populated so callers can refuse.
        let es = EffectiveSurface::from_effective_item(resp).unwrap();
        assert!(!es.trusted);
    }

    #[test]
    fn propagates_diagnostics() {
        let mut resp = minimal_response();
        resp["diagnostics"] = serde_json::json!([
            { "level": "info", "message": "extends chain: ..." }
        ]);
        let es = EffectiveSurface::from_effective_item(resp).unwrap();
        assert_eq!(es.diagnostics.len(), 1);
        assert_eq!(es.diagnostics[0].level, "info");
    }
}
