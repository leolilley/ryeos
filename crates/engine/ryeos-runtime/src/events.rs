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

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// Wire strings live in `ryeos-state` (the lower shared layer the projection also
// reads), so the enum and the projection reference one source of truth.
use ryeos_state::event_types as wire;

/// Framing prefix for structured, non-payload child timing records written to
/// captured stderr and re-emitted by the daemon after the child exits.
///
/// This is observability transport only. It is not a persisted runtime event
/// type or launch-envelope field.
pub const CAPTURED_CHILD_TIMING_PREFIX: &str = "RYEOS_CHILD_TIMING_JSON ";

/// Runtime-facing event admission ceilings. The daemon consumes these same
/// constants, so producers can form an atomic batch that the event store will
/// admit without duplicating wire limits.
pub const MAX_RUNTIME_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
pub const MAX_RUNTIME_EVENT_BATCH_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RUNTIME_EVENT_BATCH_ITEMS: usize = 64;

/// Versioned payload used only when a rendered `cognition_in` stimulus cannot
/// fit in one runtime event. The complete set is appended atomically and folds
/// back into one provider message; individual chunks are never provider turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionInChunk {
    pub schema_version: u32,
    pub content_hash: String,
    pub chunk_index: u32,
    pub chunk_count: u32,
    pub content_chunk: String,
}

/// Result of folding one `cognition_in` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CognitionInAssembly {
    /// A marker-only payload such as `{ "turn": 1 }`.
    Marker,
    /// A multipart stimulus is valid so far but is not complete.
    Pending,
    /// One complete stimulus, either inline or reassembled from chunks.
    Complete(String),
}

#[derive(Debug)]
struct PendingCognitionIn {
    content_hash: String,
    chunk_count: u32,
    next_chunk_index: u32,
    serialized_payload_bytes: usize,
    content: String,
}

/// Stateful validator/folder for the typed multipart `cognition_in` contract.
///
/// Chunk groups must be contiguous, begin at zero, have one stable digest and
/// count, stay within the runtime batch budgets, and finish with an exact
/// content-hash match. Call [`Self::finish`] at the end of a batch or replay.
#[derive(Debug, Default)]
pub struct CognitionInAssembler {
    pending: Option<PendingCognitionIn>,
}

impl CognitionInAssembler {
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn push(&mut self, payload: &Value) -> Result<CognitionInAssembly> {
        if payload.get("content_chunk").is_some() {
            let chunk: CognitionInChunk = serde_json::from_value(payload.clone())
                .context("invalid chunked cognition_in payload")?;
            validate_cognition_in_chunk(&chunk)?;
            let payload_bytes = serde_json::to_vec(payload)
                .context("serialize chunked cognition_in payload")?
                .len();
            if payload_bytes > MAX_RUNTIME_EVENT_PAYLOAD_BYTES {
                bail!(
                    "chunked cognition_in payload is {payload_bytes} bytes (max {})",
                    MAX_RUNTIME_EVENT_PAYLOAD_BYTES
                );
            }

            if chunk.chunk_index == 0 {
                if self.pending.is_some() {
                    bail!("chunked cognition_in began before the prior stimulus completed");
                }
                self.pending = Some(PendingCognitionIn {
                    content_hash: chunk.content_hash.clone(),
                    chunk_count: chunk.chunk_count,
                    next_chunk_index: 0,
                    serialized_payload_bytes: 0,
                    content: String::new(),
                });
            }

            let pending = self.pending.as_mut().ok_or_else(|| {
                anyhow::anyhow!(
                    "chunked cognition_in starts at index {}, expected 0",
                    chunk.chunk_index
                )
            })?;
            if chunk.content_hash != pending.content_hash {
                bail!("chunked cognition_in content_hash changed within one stimulus");
            }
            if chunk.chunk_count != pending.chunk_count {
                bail!("chunked cognition_in chunk_count changed within one stimulus");
            }
            if chunk.chunk_index != pending.next_chunk_index {
                bail!(
                    "chunked cognition_in index {} is out of order; expected {}",
                    chunk.chunk_index,
                    pending.next_chunk_index
                );
            }
            pending.serialized_payload_bytes = pending
                .serialized_payload_bytes
                .checked_add(payload_bytes)
                .context("chunked cognition_in byte count overflow")?;
            if pending.serialized_payload_bytes > MAX_RUNTIME_EVENT_BATCH_BYTES {
                bail!(
                    "chunked cognition_in payloads total {} bytes (max {})",
                    pending.serialized_payload_bytes,
                    MAX_RUNTIME_EVENT_BATCH_BYTES
                );
            }
            pending.content.push_str(&chunk.content_chunk);
            pending.next_chunk_index += 1;

            if pending.next_chunk_index == pending.chunk_count {
                let completed = self
                    .pending
                    .take()
                    .expect("completed cognition input must be pending");
                let observed = lillux::signature::content_hash(&completed.content);
                if observed != completed.content_hash {
                    bail!(
                        "chunked cognition_in content hash mismatch: expected {}, got {}",
                        completed.content_hash,
                        observed
                    );
                }
                return Ok(CognitionInAssembly::Complete(completed.content));
            }
            return Ok(CognitionInAssembly::Pending);
        }

        if self.pending.is_some() {
            bail!("chunked cognition_in was interrupted before all chunks arrived");
        }
        match payload.get("content") {
            Some(content) => Ok(CognitionInAssembly::Complete(
                content
                    .as_str()
                    .context("cognition_in content must be a string")?
                    .to_string(),
            )),
            None => Ok(CognitionInAssembly::Marker),
        }
    }

