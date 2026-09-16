use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::binding::{UiBindingRequest, UiBindingRequestBounds};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsEffect {
    pub id: u64,
    pub kind: RyeOsEffectKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsEffectKind {
    /// Fetch one source from the daemon-compiled session binding. The
    /// renderer receives a coordinate and bounded payload, never the source's
    /// executable item ref or capability requirements.
    FetchSource {
        tile_id: String,
        request: UiBindingRequest,
        request_bounds: UiBindingRequestBounds,
    },
    /// Invoke one content-declared affordance through the daemon-compiled
    /// session binding. The signed view owns the target and substitution
    /// template; the client sends only the producer payload.
    InvokeBinding {
        request: UiBindingRequest,
        request_bounds: UiBindingRequestBounds,
        intent: InvokeIntent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        success_notice: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route_seq: Option<u64>,
        #[serde(default)]
        ratchet_on_thread_id: bool,
    },
    SetLocationHash {
        hash: String,
    },
    CopyToClipboard {
        text: String,
    },
    OpenUrl {
        url: String,
    },
    /// Replace an immutable UI authority generation. Every renderer redeems
    /// the one-shot URL through the predecessor session, then adopts only the
    /// authenticated successor and reloads its compiled presentation.
    ReplaceSession {
        session_id: String,
        launch_url: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsEffectResult {
    pub id: u64,
    pub ok: bool,
    pub kind: RyeOsEffectResultKind,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<RyeOsUiError>,
}

/// Renderer-neutral failure returned by a platform effect or transport.
///
/// Keep the stable machine code, retryability, outcome certainty and
/// remediation separate from presentation copy. In particular, an unknown
/// mutation outcome is not an ordinary retryable refusal: renderers must
/// preserve the effect coordinate and offer observation/recovery rather than
/// submitting the operation again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsUiError {
    pub code: String,
    /// Canonical daemon error envelopes call this field `error`; Rust keeps the
    /// less ambiguous `message` name at presentation call sites.
    #[serde(rename = "error")]
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub outcome: RyeOsEffectOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Bounded structured fields retained for exact inspection. Renderers must
    /// treat this as inert data, never executable presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl RyeOsUiError {
    pub fn definite(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            outcome: RyeOsEffectOutcome::Refused,
            remediation: None,
            details: None,
        }
    }

    pub fn outcome_unknown(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            outcome: RyeOsEffectOutcome::Unknown,
            ..Self::definite(code, message)
        }
    }

    /// Fail closed if an adapter receives a contradictory wire envelope.
    /// Unknown mutation contact is observable/recoverable, never an automatic
    /// retry merely because an upstream transport also said `retryable`.
    pub fn normalized(mut self) -> Self {
        if self.outcome == RyeOsEffectOutcome::Unknown {
            self.retryable = false;
        }
        self
    }
}

impl From<String> for RyeOsUiError {
    fn from(message: String) -> Self {
        Self::definite("platform_effect_failed", message)
    }
}

impl From<&str> for RyeOsUiError {
    fn from(message: &str) -> Self {
        Self::from(message.to_string())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsEffectOutcome {
    #[default]
    Refused,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsEffectResultKind {
    BindingInvoked,
    SourceData,
    BrowserOnly,
}

/// Whether a generic invocation launches/continues a conversation or is a
/// discrete service/command intent. Set at the emit site (each site knows its
/// own intent); the result handler branches on this rather than sniffing the
/// target ref — the structural facts (`route_seq`/`ratchet`) are ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvokeIntent {
    /// Launches or continues a conversation (the routed foot input); the result
    /// runs the delivery/ratchet tower.
    Launch,
    /// A discrete service or command intent (row-management affordances); the
    /// result refreshes the affected surface and preserves the input.
    Service,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_error_uses_canonical_wire_field_and_unknown_is_never_retryable() {
        let mut error = RyeOsUiError::outcome_unknown("contact_unknown", "observe first");
        error.retryable = true;
        let value = serde_json::to_value(error.normalized()).unwrap();
        assert_eq!(value["error"], "observe first");
        assert!(value.get("message").is_none());
        assert_eq!(value["outcome"], "unknown");
        assert_eq!(value["retryable"], false);
    }
}
