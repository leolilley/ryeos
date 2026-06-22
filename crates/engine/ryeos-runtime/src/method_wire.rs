//! Generic method-call runtime wire protocol.
//!
//! Types for the daemon → method-runtime subprocess protocol. These are
//! kind-agnostic: ANY kind whose schema declares `methods` uses this
//! wire shape. The daemon builds `MethodCallEnvelope`, the runtime writes
//! `MethodCallResult` to stdout.
//!
//! Lives in `ryeos-runtime` (not in any kind-specific crate) so the
//! daemon can import it without depending on a kind-specific library.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::envelope::EnvelopeCallback;

/// The envelope for any method call on a kind's runtime.
///
/// Single-mode: the runtime always operates as a thread, with a
/// `thread_id` and a `callback` endpoint. There is no helper or
/// in-process variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodCallEnvelope {
    pub schema_version: u32,
    pub kind: String,
    pub method: String,
    pub thread_id: String,
    pub callback: EnvelopeCallback,
    pub project_root: PathBuf,
    /// Method-specific payload; deserialized by the runtime binary.
    pub payload: serde_json::Value,
}

/// What the runtime writes to stdout.
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

// -------- Generic verified-item + edge types --------
//
// These are the wire shapes for daemon→runtime communication.

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

/// Generic single-root payload: a root ref, a set of verified items,
/// and a DAG of edges. Any kind's single-root method uses this shape.
/// Method-specific args (budget, exclusions, etc.) are merged into the
/// envelope's `payload` JSON by the daemon alongside this structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleRootPayload {
    pub root_ref: String,
    pub items_by_ref: std::collections::BTreeMap<String, VerifiedItem>,
    pub edges: Vec<GraphEdge>,
}
