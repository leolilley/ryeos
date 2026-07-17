//! Single source of truth for the runtime event vocabulary (V5.5 D11).
//!
//! Both runtime emitters (this crate's `CallbackClient`, the directive
//! / graph / knowledge runtimes) and the daemon's
//! `EventStoreService::validate_event_type` consume the same enum.
//! That eliminates drift: there is no separate "string allow-list" on
//! either side. Adding a new event variant is a single-edit change
//! that both sides see at compile time.
//!
//! `RuntimeEventType::storage_class()` is the canonical mapping from
//! event type to durable-store strategy. High-frequency progressive
//! events (token deltas, reasoning chunks, foreach progress) are
//! ephemeral live-stream events; everything else is an `indexed`
//! milestone unless a caller deliberately requests `journal_only`.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

// Wire strings live in `ryeos-state` (the lower shared layer the projection also
// reads), so the enum and the projection reference one source of truth.
use ryeos_state::event_types as wire;

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
    /// retained in CAS for audit/replay flows that do not need SQL
    /// projection.
    JournalOnly,
    /// Publish to live subscribers only. Not written to CAS or the
    /// SQL projection. Used for high-frequency progressive UI events
    /// that would otherwise create one immutable object per token.
    Ephemeral,
}

impl StorageClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::JournalOnly => "journal_only",
            Self::Ephemeral => "ephemeral",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "indexed" => Ok(Self::Indexed),
            "journal_only" => Ok(Self::JournalOnly),
            "ephemeral" => Ok(Self::Ephemeral),
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
/// Wire form is `snake_case`, matching the old string vocabulary
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
    /// Slim launch-time resolution digest (extends-chain refs + content
    /// digests, composed policy facts, effective trust class) persisted at
    /// launch so the explain view can render what a thread launched with
    /// rather than a fresh re-resolve.
    AsLaunchedResolution,
    AsLaunchedRefBindings,
    RuntimeLaunchFacts,
    /// A `(key, value)` metadata tag stamped on a thread post-launch (cohort
    /// identity, e.g. `fleet`/`game`). Event-backed so it survives a projection
    /// rebuild, unlike a bare facet-table write.
    ThreadFacetSet,
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
    /// A foreach node began its iteration set. Emitted BEFORE the iterations
    /// run — the step lifecycle events land only when the node body commits,
    /// so this is the braid's only signal that a (possibly long) fanout is in
    /// flight rather than the walk being stalled. Carries the item total and
    /// concurrency shape.
    GraphForeachStarted,
    GraphForeachIteration,
    GraphFollowSuspended,
    GraphNodeRetry,

    // ── Directive resilience / accounting ───────────────────────
    ProviderRetry,
    CostUntracked,

    // ── Domain events ───────────────────────────────────────────
    /// Generic runtime-emitted domain event (namespaced `kind` + free
    /// `payload`), styled by content view-yaml. The engine stays
    /// domain-agnostic; arc/content declares the kinds.
    Milestone,

    // ── Usage settlement (O3) ───────────────────────────────────
    ThreadUsage,
}

