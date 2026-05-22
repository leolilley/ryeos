//! Transport trait — abstracted daemon communication.
//!
//! Allows swapping between signed HTTP, mock, and attach transports.

use ryeos_tui_core::ids::ThreadId;
use ryeos_tui_core::update::{DaemonEvent, PollSnapshot};

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
    CancelThread {
        thread_id: ThreadId,
    },
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

// Re-export for convenience
use std::future::Future;
use std::pin::Pin;
