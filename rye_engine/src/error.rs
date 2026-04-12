use std::path::PathBuf;

/// Structured engine errors.
///
/// Every resolution failure, trust failure, and planning failure is a
/// concrete variant — no silent fallbacks, no "try another format" paths.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    // ── Ref parsing ──────────────────────────────────────────────────

    #[error("malformed ref `{input}`: {reason}")]
    MalformedRef { input: String, reason: String },

    #[error("bare ref rejected (canonical refs required): `{input}`")]
    BareRefRejected { input: String },

    #[error("unsupported kind: `{kind}`")]
    UnsupportedKind { kind: String },

    #[error("invalid suffix on ref `{input}`: {reason}")]
    InvalidSuffix { input: String, reason: String },

    // ── Resolution ───────────────────────────────────────────────────

    #[error("item not found: `{canonical_ref}` (searched: {searched_spaces:?})")]
    ItemNotFound {
        canonical_ref: String,
        searched_spaces: Vec<String>,
    },

    #[error("ambiguous resolution for `{canonical_ref}`: multiple candidates in {space} ({candidates:?})")]
    AmbiguousResolution {
        canonical_ref: String,
        space: String,
        candidates: Vec<PathBuf>,
    },

    #[error("pinned version not found for `{canonical_ref}`: {reason}")]
    PinnedVersionNotFound {
        canonical_ref: String,
        reason: String,
    },

    #[error("invalid project context: {reason}")]
    InvalidProjectContext { reason: String },

    #[error("project context materialization failed: {reason}")]
    ProjectContextMaterializationFailed { reason: String },

    #[error("bundle discovery failed: {reason}")]
    BundleDiscoveryFailed { reason: String },

    // ── Extension registry ───────────────────────────────────────────

    #[error("no extensions registered for kind `{kind}` — extractor YAML missing or empty")]
    NoExtensionsForKind { kind: String },

    #[error("schema loader error: {reason}")]
    SchemaLoaderError { reason: String },

    #[error("parser not registered: `{parser_id}`")]
    ParserNotRegistered { parser_id: String },

    // ── Trust & verification ─────────────────────────────────────────

    #[error("signature missing on `{canonical_ref}`")]
    SignatureMissing { canonical_ref: String },

    #[error("signature verification failed for `{canonical_ref}`: {reason}")]
    SignatureVerificationFailed {
        canonical_ref: String,
        reason: String,
    },

    #[error("untrusted signer `{fingerprint}` for `{canonical_ref}`")]
    UntrustedSigner {
        canonical_ref: String,
        fingerprint: String,
    },

    #[error("content hash mismatch for `{canonical_ref}`: expected {expected}, got {actual}")]
    ContentHashMismatch {
        canonical_ref: String,
        expected: String,
        actual: String,
    },

    // ── Planning ─────────────────────────────────────────────────────

    #[error("executor not found: `{executor_id}`")]
    ExecutorNotFound { executor_id: String },

    #[error("invalid metadata on `{canonical_ref}`: {reason}")]
    InvalidMetadata {
        canonical_ref: String,
        reason: String,
    },

    #[error("unresolved nested ref `{nested_ref}` during planning of `{parent_ref}`: {reason}")]
    UnresolvedNestedRef {
        parent_ref: String,
        nested_ref: String,
        reason: String,
    },

    #[error("invalid parameter expression: {reason}")]
    InvalidParameterExpression { reason: String },

    #[error("disallowed capability requirement `{capability}` on `{canonical_ref}`")]
    DisallowedCapability {
        canonical_ref: String,
        capability: String,
    },

    #[error("cycle detected during planning: {cycle:?}")]
    CycleDetected { cycle: Vec<String> },

    // ── Execution ────────────────────────────────────────────────────

    #[error("execution failed: {reason}")]
    ExecutionFailed { reason: String },

    #[error("budget exhausted for thread `{thread_id}`")]
    BudgetExhausted { thread_id: String },

    // ── Scope & budget ─────────────────────────────────────────────

    #[error("insufficient scope: required `{required}`, available: {available:?}")]
    InsufficientScope {
        required: String,
        available: Vec<String>,
    },

    #[error("invalid budget: {reason}")]
    InvalidBudget { reason: String },

    // ── Lifecycle ────────────────────────────────────────────────────

    #[error("invalid state transition from `{from}` on event `{event}`")]
    InvalidStateTransition { from: String, event: String },

    // ── Delegation ───────────────────────────────────────────────────

    #[error("delegated principal validation failed: {reason}")]
    DelegationValidationFailed { reason: String },

    // ── Internal ─────────────────────────────────────────────────────

    #[error("internal engine error: {0}")]
    Internal(String),
}
