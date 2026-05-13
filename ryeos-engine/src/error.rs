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

    /// Path-anchoring validator caught a mismatch between metadata
    /// and on-disk location (or a missing required field). The inner
    /// `MetadataAnchoringError` carries the structured detail; the
    /// canonical ref pins which item triggered the failure.
    #[error("metadata anchoring failed for `{canonical_ref}`: {source}")]
    MetadataAnchoringFailed {
        canonical_ref: String,
        #[source]
        source: Box<crate::kind_registry::MetadataAnchoringError>,
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

    #[error("invalid `bin:` prefix in command `{raw}`: {detail}")]
    InvalidBinPrefix { raw: String, detail: String },

    #[error("binary `{bin}` not found in bundle (searched: {searched})")]
    BinNotFound { bin: String, searched: String },

    #[error("bundle manifest missing: no refs/bundles/manifest under bundle root at {bundle_root}")]
    BinManifestMissing { bundle_root: String },

    #[error("binary `{bin}` not in manifest (triple {triple})")]
    BinNotInManifest { bin: String, triple: String },

    #[error("binary `{bin}` hash mismatch: manifest declares {declared}, on-disk computed {computed}")]
    BinHashMismatch {
        bin: String,
        declared: String,
        computed: String,
    },

    #[error("binary `{bin}` not trusted: signature from fingerprint `{fingerprint}` not in trust store")]
    BinUntrusted { bin: String, fingerprint: String },

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

    #[error("host env passthrough ${{{var}}} not allowed by daemon policy (RYEOS_TOOL_ENV_PASSTHROUGH allowlist)")]
    HostEnvPassthroughNotAllowed { var: String },

    #[error("host env passthrough ${{{var}}} is allowed but {var} is unset in the daemon process env")]
    HostEnvPassthroughMissing { var: String },

    #[error("reserved host env passthrough ${{{var}}} is not allowed (RYEOS_* prefix is reserved)")]
    ReservedHostEnvPassthrough { var: String },

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

    // ── Runtime registry ─────────────────────────────────────────────

    #[error("runtime YAML invalid at {path}: {reason}")]
    RuntimeYamlInvalid { path: PathBuf, reason: String },

    #[error("runtime `{runtime}` declares abi_version `{found}` but daemon supports `{expected}`")]
    AbiVersionMismatch {
        runtime: String,
        expected: String,
        found: String,
    },

    #[error("no runtime registered for kind `{kind}`")]
    NoRuntimeFor { kind: String },

    #[error(
        "kind `{kind}` has multiple registered runtimes ({}) but none is marked `default: true`; \
         caller must use an explicit `runtime:` override",
        candidates.join(", ")
    )]
    RuntimeDefaultRequired {
        kind: String,
        candidates: Vec<String>,
    },

    #[error(
        "kind `{kind}` has multiple runtimes marked `default: true`: {}",
        defaults.join(", ")
    )]
    MultipleRuntimeDefaults {
        kind: String,
        defaults: Vec<String>,
    },

    #[error("runtime `{runtime}` serves kind `{kind}` whose terminator declares protocol `{found}`, expected `{expected}`")]
    RuntimeProtocolMismatch {
        runtime: String,
        kind: String,
        expected: String,
        found: String,
    },

    #[error("runtime `{runtime}` serves unknown kind `{kind}`")]
    RuntimeServesUnknownKind {
        kind: String,
        runtime: String,
    },

    #[error("runtime `{runtime}` serves kind `{kind}` which has no `execution:` block")]
    RuntimeServesKindNoExecution {
        kind: String,
        runtime: String,
    },

    #[error("duplicate runtime canonical ref `{canonical_ref}` across bundle roots")]
    DuplicateRuntimeRef {
        canonical_ref: String,
    },

    // ── Inventory ────────────────────────────────────────────────────

    /// A per-item failure during inventory construction. The inner
    /// `EngineError` is preserved verbatim so the launcher can map
    /// resolution / parser / verification failures to the appropriate
    /// classification (4xx vs 5xx) without re-parsing strings.
    #[error("inventory[{kind}:{bare_id}]: {source}")]
    InventoryItemFailed {
        kind: String,
        bare_id: String,
        #[source]
        source: Box<EngineError>,
    },

    /// Two inventoried items in the same kind produced the same
    /// flattened API-safe name. Silent shadowing here would let one
    /// tool overwrite another in runtime dispatchers — fail loud.
    #[error(
        "inventory[{kind}]: duplicate flattened name `{flattened}` from \
         `{first_item_id}` and `{second_item_id}` (rename one of the source items)"
    )]
    DuplicateInventoryName {
        kind: String,
        flattened: String,
        first_item_id: String,
        second_item_id: String,
    },

    // ── Internal ─────────────────────────────────────────────────────

    #[error("internal engine error: {0}")]
    Internal(String),

    // ── Handler subprocess dispatch ──────────────────────────────────

    #[error("parser `{parser_id}` failed: {kind:?}: {message}")]
    ParserFailed {
        parser_id: String,
        kind: ParseErrKind,
        message: String,
    },

    #[error("handler `{handler}` spawn failed: {detail}")]
    HandlerSpawnFailed { handler: String, detail: String },

    #[error("handler `{handler}` exited with code {exit_code}; stderr: {stderr}")]
    HandlerExitNonZero {
        handler: String,
        exit_code: i32,
        stderr: String,
    },

    #[error("handler `{handler}` returned malformed response: {detail}")]
    HandlerProtocolViolation { handler: String, detail: String },

    #[error("handler error: {0}")]
    Handler(Box<crate::handlers::HandlerError>),

    // ── Protocol references ────────────────────────────────────────

    #[error("kind `{kind}` references unknown protocol `{protocol_ref}`: {source}")]
    KindReferencesUnknownProtocol {
        kind: String,
        protocol_ref: String,
        #[source]
        source: Box<crate::protocols::ProtocolError>,
    },
}

impl From<crate::handlers::HandlerError> for EngineError {
    fn from(e: crate::handlers::HandlerError) -> Self {
        Self::Handler(Box::new(e))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ParseErrKind {
    Syntax,
    Schema,
    Internal,
}

impl ParseErrKind {
    pub fn from_wire(wire: ryeos_handler_protocol::ParseErrKind) -> Self {
        match wire {
            ryeos_handler_protocol::ParseErrKind::Syntax => Self::Syntax,
            ryeos_handler_protocol::ParseErrKind::Schema => Self::Schema,
            ryeos_handler_protocol::ParseErrKind::Internal => Self::Internal,
        }
    }
}
