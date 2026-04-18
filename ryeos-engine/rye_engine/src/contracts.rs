//! Data contracts — the seam between daemon and engine.
//!
//! These are the concrete structs that flow across the boundary.
//! The daemon calls concrete `Engine` methods and receives these types.
//! No trait boundary, no dyn dispatch at the seam.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;

// ── Signature envelope ───────────────────────────────────────────────

/// How a `rye:signed:...` payload is embedded in a source file.
///
/// Varies by file type — loaded from extractor YAML, never hardcoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureEnvelope {
    /// Comment prefix character(s), e.g. `"#"`, `"//"`, `"<!--"`
    pub prefix: String,
    /// Optional comment suffix, e.g. `"-->"` for markdown/HTML wrapping
    pub suffix: Option<String>,
    /// Whether the signature line goes after a shebang line
    pub after_shebang: bool,
}

// ── Source format (carried on each ResolvedItem) ─────────────────────

/// Per-item format facts derived from the `KindRegistry` during
/// resolution. Downstream consumers (trust verification, chain builder)
/// use this instead of consulting the full registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSourceFormat {
    /// The matched file extension, e.g. `".py"`, `".md"`
    pub extension: String,
    /// Parser ID for lightweight metadata extraction, e.g. `"python/ast"`
    pub parser_id: String,
    /// Signature embedding envelope for this file type
    pub signature: SignatureEnvelope,
}

// ── Item spaces ──────────────────────────────────────────────────────

/// The three-tier resolution space where an item was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemSpace {
    Project,
    User,
    System,
}

impl ItemSpace {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::System => "system",
        }
    }
}

// ── Project context ──────────────────────────────────────────────────

/// Portable project identity for local and remote execution.
///
/// Always present on requests. `None` is the explicit "no project" variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectContext {
    None,
    LocalPath { path: PathBuf },
    SnapshotHash { hash: String },
    ProjectRef { principal: String, ref_name: String },
}

// ── Materialized project context ─────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MaterializedProjectContext {
    pub project_root: Option<PathBuf>,
    pub source: ProjectContext,
}

// ── Signature header ─────────────────────────────────────────────────

/// Parsed canonical signed payload extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureHeader {
    pub timestamp: String,
    pub content_hash: String,
    pub signature_b64: String,
    pub signer_fingerprint: String,
}

// ── Item metadata ────────────────────────────────────────────────────

/// Lightweight metadata extracted during resolution.
///
/// Contains enough to discover the executor and build a dispatch plan.
/// Full body parsing is the executor/adapter's responsibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemMetadata {
    /// Executor ID from `__executor_id__` or frontmatter
    pub executor_id: Option<String>,
    /// Item version from `__version__`
    pub version: Option<String>,
    /// Item description
    pub description: Option<String>,
    /// Item category
    pub category: Option<String>,
    /// Vault secret IDs this item requires (e.g. `["openai-api-key"]`).
    /// The daemon resolves these per-principal and injects as `RYE_VAULT_*` env vars.
    #[serde(default)]
    pub required_secrets: Vec<String>,
    /// Arbitrary additional metadata fields (kind-specific fields live here)
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

// ── Resolution output ────────────────────────────────────────────────

/// Result of successful item resolution.
#[derive(Debug, Clone)]
pub struct ResolvedItem {
    pub canonical_ref: CanonicalRef,
    pub kind: String,
    pub source_path: PathBuf,
    pub source_space: ItemSpace,
    pub materialized_project_root: Option<PathBuf>,
    pub content_hash: String,
    pub signature_header: Option<SignatureHeader>,
    pub source_format: ResolvedSourceFormat,
    pub metadata: ItemMetadata,
}

// ── Trust classes ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustClass {
    /// Signed by a trusted signer
    Trusted,
    /// Signed but signer is not in the trust store
    Untrusted,
    /// Unsigned item (may be allowed in dev/project space)
    Unsigned,
}

/// Signer identity from the signature header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerFingerprint(pub String);

/// Pinned version reference for signed/temporal resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedVersion {
    pub content_hash: String,
    pub signature: String,
}

// ── Verification output ──────────────────────────────────────────────

/// Result of trust and integrity verification.
#[derive(Debug, Clone)]
pub struct VerifiedItem {
    pub resolved: ResolvedItem,
    pub signer: Option<SignerFingerprint>,
    pub trust_class: TrustClass,
    pub pinned_version: Option<PinnedVersion>,
}

// ── Launch mode ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Inline,
    Detached,
}

// ── Execution hints ──────────────────────────────────────────────────

/// Open map passed through to executors. The engine does not interpret
/// its contents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionHints {
    #[serde(flatten)]
    pub values: HashMap<String, Value>,
}

// ── Principal ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    pub fingerprint: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegatedPrincipal {
    pub protocol_version: String,
    pub delegation_id: String,
    pub caller_fingerprint: String,
    pub origin_site_id: String,
    pub audience_site_id: String,
    pub delegated_scopes: Vec<String>,
    pub budget_lease_id: Option<String>,
    pub request_hash: String,
    pub idempotency_key: String,
    pub issued_at: String,
    pub expires_at: String,
    pub non_redelegable: bool,
    pub origin_signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectivePrincipal {
    Local(Principal),
    Delegated(DelegatedPrincipal),
}

// ── Plan context (resolve/verify/build_plan) ─────────────────────────

