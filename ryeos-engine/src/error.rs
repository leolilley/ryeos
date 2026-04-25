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

    #[error("executor chain exceeded max depth {max_depth}; chain: {chain:?}")]
    ChainTooDeep { max_depth: usize, chain: Vec<String> },

    #[error("invalid runtime config in {path}: {reason}")]
    InvalidRuntimeConfig { path: String, reason: String },

    #[error("multiple runtime configs in chain: {chain:?}")]
    MultipleRuntimeConfigs { chain: Vec<String> },

    #[error("no runtime config found in executor chain: {chain:?}")]
    NoRuntimeConfig { chain: Vec<String> },

    #[error("unknown template token: {{{token}}}")]
    UnknownTemplateToken { token: String },

    #[error("template requires {{{token}}} but no {token} available")]
    TemplateMissingContext { token: String },

    #[error("unknown runtime block `{key}` on kind `{kind}` at {source_path:?} \
             (no handler registered and not in the kind schema's `runtime.ignored_keys`)")]
    UnknownRuntimeBlock {
        key: String,
        kind: String,
        source_path: PathBuf,
    },

    #[error("runtime block `{key}` is declared as Singleton but appears on \
             multiple chain elements: {paths:?}")]
    DuplicateSingletonBlock {
        key: String,
        paths: Vec<PathBuf>,
    },

    #[error("parameter validation failed for `{tool}`:\n  - {}", errors.join("\n  - "))]
    ParameterValidationFailed {
        tool: String,
        errors: Vec<String>,
    },

    #[error("runtime binary not found: {binary}")]
    RuntimeBinaryNotFound { binary: String },

    #[error("runtime config uses reserved env key: {key}")]
    ReservedEnvKey { key: String },

    #[error("unknown executor alias `{alias}` for kind `{kind}`")]
    UnknownAlias { alias: String, kind: String },

    #[error("item `{item_ref}` does not declare an executor_id")]
    MissingExecutorId { item_ref: String },

    #[error("kind `{kind}` is not executable (no execution block in kind schema)")]
    KindNotExecutable { kind: String },

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
