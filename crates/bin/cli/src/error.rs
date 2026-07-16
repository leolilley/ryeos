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

    #[error("audience discovery failed for {url}: {detail}")]
    AudienceDiscoveryFailed { url: String, detail: String },
}

// ---------------------------------------------------------------------------
// Dispatch errors — runtime failures during command execution
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CliDispatchError {
    #[error(transparent)]
    Transport(#[from] CliTransportError),

    #[error(transparent)]
    Config(#[from] CliConfigError),
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

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Failure inside a hardcoded lifecycle/bootstrap command.
    /// These run in-process without daemon dispatch.
    #[error("{detail}")]
    Local { detail: String },

    /// The command already rendered its complete result or diagnostic.
    #[error("{detail}")]
    Reported { detail: String },

    /// Client-side project-path resolution failed: discovery returned
    /// nothing and no `--no-project` / `--project` was provided, or the
    /// provided path could not be canonicalized.
    #[error("project resolution: {0}")]
    ProjectResolution(String),
}

impl CliError {
    pub fn diagnostic(&self) -> crate::tty::Diagnostic {
        let mut diagnostic = crate::tty::Diagnostic::error(self.to_string());
        match self {
            Self::Config(_) | Self::Dispatch(CliDispatchError::Config(_)) => {
                diagnostic.heading = Some("CONFIGURATION FAILED".to_string());
                diagnostic.hint = Some(crate::tty::Hint::new(
                    "run `ryeos node doctor` for verified node diagnostics",
                ));
            }
            Self::Transport(CliTransportError::DaemonJsonMissing { .. })
            | Self::Transport(CliTransportError::Unreachable { .. })
            | Self::Dispatch(CliDispatchError::Transport(
                CliTransportError::DaemonJsonMissing { .. } | CliTransportError::Unreachable { .. },
            )) => {
                diagnostic.heading = Some("NODE UNAVAILABLE".to_string());
                diagnostic.hint = Some(crate::tty::Hint::new(
                    "run `ryeos node status`, then `ryeos start` if the node is offline",
                ));
            }
            Self::Transport(CliTransportError::HttpError { status, .. })
            | Self::Dispatch(CliDispatchError::Transport(CliTransportError::HttpError {
                status,
                ..
            })) => {
                diagnostic.heading = Some("DAEMON REQUEST FAILED".to_string());
                diagnostic.context.push(format!("HTTP status {status}"));
                diagnostic.hint = Some(crate::tty::Hint::new(
                    "run `ryeos node doctor` for verified node diagnostics",
                ));
            }
            Self::ProjectResolution(_) => {
                diagnostic.heading = Some("PROJECT RESOLUTION FAILED".to_string());
                diagnostic.hint = Some(crate::tty::Hint::new(
                    "pass `--project <DIR>` or `--no-project` explicitly",
                ));
            }
            Self::Reported { .. } => {}
            _ => {}
        }
        diagnostic
    }
}