impl RuntimeEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ThreadCreated => wire::THREAD_CREATED,
            Self::ThreadStarted => wire::THREAD_STARTED,
            Self::ThreadCompleted => wire::THREAD_COMPLETED,
            Self::ThreadFailed => wire::THREAD_FAILED,
            Self::ThreadCancelled => wire::THREAD_CANCELLED,
            Self::ThreadKilled => wire::THREAD_KILLED,
            Self::ThreadTimedOut => wire::THREAD_TIMED_OUT,
            Self::ThreadContinued => wire::THREAD_CONTINUED,
            Self::EdgeRecorded => wire::EDGE_RECORDED,
            Self::ChildThreadSpawned => wire::CHILD_THREAD_SPAWNED,
            Self::ContinuationRequested => wire::CONTINUATION_REQUESTED,
            Self::ContinuationAccepted => wire::CONTINUATION_ACCEPTED,
            Self::CommandSubmitted => wire::COMMAND_SUBMITTED,
            Self::CommandClaimed => wire::COMMAND_CLAIMED,
            Self::CommandCompleted => wire::COMMAND_COMPLETED,
            Self::StreamOpened => wire::STREAM_OPENED,
            Self::TokenDelta => wire::TOKEN_DELTA,
            Self::StreamSnapshot => wire::STREAM_SNAPSHOT,
            Self::StreamClosed => wire::STREAM_CLOSED,
            Self::ArtifactPublished => wire::ARTIFACT_PUBLISHED,
            Self::AsLaunchedResolution => wire::AS_LAUNCHED_RESOLUTION,
            Self::AsLaunchedRefBindings => wire::AS_LAUNCHED_REF_BINDINGS,
            Self::RuntimeLaunchFacts => wire::RUNTIME_LAUNCH_FACTS,
            Self::ThreadFacetSet => wire::THREAD_FACET_SET,
            Self::ThreadReconciled => wire::THREAD_RECONCILED,
            Self::OrphanProcessKilled => wire::ORPHAN_PROCESS_KILLED,
            Self::SystemPrompt => wire::SYSTEM_PROMPT,
            Self::ContextInjected => wire::CONTEXT_INJECTED,
            Self::CognitionIn => wire::COGNITION_IN,
            Self::CognitionOut => wire::COGNITION_OUT,
            Self::CognitionReasoning => wire::COGNITION_REASONING,
            Self::ToolCallStart => wire::TOOL_CALL_START,
            Self::ToolCallResult => wire::TOOL_CALL_RESULT,
            Self::GraphStarted => wire::GRAPH_STARTED,
            Self::GraphCompleted => wire::GRAPH_COMPLETED,
            Self::GraphStepStarted => wire::GRAPH_STEP_STARTED,
            Self::GraphStepCompleted => wire::GRAPH_STEP_COMPLETED,
            Self::GraphBranchTaken => wire::GRAPH_BRANCH_TAKEN,
            Self::GraphForeachStarted => wire::GRAPH_FOREACH_STARTED,
            Self::GraphForeachIteration => wire::GRAPH_FOREACH_ITERATION,
            Self::GraphFollowSuspended => wire::GRAPH_FOLLOW_SUSPENDED,
            Self::GraphNodeRetry => wire::GRAPH_NODE_RETRY,
            Self::ProviderRetry => wire::PROVIDER_RETRY,
            Self::CostUntracked => wire::COST_UNTRACKED,
            Self::Milestone => wire::MILESTONE,
            Self::ThreadUsage => wire::THREAD_USAGE,
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            wire::THREAD_CREATED => Ok(Self::ThreadCreated),
            wire::THREAD_STARTED => Ok(Self::ThreadStarted),
            wire::THREAD_COMPLETED => Ok(Self::ThreadCompleted),
            wire::THREAD_FAILED => Ok(Self::ThreadFailed),
            wire::THREAD_CANCELLED => Ok(Self::ThreadCancelled),
            wire::THREAD_KILLED => Ok(Self::ThreadKilled),
            wire::THREAD_TIMED_OUT => Ok(Self::ThreadTimedOut),
            wire::THREAD_CONTINUED => Ok(Self::ThreadContinued),
            wire::EDGE_RECORDED => Ok(Self::EdgeRecorded),
            wire::CHILD_THREAD_SPAWNED => Ok(Self::ChildThreadSpawned),
            wire::CONTINUATION_REQUESTED => Ok(Self::ContinuationRequested),
            wire::CONTINUATION_ACCEPTED => Ok(Self::ContinuationAccepted),
            wire::COMMAND_SUBMITTED => Ok(Self::CommandSubmitted),
            wire::COMMAND_CLAIMED => Ok(Self::CommandClaimed),
            wire::COMMAND_COMPLETED => Ok(Self::CommandCompleted),
            wire::STREAM_OPENED => Ok(Self::StreamOpened),
            wire::TOKEN_DELTA => Ok(Self::TokenDelta),
            wire::STREAM_SNAPSHOT => Ok(Self::StreamSnapshot),
            wire::STREAM_CLOSED => Ok(Self::StreamClosed),
            wire::ARTIFACT_PUBLISHED => Ok(Self::ArtifactPublished),
            wire::AS_LAUNCHED_RESOLUTION => Ok(Self::AsLaunchedResolution),
            wire::AS_LAUNCHED_REF_BINDINGS => Ok(Self::AsLaunchedRefBindings),
            wire::RUNTIME_LAUNCH_FACTS => Ok(Self::RuntimeLaunchFacts),
            wire::THREAD_FACET_SET => Ok(Self::ThreadFacetSet),
            wire::THREAD_RECONCILED => Ok(Self::ThreadReconciled),
            wire::ORPHAN_PROCESS_KILLED => Ok(Self::OrphanProcessKilled),
            wire::SYSTEM_PROMPT => Ok(Self::SystemPrompt),
            wire::CONTEXT_INJECTED => Ok(Self::ContextInjected),
            wire::COGNITION_IN => Ok(Self::CognitionIn),
            wire::COGNITION_OUT => Ok(Self::CognitionOut),
            wire::COGNITION_REASONING => Ok(Self::CognitionReasoning),
            wire::TOOL_CALL_START => Ok(Self::ToolCallStart),
            wire::TOOL_CALL_RESULT => Ok(Self::ToolCallResult),
            wire::GRAPH_STARTED => Ok(Self::GraphStarted),
            wire::GRAPH_COMPLETED => Ok(Self::GraphCompleted),
            wire::GRAPH_STEP_STARTED => Ok(Self::GraphStepStarted),
            wire::GRAPH_STEP_COMPLETED => Ok(Self::GraphStepCompleted),
            wire::GRAPH_BRANCH_TAKEN => Ok(Self::GraphBranchTaken),
            wire::GRAPH_FOREACH_STARTED => Ok(Self::GraphForeachStarted),
            wire::GRAPH_FOREACH_ITERATION => Ok(Self::GraphForeachIteration),
            wire::GRAPH_FOLLOW_SUSPENDED => Ok(Self::GraphFollowSuspended),
            wire::GRAPH_NODE_RETRY => Ok(Self::GraphNodeRetry),
            wire::PROVIDER_RETRY => Ok(Self::ProviderRetry),
            wire::COST_UNTRACKED => Ok(Self::CostUntracked),
            wire::MILESTONE => Ok(Self::Milestone),
            wire::THREAD_USAGE => Ok(Self::ThreadUsage),
            other if other.trim().is_empty() => bail!("event_type must not be empty"),
            other => bail!("invalid event_type: {other}"),
        }
    }

    /// Whether this event carries part of the cognition transcript — the
    /// stimulus/response/tool exchange a chained successor folds to rebuild
    /// context. Transcript events must hard-fail on a missing callback channel
    /// rather than be silently dropped. Lifecycle, usage, streaming-delta and
    /// graph milestones are observability, not transcript.
    pub fn is_transcript(self) -> bool {
        matches!(
            self,
            Self::CognitionIn | Self::CognitionOut | Self::ToolCallStart | Self::ToolCallResult
        )
    }

    /// Whether this event ASSERTS the thread reached a terminal state — the
    /// thread-lifecycle terminals plus a graph's self-completion signal. A
    /// terminal event arriving for an already-terminal thread is a contradiction
    /// the braid must never silently carry as ordinary content, so the daemon
    /// append guard (`EventStoreService::append_batch`) rejects it; this is the
    /// shared SSOT classifier, so the emitter vocabulary and that guard cannot
    /// drift on what counts as terminal.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::ThreadCompleted
                | Self::ThreadFailed
                | Self::ThreadCancelled
                | Self::ThreadKilled
                | Self::ThreadTimedOut
                | Self::ThreadContinued
                | Self::GraphCompleted
        )
    }

    /// Canonical storage class for this event type.
    ///
    /// High-frequency progressive events (token deltas, reasoning
    /// chunks, foreach progress) are live-only `ephemeral`; everything
    /// else is an indexed milestone. The mapping is intentionally
    /// `match`-exhaustive so adding a new variant requires deciding
    /// its storage class up front.
    pub fn storage_class(self) -> StorageClass {
        match self {
            Self::TokenDelta
            | Self::StreamSnapshot
            | Self::CognitionReasoning
            | Self::GraphForeachIteration => StorageClass::Ephemeral,
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
            | Self::AsLaunchedResolution
            | Self::AsLaunchedRefBindings
            | Self::RuntimeLaunchFacts
            | Self::ThreadFacetSet
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
            | Self::GraphForeachStarted
            | Self::GraphFollowSuspended
            | Self::GraphNodeRetry
            | Self::ProviderRetry
            | Self::CostUntracked
            | Self::Milestone
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
            RuntimeEventType::AsLaunchedResolution,
            RuntimeEventType::AsLaunchedRefBindings,
            RuntimeEventType::RuntimeLaunchFacts,
            RuntimeEventType::ThreadFacetSet,
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
            RuntimeEventType::GraphForeachStarted,
            RuntimeEventType::GraphForeachIteration,
            RuntimeEventType::GraphFollowSuspended,
            RuntimeEventType::GraphNodeRetry,
            RuntimeEventType::ProviderRetry,
            RuntimeEventType::CostUntracked,
            RuntimeEventType::Milestone,
            RuntimeEventType::ThreadUsage,
        ];
        for v in variants {
            let s = v.as_str();
            let parsed =
                RuntimeEventType::parse(s).unwrap_or_else(|_| panic!("round-trip failed for {s}"));
            assert_eq!(v, parsed, "round-trip mismatch for {s}");
        }
    }

    #[test]
    fn terminal_events_are_exactly_the_lifecycle_terminals_plus_graph_completed() {
        // The daemon append guard rejects any of these onto an already-terminal
        // thread; a drift here silently reopens that gap. Thread-lifecycle
        // terminals plus the graph's self-completion signal are terminal.
        let terminal: Vec<&'static str> = [
            RuntimeEventType::ThreadCompleted,
            RuntimeEventType::ThreadFailed,
            RuntimeEventType::ThreadCancelled,
            RuntimeEventType::ThreadKilled,
            RuntimeEventType::ThreadTimedOut,
            RuntimeEventType::ThreadContinued,
            RuntimeEventType::GraphCompleted,
        ]
        .iter()
        .copied()
        .filter(|v| v.is_terminal())
        .map(RuntimeEventType::as_str)
        .collect();
        assert_eq!(
            terminal,
            vec![
                "thread_completed",
                "thread_failed",
                "thread_cancelled",
                "thread_killed",
                "thread_timed_out",
                "thread_continued",
                "graph_completed",
            ]
        );

        // Non-terminal events an already-terminal thread may still legitimately
        // carry (facet tags) or that mid-run precede terminal are not flagged.
        for v in [
            RuntimeEventType::ThreadCreated,
            RuntimeEventType::ThreadStarted,
            RuntimeEventType::GraphStarted,
            RuntimeEventType::GraphStepCompleted,
            RuntimeEventType::ThreadFacetSet,
            RuntimeEventType::CognitionOut,
            RuntimeEventType::Milestone,
        ] {
            assert!(!v.is_terminal(), "{} must not be terminal", v.as_str());
        }
    }

    #[test]
    fn parse_rejects_empty_and_unknown() {
        assert!(RuntimeEventType::parse("").is_err());
        assert!(RuntimeEventType::parse("   ").is_err());
        assert!(RuntimeEventType::parse("not_a_real_event").is_err());
    }

    #[test]
    fn ephemeral_set_is_exactly_the_progressive_events() {
        // Tightening this list belongs in a deliberate change — the
        // assertion guards against silent storage-class drift.
        let ephemeral: Vec<&'static str> = [
            RuntimeEventType::TokenDelta,
            RuntimeEventType::StreamSnapshot,
            RuntimeEventType::CognitionReasoning,
            RuntimeEventType::GraphForeachIteration,
        ]
        .iter()
        .copied()
        .filter(|v| v.storage_class() == StorageClass::Ephemeral)
        .map(|v| v.as_str())
        .collect();
        assert_eq!(
            ephemeral,
            vec![
                "token_delta",
                "stream_snapshot",
                "cognition_reasoning",
                "graph_foreach_iteration"
            ]
        );
    }

    #[test]
    fn storage_class_round_trip() {
        assert_eq!(StorageClass::Indexed.as_str(), "indexed");
        assert_eq!(StorageClass::JournalOnly.as_str(), "journal_only");
        assert_eq!(StorageClass::Ephemeral.as_str(), "ephemeral");
        assert_eq!(
            StorageClass::parse("indexed").unwrap(),
            StorageClass::Indexed
        );
        assert_eq!(
            StorageClass::parse("journal_only").unwrap(),
            StorageClass::JournalOnly
        );
        assert_eq!(
            StorageClass::parse("ephemeral").unwrap(),
            StorageClass::Ephemeral
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
