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

/// Generate mock daemon events for a rich demo thread.
/// These simulate a real agent execution with thinking, tool calls, and streaming text.
pub fn mock_thread_events() -> Vec<DaemonEvent> {
    use ryeos_tui_core::store::ThreadUsage;

    let tid = ryeos_tui_core::ids::ThreadId::new(10);
    vec![
        DaemonEvent::ThreadCreated {
            id: tid,
            item_ref: Some("agent:code-review".into()),
        },
        DaemonEvent::ThreadStarted { id: tid },
        DaemonEvent::TextDelta {
            thread_id: tid,
            text: "I'll review the code changes in this PR. Let me start by examining the diff.\n".into(),
        },
        DaemonEvent::ToolCallStart {
            thread_id: tid,
            name: "rye/bash/bash".into(),
        },
        DaemonEvent::ToolCallResult {
            thread_id: tid,
            name: "rye/bash/bash".into(),
            duration_ms: Some(850),
        },
        DaemonEvent::TextDelta {
            thread_id: tid,
            text: "I can see changes in the thread view and update reducer. Let me analyze the specifics.\n\n".into(),
        },
        DaemonEvent::TextDelta {
            thread_id: tid,
            text: "The thread view has been enhanced with expand/collapse functionality for assistant messages and tool calls. The update reducer now includes tile management keybindings (Ctrl+S, Ctrl+V, Ctrl+X, Ctrl+R) and list navigation (j/k).\n".into(),
        },
        DaemonEvent::ToolCallStart {
            thread_id: tid,
            name: "rye/file-system/read".into(),
        },
        DaemonEvent::ToolCallResult {
            thread_id: tid,
            name: "rye/file-system/read".into(),
            duration_ms: Some(120),
        },
        DaemonEvent::TextDelta {
            thread_id: tid,
            text: "## Review Summary\n\n**Overall Assessment: Good**\n\nThe changes are well-structured and maintain consistency with the existing codebase.\n\n### Key improvements:\n1. Expand/collapse makes long threads manageable\n2. Word wrap prevents horizontal scrolling\n3. Widget utilities are properly tested\n\n### Minor suggestions:\n- Consider adding a max-height for expanded tool output\n- The scroll indicator could show percentage\n".into(),
        },
        DaemonEvent::UsageUpdate {
            thread_id: tid,
            usage: ThreadUsage {
                input_tokens: 8500,
                output_tokens: 2100,
                spend_usd: 0.12,
                elapsed_ms: 45000,
            },
        },
        DaemonEvent::ThreadCompleted { id: tid },
    ]
}
