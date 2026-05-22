//! Mock transport — returns sample data for testing without a daemon.

use ryeos_tui_core::ids::RemoteId;
use ryeos_tui_core::update::{DaemonEvent, PollSnapshot, RemoteSummary, ThreadSummary};

/// Generate a mock poll snapshot with sample data.
pub fn mock_poll_snapshot() -> PollSnapshot {
    PollSnapshot {
        threads: vec![
            ThreadSummary {
                id: ryeos_tui_core::ids::ThreadId::new(1),
                status: "completed".into(),
                item_ref: Some("deploy:production".into()),
                parent_id: None,
                started_at_ms: Some(1700000000000),
                duration_ms: Some(45000),
                cost_usd: Some(0.23),
            },
            ThreadSummary {
                id: ryeos_tui_core::ids::ThreadId::new(2),
                status: "running".into(),
                item_ref: Some("scrape:docs".into()),
                parent_id: None,
                started_at_ms: Some(1700000050000),
                duration_ms: None,
                cost_usd: Some(0.08),
            },
            ThreadSummary {
                id: ryeos_tui_core::ids::ThreadId::new(3),
                status: "failed".into(),
                item_ref: Some("test:integration".into()),
                parent_id: Some(ryeos_tui_core::ids::ThreadId::new(2)),
                started_at_ms: Some(1700000040000),
                duration_ms: Some(12000),
                cost_usd: Some(0.02),
            },
        ],
        remotes: vec![
            RemoteSummary {
                id: RemoteId::new(1),
                name: "default".into(),
                url: "http://remote.example.com:7400".into(),
                alive: true,
            },
            RemoteSummary {
                id: RemoteId::new(2),
                name: "staging".into(),
                url: "http://staging.example.com:7400".into(),
                alive: false,
            },
        ],
        daemon_url: Some("http://localhost:7400".into()),
        daemon_alive: true,
    }
}