    pub fn finish(&self) -> Result<()> {
        if let Some(pending) = &self.pending {
            bail!(
                "chunked cognition_in ended after {} of {} chunks",
                pending.next_chunk_index,
                pending.chunk_count
            );
        }
        Ok(())
    }
}

fn validate_cognition_in_chunk(chunk: &CognitionInChunk) -> Result<()> {
    if chunk.schema_version != 1 {
        bail!(
            "unsupported chunked cognition_in schema_version {}",
            chunk.schema_version
        );
    }
    if chunk.content_hash.len() != 64
        || !chunk
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("chunked cognition_in content_hash must be lowercase SHA-256 hex");
    }
    if !(2..=MAX_RUNTIME_EVENT_BATCH_ITEMS as u32).contains(&chunk.chunk_count) {
        bail!(
            "chunked cognition_in chunk_count must be between 2 and {}",
            MAX_RUNTIME_EVENT_BATCH_ITEMS
        );
    }
    if chunk.chunk_index >= chunk.chunk_count {
        bail!(
            "chunked cognition_in chunk_index {} is outside chunk_count {}",
            chunk.chunk_index,
            chunk.chunk_count
        );
    }
    if chunk.content_chunk.is_empty() {
        bail!("chunked cognition_in content_chunk must not be empty");
    }
    Ok(())
}

