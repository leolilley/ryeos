use std::path::PathBuf;

/// Network, daemon-discovery and signing failures of the client transport.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
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
