//! Single source of truth for the runtime event vocabulary (V5.5 D11).
//!
//! Both runtime emitters (this crate's `CallbackClient`, the directive
//! / graph / knowledge runtimes) and the daemon's
//! `EventStoreService::validate_event_type` consume the same enum.
//! That eliminates drift: there is no separate "string allow-list" on
//! either side. Adding a new event variant is a single-edit change
//! that both sides see at compile time.
//!
//! The daemon stores every runtime-appended event under the node's signed
//! `event_storage` policy. `RuntimeEventType::is_progressive()` is the code
//! floor: only progressive events may be stored as `ephemeral` or
//! `journal_only`; every lifecycle, audit, launch and terminal event is
//! always `indexed`. Runtimes send no storage class. `RuntimeEventType::storage_class()`
//! is the class the shipped init profiles assign.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_provider_contract::Part;

use crate::cognition::{CognitionInput, TurnMarker, validate_token};

// Wire strings live in `ryeos-state` (the lower shared layer the projection also
// reads), so the enum and the projection reference one source of truth.
use ryeos_state::event_types as wire;

/// Framing prefix for structured child observations written to captured stderr
/// and re-emitted by the daemon after the child exits.
///
/// This is observability transport only. Record names, versions, clock domains,
/// and admission bounds are declared by the selected signed runtime descriptor;
/// the engine and executor treat the remaining payload as opaque.
pub const CAPTURED_CHILD_OBSERVATION_PREFIX: &str = "RYEOS_CHILD_OBSERVATION_JSON ";

const MAX_CHILD_OBSERVATION_ID_BYTES: usize = 256;

/// Generic outer frame for one signed-runtime-declared child observation.
///
/// Runtime-owned fields are flattened into `payload`. The daemon validates only
/// this generic identity/version/clock frame against the selected runtime's
/// signed declaration and never interprets payload names or values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeChildObservationRecord {
    pub event: String,
    pub schema_version: u32,
    pub clock_domain: String,
    pub invocation_id: Option<String>,
    pub thread_id: Option<String>,
    #[serde(flatten)]
    pub payload: BTreeMap<String, Value>,
}

impl RuntimeChildObservationRecord {
    pub fn decode_declared(
        expected_thread_id: &str,
        encoded: &str,
        declarations: &BTreeMap<String, ryeos_engine::runtime_registry::ChildObservationDecl>,
    ) -> Option<Self> {
        let record = serde_json::from_str::<Self>(encoded).ok()?;
        let declaration = declarations.get(&record.event)?;
        let identity_is_valid = record.invocation_id.as_deref().is_some_and(|value| {
            !value.is_empty()
                && value.len() <= MAX_CHILD_OBSERVATION_ID_BYTES
                && !value.chars().any(char::is_control)
        });
        (encoded.len() <= declaration.max_record_bytes as usize
            && record.schema_version == declaration.schema_version
            && record.clock_domain == declaration.clock_domain
            && identity_is_valid
            && record.thread_id.as_deref() == Some(expected_thread_id))
        .then_some(record)
    }
}

/// Runtime-facing event admission ceilings. The daemon consumes these same
/// constants, so producers can form an atomic batch that the event store will
/// admit without duplicating wire limits.
pub const MAX_RUNTIME_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
pub const MAX_RUNTIME_EVENT_BATCH_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RUNTIME_EVENT_BATCH_ITEMS: usize = 64;