/// Context for the planning phases: resolve, verify, build_plan.
///
/// Does NOT carry thread IDs or daemon runtime bindings.
/// This is what makes `validate_only` safe.
#[derive(Debug, Clone)]
pub struct PlanContext {
    pub requested_by: EffectivePrincipal,
    pub project_context: ProjectContext,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub execution_hints: ExecutionHints,
    /// When true, the daemon should not call `execute_plan` after
    /// `build_plan` succeeds. The engine does not enforce this — it is
    /// safe structurally because `PlanContext` does not carry thread IDs.
    pub validate_only: bool,
}

// ── Engine context (execute_plan) ────────────────────────────────────

/// Context for plan execution. Carries everything in `PlanContext` plus
/// daemon-allocated thread identity and runtime bindings.
#[derive(Debug, Clone)]
pub struct EngineContext {
    pub thread_id: String,
    pub chain_root_id: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_site_id: Option<String>,
    pub upstream_thread_id: Option<String>,
    pub continuation_from_id: Option<String>,
    pub requested_by: EffectivePrincipal,
    pub project_context: ProjectContext,
    pub launch_mode: LaunchMode,
}

// ── Plan IR ──────────────────────────────────────────────────────────

/// Unique identifier for a plan node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanNodeId(pub String);

/// Plan capabilities declared by the execution plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanCapabilities {
    pub requires_model: bool,
    pub requires_subprocess: bool,
    pub requires_network: bool,
    pub custom: Vec<String>,
}

/// Materialization requirement for plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializationRequirement {
    pub kind: String,
    pub ref_string: String,
}

/// A node in the execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "node_type", rename_all = "snake_case")]
pub enum PlanNode {
    DispatchSubprocess {
        id: PlanNodeId,
        script_path: PathBuf,
        interpreter: Option<String>,
        working_directory: Option<PathBuf>,
        environment: HashMap<String, String>,
        arguments: Vec<String>,
        /// Daemon-injected env vars declared by the engine, filled at execution time.
        #[serde(default)]
        runtime_bindings: HashMap<String, String>,
    },
    SpawnChild {
        id: PlanNodeId,
        child_ref: String,
        thread_kind: String,
        edge_type: String,
    },
    Complete {
        id: PlanNodeId,
    },
}

impl PlanNode {
    pub fn id(&self) -> &PlanNodeId {
        match self {
            Self::DispatchSubprocess { id, .. }
            | Self::SpawnChild { id, .. }
            | Self::Complete { id, .. } => id,
        }
    }
}

/// Normalized execution plan — the engine's output from `build_plan`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub plan_id: String,
    pub root_executor_id: String,
    pub root_ref: String,
    pub item_kind: String,
    pub nodes: Vec<PlanNode>,
    pub entrypoint: PlanNodeId,
    pub capabilities: PlanCapabilities,
    pub materialization_requirements: Vec<MaterializationRequirement>,
    pub cache_key: String,
    /// Daemon supervision profile hint, derived from the root item's kind.
    #[serde(default)]
    pub thread_kind: Option<String>,
    /// Executor IDs traversed during chain resolution.
    #[serde(default)]
    pub executor_chain: Vec<String>,
}

// ── Execution completion ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadTerminalStatus {
    Completed,
    Failed,
    Cancelled,
    Continued,
    Killed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionArtifact {
    pub artifact_type: String,
    pub uri: String,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalCost {
    #[serde(default)]
    pub turns: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub spend: f64,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinuationRequest {
    pub reason: String,
    pub successor_parameters: Option<Value>,
}

/// Structured completion returned from plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCompletion {
    pub status: ThreadTerminalStatus,
    #[serde(default)]
    pub outcome_code: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub artifacts: Vec<ExecutionArtifact>,
    #[serde(default)]
    pub final_cost: Option<FinalCost>,
    #[serde(default)]
    pub continuation_request: Option<ContinuationRequest>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

// ── Budget lease / settlement (daemon-to-daemon) ─────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetLease {
    pub lease_id: String,
    pub issuer_site_id: String,
    pub parent_thread_id: String,
    pub reserved_max_spend: f64,
    pub issued_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendReport {
    pub lease_id: String,
    pub spend_report_id: String,
    pub report_seq: i64,
    pub amount: f64,
    #[serde(default)]
    pub runtime_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalSettlement {
    pub lease_id: String,
    pub settlement_id: String,
    pub final_spend: f64,
    pub terminal_status: String,
}

// ── Capability lease / settlement ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityLease {
    pub lease_id: String,
    pub capability_id: String,
    pub issuer_site_id: String,
    pub parent_thread_id: String,
    pub max_uses: i64,
    pub constraints_hash: String,
    pub issued_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityUseReport {
    pub lease_id: String,
    pub use_report_id: String,
    pub report_seq: i64,
    pub uses: i64,
    #[serde(default)]
    pub runtime_metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityFinalSettlement {
    pub lease_id: String,
    pub settlement_id: String,
    pub final_use_count: i64,
    pub terminal_status: String,
}

// ── Event contracts ──────────────────────────────────────────────────

/// What adapters and the engine send to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAppendRequest {
    pub event_type: String,
    pub payload: Value,
}

/// What the daemon persists and streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event_id: String,
    pub thread_id: String,
    pub chain_root_id: String,
    pub site_id: String,
    pub event_type: String,
    pub payload: Value,
    pub timestamp: String,
    pub sequence: i64,
}


