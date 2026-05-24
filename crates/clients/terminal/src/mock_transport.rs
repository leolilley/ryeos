//! Mock transport — returns sample data for testing without a daemon.

use ryeos_client_base::ids::{ItemId, ProjectId, RemoteId};
use ryeos_client_base::store::{IdentityModel, ItemCounts, ItemModel, ProjectModel};
use ryeos_client_base::update::{DaemonEvent, PollSnapshot, RemoteSummary, ThreadSummary};

/// Generate a mock poll snapshot with sample data.
pub fn mock_poll_snapshot() -> PollSnapshot {
    PollSnapshot {
        threads: vec![
            ThreadSummary {
                id: ryeos_client_base::ids::ThreadId::new(1),
                status: "completed".into(),
                item_ref: Some("deploy:production".into()),
                parent_id: None,
                started_at_ms: Some(1700000000000),
                duration_ms: Some(45000),
                cost_usd: Some(0.23),
            },
            ThreadSummary {
                id: ryeos_client_base::ids::ThreadId::new(2),
                status: "running".into(),
                item_ref: Some("scrape:docs".into()),
                parent_id: None,
                started_at_ms: Some(1700000050000),
                duration_ms: None,
                cost_usd: Some(0.08),
            },
            ThreadSummary {
                id: ryeos_client_base::ids::ThreadId::new(3),
                status: "failed".into(),
                item_ref: Some("test:integration".into()),
                parent_id: Some(ryeos_client_base::ids::ThreadId::new(2)),
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
    use ryeos_client_base::store::ThreadUsage;

    let tid = ryeos_client_base::ids::ThreadId::new(10);
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

/// Generate mock items for space browser.
pub fn mock_items() -> Vec<ItemModel> {
    vec![
        ItemModel {
            id: ItemId::new(1),
            kind: "directive".into(),
            name: "init".into(),
            category: Some("rye/core".into()),
            description: Some("Initialize a new Rye OS project".into()),
            signed: true,
        },
        ItemModel {
            id: ItemId::new(2),
            kind: "directive".into(),
            name: "rye/core/create_tool".into(),
            category: Some("rye/core".into()),
            description: Some("Create a new tool from template".into()),
            signed: true,
        },
        ItemModel {
            id: ItemId::new(3),
            kind: "tool".into(),
            name: "rye/bash/bash".into(),
            category: Some("rye/core".into()),
            description: Some("Execute bash commands".into()),
            signed: true,
        },
        ItemModel {
            id: ItemId::new(4),
            kind: "tool".into(),
            name: "rye/file-system/read".into(),
            category: Some("rye/core".into()),
            description: Some("Read file contents".into()),
            signed: true,
        },
        ItemModel {
            id: ItemId::new(5),
            kind: "tool".into(),
            name: "rye/file-system/write".into(),
            category: Some("rye/core".into()),
            description: Some("Write content to a file".into()),
            signed: true,
        },
        ItemModel {
            id: ItemId::new(6),
            kind: "knowledge".into(),
            name: "rye/core/signing".into(),
            category: Some("rye/core".into()),
            description: Some("Signing and verification reference".into()),
            signed: true,
        },
        ItemModel {
            id: ItemId::new(7),
            kind: "directive".into(),
            name: "deploy".into(),
            category: None,
            description: Some("Deploy project to remote".into()),
            signed: false,
        },
        ItemModel {
            id: ItemId::new(8),
            kind: "tool".into(),
            name: "rye/web/fetch".into(),
            category: Some("rye/web".into()),
            description: Some("Fetch URL content".into()),
            signed: true,
        },
    ]
}

/// Generate mock projects.
pub fn mock_projects() -> Vec<ProjectModel> {
    vec![
        ProjectModel {
            id: ProjectId::new(1),
            path: "/home/user/projects/my-app".into(),
            name: "my-app".into(),
            item_counts: ItemCounts {
                directives: 12,
                tools: 8,
                knowledge: 5,
            },
        },
        ProjectModel {
            id: ProjectId::new(2),
            path: "/home/user/projects/api-server".into(),
            name: "api-server".into(),
            item_counts: ItemCounts {
                directives: 6,
                tools: 3,
                knowledge: 2,
            },
        },
    ]
}

/// Generate mock identity.
pub fn mock_identity() -> IdentityModel {
    IdentityModel {
        fingerprint: "SHA256:abc123def456...789".into(),
        has_signing_key: true,
    }
}
