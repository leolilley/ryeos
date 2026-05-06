use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Config errors — config parsing failures (exit 78)
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CliConfigError {
    #[error("invalid `execute` ref \"{item_ref}\" in {path}: {detail}")]
    InvalidExecuteRef {
        path: String,
        item_ref: String,
        detail: String,
    },
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
pub enum CliDispatchError {
    #[error(transparent)]
    Transport(#[from] CliTransportError),
}

// ---------------------------------------------------------------------------
// Top-level CLI error — mapped to exit codes
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] CliConfigError),

    #[error(transparent)]
    Dispatch(#[from] CliDispatchError),

    #[error(transparent)]
    Transport(#[from] CliTransportError),

    #[error("unknown verb: {argv:?}")]
    UnknownVerb { argv: Vec<String> },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Failure inside a hardcoded local verb (`init`, `trust pin`, `publish`).
    /// These run in-process without daemon dispatch.
    #[error("{detail}")]
    Local { detail: String },
}
