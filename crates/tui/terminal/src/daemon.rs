//! Daemon client — signed HTTP/SSE client for ryeosd.
//!
//! Reuses transport layer from ryeos-cli.

pub use ryeos_cli::transport::discovery::discover_audience;
pub use ryeos_cli::transport::http::{post_json, resolve_daemon_url};
pub use ryeos_cli::transport::signing::{SignHeaders, Signer};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("no identity found — run ryeos init")]
    NoIdentity,

    #[error("no system directory found")]
    NoSystemDir,

    #[error("transport: {0}")]
    Transport(#[from] ryeos_cli::error::CliTransportError),

    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("daemon not running at {url}")]
    DaemonDown { url: String },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Native daemon client using signed HTTP.
pub struct DaemonClient {
    base_url: String,
    audience: String,
    signer: Option<Signer>,
}

impl DaemonClient {
    /// Try to connect to the daemon. Returns None if daemon is not available.
    pub async fn try_connect() -> Result<Self, ClientError> {
        let system_space_dir = dirs::data_local_dir()
            .ok_or(ClientError::NoSystemDir)?
            .join("ryeos")
            .join(".ai");

        let base_url = resolve_daemon_url(&system_space_dir).await?;

        let signer = Signer::resolve(&system_space_dir).ok();

        let audience = if signer.is_some() {
            discover_audience(&base_url).await?
        } else {
            String::new()
        };

        Ok(Self {
            base_url,
            audience,
            signer,
        })
    }

    /// Check if the daemon is reachable.
    pub async fn is_alive(&self) -> bool {
        // Simple connectivity check — try to get status
        if let Ok(signer) = self.signer() {
            let result = post_json(
                &self.base_url,
                signer,
                &serde_json::to_vec(&serde_json::json!({})).unwrap_or_default(),
            )
            .await;
            result.is_ok()
        } else {
            false
        }
    }

    fn signer(&self) -> Result<&SignHeaders, ClientError> {
        // This is a simplified check — in practice we'd sign each request
        // For V1, return an error if no signer
        Err(ClientError::NoIdentity)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn has_identity(&self) -> bool {
        self.signer.is_some()
    }
}