/// Encode a rendered stimulus as one inline payload when possible, otherwise
/// as an atomic, hash-bound multipart payload set under the runtime event
/// admission ceilings.
pub fn encode_cognition_in_payloads(content: &str) -> Result<Vec<Value>> {
    let inline = json!({ "content": content });
    if serde_json::to_vec(&inline)
        .context("serialize cognition_in payload")?
        .len()
        <= MAX_RUNTIME_EVENT_PAYLOAD_BYTES
    {
        return Ok(vec![inline]);
    }
    if content.len() > MAX_RUNTIME_EVENT_BATCH_BYTES {
        bail!(
            "rendered cognition_in is {} UTF-8 bytes (atomic event-batch max {})",
            content.len(),
            MAX_RUNTIME_EVENT_BATCH_BYTES
        );
    }

    let content_hash = lillux::signature::content_hash(content);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < content.len() {
        let mut low = start + 1;
        let mut high = content.len();
        let mut best = None;
        while low <= high {
            let middle = low + (high - low) / 2;
            let mut end = middle;
            while end > start && !content.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                low = middle + 1;
                continue;
            }
            let candidate = CognitionInChunk {
                schema_version: 1,
                content_hash: content_hash.clone(),
                chunk_index: (MAX_RUNTIME_EVENT_BATCH_ITEMS - 1) as u32,
                chunk_count: MAX_RUNTIME_EVENT_BATCH_ITEMS as u32,
                content_chunk: content[start..end].to_string(),
            };
            let bytes = serde_json::to_vec(&candidate)
                .context("serialize candidate cognition_in chunk")?
                .len();
            if bytes <= MAX_RUNTIME_EVENT_PAYLOAD_BYTES {
                best = Some(end);
                low = middle + 1;
            } else {
                high = end.saturating_sub(1);
            }
        }
        let end = best.context("one UTF-8 scalar does not fit in a cognition_in chunk")?;
        chunks.push(content[start..end].to_string());
        if chunks.len() > MAX_RUNTIME_EVENT_BATCH_ITEMS {
            bail!(
                "rendered cognition_in requires more than {} atomic event chunks",
                MAX_RUNTIME_EVENT_BATCH_ITEMS
            );
        }
        start = end;
    }
    if chunks.len() < 2 {
        bail!("oversized cognition_in did not produce a multipart payload");
    }

    let chunk_count = u32::try_from(chunks.len()).context("cognition_in chunk count overflow")?;
    let payloads = chunks
        .into_iter()
        .enumerate()
        .map(|(chunk_index, content_chunk)| {
            serde_json::to_value(CognitionInChunk {
                schema_version: 1,
                content_hash: content_hash.clone(),
                chunk_index: u32::try_from(chunk_index)
                    .expect("runtime event chunk count is bounded by u32"),
                chunk_count,
                content_chunk,
            })
            .expect("CognitionInChunk serialization cannot fail")
        })
        .collect::<Vec<_>>();

    let total_bytes = payloads.iter().try_fold(0usize, |total, payload| {
        let bytes = serde_json::to_vec(payload)
            .context("serialize chunked cognition_in payload")?
            .len();
        total
            .checked_add(bytes)
            .context("chunked cognition_in byte count overflow")
    })?;
    if total_bytes > MAX_RUNTIME_EVENT_BATCH_BYTES {
        bail!(
            "chunked cognition_in payloads total {total_bytes} bytes (max {})",
            MAX_RUNTIME_EVENT_BATCH_BYTES
        );
    }

    let mut assembler = CognitionInAssembler::default();
    let mut recovered = None;
    for payload in &payloads {
        if let CognitionInAssembly::Complete(content) = assembler.push(payload)? {
            recovered = Some(content);
        }
    }
    assembler.finish()?;
    if recovered.as_deref() != Some(content) {
        bail!("chunked cognition_in failed its local reassembly check");
    }

    Ok(payloads)
}

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
    /// A launch augmentation reused a process-local projection instead of
    /// fabricating a child execution. Kept outside the composed launch input.
    LaunchAugmentationCacheHit,
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
    ProviderAttemptBudgetTransitionV1,
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
            Self::LaunchAugmentationCacheHit => wire::LAUNCH_AUGMENTATION_CACHE_HIT,
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
            Self::ProviderAttemptBudgetTransitionV1 => wire::PROVIDER_ATTEMPT_BUDGET_TRANSITION_V1,
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
            wire::LAUNCH_AUGMENTATION_CACHE_HIT => Ok(Self::LaunchAugmentationCacheHit),
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
            wire::PROVIDER_ATTEMPT_BUDGET_TRANSITION_V1 => {
                Ok(Self::ProviderAttemptBudgetTransitionV1)
            }
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
            | Self::LaunchAugmentationCacheHit
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
            | Self::ProviderAttemptBudgetTransitionV1
            | Self::CostUntracked
            | Self::Milestone
            | Self::ThreadUsage => StorageClass::Indexed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_cognition_input_round_trips_as_bounded_atomic_chunks() {
        let content = "ARC evidence \"quoted\"\n".repeat(16_000);
        let payloads = encode_cognition_in_payloads(&content).unwrap();

        assert!(payloads.len() > 1);
        assert!(payloads.len() <= MAX_RUNTIME_EVENT_BATCH_ITEMS);
        let total = payloads
            .iter()
            .map(|payload| {
                let bytes = serde_json::to_vec(payload).unwrap().len();
                assert!(bytes <= MAX_RUNTIME_EVENT_PAYLOAD_BYTES);
                bytes
            })
            .sum::<usize>();
        assert!(total <= MAX_RUNTIME_EVENT_BATCH_BYTES);

        let mut assembler = CognitionInAssembler::default();
        let mut recovered = None;
        for payload in &payloads {
            if let CognitionInAssembly::Complete(content) = assembler.push(payload).unwrap() {
                recovered = Some(content);
            }
        }
        assembler.finish().unwrap();
        assert_eq!(recovered.as_deref(), Some(content.as_str()));
    }

    #[test]
    fn chunked_cognition_input_rejects_missing_or_tampered_parts() {
        let content = "x".repeat(MAX_RUNTIME_EVENT_PAYLOAD_BYTES + 1024);
        let mut payloads = encode_cognition_in_payloads(&content).unwrap();
        assert!(payloads.len() > 1);

        let mut incomplete = CognitionInAssembler::default();
        incomplete.push(&payloads[0]).unwrap();
        assert!(incomplete.finish().is_err());

        payloads[1]["content_chunk"] = json!("tampered");
        let mut tampered = CognitionInAssembler::default();
        let mut error = None;
        for payload in &payloads {
            let push_result = tampered.push(payload);
            match push_result {
                Ok(_) => {}
                Err(observed) => {
                    error = Some(observed);
                    break;
                }
            }
        }
        assert!(error
            .map(|error| error.to_string().contains("content hash mismatch"))
            .unwrap_or(false));
    }

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
            RuntimeEventType::LaunchAugmentationCacheHit,
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
            RuntimeEventType::ProviderAttemptBudgetTransitionV1,
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
