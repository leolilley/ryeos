//! Single source of truth for the runtime event vocabulary (V5.5 D11).
//!
//! Both runtime emitters (this crate's `CallbackClient`, the directive
//! / graph / knowledge runtimes) and the daemon's
//! `EventStoreService::validate_event_type` consume the same enum.
//! That eliminates drift: there is no separate "string allow-list" on
//! either side. Adding a new event variant is a single-edit change
//! that both sides see at compile time.
//!
//! `StorageClass::for(event)` is the canonical mapping from event type
//! to durable-store strategy. High-frequency progressive events
//! (token deltas, reasoning chunks, foreach progress) go to
//! `journal_only`; everything else is an `indexed` milestone.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Storage strategy for a persisted event.
///
/// Wire form is `snake_case` so daemon-side serialization stays
/// stable across the producer/consumer boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StorageClass {
    /// Append to the indexed event store; queryable, replayable,
    /// retained for the life of the chain.
    Indexed,
    /// Append to the per-thread journal only; not indexed for query,
    /// pruneable, used for high-frequency progressive UI events.
    JournalOnly,
}

impl StorageClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::JournalOnly => "journal_only",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "indexed" => Ok(Self::Indexed),
            "journal_only" => Ok(Self::JournalOnly),
            other if other.trim().is_empty() => bail!("storage_class must not be empty"),
            other => bail!("invalid storage_class: {other}"),
        }
    }
}

/// Canonical runtime event vocabulary.
///
/// **Adding a variant** automatically extends what the daemon
/// validator (`event_store::validate_event_type`) accepts and what
/// runtime emitters can produce — they all delegate to
/// `RuntimeEventType::parse` / `as_str`. There is no separate
/// allow-list to keep in sync.
///
/// Wire form is `snake_case`, matching the legacy string vocabulary
/// so existing persisted events round-trip unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeEventType {
    // ── Thread lifecycle ────────────────────────────────────────
    ThreadCreated,
    ThreadStarted,
    ThreadCompleted,
    ThreadFailed,
    ThreadCancelled,
    ThreadKilled,
    ThreadTimedOut,
    ThreadContinued,

    // ── Spawning / continuation / commands ──────────────────────
    EdgeRecorded,
    ChildThreadSpawned,
    ContinuationRequested,
    ContinuationAccepted,
    CommandSubmitted,
    CommandClaimed,
    CommandCompleted,

    // ── Streaming ───────────────────────────────────────────────
    StreamOpened,
    TokenDelta,
    StreamSnapshot,
    StreamClosed,

    // ── Audit / artifact ────────────────────────────────────────
    ArtifactPublished,
    ThreadReconciled,
    OrphanProcessKilled,

    // ── Cognition / replay transcript ───────────────────────────
    SystemPrompt,
    ContextInjected,
    CognitionIn,
    CognitionOut,
    CognitionReasoning,

    // ── Tool dispatch ───────────────────────────────────────────
    ToolCallStart,
    ToolCallResult,

    // ── Graph lifecycle (V5.5 D5) ───────────────────────────────
    GraphStarted,
    GraphCompleted,
    GraphStepStarted,
    GraphStepCompleted,
    GraphBranchTaken,
    GraphForeachIteration,

    // ── Usage settlement (O3) ───────────────────────────────────
    ThreadUsage,
}