pub const HOOK_OBSERVATION_SCHEMA: &str = "ryeos.hook_observation.v1";
pub const HOOK_FAILURE_SCHEMA: &str = "ryeos.hook_failure.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEvidenceDescriptor {
    pub id: String,
    pub layer: crate::hooks_loader::HookLayer,
    pub event: String,
    pub occurrence: crate::callback::HookDispatchOccurrence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookObservationRecordedPayload {
    pub schema_version: String,
    pub observation_id: String,
    pub dispatch_key: String,
    pub occurrence_thread_id: String,
    pub hook: HookEvidenceDescriptor,
    pub context_hash: String,
    pub response_hash: String,
    pub observation: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailureClass {
    Child,
    ChildIntegrity,
    DispatchEnvelopeIntegrity,
    ObservationInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookFailedPayload {
    pub schema_version: String,
    pub failure_id: String,
    pub dispatch_key: String,
    pub occurrence_thread_id: String,
    pub hook: HookEvidenceDescriptor,
    pub context_hash: String,
    pub response_hash: String,
    pub failure_class: HookFailureClass,
}

/// Versioned payload used only when a rendered `cognition_in` input cannot
/// fit in one runtime event. The complete set is appended atomically and folds
/// back into one input; individual chunks are never provider turns. Every
/// chunk of a set repeats the input's origin, kind, acknowledgement and pin,
/// and the assembler refuses a set whose chunks disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionInChunk {
    pub schema_version: u32,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    pub content_hash: String,
    pub chunk_index: u32,
    pub chunk_count: u32,
    pub content_chunk: String,
}

/// Result of folding one `cognition_in` payload.
#[derive(Debug, Clone, PartialEq)]
pub enum CognitionInAssembly {
    /// A turn marker such as `{ "turn": 1 }`.
    Marker(TurnMarker),
    /// A multipart input is valid so far but is not complete.
    Pending,
    /// One complete input, either inline or reassembled from chunks.
    Complete(CognitionInput),
}

#[derive(Debug)]
struct PendingCognitionIn {
    origin: String,
    kind: Option<String>,
    ack: Option<String>,
    pinned: bool,
    content_hash: String,
    chunk_count: u32,
    next_chunk_index: u32,
    serialized_payload_bytes: usize,
    content: String,
}

/// Stateful validator/folder for the typed multipart `cognition_in` contract.
///
/// Chunk groups must be contiguous, begin at zero, have one stable digest,
/// count and origin, stay within the runtime batch budgets, and finish with an
/// exact content-hash match. Call [`Self::finish`] at the end of a batch or
/// replay.
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
                    bail!("chunked cognition_in began before the prior input completed");
                }
                self.pending = Some(PendingCognitionIn {
                    origin: chunk.origin.clone(),
                    kind: chunk.kind.clone(),
                    ack: chunk.ack.clone(),
                    pinned: chunk.pinned,
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
                bail!("chunked cognition_in content_hash changed within one input");
            }
            if chunk.chunk_count != pending.chunk_count {
                bail!("chunked cognition_in chunk_count changed within one input");
            }
            if chunk.origin != pending.origin
                || chunk.kind != pending.kind
                || chunk.ack != pending.ack
                || chunk.pinned != pending.pinned
            {
                bail!("chunked cognition_in origin, kind, ack or pinned changed within one input");
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
                let input = CognitionInput {
                    origin: completed.origin,
                    parts: vec![Part::text(completed.content)],
                    kind: completed.kind,
                    ack: completed.ack,
                    pinned: completed.pinned,
                };
                input.validate()?;
                return Ok(CognitionInAssembly::Complete(input));
            }
            return Ok(CognitionInAssembly::Pending);
        }

        if self.pending.is_some() {
            bail!("chunked cognition_in was interrupted before all chunks arrived");
        }
        if payload.get("turn").is_some() {
            let marker: TurnMarker = serde_json::from_value(payload.clone())
                .context("invalid cognition_in turn marker")?;
            marker.validate()?;
            return Ok(CognitionInAssembly::Marker(marker));
        }
        let input: CognitionInput = serde_json::from_value(payload.clone())
            .context("cognition_in is neither a turn marker nor an input with origin and parts")?;
        input.validate()?;
        Ok(CognitionInAssembly::Complete(input))
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
    validate_token("chunked cognition_in origin", &chunk.origin)?;
    if let Some(kind) = &chunk.kind {
        validate_token("chunked cognition_in kind", kind)?;
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

/// Encode an input as one inline payload when possible, otherwise as an
/// atomic, hash-bound multipart payload set under the runtime event
/// admission ceilings. Only an input that is exactly one text part can be
/// split; an input with images never is.
pub fn encode_cognition_in_payloads(input: &CognitionInput) -> Result<Vec<Value>> {
    input.validate()?;
    let inline = serde_json::to_value(input).context("serialize cognition_in payload")?;
    if serde_json::to_vec(&inline)
        .context("serialize cognition_in payload")?
        .len()
        <= MAX_RUNTIME_EVENT_PAYLOAD_BYTES
    {
        return Ok(vec![inline]);
    }
    let Some(content) = input.single_text() else {
        bail!(
            "cognition_in with {} parts exceeds the {} byte event payload ceiling and only a \
             single text part can be split",
            input.parts.len(),
            MAX_RUNTIME_EVENT_PAYLOAD_BYTES
        );
    };
    if content.len() > MAX_RUNTIME_EVENT_BATCH_BYTES {
        bail!(
            "rendered cognition_in is {} UTF-8 bytes (atomic event-batch max {})",
            content.len(),
            MAX_RUNTIME_EVENT_BATCH_BYTES
        );
    }

    let content_hash = lillux::signature::content_hash(content);
    let payloads = encode_bounded_text_payloads(content, |chunk_index, chunk_count, chunk| {
        serde_json::to_value(CognitionInChunk {
            schema_version: 1,
            origin: input.origin.clone(),
            kind: input.kind.clone(),
            ack: input.ack.clone(),
            pinned: input.pinned,
            content_hash: content_hash.clone(),
            chunk_index,
            chunk_count,
            content_chunk: chunk.to_string(),
        })
        .expect("a cognition_in chunk serialises")
    })?;

    let mut assembler = CognitionInAssembler::default();
    let mut recovered = None;
    for payload in &payloads {
        if let CognitionInAssembly::Complete(complete) = assembler.push(payload)? {
            recovered = Some(complete);
        }
    }
    assembler.finish()?;
    if recovered.as_ref() != Some(input) {
        bail!("chunked cognition_in failed its local reassembly check");
    }

    Ok(payloads)
}

/// Encodes several inputs as the payloads of one atomic batch under a single
/// budget. `reserved_items` and `reserved_bytes` are what the batch's other
/// events (a frame) already use. The batch may hold at most
/// [`MAX_RUNTIME_EVENT_BATCH_ITEMS`] events and
/// [`MAX_RUNTIME_EVENT_BATCH_BYTES`] bytes of payload in all; one that needs
/// more fails here, before anything is appended.
pub fn encode_cognition_in_batch(
    reserved_items: usize,
    reserved_bytes: usize,
    inputs: &[CognitionInput],
) -> Result<Vec<Value>> {
    let mut payloads = Vec::new();
    for input in inputs {
        payloads.extend(encode_cognition_in_payloads(input)?);
    }
    let bytes = payloads.iter().try_fold(reserved_bytes, |total, payload| {
        Ok::<_, anyhow::Error>(total + serde_json::to_vec(payload)?.len())
    })?;
    let items = reserved_items + payloads.len();
    if items > MAX_RUNTIME_EVENT_BATCH_ITEMS || bytes > MAX_RUNTIME_EVENT_BATCH_BYTES {
        bail!(
            "launch records need {items} events and {bytes} payload bytes; one atomic batch \
             holds at most {MAX_RUNTIME_EVENT_BATCH_ITEMS} events and \
             {MAX_RUNTIME_EVENT_BATCH_BYTES} bytes"
        );
    }
    Ok(payloads)
}

/// Project bounded UTF-8 content into one atomic event batch. The caller owns
/// the payload schema; this helper owns only exact JSON byte budgeting and
/// scalar boundaries. It does not publish, truncate, or introduce a queue.
/// The encoder must not grow when its indices/count shrink from the maximum.
pub fn encode_bounded_text_payloads(
    content: &str,
    encode: impl Fn(u32, u32, &str) -> Value,
) -> Result<Vec<Value>> {
    if content.len() > MAX_RUNTIME_EVENT_BATCH_BYTES {
        bail!("text exceeds atomic event batch byte ceiling");
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    loop {
        let mut low = start;
        let mut high = content.len();
        let mut best = None;
        while low <= high {
            let middle = low + (high - low) / 2;
            let mut end = middle;
            while end > start && !content.is_char_boundary(end) {
                end -= 1;
            }
            let candidate = encode(
                (MAX_RUNTIME_EVENT_BATCH_ITEMS - 1) as u32,
                MAX_RUNTIME_EVENT_BATCH_ITEMS as u32,
                &content[start..end],
            );
            if serde_json::to_vec(&candidate)?.len() <= MAX_RUNTIME_EVENT_PAYLOAD_BYTES {
                best = Some(end);
                low = middle + 1;
            } else if middle == 0 {
                break;
            } else {
                high = middle - 1;
            }
        }
        let end = best.context("event metadata exceeds payload byte ceiling")?;
        if end == start && start < content.len() {
            bail!("one UTF-8 scalar does not fit in event payload");
        }
        chunks.push(&content[start..end]);
        if chunks.len() > MAX_RUNTIME_EVENT_BATCH_ITEMS {
            bail!("text exceeds atomic event batch item ceiling");
        }
        start = end;
        if start == content.len() {
            break;
        }
    }
    let count = chunks.len() as u32;
    let mut total = 0usize;
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let payload = encode(index as u32, count, chunk);
            let bytes = serde_json::to_vec(&payload)?.len();
            total = total
                .checked_add(bytes)
                .context("event batch byte overflow")?;
            if bytes > MAX_RUNTIME_EVENT_PAYLOAD_BYTES || total > MAX_RUNTIME_EVENT_BATCH_BYTES {
                bail!("encoded text exceeds atomic event admission ceilings");
            }
            Ok(payload)
        })
        .collect()
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
    ChildThreadSettled,
    WorkerContinuationAdmitted,
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
    /// Generic ephemeral runtime observation. The payload carries a
    /// runtime-owned namespaced `kind`; shared layers do not interpret it.
    Observation,

    // ── Daemon-authored hook evidence ──────────────────────────
    HookObservationRecorded,
    HookFailed,
    ProjectObservationRecorded,
    ProviderCallObservationRecorded,

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
    /// Daemon-authored record of the root kind a launch attempt's process
    /// inputs were laid out in (no paths).
    ProcessInputRootSelected,
    /// Daemon-authored record, once per directive launch attempt, of the
    /// inventory tools that declare `reads` (tool ref, argument, space).
    LaunchToolReads,
    /// Daemon-authored record, in a rebound successor's birth batch, of the
    /// ref bindings an operator replaced and the launch facts before and after.
    LaunchBindingRebound,
    /// A `(key, value)` metadata tag stamped on a thread post-launch (cohort
    /// identity, e.g. `fleet`/`game`). Event-backed so it survives a projection
    /// rebuild, unlike a bare facet-table write.
    ThreadFacetSet,

    // ── Cognition / replay transcript ───────────────────────────
    CognitionIn,
    CognitionOut,
    CognitionReasoning,
    /// What the model is given that is not a conversation message and is
    /// stable across turns (system parts, tool inventory reference, request
    /// options, provider configuration hash). Always indexed.
    CognitionFrame,
    /// Reserved record that a span of earlier turns is replaced by a summary.
    /// Registered so the vocabulary is closed; nothing emits it yet. Always
    /// indexed.
    CognitionCompaction,

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
    /// Operator-authored triage record: one attention item was acknowledged.
    /// Daemon-authored through the attention service; a runtime cannot append it.
    AttentionAcknowledged,
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
            Self::ChildThreadSettled => wire::CHILD_THREAD_SETTLED,
            Self::WorkerContinuationAdmitted => wire::WORKER_CONTINUATION_ADMITTED,
            Self::ContinuationRequested => wire::CONTINUATION_REQUESTED,
            Self::ContinuationAccepted => wire::CONTINUATION_ACCEPTED,
            Self::CommandSubmitted => wire::COMMAND_SUBMITTED,
            Self::CommandClaimed => wire::COMMAND_CLAIMED,
            Self::CommandCompleted => wire::COMMAND_COMPLETED,
            Self::StreamOpened => wire::STREAM_OPENED,
            Self::TokenDelta => wire::TOKEN_DELTA,
            Self::StreamSnapshot => wire::STREAM_SNAPSHOT,
            Self::StreamClosed => wire::STREAM_CLOSED,
            Self::Observation => wire::OBSERVATION,
            Self::HookObservationRecorded => wire::HOOK_OBSERVATION_RECORDED,
            Self::HookFailed => wire::HOOK_FAILED,
            Self::ProjectObservationRecorded => wire::PROJECT_OBSERVATION_RECORDED,
            Self::ProviderCallObservationRecorded => wire::PROVIDER_CALL_OBSERVATION_RECORDED,
            Self::ArtifactPublished => wire::ARTIFACT_PUBLISHED,
            Self::AsLaunchedResolution => wire::AS_LAUNCHED_RESOLUTION,
            Self::AsLaunchedRefBindings => wire::AS_LAUNCHED_REF_BINDINGS,
            Self::RuntimeLaunchFacts => wire::RUNTIME_LAUNCH_FACTS,
            Self::LaunchAugmentationCacheHit => wire::LAUNCH_AUGMENTATION_CACHE_HIT,
            Self::ProcessInputRootSelected => wire::PROCESS_INPUT_ROOT_SELECTED,
            Self::LaunchToolReads => wire::LAUNCH_TOOL_READS,
            Self::LaunchBindingRebound => wire::LAUNCH_BINDING_REBOUND,
            Self::ThreadFacetSet => wire::THREAD_FACET_SET,
            Self::CognitionIn => wire::COGNITION_IN,
            Self::CognitionOut => wire::COGNITION_OUT,
            Self::CognitionReasoning => wire::COGNITION_REASONING,
            Self::CognitionFrame => wire::COGNITION_FRAME,
            Self::CognitionCompaction => wire::COGNITION_COMPACTION,
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
            Self::AttentionAcknowledged => wire::ATTENTION_ACKNOWLEDGED,
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
            wire::CHILD_THREAD_SETTLED => Ok(Self::ChildThreadSettled),
            wire::WORKER_CONTINUATION_ADMITTED => Ok(Self::WorkerContinuationAdmitted),
            wire::CONTINUATION_REQUESTED => Ok(Self::ContinuationRequested),
            wire::CONTINUATION_ACCEPTED => Ok(Self::ContinuationAccepted),
            wire::COMMAND_SUBMITTED => Ok(Self::CommandSubmitted),
            wire::COMMAND_CLAIMED => Ok(Self::CommandClaimed),
            wire::COMMAND_COMPLETED => Ok(Self::CommandCompleted),
            wire::STREAM_OPENED => Ok(Self::StreamOpened),
            wire::TOKEN_DELTA => Ok(Self::TokenDelta),
            wire::STREAM_SNAPSHOT => Ok(Self::StreamSnapshot),
            wire::STREAM_CLOSED => Ok(Self::StreamClosed),
            wire::OBSERVATION => Ok(Self::Observation),
            wire::HOOK_OBSERVATION_RECORDED => Ok(Self::HookObservationRecorded),
            wire::HOOK_FAILED => Ok(Self::HookFailed),
            wire::PROJECT_OBSERVATION_RECORDED => Ok(Self::ProjectObservationRecorded),
            wire::PROVIDER_CALL_OBSERVATION_RECORDED => Ok(Self::ProviderCallObservationRecorded),
            wire::ARTIFACT_PUBLISHED => Ok(Self::ArtifactPublished),
            wire::AS_LAUNCHED_RESOLUTION => Ok(Self::AsLaunchedResolution),
            wire::AS_LAUNCHED_REF_BINDINGS => Ok(Self::AsLaunchedRefBindings),
            wire::RUNTIME_LAUNCH_FACTS => Ok(Self::RuntimeLaunchFacts),
            wire::LAUNCH_AUGMENTATION_CACHE_HIT => Ok(Self::LaunchAugmentationCacheHit),
            wire::PROCESS_INPUT_ROOT_SELECTED => Ok(Self::ProcessInputRootSelected),
            wire::LAUNCH_TOOL_READS => Ok(Self::LaunchToolReads),
            wire::LAUNCH_BINDING_REBOUND => Ok(Self::LaunchBindingRebound),
            wire::THREAD_FACET_SET => Ok(Self::ThreadFacetSet),
            wire::COGNITION_IN => Ok(Self::CognitionIn),
            wire::COGNITION_OUT => Ok(Self::CognitionOut),
            wire::COGNITION_REASONING => Ok(Self::CognitionReasoning),
            wire::COGNITION_FRAME => Ok(Self::CognitionFrame),
            wire::COGNITION_COMPACTION => Ok(Self::CognitionCompaction),
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
            wire::ATTENTION_ACKNOWLEDGED => Ok(Self::AttentionAcknowledged),
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
            Self::CognitionIn
                | Self::CognitionOut
                | Self::CognitionFrame
                | Self::CognitionCompaction
                | Self::ToolCallStart
                | Self::ToolCallResult
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

    /// Whether the node's signed `event_storage` policy chooses this event's
    /// storage class. Only these high-frequency progressive events may be
    /// stored as `ephemeral` or `journal_only`; every other event (lifecycle,
    /// audit, launch, terminal, transcript) is always `indexed`, whatever a
    /// policy or a runtime says. Intentionally `match`-exhaustive.
    pub fn is_progressive(self) -> bool {
        match self {
            Self::TokenDelta
            | Self::StreamSnapshot
            | Self::CognitionReasoning
            | Self::Observation
            | Self::GraphForeachIteration => true,
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
            | Self::ChildThreadSettled
            | Self::WorkerContinuationAdmitted
            | Self::ContinuationRequested
            | Self::ContinuationAccepted
            | Self::CommandSubmitted
            | Self::CommandClaimed
            | Self::CommandCompleted
            | Self::StreamOpened
            | Self::StreamClosed
            | Self::HookObservationRecorded
            | Self::HookFailed
            | Self::ProjectObservationRecorded
            | Self::ProviderCallObservationRecorded
            | Self::ArtifactPublished
            | Self::AsLaunchedResolution
            | Self::AsLaunchedRefBindings
            | Self::RuntimeLaunchFacts
            | Self::LaunchAugmentationCacheHit
            | Self::ProcessInputRootSelected
            | Self::LaunchToolReads
            | Self::LaunchBindingRebound
            | Self::ThreadFacetSet
            | Self::CognitionIn
            | Self::CognitionOut
            | Self::CognitionFrame
            | Self::CognitionCompaction
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
            | Self::ThreadUsage
            | Self::AttentionAcknowledged => false,
        }
    }

    /// The storage class every shipped init profile assigns. Daemon-authored
    /// events written straight to the state store (all on the indexed floor)
    /// use it; a runtime-appended event carries no class and is stored under
    /// the node's signed `event_storage` policy within the floor of
    /// [`Self::is_progressive`].
    ///
    /// Token, stream-snapshot and reasoning deltas are live-only
    /// `ephemeral`: the turn-complete `cognition_out` keeps their text, so
    /// only timing is lost. `observation` and `graph_foreach_iteration`
    /// carry information nothing else keeps, so they are `journal_only`:
    /// stored in CAS, not indexed. Everything else is an indexed
    /// milestone. The mapping is intentionally `match`-exhaustive so adding
    /// a new variant requires deciding its storage class up front.
    pub fn storage_class(self) -> StorageClass {
        match self {
            Self::TokenDelta | Self::StreamSnapshot | Self::CognitionReasoning => {
                StorageClass::Ephemeral
            }
            Self::Observation | Self::GraphForeachIteration => StorageClass::JournalOnly,
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
            | Self::ChildThreadSettled
            | Self::WorkerContinuationAdmitted
            | Self::ContinuationRequested
            | Self::ContinuationAccepted
            | Self::CommandSubmitted
            | Self::CommandClaimed
            | Self::CommandCompleted
            | Self::StreamOpened
            | Self::StreamClosed
            | Self::HookObservationRecorded
            | Self::HookFailed
            | Self::ProjectObservationRecorded
            | Self::ProviderCallObservationRecorded
            | Self::ArtifactPublished
            | Self::AsLaunchedResolution
            | Self::AsLaunchedRefBindings
            | Self::RuntimeLaunchFacts
            | Self::LaunchAugmentationCacheHit
            | Self::ProcessInputRootSelected
            | Self::LaunchToolReads
            | Self::LaunchBindingRebound
            | Self::ThreadFacetSet
            | Self::CognitionIn
            | Self::CognitionOut
            | Self::CognitionFrame
            | Self::CognitionCompaction
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
            | Self::ThreadUsage
            | Self::AttentionAcknowledged => StorageClass::Indexed,
        }
    }
}

