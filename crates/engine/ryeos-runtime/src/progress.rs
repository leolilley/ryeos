//! Standard streaming event contract for `native_async` tools.
//!
//! Tools that declare `runtime.handlers.native_async = true` (or the
//! rich form) opt in to driving their own event stream during
//! execution. The engine does not enforce or interpret these events —
//! the `native_async` flag signals **intent only**: the tool promises
//! to emit progress/status updates and (optionally) artifacts via this
//! contract, and the daemon promises to respect the configured
//! cancellation policy when terminating the subprocess.
//!
//! Tools without `native_async` are free to emit nothing; their
//! lifecycle remains observed only via process exit + final result
//! capture, exactly as before.
//!
//! This module defines the typed payload shapes. The actual transport
//! is `CallbackClient::emit_progress`, `emit_status`, and the
//! pre-existing `publish_artifact` (which already accepts a structured
//! JSON value).

use serde::{Deserialize, Serialize};

/// Periodic progress update emitted by long-running native_async tools.
///
/// Convention: `phase` is a short machine-readable identifier
/// (`"download"`, `"compile"`, `"upload"`); `message` is a human
/// summary; `percent` is a 0.0–100.0 value when meaningful, or `None`
/// for indeterminate progress.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressEvent {
    pub phase: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f32>,
}

impl ProgressEvent {
    pub fn new(phase: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            message: message.into(),
            percent: None,
        }
    }

    pub fn with_percent(mut self, percent: f32) -> Self {
        self.percent = Some(percent);
        self
    }
}

/// Coarse-grained status update — typically emitted on lifecycle
/// transitions (`"connecting"`, `"ready"`, `"draining"`).
///
/// Distinct from `ProgressEvent` so consumers can render them
/// differently (status = sticky banner; progress = scrolling bar).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusEvent {
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl StatusEvent {
    pub fn new(state: impl Into<String>) -> Self {
        Self {
            state: state.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_serializes_without_optional_percent() {
        let p = ProgressEvent::new("download", "fetching index");
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["phase"], "download");
        assert_eq!(json["message"], "fetching index");
        assert!(json.get("percent").is_none());
    }

    #[test]
    fn progress_serializes_with_percent() {
        let p = ProgressEvent::new("upload", "sending bytes").with_percent(42.5);
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["percent"], 42.5);
    }

    #[test]
    fn progress_roundtrip() {
        let p = ProgressEvent::new("compile", "linking").with_percent(99.0);
        let s = serde_json::to_string(&p).unwrap();
        let back: ProgressEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn status_serializes_without_optional_detail() {
        let s = StatusEvent::new("ready");
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["state"], "ready");
        assert!(json.get("detail").is_none());
    }

    #[test]
    fn status_serializes_with_detail() {
        let s = StatusEvent::new("draining").with_detail("3 in-flight requests");
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["detail"], "3 in-flight requests");
    }

    #[test]
    fn status_roundtrip() {
        let s = StatusEvent::new("connecting").with_detail("attempt 2/3");
        let raw = serde_json::to_string(&s).unwrap();
        let back: StatusEvent = serde_json::from_str(&raw).unwrap();
        assert_eq!(s, back);
    }
}
