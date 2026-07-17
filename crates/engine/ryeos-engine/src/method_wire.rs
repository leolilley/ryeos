//! Generic method-runtime wire contract.
//!
//! These types live beside the signed protocol vocabulary so the engine,
//! daemon, and runtime deserialize one exact `method_call_*_v1` shape. Runtime
//! crates re-export them; no kind-specific crate owns the wire.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::launch_envelope_types::EnvelopeCallback;

pub const METHOD_CALL_SCHEMA_VERSION: u32 = 1;

/// The envelope for any method call on a kind's selected runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodCallEnvelope {
    pub schema_version: u32,
    pub kind: String,
    pub method: String,
    pub thread_id: String,
    pub callback: EnvelopeCallback,
    /// Callback authorization/state anchor. This may differ from
    /// `project_root` when execution deliberately overrides its state root.
    pub callback_project_path: PathBuf,
    /// Source project root used for item/config resolution.
    pub project_root: PathBuf,
    /// Daemon-resolved runtime/operator config snapshots keyed by the invoked
    /// method's declared config name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_config: BTreeMap<String, serde_json::Value>,
    /// Method-specific payload; deserialized by the runtime binary.
    pub payload: serde_json::Value,
}

impl MethodCallEnvelope {
    pub fn validate_schema_version(&self) -> Result<(), String> {
        if self.schema_version != METHOD_CALL_SCHEMA_VERSION {
            return Err(format!(
                "unsupported method-call schema version {}; expected {METHOD_CALL_SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        Ok(())
    }
}

/// What a method runtime writes to terminal stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodCallResult {
    pub success: bool,
    pub kind: String,
    pub method: String,
    pub output: Option<serde_json::Value>,
    pub error: Option<MethodCallError>,
    pub warnings: Vec<String>,
}

impl MethodCallResult {
    pub fn success(envelope: &MethodCallEnvelope, output: serde_json::Value) -> Self {
        Self {
            success: true,
            kind: envelope.kind.clone(),
            method: envelope.method.clone(),
            output: Some(output),
            error: None,
            warnings: Vec::new(),
        }
    }

    pub fn failure(envelope: &MethodCallEnvelope, error: MethodCallError) -> Self {
        Self {
            success: false,
            kind: envelope.kind.clone(),
            method: envelope.method.clone(),
            output: None,
            error: Some(error),
            warnings: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match (self.success, self.output.is_some(), self.error.is_some()) {
            (true, true, false) | (false, false, true) => Ok(()),
            (true, output, error) => Err(format!(
                "successful method result must contain output and no error; got output={output}, error={error}"
            )),
            (false, output, error) => Err(format!(
                "failed method result must contain an error and no output; got output={output}, error={error}"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum MethodCallError {
    MethodFailed {
        reason: String,
    },
    NotImplemented {
        method: String,
        phase: u8,
    },
    UnknownMethod {
        kind: String,
        requested: String,
        declared: Vec<String>,
    },
    InvalidArg {
        method: String,
        field: Option<String>,
        reason: String,
    },
}

// Generic verified-item + edge payload types for daemon→method-runtime calls.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedItem {
    pub raw_content: String,
    pub raw_content_digest: String,
    pub metadata: serde_json::Value,
    pub trust_class: TrustClass,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub depth_from_root: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Extends,
    References,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustClass {
    TrustedBundle,
    TrustedProject,
    UntrustedProject,
    Unsigned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleRootPayload {
    pub root_ref: String,
    pub items_by_ref: BTreeMap<String, VerifiedItem>,
    pub edges: Vec<GraphEdge>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_envelope_rejects_unknown_schema_version() {
        let envelope = MethodCallEnvelope {
            schema_version: METHOD_CALL_SCHEMA_VERSION + 1,
            kind: "knowledge".to_string(),
            method: "compose".to_string(),
            thread_id: "T-test".to_string(),
            callback: EnvelopeCallback {
                socket_path: PathBuf::from("/tmp/daemon.sock"),
                token: "token".to_string(),
            },
            callback_project_path: PathBuf::from("/project"),
            project_root: PathBuf::from("/project"),
            runtime_config: BTreeMap::new(),
            payload: serde_json::Value::Null,
        };
        assert!(envelope.validate_schema_version().is_err());
    }

    #[test]
    fn method_result_rejects_incoherent_success_and_failure_shapes() {
        let mut envelope = MethodCallEnvelope {
            schema_version: METHOD_CALL_SCHEMA_VERSION,
            kind: "knowledge".to_string(),
            method: "compose".to_string(),
            thread_id: "T-test".to_string(),
            callback: EnvelopeCallback {
                socket_path: PathBuf::from("/tmp/daemon.sock"),
                token: "token".to_string(),
            },
            callback_project_path: PathBuf::from("/project"),
            project_root: PathBuf::from("/project"),
            runtime_config: BTreeMap::new(),
            payload: serde_json::Value::Null,
        };
        let success = MethodCallResult::success(&envelope, serde_json::Value::Null);
        assert!(success.validate().is_ok());

        let mut invalid_success = success.clone();
        invalid_success.output = None;
        assert!(invalid_success.validate().is_err());

        let failure = MethodCallResult::failure(
            &envelope,
            MethodCallError::MethodFailed {
                reason: "failed".to_string(),
            },
        );
        assert!(failure.validate().is_ok());

        envelope.method = "query".to_string();
        let mut invalid_failure = MethodCallResult::failure(
            &envelope,
            MethodCallError::MethodFailed {
                reason: "failed".to_string(),
            },
        );
        invalid_failure.output = Some(serde_json::Value::Null);
        assert!(invalid_failure.validate().is_err());
    }
}