/// Whether a `cognition_out` payload is a progressive streaming fragment
/// (a text `delta`, a `tool_use_partial`, or a mid-stream complete
/// `tool_use`) rather than the turn-complete record. Fragments are live-only:
/// the durable record of a turn, its tool calls included, is the
/// turn-complete `cognition_out{tool_calls}`, and persisting the mid-stream
/// `tool_use` too would fold a spurious extra assistant turn on resume.
/// Runtime emitters and the daemon's storage decision share this rule.
pub fn cognition_out_is_progressive(payload: &Value) -> bool {
    payload.get("delta").is_some()
        || payload.get("tool_use_partial").is_some()
        || payload.get("tool_use").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn launch_input(content: &str) -> CognitionInput {
        CognitionInput::text("launch", content).pinned()
    }

    #[test]
    fn large_cognition_input_round_trips_as_bounded_atomic_chunks() {
        let content = "ARC evidence \"quoted\"\n".repeat(16_000);
        let input = launch_input(&content);
        let payloads = encode_cognition_in_payloads(&input).unwrap();

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
        assert_eq!(recovered.as_ref(), Some(&input));
    }

    #[test]
    fn chunked_cognition_input_rejects_missing_or_tampered_parts() {
        let content = "x".repeat(MAX_RUNTIME_EVENT_PAYLOAD_BYTES + 1024);
        let mut payloads = encode_cognition_in_payloads(&launch_input(&content)).unwrap();
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
        assert!(
            error
                .map(|error| error.to_string().contains("content hash mismatch"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn a_batch_of_inputs_shares_one_budget() {
        let small = CognitionInput::text("context", "background");
        let payloads =
            encode_cognition_in_batch(1, 1_000, &[small.clone(), launch_input("go"), small])
                .unwrap();
        assert_eq!(payloads.len(), 3);

        // Each input fits alone; together with the reserved frame they do not.
        let big = launch_input(&"z".repeat(MAX_RUNTIME_EVENT_BATCH_BYTES - 512 * 1024));
        let error = encode_cognition_in_batch(1, 3 * 1024 * 1024, &[big])
            .unwrap_err()
            .to_string();
        assert!(error.contains("one atomic batch holds at most"), "{error}");

        let many: Vec<_> = (0..MAX_RUNTIME_EVENT_BATCH_ITEMS)
            .map(|index| CognitionInput::text("context", format!("c{index}")))
            .collect();
        assert!(encode_cognition_in_batch(0, 0, &many).is_ok());
        assert!(encode_cognition_in_batch(1, 0, &many).is_err());
    }

    #[test]
    fn chunks_of_one_input_must_agree_on_origin_and_pin() {
        let content = "y".repeat(MAX_RUNTIME_EVENT_PAYLOAD_BYTES + 1024);
        let mut payloads = encode_cognition_in_payloads(&launch_input(&content)).unwrap();
        payloads[1]["origin"] = json!("operator");
        let mut assembler = CognitionInAssembler::default();
        assembler.push(&payloads[0]).unwrap();
        let error = assembler.push(&payloads[1]).unwrap_err().to_string();
        assert!(error.contains("changed within one input"), "{error}");
    }

    #[test]
    fn inline_input_marker_and_malformed_payloads_are_told_apart() {
        let mut assembler = CognitionInAssembler::default();
        assert_eq!(
            assembler
                .push(&json!({"turn": 3, "purpose": "turn"}))
                .unwrap(),
            CognitionInAssembly::Marker(TurnMarker {
                turn: 3,
                purpose: crate::cognition::TurnPurpose::Turn,
                tools_offered: None
            })
        );
        assert!(matches!(
            assembler
                .push(&json!({"origin": "operator", "parts": [{"type": "text", "text": "hi"}]}))
                .unwrap(),
            CognitionInAssembly::Complete(_)
        ));
        // The old string-content shape and a payload with neither shape are
        // refused rather than read as a marker.
        assert!(assembler.push(&json!({"content": "hi"})).is_err());
        assert!(assembler.push(&json!({})).is_err());
        assert!(
            assembler
                .push(&json!({"origin": "operator", "parts": []}))
                .is_err()
        );
    }

    #[test]
    fn chunk_wire_shape_matches_the_shared_fixture() {
        // The fixture is the wire contract for a chunked input; every reader
        // of chunked inputs is tested against the same file.
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/cognition_in_chunk.json"))
                .unwrap();
        let chunk: CognitionInChunk = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(chunk.origin, "launch");
        assert!(chunk.pinned);
        assert_eq!(serde_json::to_value(&chunk).unwrap(), fixture);
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
            RuntimeEventType::ChildThreadSettled,
            RuntimeEventType::WorkerContinuationAdmitted,
            RuntimeEventType::ContinuationRequested,
            RuntimeEventType::ContinuationAccepted,
            RuntimeEventType::CommandSubmitted,
            RuntimeEventType::CommandClaimed,
            RuntimeEventType::CommandCompleted,
            RuntimeEventType::StreamOpened,
            RuntimeEventType::TokenDelta,
            RuntimeEventType::StreamSnapshot,
            RuntimeEventType::StreamClosed,
            RuntimeEventType::Observation,
            RuntimeEventType::ArtifactPublished,
            RuntimeEventType::AsLaunchedResolution,
            RuntimeEventType::AsLaunchedRefBindings,
            RuntimeEventType::RuntimeLaunchFacts,
            RuntimeEventType::LaunchAugmentationCacheHit,
            RuntimeEventType::ProcessInputRootSelected,
            RuntimeEventType::LaunchToolReads,
            RuntimeEventType::LaunchBindingRebound,
            RuntimeEventType::ThreadFacetSet,
            RuntimeEventType::CognitionIn,
            RuntimeEventType::CognitionOut,
            RuntimeEventType::CognitionReasoning,
            RuntimeEventType::CognitionFrame,
            RuntimeEventType::CognitionCompaction,
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
            RuntimeEventType::AttentionAcknowledged,
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

    /// Registered wire types the runtime enum deliberately does not carry:
    /// they are authored by the daemon and never appended through the runtime.
    const DAEMON_ONLY_WIRE_TYPES: &[&str] = &[
        wire::SUBPROCESS_OUTPUT_OBSERVED,
        wire::RESOURCE_BUDGET_TRANSITION_V1,
        wire::RESOURCE_PRECONTACT_BUDGET_TRANSITION_V1,
    ];

    /// Every variant, for whole-vocabulary storage-class assertions: the single
    /// registered list, minus the daemon-only types named above.
    fn every_event_type() -> Vec<RuntimeEventType> {
        wire::ALL
            .iter()
            .filter(|name| !DAEMON_ONLY_WIRE_TYPES.contains(name))
            .map(|name| {
                RuntimeEventType::parse(name).unwrap_or_else(|_| panic!("no variant for {name}"))
            })
            .collect()
    }

    #[test]
    fn daemon_only_wire_types_are_exactly_the_unparsed_registered_types() {
        let unparsed: Vec<&str> = wire::ALL
            .iter()
            .copied()
            .filter(|name| RuntimeEventType::parse(name).is_err())
            .collect();
        assert_eq!(unparsed, DAEMON_ONLY_WIRE_TYPES);
    }

    fn event_types_of_class(class: StorageClass) -> Vec<&'static str> {
        every_event_type()
            .into_iter()
            .filter(|event_type| event_type.storage_class() == class)
            .map(RuntimeEventType::as_str)
            .collect()
    }

    #[test]
    fn ephemeral_set_is_exactly_the_progressive_events() {
        // Tightening this list belongs in a deliberate change — the
        // assertion guards against silent storage-class drift.
        assert_eq!(
            event_types_of_class(StorageClass::Ephemeral),
            vec!["token_delta", "stream_snapshot", "cognition_reasoning"]
        );
    }

    #[test]
    fn journal_set_is_exactly_observation_and_foreach() {
        assert_eq!(
            event_types_of_class(StorageClass::JournalOnly),
            vec!["observation", "graph_foreach_iteration"]
        );
    }

    #[test]
    fn only_progressive_events_leave_the_indexed_floor() {
        let progressive: Vec<&'static str> = every_event_type()
            .into_iter()
            .filter(|event_type| event_type.is_progressive())
            .map(RuntimeEventType::as_str)
            .collect();
        assert_eq!(
            progressive,
            vec![
                "token_delta",
                "stream_snapshot",
                "observation",
                "cognition_reasoning",
                "graph_foreach_iteration"
            ]
        );
        for event_type in every_event_type() {
            if !event_type.is_progressive() {
                assert_eq!(
                    event_type.storage_class(),
                    StorageClass::Indexed,
                    "{} is below the indexed floor",
                    event_type.as_str()
                );
            }
        }
    }

    #[test]
    fn cognition_out_fragments_are_progressive() {
        for payload in [
            json!({"turn": 1, "delta": "hi"}),
            json!({"turn": 1, "tool_use_partial": {"id": "c", "delta": "{}"}}),
            json!({"turn": 1, "tool_use": {"id": "c", "name": "f", "arguments": {}}}),
        ] {
            assert!(cognition_out_is_progressive(&payload), "{payload}");
        }
        for payload in [
            json!({"turn": 1}),
            json!({"content": "done", "tool_calls": []}),
            json!({"content": "partial", "interrupted": true}),
        ] {
            assert!(!cognition_out_is_progressive(&payload), "{payload}");
        }
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
