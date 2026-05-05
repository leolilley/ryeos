use crate::error::{CliDispatchError, CliError, CliTransportError};

pub const EX_USAGE: i32 = 64;
pub const EX_SOFTWARE: i32 = 70;
pub const EX_TEMPFAIL: i32 = 75;
pub const EX_CONFIG: i32 = 78;

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Config(_) => EX_CONFIG,
            CliError::Dispatch(d) => d.exit_code(),
            CliError::Transport(t) => t.exit_code(),
            CliError::UnknownVerb { .. } => EX_USAGE,
            CliError::InsufficientCapabilities { .. } => 77, // EX_NOPERM
            CliError::Io(_) => EX_SOFTWARE,
            CliError::Internal { .. } => EX_SOFTWARE,
            CliError::Local { .. } => 1,
        }
    }
}

impl CliDispatchError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliDispatchError::Transport(t) => t.exit_code(),
        }
    }
}

impl CliTransportError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliTransportError::DaemonJsonMissing { .. } => EX_TEMPFAIL,
            CliTransportError::DaemonJsonMalformed { .. } => EX_CONFIG,
            CliTransportError::Unreachable { .. } => EX_TEMPFAIL,
            CliTransportError::HttpError { .. } => 1,
            CliTransportError::BodyDecode { .. } => EX_SOFTWARE,
            CliTransportError::SigningKeyMissing { .. } => EX_TEMPFAIL,
            CliTransportError::SigningKeyUnloadable { .. } => EX_CONFIG,
        }
    }
}
