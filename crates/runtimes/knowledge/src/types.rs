//! Knowledge request types, output types, and error types.

use ryeos_runtime::op_wire::{GraphEdge, VerifiedItem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

// -------- Tagged request enum --------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum KnowledgeRequest {
    Compose(ComposePayload),
    ComposePositions(ComposeContextPayload),
    Query(serde_json::Value),
    Graph(serde_json::Value),
    Validate(serde_json::Value),
    Snapshot(serde_json::Value),
    Index(serde_json::Value),
}

// -------- Compose --------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposePayload {
    #[serde(flatten)]
    pub root: ryeos_runtime::op_wire::SingleRootPayload,
    pub inputs: ComposeInputs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeInputs {
    pub token_budget: usize,
    #[serde(default)]
    pub exclude_refs: Vec<String>,
    #[serde(default)]
    pub position: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeOutput {
    pub content: String,
    pub composition: ComposeMeta,
    pub tokens_used: usize,
    pub token_budget: usize,
    pub tokens_remaining: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeMeta {
    pub resolved_items: Vec<ComposeItem>,
    pub items_omitted: Vec<OmittedItem>,
    pub edges: Vec<ComposeEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeItem {
    pub item_id: String,
    pub role: ComposeRole,
    pub depth: usize,
    pub tokens: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComposeRole {
    Primary,
    Extends,
    Reference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OmittedItem {
    pub item_id: String,
    pub reason: OmissionReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OmissionReason {
    OverBudget,
    Excluded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeEdge {
    pub from: String,
    pub to: String,
    pub kind: ComposeEdgeKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposeEdgeKind {
    Extends,
    References,
}

// -------- ComposePositions (multi-root) --------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeContextPayload {
    pub items_by_ref: BTreeMap<String, VerifiedItem>,
    pub edges: Vec<GraphEdge>,
    pub roots_by_position: BTreeMap<String, Vec<String>>,
    pub per_position_budget: BTreeMap<String, usize>,
    pub default_budget: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedContexts {
    pub rendered: BTreeMap<String, RenderedPosition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedPosition {
    pub content: String,
    pub composition: ComposeMeta,
    pub tokens_used: usize,
    pub token_budget: usize,
}

// -------- Errors --------

#[derive(Debug, Error)]
pub enum KnowledgeError {
    #[error("op `{op}` not implemented in phase 2 (ships in phase {phase})")]
    NotImplemented { op: String, phase: u8 },

    #[error("invalid input for op `{op}`: {reason}")]
    InvalidInput { op: String, reason: String },

    #[error("frontmatter parse failure for {item_id}: {reason}")]
    FrontmatterParse { item_id: String, reason: String },

    #[error("malformed envelope: {0}")]
    MalformedEnvelope(String),

    #[error("internal: {0}")]
    Internal(String),
}
