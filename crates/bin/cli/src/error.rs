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

    /// The matched command requires a project, so `--no-project` is not a
    /// valid recovery choice.
    #[error("project resolution: {0}")]
    ProjectRequired(String),
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
                    "pass `--project <DIR>` or `--no-project` explicitly; either selector may appear before or after the command"
                ));
            }
            Self::ProjectRequired(_) => {
                diagnostic.heading = Some("PROJECT RESOLUTION FAILED".to_string());
                diagnostic.hint = Some(crate::tty::Hint::new(
                    "run from the intended project or pass `--project <DIR>` explicitly",
                ));
            }
            Self::Reported { .. } => {}
            _ => {}
        }
        diagnostic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_project_error_does_not_recommend_no_project() {
        let diagnostic =
            CliError::ProjectRequired("command 'bundle-smoke' requires a project".into())
                .diagnostic();
        let hint = diagnostic.hint.expect("project resolution hint").0;
        assert!(hint.contains("--project <DIR>"));
        assert!(!hint.contains("--no-project"));
    }

    #[test]
    fn optional_project_error_documents_both_global_selector_positions() {
        let diagnostic = CliError::ProjectResolution("project not found".into()).diagnostic();
        let hint = diagnostic.hint.expect("project resolution hint").0;
        assert!(hint.contains("--project <DIR>"));
        assert!(hint.contains("--no-project"));
        assert!(hint.contains("before or after"));
    }
}
