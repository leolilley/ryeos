//! Transport trait — abstracted daemon communication.
//!
//! Allows swapping between signed HTTP, mock, and attach transports.

use ryeos_tui_core::ids::ThreadId;
use ryeos_tui_core::update::{DaemonEvent, PollSnapshot};

use crate::daemon::DaemonClient;
use crate::sse::SseEvent;

/// Transport-level request to the daemon.
#[derive(Debug, Clone)]
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
    CancelThread { thread_id: ThreadId },
}

/// Transport-level response from the daemon.
#[derive(Debug, Clone)]
pub enum DaemonResponse {
    Json(serde_json::Value),
    StreamStarted { thread_id: ThreadId },
    Ok,
    Error { message: String },
}

/// Trait for daemon transport implementations.
pub trait DaemonTransport {
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
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("not connected")]
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
    fn request(
        &self,
        _req: DaemonRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<DaemonResponse, TransportError>> + Send + '_>,
    > {
        Box::pin(async {
            Ok(DaemonResponse::Json(serde_json::json!({"status": "mock"})))
        })
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

    pub fn client(&self) -> &DaemonClient {
        &self.client
    }
}

impl DaemonTransport for SignedHttpTransport {
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
                    Ok(v) => Ok(DaemonResponse::Json(serde_json::to_value(v).unwrap_or_default())),
                    Err(e) => Err(TransportError::Daemon(e.to_string())),
                },
                DaemonRequest::Execute {
                    item_ref,
                    project_path,
                    parameters,
                } => match self.client.execute(&item_ref, &project_path, &parameters).await {
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
                    match self
                        .client
                        .signed_post(&path, &serde_json::json!({}))
                        .await
                    {
                        Ok(_) => Ok(DaemonResponse::Ok),
                        Err(e) => Err(TransportError::Daemon(e.to_string())),
                    }
                }
                _ => Err(TransportError::Transport("not implemented".into())),
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
