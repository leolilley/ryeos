//! Transport trait — abstracted daemon communication.
//!
//! Allows swapping between signed HTTP, mock, and attach transports.

use ryeos_client_base::ids::ThreadId;
use ryeos_client_base::update::PollSnapshot;

use crate::daemon::DaemonClient;

/// Transport-level request to the daemon.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DaemonRequest {
    GetStatus,
    GetThreads,
    GetRemotes,
    Execute {
        item_ref: String,
        project_path: String,
        parameters: serde_json::Value,
    },
    ExecuteStream {
        item_ref: String,
        project_path: String,
        parameters: serde_json::Value,
    },
    CancelThread {
        thread_id: ThreadId,
    },
    /// Resolve an effective surface by canonical ref.
    GetEffectiveSurface {
        ref_str: String,
    },
}

/// Transport-level response from the daemon.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DaemonResponse {
    Json(serde_json::Value),
    StreamStarted { thread_id: ThreadId },
    Ok,
    Error { message: String },
}

/// Trait for daemon transport implementations.
pub trait DaemonTransport {
    /// Transport name for debugging and feature detection.
    fn name(&self) -> &str {
        "unknown"
    }

    fn request(
        &self,
        req: DaemonRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DaemonResponse, TransportError>> + Send + '_>,
    >;

    fn poll_snapshot(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<PollSnapshot, TransportError>> + Send + '_>,
    >;

    /// Resolve an effective surface from the daemon's item services.
    /// Returns the composed_value JSON if the daemon supports it.
    #[allow(dead_code)]
    fn resolve_effective_surface(
        &self,
        _ref_str: &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, TransportError>> + Send + '_,
        >,
    > {
        Box::pin(async { Err(TransportError::Transport("not implemented".into())) })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("not connected")]
    #[allow(dead_code)]
    NotConnected,
    #[error("daemon error: {0}")]
    Daemon(String),
    #[error("transport: {0}")]
    Transport(String),
}

// ---------------------------------------------------------------------------
// Mock transport
// ---------------------------------------------------------------------------

use crate::mock_transport;

pub struct MockTransport;

impl DaemonTransport for MockTransport {
    fn name(&self) -> &str {
        "mock"
    }

    fn request(
        &self,
        _req: DaemonRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DaemonResponse, TransportError>> + Send + '_>,
    > {
        Box::pin(async { Ok(DaemonResponse::Json(serde_json::json!({"status": "mock"}))) })
    }

    fn poll_snapshot(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<PollSnapshot, TransportError>> + Send + '_>,
    > {
        Box::pin(async { Ok(mock_transport::mock_poll_snapshot()) })
    }
}

// ---------------------------------------------------------------------------
// Signed HTTP transport
// ---------------------------------------------------------------------------

pub struct SignedHttpTransport {
    client: DaemonClient,
}

impl SignedHttpTransport {
    pub async fn connect() -> Result<Self, TransportError> {
        let client = DaemonClient::try_connect()
            .await
            .map_err(|e| TransportError::Transport(e.to_string()))?;
        Ok(Self { client })
    }

    #[allow(dead_code)]
    pub fn client(&self) -> &DaemonClient {
        &self.client
    }
}

impl DaemonTransport for SignedHttpTransport {
    fn name(&self) -> &str {
        "signed-http"
    }

    fn request(
        &self,
        req: DaemonRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DaemonResponse, TransportError>> + Send + '_>,
    > {
        Box::pin(async move {
            match req {
                DaemonRequest::GetStatus => match self.client.get_status().await {
                    Ok(v) => Ok(DaemonResponse::Json(v)),
                    Err(e) => Err(TransportError::Daemon(e.to_string())),
                },
                DaemonRequest::GetThreads => match self.client.get_threads().await {
                    Ok(v) => Ok(DaemonResponse::Json(
                        serde_json::to_value(v).unwrap_or_default(),
                    )),
                    Err(e) => Err(TransportError::Daemon(e.to_string())),
                },
                DaemonRequest::Execute {
                    item_ref,
                    project_path,
                    parameters,
                } => match self
                    .client
                    .execute(&item_ref, &project_path, &parameters)
                    .await
                {
                    Ok(v) => {
                        let thread_id = v
                            .get("thread_id")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse().ok())
                            .map(ThreadId::new);
                        if let Some(tid) = thread_id {
                            Ok(DaemonResponse::StreamStarted { thread_id: tid })
                        } else {
                            Ok(DaemonResponse::Json(v))
                        }
                    }
                    Err(e) => Err(TransportError::Daemon(e.to_string())),
                },
                DaemonRequest::CancelThread { thread_id } => {
                    let path = format!("/threads/{}/cancel", thread_id.0);
                    match self.client.signed_post(&path, &serde_json::json!({})).await {
                        Ok(_) => Ok(DaemonResponse::Ok),
                        Err(e) => Err(TransportError::Daemon(e.to_string())),
                    }
                }
                DaemonRequest::GetEffectiveSurface { ref ref_str } => {
                    match self.client.resolve_effective_surface(ref_str, None).await {
                        Ok(v) => Ok(DaemonResponse::Json(v)),
                        Err(e) => Err(TransportError::Daemon(e.to_string())),
                    }
                }
                DaemonRequest::GetRemotes => match self.client.get_remotes().await {
                    Ok(remotes) => Ok(DaemonResponse::Json(
                        serde_json::to_value(remotes).unwrap_or_default(),
                    )),
                    Err(e) => Err(TransportError::Daemon(e.to_string())),
                },
                DaemonRequest::ExecuteStream { .. } => {
                    // ExecuteStream is handled directly by the event loop via
                    // DaemonClient::execute_stream, not through the request method.
                    Err(TransportError::Transport(
                        "ExecuteStream must be called via execute_stream, not request()".into(),
                    ))
                }
            }
        })
    }

    fn poll_snapshot(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<PollSnapshot, TransportError>> + Send + '_>,
    > {
        Box::pin(async move {
            self.client
                .poll_snapshot()
                .await
                .map_err(|e| TransportError::Transport(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_transport_returns_bootstrap_snapshot() {
        let transport = MockTransport;
        let snapshot = transport.poll_snapshot().await.unwrap();
        assert!(!snapshot.threads.is_empty());
        assert!(snapshot.daemon_alive);
    }
}
