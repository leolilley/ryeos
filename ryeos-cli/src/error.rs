use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Config errors — verb loading failures (exit 78)
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum CliConfigError {
    #[error("unsigned verb YAML: {path}")]
    UnsignedYaml { path: PathBuf },

    #[error("bad signature in {path}: {detail}")]
    BadSignature { path: PathBuf, detail: String },

    #[error("untrusted signer {fingerprint} in {path}")]
    UntrustedSigner { path: PathBuf, fingerprint: String },

    #[error("wrong kind in {path}: expected \"config\", got \"{got}\"")]
    WrongKind { path: PathBuf, got: String },

    #[error("wrong category in {path}: expected \"cli\", got \"{got}\"")]
    WrongCategory { path: PathBuf, got: String },

    #[error("schema error in {path}: {detail}")]
    SchemaError { path: PathBuf, detail: String },

    #[error("IO error reading {path}: {detail}")]
    IoError { path: PathBuf, detail: String },

    #[error("duplicate verb_tokens {tokens:?} in {paths:?}")]
    DuplicateVerbTokens {
        tokens: Vec<String>,
        paths: Vec<PathBuf>,
    },

    #[error("prefix-overlap: {a:?} vs {b:?} in {paths:?}")]
    PrefixOverlap {
        a: Vec<String>,
        b: Vec<String>,
        paths: Vec<PathBuf>,
    },

    #[error("empty verb_tokens in {path}")]
    EmptyVerbTokens { path: String },

    #[error("missing `execute` field in {path}")]
    MissingExecute { path: String },

    #[error("invalid `execute` ref \"{item_ref}\" in {path}: {detail}")]
    InvalidExecuteRef {
        path: String,
        item_ref: String,
        detail: String,
    },

    #[error("failed to load trust store: {detail}")]
    TrustStoreLoad { detail: String },

    #[error("failed to load kind registry: {detail}")]
    KindRegistryLoad { detail: String },
}

// ---------------------------------------------------------------------------
// Transport errors — network / signing failures
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CliTransportError {
    #[error("daemon.json not found at {path}")]
    DaemonJsonMissing { path: PathBuf },

    #[error("daemon.json malformed: {detail}")]
    DaemonJsonMalformed { detail: String },

    #[error("daemon TCP unreachable at {bind}: {detail}")]
    Unreachable { bind: String, detail: String },

    #[error("daemon returned HTTP {status}: {body}")]
    HttpError { status: u16, body: String },

    #[error("response body decode failed: {detail}")]
    BodyDecode { detail: String },

    #[error("signing key not found at {path}")]
    SigningKeyMissing { path: PathBuf },

    #[error("signing key unloadable at {path}: {detail}")]
    SigningKeyUnloadable { path: PathBuf, detail: String },
}

// ---------------------------------------------------------------------------
// Dispatch errors — runtime failures during command execution
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum CliDispatchError {
    #[error("bad arguments: {0}")]
    BadArgs(String),

    #[error(transparent)]
    Transport(#[from] CliTransportError),

    #[error("internal error: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// Top-level CLI error — mapped to exit codes
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] CliConfigError),

    #[error(transparent)]
    Dispatch(#[from] CliDispatchError),

    #[error(transparent)]
    Transport(#[from] CliTransportError),

    #[error("unknown verb: {argv:?}")]
    UnknownVerb { argv: Vec<String> },

    #[error("interrupted")]
    Interrupted,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("internal error: {detail}")]
    Internal { detail: String },

    /// Failure inside a hardcoded local verb (`init`, `trust pin`, `publish`).
    /// These run in-process without daemon dispatch.
    #[error("{detail}")]
    Local { detail: String },
}
