//! Knowledge request types, output types, and error types.

use ryeos_runtime::op_wire::{GraphEdge, VerifiedItem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

// Operations are dispatched by wire op name in `dispatch.rs` (keyed off
// `BatchOpEnvelope.op`), each parsing its own typed payload below. There is
// deliberately no enum mirror of the schema-declared op vocabulary here.

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

// -------- Query (BM25 lexical retrieval) --------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPayload {
    pub items_by_ref: BTreeMap<String, VerifiedItem>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
    pub inputs: QueryInputs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryInputs {
    pub query: String,
    #[serde(default = "default_query_limit")]
    pub limit: usize,
    #[serde(default)]
    pub include_content: bool,
    #[serde(default)]
    pub filters: QueryFilters,
}

fn default_query_limit() -> usize {
    10
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryFilters {
    #[serde(default)]
    pub ref_prefixes: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryOutput {
    pub query: String,
    pub matches: Vec<QueryMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryMatch {
    pub item_ref: String,
    pub score: f64,
    pub title: Option<String>,
    pub excerpt: String,
    pub metadata: serde_json::Value,
    pub raw_content_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

// -------- Graph (adjacency / reachable subgraph) --------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphPayload {
    pub items_by_ref: BTreeMap<String, VerifiedItem>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
    pub inputs: GraphInputs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphInputs {
    /// Roots to traverse from. Empty means "every item is a root" — the
    /// whole provided corpus is returned.
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default = "default_graph_depth")]
    pub depth: usize,
}

fn default_graph_depth() -> usize {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphOutput {
    /// Refs reachable from `roots` within `depth`, sorted.
    pub nodes: Vec<String>,
    /// Edges whose endpoints are both within the reachable set.
    pub edges: Vec<GraphEdgeOut>,
    pub roots: Vec<String>,
    /// Edge endpoints that are not present in `items_by_ref`.
    pub missing_refs: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdgeOut {
    pub from: String,
    pub to: String,
    pub kind: String,
}

// -------- Validate (corpus + reference integrity) --------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatePayload {
    pub items_by_ref: BTreeMap<String, VerifiedItem>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
    // Op inputs are nested under `inputs` by the executor (it merges
    // schema-validated op args under `payload.inputs`), so `roots` must
    // live here — NOT at the top level, where it would be silently dropped.
    #[serde(default)]
    pub inputs: ValidateInputs,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidateInputs {
    #[serde(default)]
    pub roots: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateOutput {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub item_count: usize,
    pub edge_count: usize,
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

    #[error("internal: {0}")]
    Internal(String),
}