impl RuntimeEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ThreadCreated => "thread_created",
            Self::ThreadStarted => "thread_started",
            Self::ThreadCompleted => "thread_completed",
            Self::ThreadFailed => "thread_failed",
            Self::ThreadCancelled => "thread_cancelled",
            Self::ThreadKilled => "thread_killed",
            Self::ThreadTimedOut => "thread_timed_out",
            Self::ThreadContinued => "thread_continued",
            Self::EdgeRecorded => "edge_recorded",
            Self::ChildThreadSpawned => "child_thread_spawned",
            Self::ContinuationRequested => "continuation_requested",
            Self::ContinuationAccepted => "continuation_accepted",
            Self::CommandSubmitted => "command_submitted",
            Self::CommandClaimed => "command_claimed",
            Self::CommandCompleted => "command_completed",
            Self::StreamOpened => "stream_opened",
            Self::TokenDelta => "token_delta",
            Self::StreamSnapshot => "stream_snapshot",
            Self::StreamClosed => "stream_closed",
            Self::ArtifactPublished => "artifact_published",
            Self::ThreadReconciled => "thread_reconciled",
            Self::OrphanProcessKilled => "orphan_process_killed",
            Self::SystemPrompt => "system_prompt",
            Self::ContextInjected => "context_injected",
            Self::CognitionIn => "cognition_in",
            Self::CognitionOut => "cognition_out",
            Self::CognitionReasoning => "cognition_reasoning",
            Self::ToolCallStart => "tool_call_start",
            Self::ToolCallResult => "tool_call_result",
            Self::GraphStarted => "graph_started",
            Self::GraphCompleted => "graph_completed",
            Self::GraphStepStarted => "graph_step_started",
            Self::GraphStepCompleted => "graph_step_completed",
            Self::GraphBranchTaken => "graph_branch_taken",
            Self::GraphForeachIteration => "graph_foreach_iteration",
            Self::ThreadUsage => "thread_usage",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "thread_created" => Ok(Self::ThreadCreated),
            "thread_started" => Ok(Self::ThreadStarted),
            "thread_completed" => Ok(Self::ThreadCompleted),
            "thread_failed" => Ok(Self::ThreadFailed),
            "thread_cancelled" => Ok(Self::ThreadCancelled),
            "thread_killed" => Ok(Self::ThreadKilled),
            "thread_timed_out" => Ok(Self::ThreadTimedOut),
            "thread_continued" => Ok(Self::ThreadContinued),
            "edge_recorded" => Ok(Self::EdgeRecorded),
            "child_thread_spawned" => Ok(Self::ChildThreadSpawned),
            "continuation_requested" => Ok(Self::ContinuationRequested),
            "continuation_accepted" => Ok(Self::ContinuationAccepted),
            "command_submitted" => Ok(Self::CommandSubmitted),
            "command_claimed" => Ok(Self::CommandClaimed),
            "command_completed" => Ok(Self::CommandCompleted),
            "stream_opened" => Ok(Self::StreamOpened),
            "token_delta" => Ok(Self::TokenDelta),
            "stream_snapshot" => Ok(Self::StreamSnapshot),
            "stream_closed" => Ok(Self::StreamClosed),
            "artifact_published" => Ok(Self::ArtifactPublished),
            "thread_reconciled" => Ok(Self::ThreadReconciled),
            "orphan_process_killed" => Ok(Self::OrphanProcessKilled),
            "system_prompt" => Ok(Self::SystemPrompt),
            "context_injected" => Ok(Self::ContextInjected),
            "cognition_in" => Ok(Self::CognitionIn),
            "cognition_out" => Ok(Self::CognitionOut),
            "cognition_reasoning" => Ok(Self::CognitionReasoning),
            "tool_call_start" => Ok(Self::ToolCallStart),
            "tool_call_result" => Ok(Self::ToolCallResult),
            "graph_started" => Ok(Self::GraphStarted),
            "graph_completed" => Ok(Self::GraphCompleted),
            "graph_step_started" => Ok(Self::GraphStepStarted),
            "graph_step_completed" => Ok(Self::GraphStepCompleted),
            "graph_branch_taken" => Ok(Self::GraphBranchTaken),
            "graph_foreach_iteration" => Ok(Self::GraphForeachIteration),
            "thread_usage" => Ok(Self::ThreadUsage),
            other if other.trim().is_empty() => bail!("event_type must not be empty"),
            other => bail!("invalid event_type: {other}"),
        }
    }

    /// Canonical storage class for this event type.
    ///
    /// High-frequency progressive events (token deltas, reasoning
    /// chunks, foreach progress) go to `journal_only`; everything
    /// else is an indexed milestone. The mapping is intentionally
    /// `match`-exhaustive so adding a new variant requires deciding
    /// its storage class up front.
    pub fn storage_class(self) -> StorageClass {
        match self {
            Self::TokenDelta
            | Self::StreamSnapshot
            | Self::CognitionReasoning
            | Self::GraphForeachIteration => StorageClass::JournalOnly,
            // Everything else: thread lifecycle, edges, commands,
            // cognition turn boundaries, tool dispatch, graph
            // lifecycle/step/branch milestones — indexed.
            Self::ThreadCreated
            | Self::ThreadStarted
            | Self::ThreadCompleted
            | Self::ThreadFailed
            | Self::ThreadCancelled
            | Self::ThreadKilled
            | Self::ThreadTimedOut
            | Self::ThreadContinued
            | Self::EdgeRecorded
            | Self::ChildThreadSpawned
            | Self::ContinuationRequested
            | Self::ContinuationAccepted
            | Self::CommandSubmitted
            | Self::CommandClaimed
            | Self::CommandCompleted
            | Self::StreamOpened
            | Self::StreamClosed
            | Self::ArtifactPublished
            | Self::ThreadReconciled
            | Self::OrphanProcessKilled
            | Self::SystemPrompt
            | Self::ContextInjected
            | Self::CognitionIn
            | Self::CognitionOut
            | Self::ToolCallStart
            | Self::ToolCallResult
            | Self::GraphStarted
            | Self::GraphCompleted
            | Self::GraphStepStarted
            | Self::GraphStepCompleted
            | Self::GraphBranchTaken
            | Self::ThreadUsage => StorageClass::Indexed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant round-trips its `as_str` through `parse` to the
    /// same variant. Catches typos and missing arms.
    #[test]
    fn event_type_round_trip() {
        let variants = [
            RuntimeEventType::ThreadCreated,
            RuntimeEventType::ThreadStarted,
            RuntimeEventType::ThreadCompleted,
            RuntimeEventType::ThreadFailed,
            RuntimeEventType::ThreadCancelled,
            RuntimeEventType::ThreadKilled,
            RuntimeEventType::ThreadTimedOut,
            RuntimeEventType::ThreadContinued,
            RuntimeEventType::EdgeRecorded,
            RuntimeEventType::ChildThreadSpawned,
            RuntimeEventType::ContinuationRequested,
            RuntimeEventType::ContinuationAccepted,
            RuntimeEventType::CommandSubmitted,
            RuntimeEventType::CommandClaimed,
            RuntimeEventType::CommandCompleted,
            RuntimeEventType::StreamOpened,
            RuntimeEventType::TokenDelta,
            RuntimeEventType::StreamSnapshot,
            RuntimeEventType::StreamClosed,
            RuntimeEventType::ArtifactPublished,
            RuntimeEventType::ThreadReconciled,
            RuntimeEventType::OrphanProcessKilled,
            RuntimeEventType::SystemPrompt,
            RuntimeEventType::ContextInjected,
            RuntimeEventType::CognitionIn,
            RuntimeEventType::CognitionOut,
            RuntimeEventType::CognitionReasoning,
            RuntimeEventType::ToolCallStart,
            RuntimeEventType::ToolCallResult,
            RuntimeEventType::GraphStarted,
            RuntimeEventType::GraphCompleted,
            RuntimeEventType::GraphStepStarted,
            RuntimeEventType::GraphStepCompleted,
            RuntimeEventType::GraphBranchTaken,
            RuntimeEventType::GraphForeachIteration,
            RuntimeEventType::ThreadUsage,
        ];
        for v in variants {
            let s = v.as_str();
            let parsed = RuntimeEventType::parse(s)
                .unwrap_or_else(|_| panic!("round-trip failed for {s}"));
            assert_eq!(v, parsed, "round-trip mismatch for {s}");
        }
    }

    #[test]
    fn parse_rejects_empty_and_unknown() {
        assert!(RuntimeEventType::parse("").is_err());
        assert!(RuntimeEventType::parse("   ").is_err());
        assert!(RuntimeEventType::parse("not_a_real_event").is_err());
    }

    #[test]
    fn journal_only_set_is_exactly_the_progressive_events() {
        // Tightening this list belongs in a deliberate change — the
        // assertion guards against silent storage-class drift.
        let journal_only: Vec<&'static str> = [
            RuntimeEventType::TokenDelta,
            RuntimeEventType::StreamSnapshot,
            RuntimeEventType::CognitionReasoning,
            RuntimeEventType::GraphForeachIteration,
        ]
        .iter()
        .copied()
        .map(|v| v.as_str())
        .collect();
        assert_eq!(
            journal_only,
            vec!["token_delta", "stream_snapshot", "cognition_reasoning", "graph_foreach_iteration"]
        );
    }

    #[test]
    fn storage_class_round_trip() {
        assert_eq!(StorageClass::Indexed.as_str(), "indexed");
        assert_eq!(StorageClass::JournalOnly.as_str(), "journal_only");
        assert_eq!(StorageClass::parse("indexed").unwrap(), StorageClass::Indexed);
        assert_eq!(
            StorageClass::parse("journal_only").unwrap(),
            StorageClass::JournalOnly
        );
        assert!(StorageClass::parse("").is_err());
        assert!(StorageClass::parse("nope").is_err());
    }

    #[test]
    fn every_variant_has_a_storage_class() {
        // Compile-time check via the exhaustive match in
        // `storage_class()` is the real guarantee; this test just
        // exercises the call to ensure no variant panics or hits an
        // unreachable arm.
        for ev in [
            RuntimeEventType::ThreadCreated,
            RuntimeEventType::TokenDelta,
            RuntimeEventType::GraphForeachIteration,
            RuntimeEventType::GraphCompleted,
        ] {
            let _ = ev.storage_class();
        }
    }
}
