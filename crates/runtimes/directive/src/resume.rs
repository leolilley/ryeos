use anyhow::{Context, Result};
use serde_json::Value;

use ryeos_runtime::ReplayedEventRecord;
use ryeos_runtime::callback_client::{CallbackClient, RuntimeReplayBudget};
use ryeos_state::ThreadUsage;

use crate::directive::{DIRECTIVE_RETURN_TOOL, ProviderMessage};
use crate::result_guard::ResultGuard;
use crate::runner::ToolOccurrence;

#[derive(Debug)]
pub struct ResumeState {
    pub messages: Vec<ProviderMessage>,
    /// Model-proposed calls that do not yet have a matching durable result.
    /// These resume before another provider request and reuse their exact
    /// logical operation identities.
    pub pending_tools: Vec<ToolOccurrence>,
    /// Runtime-owned next state reconstructed from the retained event braid.
    /// This is process-local recovery data, not a new persisted authority.
    pub disposition: ResumeDisposition,
    /// Last durable provider-turn coordinate, including interrupted turns.
    /// Unlike `turns_completed`, this never refunds an interrupted coordinate.
    pub last_turn_coordinate: u32,
    /// Turn-start marker with no matching final cognition in this exact native
    /// thread. It is process-recovery evidence only: provider-attempt contact
    /// authority still comes exclusively from the daemon ledger.
    pub active_provider_turn: Option<u32>,
    pub turns_completed: u32,
    /// Exact non-lifecycle starts, in braid order. The runner combines this
    /// evidence with the signed tool-call ceiling to deterministically recover
    /// which starts were admitted; a start alone is not admission authority.
    pub started_tool_occurrences: Vec<StartedToolOccurrence>,
    pub thread_usage: Option<ThreadUsage>,
    /// Ordering-sensitive result-deduplication state reconstructed only for a
    /// native respawn of this exact thread. A continuation is a new runtime
    /// process and starts a fresh guard, matching uninterrupted execution.
    pub result_guard: ResultGuard,
    /// Exact retry decisions already represented in the current thread braid.
    /// Used only to make testimony append lost-reply recovery idempotent.
    pub provider_retry_testimony_digests: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// Re-spawn of the same unfinished thread after process/daemon failure.
    NativeSameThread,
    /// A new continuation thread folding a settled predecessor chain.
    Continuation,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResumeDisposition {
    ContinueProvider,
    ParseRetainedResponse,
    AfterSettledToolBatch { turn: u32 },
    CheckContinuation,
    FinalizeDirectiveReturn { outputs: Value },
}

impl Default for ResumeDisposition {
    fn default() -> Self {
        Self::ContinueProvider
    }
}

#[derive(Debug, Clone)]
pub struct StartedToolOccurrence {
    pub operation_id: String,
    pub call: crate::directive::ToolCall,
}

/// Stable logical identity for one ordered tool-call occurrence in a
/// directive assistant turn. Provider call IDs are transcript data and are
/// not unique enough to own retry/recovery.
pub(crate) fn directive_tool_operation_id(
    source_thread_id: &str,
    turn: u32,
    call_index: usize,
) -> String {
    let identity = serde_json::json!({
        "domain": "ryeos.directive.tool_occurrence.v1",
        "source_thread_id": source_thread_id,
        "turn": turn,
        "call_index": call_index,
    });
    let canonical = lillux::canonical_json(&identity)
        .expect("directive tool occurrence identity is canonical JSON");
    lillux::sha256_hex(canonical.as_bytes())
}

/// Backstop on the continuation-path walk — far above any real conversation
/// length. Exceeding it is treated as a runaway/cyclic chain and errors loudly
/// rather than silently truncating (which would drop conversation history).
const MAX_CONTINUATION_PATH: usize = 10_000;

pub async fn load_resume_state(
    callback: &CallbackClient,
    previous_thread_id: &str,
    carry_turns: u32,
    mode: ResumeMode,
) -> Result<ResumeState> {
    // Fold the linear CONTINUATION PATH (turn 1 → … → predecessor), not the
    // whole chain namespace: a conversation is a chain of turns, so turn N must
    // see turns 1..N-1 — but a chain root can also contain non-continuation
    // child threads (compose-context, sub-dispatch) that share `chain_root_id`
    // and emit transcript events. Walking `upstream_thread_id` from the
    // predecessor yields only conversation turns (a turn's upstream is always
    // its continuation predecessor, never a child), and replaying each turn
    // thread-scoped structurally excludes those children.
    let path = continuation_path(callback, previous_thread_id).await?;

    let mut events: Vec<ReplayedEventRecord> = Vec::new();
    let mut source_thread_ids = Vec::new();
    let mut replay_budget = RuntimeReplayBudget::default();
    for thread_id in &path {
        let page = callback
            .replay_thread_with_budget(thread_id, &mut replay_budget)
            .await?;
        source_thread_ids.extend(std::iter::repeat_n(thread_id.clone(), page.events.len()));
        events.extend(page.events);
    }

    let messages = reconstruct_messages(&events)?;
    let recovered_tools = recover_tool_state(&events, &source_thread_ids)?;
    if !recovered_tools.pending.is_empty() {
        let pending_source = recovered_tools
            .pending_source_thread_id
            .as_deref()
            .ok_or_else(|| {
                anyhow::anyhow!("resume: unresolved tool batch lost its source thread")
            })?;
        match mode {
            ResumeMode::NativeSameThread if pending_source != previous_thread_id => {
                anyhow::bail!(
                    "resume: unresolved tool batch belongs to predecessor thread {pending_source}, not native-resume thread {previous_thread_id}"
                );
            }
            ResumeMode::Continuation => {
                anyhow::bail!("resume: continuation predecessor contains an unresolved tool batch");
            }
            ResumeMode::NativeSameThread => {}
        }
    }
    let result_guard = if mode == ResumeMode::NativeSameThread {
        recover_result_guard(&events, &source_thread_ids, previous_thread_id)?
    } else {
        ResultGuard::new()
    };
    let turn_progress = recover_turn_progress(&events)?;
    let active_provider_turn =
        recover_active_provider_turn(&events, &source_thread_ids, previous_thread_id, mode)?;
    let provider_retry_testimony_digests =
        recover_provider_retry_testimonies(&events, &source_thread_ids, previous_thread_id, mode)?;
    let thread_usage = recover_thread_usage(&events, turn_progress)?;
    let disposition = if mode == ResumeMode::NativeSameThread {
        recover_resume_disposition(&events, &recovered_tools)?
    } else {
        ResumeDisposition::ContinueProvider
    };
    // An unresolved tool result must retain its owning assistant message on
    // the provider wire even when a continuation requested zero carried
    // turns. Otherwise the recovered result would be an orphan tool message.
    let must_retain_active_turn = mode == ResumeMode::NativeSameThread
        && (!recovered_tools.pending.is_empty()
            || !matches!(disposition, ResumeDisposition::ContinueProvider)
            || events
                .iter()
                .any(|event| final_cognition_turn(event).is_ok_and(|v| v.is_some())));
    let retained_turns = if must_retain_active_turn {
        carry_turns.max(1)
    } else {
        carry_turns
    };
    let messages = trim_to_recent_turns(messages, retained_turns);
    let pin_last_assistant = mode == ResumeMode::NativeSameThread
        && (must_retain_active_turn || !recovered_tools.pending.is_empty());
    let trimmed = trim_to_token_budget(messages, 16_000, pin_last_assistant)?;

    Ok(ResumeState {
        messages: trimmed,
        pending_tools: recovered_tools.pending,
        disposition,
        last_turn_coordinate: turn_progress.last_coordinate,
        active_provider_turn,
        turns_completed: turn_progress.completed,
        started_tool_occurrences: recovered_tools.started,
        thread_usage,
        result_guard,
        provider_retry_testimony_digests,
    })
}

fn replayed_result_text(event: &ReplayedEventRecord) -> Result<&str> {
    let text = event
        .payload
        .get("result_text")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("resume: tool_call_result has no exact string result_text")
        })?;
    if text.len() > ryeos_runtime::callback_client::TOOL_RESULT_INLINE_MAX_BYTES {
        anyhow::bail!("resume: tool_call_result result_text exceeds the runtime event bound");
    }
    if event.payload.get("result").is_some() {
        anyhow::bail!("resume: tool_call_result carries obsolete parsed result authority");
    }
    Ok(text)
}

fn optional_replay_string<'a>(payload: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match payload.get(field) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => anyhow::bail!("resume: replay field {field:?} is not a string"),
    }
}

fn optional_replay_bool(payload: &Value, field: &str) -> Result<Option<bool>> {
    match payload.get(field) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => anyhow::bail!("resume: replay field {field:?} is not a boolean"),
    }
}

/// Rebuild only the current process generation's duplicate guard. Upstream
/// continuation threads ran in distinct processes and intentionally begin a
/// fresh guard; including them would cause a new continuation to omit a result
/// that its provider context may no longer retain.
fn recover_result_guard(
    events: &[ReplayedEventRecord],
    source_thread_ids: &[String],
    current_thread_id: &str,
) -> Result<ResultGuard> {
    if events.len() != source_thread_ids.len() {
        anyhow::bail!("resume: replay source/event cardinality mismatch");
    }
    let mut guard = ResultGuard::new();
    for (event, source_thread_id) in events.iter().zip(source_thread_ids) {
        if source_thread_id != current_thread_id || event.event_type != "tool_call_result" {
            continue;
        }
        let text = replayed_result_text(event)?;
        let tool = event
            .payload
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("resume: tool_call_result has no tool identity"))?;
        let duplicate_of = event
            .payload
            .get("duplicate_of")
            .map(|value| {
                value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("resume: tool_call_result duplicate_of is not a string")
                })
            })
            .transpose()?;
        let deduplicated = optional_replay_bool(&event.payload, "deduplicated")?;
        if duplicate_of.is_some() != (deduplicated == Some(true)) {
            anyhow::bail!("resume: tool_call_result duplicate metadata is contradictory");
        }
        let truncated_reason = optional_replay_string(&event.payload, "truncated_reason")?;
        if tool == DIRECTIVE_RETURN_TOOL || truncated_reason == Some("error_envelope") {
            if duplicate_of.is_some() {
                anyhow::bail!("resume: unguarded tool result claims duplicate metadata");
            }
            continue;
        }
        guard
            .restore_result(text, duplicate_of)
            .map_err(|error| anyhow::anyhow!("resume: invalid result-guard evidence: {error}"))?;
    }
    Ok(guard)
}

#[derive(Debug)]
struct RecoveredToolState {
    pending: Vec<ToolOccurrence>,
    pending_source_thread_id: Option<String>,
    started: Vec<StartedToolOccurrence>,
    current_batch: Option<RecoveredToolBatch>,
    /// A successful lifecycle return terminalizes the directive. Keep that
    /// settlement independently of `current_batch`: later braid evidence must
    /// never reset terminal authority and authorize another provider turn.
    terminal_lifecycle_settlement: Option<RecoveredLifecycleSettlement>,
}

#[derive(Debug)]
struct ReplayedToolOccurrence {
    occurrence: ToolOccurrence,
    /// Exact replay thread that authored the assistant proposal. A later
    /// continuation thread cannot settle this occurrence merely by repeating
    /// its opaque operation ID.
    source_thread_id: String,
    settled: bool,
    abandoned: bool,
}

#[derive(Debug)]
struct RecoveredToolBatch {
    turn: u32,
    contains_lifecycle: bool,
    lifecycle_settlement: Option<RecoveredLifecycleSettlement>,
}

#[derive(Debug, Clone)]
enum RecoveredLifecycleSettlement {
    Failed { event_index: usize },
    Succeeded { event_index: usize, outputs: Value },
}

/// Recover the unclosed tool-call suffix from the directive's event braid.
/// The proposal (`cognition_out`) is the source of occurrence identity; start
/// and result events may only confirm that exact proposal. No separate runtime
/// checkpoint or process-local registry participates in recovery.
fn recover_tool_state(
    events: &[ReplayedEventRecord],
    source_thread_ids: &[String],
) -> Result<RecoveredToolState> {
    if events.len() != source_thread_ids.len() {
        anyhow::bail!("resume: replay source/event cardinality mismatch");
    }

    let mut open: Vec<ReplayedToolOccurrence> = Vec::new();
    let mut started = Vec::new();
    let mut current_batch: Option<RecoveredToolBatch> = None;
    let mut terminal_lifecycle_settlement = None;
    for (event_index, (event, source_thread_id)) in events.iter().zip(source_thread_ids).enumerate()
    {
        if terminal_lifecycle_settlement.is_some()
            && matches!(
                event.event_type.as_str(),
                "cognition_in" | "cognition_out" | "tool_call_start" | "tool_call_result"
            )
        {
            anyhow::bail!(
                "resume: transcript or tool evidence appeared after successful directive_return"
            );
        }
        validate_replayed_cognition_out(event)?;
        if event.event_type == "cognition_in"
            && open.iter().any(|entry| !entry.settled && !entry.abandoned)
        {
            anyhow::bail!(
                "resume: cognition input appeared before every retained tool occurrence settled"
            );
        }
        match event.event_type.as_str() {
            "cognition_out"
                if event.payload.get("delta").is_some()
                    || event.payload.get("tool_use").is_some()
                    || event.payload.get("tool_use_partial").is_some() => {}
            "cognition_out" => {
                if open.iter().any(|entry| !entry.settled && !entry.abandoned) {
                    anyhow::bail!(
                        "resume: a new cognition_out appeared before every prior tool occurrence settled"
                    );
                }
                current_batch = None;
                let Some(tool_calls) = replayed_tool_calls(&event.payload)? else {
                    open.clear();
                    continue;
                };
                if tool_calls.is_empty() {
                    open.clear();
                    continue;
                }
                let turn = event
                    .payload
                    .get("turn")
                    .and_then(Value::as_u64)
                    .and_then(|turn| u32::try_from(turn).ok())
                    .filter(|turn| *turn > 0)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "resume: transcript-bearing cognition_out with tool calls has no positive u32 turn"
                        )
                    })?;
                let contains_lifecycle = tool_calls
                    .iter()
                    .any(|call| call.name == DIRECTIVE_RETURN_TOOL);
                open = tool_calls
                    .into_iter()
                    .enumerate()
                    .map(|(call_index, call)| ReplayedToolOccurrence {
                        occurrence: ToolOccurrence {
                            call,
                            operation_id: directive_tool_operation_id(
                                source_thread_id,
                                turn,
                                call_index,
                            ),
                            start_recorded: false,
                            counted: false,
                            spawn_counted: false,
                        },
                        source_thread_id: source_thread_id.clone(),
                        settled: false,
                        abandoned: false,
                    })
                    .collect();
                current_batch = Some(RecoveredToolBatch {
                    turn,
                    contains_lifecycle,
                    lifecycle_settlement: None,
                });
            }
            "tool_call_start" => {
                let operation_id = replay_operation_id(event)?;
                let occurrence_index = find_open_occurrence_index(&open, operation_id, "start")?;
                if open[occurrence_index].source_thread_id != *source_thread_id {
                    anyhow::bail!(
                        "resume: tool_call_start source thread differs from its cognition proposal"
                    );
                }
                if open[occurrence_index].abandoned {
                    anyhow::bail!(
                        "resume: tool_call_start appeared after directive_return abandoned operation {operation_id}"
                    );
                }
                if open[occurrence_index].occurrence.start_recorded {
                    anyhow::bail!("resume: duplicate tool_call_start for operation {operation_id}");
                }
                let contains_lifecycle = current_batch
                    .as_ref()
                    .is_some_and(|batch| batch.contains_lifecycle);
                if contains_lifecycle {
                    if open[..occurrence_index].iter().any(|entry| !entry.settled) {
                        anyhow::bail!(
                            "resume: lifecycle-bearing tool batch started out of serial order"
                        );
                    }
                } else {
                    // The live concurrent path records every admitted start in
                    // proposal order before it dispatches any work. Starts are
                    // therefore a strict proposal prefix and can never appear
                    // after Phase C has begun publishing results.
                    if open[..occurrence_index]
                        .iter()
                        .any(|entry| !entry.occurrence.start_recorded)
                    {
                        anyhow::bail!(
                            "resume: plain tool batch started outside the proposal-order prefix"
                        );
                    }
                    if open.iter().any(|entry| entry.settled) {
                        anyhow::bail!(
                            "resume: plain tool batch recorded a start after result settlement began"
                        );
                    }
                }
                validate_replayed_tool_event(
                    event,
                    &open[occurrence_index].occurrence.call,
                    false,
                )?;
                open[occurrence_index].occurrence.start_recorded = true;
                if open[occurrence_index].occurrence.call.name != DIRECTIVE_RETURN_TOOL {
                    started.push(StartedToolOccurrence {
                        operation_id: operation_id.to_string(),
                        call: open[occurrence_index].occurrence.call.clone(),
                    });
                }
            }
            "tool_call_result" => {
                let operation_id = replay_operation_id(event)?;
                let occurrence_index = find_open_occurrence_index(&open, operation_id, "result")?;
                if open[occurrence_index].source_thread_id != *source_thread_id {
                    anyhow::bail!(
                        "resume: tool_call_result source thread differs from its cognition proposal"
                    );
                }
                if open[occurrence_index].abandoned {
                    anyhow::bail!(
                        "resume: tool_call_result appeared after directive_return abandoned operation {operation_id}"
                    );
                }
                if !open[occurrence_index].occurrence.start_recorded {
                    anyhow::bail!(
                        "resume: tool_call_result precedes tool_call_start for operation {operation_id}"
                    );
                }
                if open[occurrence_index].settled {
                    anyhow::bail!(
                        "resume: duplicate tool_call_result for operation {operation_id}"
                    );
                }
                // Both the serial lifecycle path and concurrent Phase C fold
                // results in proposal order. A later result cannot become
                // admission authority while an earlier occurrence is open.
                if open[..occurrence_index].iter().any(|entry| !entry.settled) {
                    anyhow::bail!(
                        "resume: tool_call_result appeared outside the proposal-order result prefix"
                    );
                }
                validate_replayed_tool_event(event, &open[occurrence_index].occurrence.call, true)?;
                if open[occurrence_index].occurrence.call.name == DIRECTIVE_RETURN_TOOL {
                    if open[..occurrence_index].iter().any(|entry| !entry.settled) {
                        anyhow::bail!(
                            "resume: directive_return settled before an earlier serial occurrence"
                        );
                    }
                    let batch = current_batch.as_mut().ok_or_else(|| {
                        anyhow::anyhow!("resume: directive_return result has no owning tool batch")
                    })?;
                    if batch.lifecycle_settlement.is_some() {
                        anyhow::bail!("resume: duplicate directive_return settlement");
                    }
                    let settlement = recover_lifecycle_settlement(
                        event,
                        &open[occurrence_index].occurrence.call,
                        event_index,
                    )?;
                    if matches!(settlement, RecoveredLifecycleSettlement::Succeeded { .. }) {
                        terminal_lifecycle_settlement = Some(settlement.clone());
                    }
                    batch.lifecycle_settlement = Some(settlement);
                    for entry in &mut open[occurrence_index + 1..] {
                        entry.abandoned = true;
                    }
                }
                open[occurrence_index].settled = true;
            }
            _ => {}
        }
    }

    let pending_source_thread_id = open
        .iter()
        .find(|entry| !entry.settled && !entry.abandoned)
        .map(|entry| entry.source_thread_id.clone());
    Ok(RecoveredToolState {
        pending: open
            .iter()
            .filter(|entry| !entry.settled && !entry.abandoned)
            .map(|entry| entry.occurrence.clone())
            .collect(),
        pending_source_thread_id,
        started,
        current_batch,
        terminal_lifecycle_settlement,
    })
}

fn recover_lifecycle_settlement(
    event: &ReplayedEventRecord,
    call: &crate::directive::ToolCall,
    event_index: usize,
) -> Result<RecoveredLifecycleSettlement> {
    match optional_replay_string(&event.payload, "truncated_reason")? {
        Some("error_envelope") => Ok(RecoveredLifecycleSettlement::Failed { event_index }),
        Some(reason) => anyhow::bail!(
            "resume: directive_return result has impossible truncation reason {reason:?}"
        ),
        None => {
            if event.payload.get("truncated").and_then(Value::as_bool) != Some(false) {
                anyhow::bail!("resume: successful directive_return result is truncated");
            }
            let result_text = replayed_result_text(event)?;
            let outputs: Value = serde_json::from_str(result_text).context(
                "resume: successful directive_return result_text is not exact JSON outputs",
            )?;
            let proposed = crate::adapter::parse_tool_arguments(&call.arguments.to_string())
                .map_err(|error| {
                    anyhow::anyhow!(
                        "resume: successful directive_return proposal no longer parses: {error}"
                    )
                })?;
            if outputs != proposed {
                anyhow::bail!(
                    "resume: successful directive_return result contradicts its exact proposal"
                );
            }
            Ok(RecoveredLifecycleSettlement::Succeeded {
                event_index,
                outputs,
            })
        }
    }
}

fn replay_operation_id(event: &ReplayedEventRecord) -> Result<&str> {
    let operation_id = event
        .payload
        .get("operation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("resume: {} has no operation_id", event.event_type))?;
    if !ryeos_runtime::callback::valid_action_operation_id(operation_id) {
        anyhow::bail!(
            "resume: {} operation_id is not a canonical lowercase SHA-256 digest",
            event.event_type
        );
    }
    Ok(operation_id)
}

fn find_open_occurrence_index(
    open: &[ReplayedToolOccurrence],
    operation_id: &str,
    phase: &str,
) -> Result<usize> {
    open.iter()
        .position(|entry| entry.occurrence.operation_id == operation_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "resume: tool {phase} references operation {operation_id} without an open cognition proposal"
            )
        })
}

fn validate_replayed_tool_event(
    event: &ReplayedEventRecord,
    call: &crate::directive::ToolCall,
    result_phase: bool,
) -> Result<()> {
    let observed_tool = event.payload.get("tool").and_then(Value::as_str);
    if observed_tool != Some(call.name.as_str()) {
        anyhow::bail!(
            "resume: {} tool identity does not match its cognition proposal",
            event.event_type
        );
    }
    let expected_call_id = if result_phase {
        Some(call.id.as_deref().unwrap_or(""))
    } else {
        call.id.as_deref()
    };
    if event.payload.get("call_id").and_then(Value::as_str) != expected_call_id {
        anyhow::bail!(
            "resume: {} call_id does not match its cognition proposal",
            event.event_type
        );
    }
    Ok(())
}

/// Resolve the linear continuation path ending at `predecessor_id`, ordered
/// root-first. Walks `upstream_thread_id` (the continuation predecessor link)
/// until the root (no upstream), guarding against cycles.
async fn continuation_path(callback: &CallbackClient, predecessor_id: &str) -> Result<Vec<String>> {
    let mut path = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current = Some(predecessor_id.to_string());

    while let Some(thread_id) = current {
        if !seen.insert(thread_id.clone()) {
            anyhow::bail!("resume: continuation path cycle at {thread_id}");
        }
        let detail = callback.get_thread_by_id(&thread_id).await?;
        let upstream = detail
            .get("thread")
            .and_then(|thread| thread.get("upstream_thread_id"))
            .and_then(|value| value.as_str())
            .map(str::to_string);
        path.push(thread_id);
        if path.len() >= MAX_CONTINUATION_PATH && upstream.is_some() {
            anyhow::bail!(
                "resume: continuation path exceeds {MAX_CONTINUATION_PATH} turns \
                 (runaway or cyclic chain); refusing to fold a truncated history"
            );
        }
        current = upstream;
    }

    path.reverse(); // root first → turns fold in order
    Ok(path)
}

fn is_stream_fragment(event: &ReplayedEventRecord) -> bool {
    event.event_type == "cognition_out"
        && (event.payload.get("delta").is_some()
            || event.payload.get("tool_use").is_some()
            || event.payload.get("tool_use_partial").is_some())
}

fn validate_replayed_cognition_out(event: &ReplayedEventRecord) -> Result<()> {
    if event.event_type != "cognition_out" {
        return Ok(());
    }
    if is_stream_fragment(event) {
        for final_key in [
            "content",
            "tool_calls",
            "reasoning_content",
            "interrupted",
            "provider_accounting",
            "input_tokens",
            "output_tokens",
        ] {
            if event.payload.get(final_key).is_some() {
                anyhow::bail!(
                    "resume: streaming cognition_out also carries final field {final_key:?}"
                );
            }
        }
        return Ok(());
    }
    if let Some(interrupted) = event.payload.get("interrupted") {
        if interrupted.as_bool() != Some(true) {
            anyhow::bail!("resume: cognition_out interrupted marker is not true");
        }
        if event.payload.get("tool_calls").is_some() {
            anyhow::bail!(
                "resume: interrupted cognition_out cannot authorize completed tool calls"
            );
        }
    }
    Ok(())
}

fn final_cognition_turn(event: &ReplayedEventRecord) -> Result<Option<u32>> {
    validate_replayed_cognition_out(event)?;
    if event.event_type != "cognition_out" || is_stream_fragment(event) {
        return Ok(None);
    }
    let turn = event
        .payload
        .get("turn")
        .and_then(Value::as_u64)
        .and_then(|turn| u32::try_from(turn).ok())
        .filter(|turn| *turn > 0)
        .ok_or_else(|| {
            anyhow::anyhow!("resume: transcript-bearing cognition_out has no positive u32 turn")
        })?;
    Ok(Some(turn))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecoveredTurnProgress {
    last_coordinate: u32,
    completed: u32,
}

/// Recover the independent provider-turn coordinate and completed-turn budget.
/// Every final cognition consumes a monotonic coordinate; an interrupted
/// cognition refunds only the completed-turn budget, never its identity.
fn recover_turn_progress(events: &[ReplayedEventRecord]) -> Result<RecoveredTurnProgress> {
    let mut progress = RecoveredTurnProgress {
        last_coordinate: 0,
        completed: 0,
    };
    for event in events {
        let Some(turn) = final_cognition_turn(event)? else {
            continue;
        };
        let expected = progress
            .last_coordinate
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("resume: provider-turn coordinate overflow"))?;
        if turn != expected {
            anyhow::bail!(
                "resume: cognition turn coordinate {turn} does not follow {}",
                progress.last_coordinate
            );
        }
        progress.last_coordinate = turn;
        if event
            .payload
            .get("interrupted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        progress.completed = progress
            .completed
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("resume: completed-turn counter overflow"))?;
    }
    Ok(progress)
}

fn turn_start_coordinate(event: &ReplayedEventRecord) -> Result<Option<u32>> {
    if event.event_type != "cognition_in" || event.payload.get("turn").is_none() {
        return Ok(None);
    }
    let turn = event
        .payload
        .get("turn")
        .and_then(Value::as_u64)
        .and_then(|turn| u32::try_from(turn).ok())
        .filter(|turn| *turn > 0)
        .ok_or_else(|| anyhow::anyhow!("resume: cognition_in turn marker is not a positive u32"))?;
    Ok(Some(turn))
}

fn recover_active_provider_turn(
    events: &[ReplayedEventRecord],
    source_thread_ids: &[String],
    current_thread_id: &str,
    mode: ResumeMode,
) -> Result<Option<u32>> {
    if events.len() != source_thread_ids.len() {
        anyhow::bail!("resume: replay source/event cardinality mismatch");
    }
    let mut last_final = 0u32;
    let mut current_active = None;
    for (event, source_thread_id) in events.iter().zip(source_thread_ids) {
        if source_thread_id == current_thread_id
            && let Some(turn) = turn_start_coordinate(event)?
        {
            let expected = last_final
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("resume: provider-turn coordinate overflow"))?;
            match current_active {
                Some(active) if active == turn => {
                    // An exact duplicate marker can exist only after a lost
                    // append reply from an older process. It does not advance
                    // the turn coordinate a second time.
                }
                None if turn == expected => current_active = Some(turn),
                _ => anyhow::bail!(
                    "resume: active provider turn marker {turn} contradicts last final turn {last_final}"
                ),
            }
        }
        if let Some(turn) = final_cognition_turn(event)? {
            if source_thread_id == current_thread_id {
                if let Some(active) = current_active
                    && active != turn
                {
                    anyhow::bail!(
                        "resume: final cognition turn {turn} contradicts active turn {active}"
                    );
                }
                current_active = None;
            }
            last_final = turn;
        }
    }
    if mode == ResumeMode::Continuation && current_active.is_some() {
        anyhow::bail!("resume: continuation predecessor contains an unfinished provider turn");
    }
    Ok((mode == ResumeMode::NativeSameThread)
        .then_some(current_active)
        .flatten())
}

fn recover_provider_retry_testimonies(
    events: &[ReplayedEventRecord],
    source_thread_ids: &[String],
    current_thread_id: &str,
    mode: ResumeMode,
) -> Result<std::collections::BTreeSet<String>> {
    if events.len() != source_thread_ids.len() {
        anyhow::bail!("resume: replay source/event cardinality mismatch");
    }
    if mode != ResumeMode::NativeSameThread {
        return Ok(std::collections::BTreeSet::new());
    }
    let mut by_coordinate = std::collections::BTreeMap::<(u32, u32), String>::new();
    let mut digests = std::collections::BTreeSet::new();
    let mut active_turn = None;
    let mut next_failed_attempt = 1u32;
    for (event, source_thread_id) in events.iter().zip(source_thread_ids) {
        if source_thread_id != current_thread_id {
            continue;
        }
        if let Some(turn) = turn_start_coordinate(event)? {
            match active_turn {
                Some(active) if active == turn => {}
                None => {
                    active_turn = Some(turn);
                    next_failed_attempt = 1;
                }
                Some(active) => anyhow::bail!(
                    "resume: provider turn {turn} started while turn {active} remained active"
                ),
            }
        }
        if let Some(turn) = final_cognition_turn(event)? {
            if let Some(active) = active_turn {
                if active != turn {
                    anyhow::bail!(
                        "resume: final cognition turn {turn} contradicts active retry turn {active}"
                    );
                }
                active_turn = None;
                next_failed_attempt = 1;
            }
            continue;
        }
        if event.event_type != "provider_retry" {
            continue;
        }
        let advance: ryeos_accounting::ProviderRetryAdvance =
            serde_json::from_value(event.payload.get("retry_advance").cloned().ok_or_else(
                || anyhow::anyhow!("resume: provider_retry has no exact advancement"),
            )?)
            .context("resume: invalid provider_retry advancement")?;
        advance.validate().map_err(|error| {
            anyhow::anyhow!("resume: invalid provider_retry advancement: {error}")
        })?;
        let digest = event
            .payload
            .get("decision_digest")
            .and_then(Value::as_str)
            .filter(|digest| *digest == advance.decision_digest.as_str())
            .ok_or_else(|| anyhow::anyhow!("resume: provider_retry decision digest mismatch"))?
            .to_string();
        let coordinate = (
            advance.decision.turn,
            advance.decision.failed_attempt_number,
        );
        if active_turn != Some(coordinate.0) {
            anyhow::bail!(
                "resume: provider retry coordinate {}/{} is outside its active provider turn",
                coordinate.0,
                coordinate.1
            );
        }
        for (field, expected) in [
            ("turn", u64::from(coordinate.0)),
            ("attempt", u64::from(coordinate.1)),
            ("backoff_ms", advance.decision.backoff_ms),
        ] {
            if event.payload.get(field).and_then(Value::as_u64) != Some(expected) {
                anyhow::bail!("resume: provider_retry {field} contradicts its exact advancement");
            }
        }
        match by_coordinate.get(&coordinate) {
            Some(prior) if prior != &digest => {
                anyhow::bail!(
                    "resume: provider retry coordinate {}/{} carries conflicting decisions",
                    coordinate.0,
                    coordinate.1
                );
            }
            Some(_) => {}
            None => {
                if coordinate.1 != next_failed_attempt {
                    anyhow::bail!(
                        "resume: provider retry attempt {} does not follow expected attempt {}",
                        coordinate.1,
                        next_failed_attempt
                    );
                }
                next_failed_attempt = next_failed_attempt.checked_add(1).ok_or_else(|| {
                    anyhow::anyhow!("resume: provider retry attempt coordinate overflow")
                })?;
                by_coordinate.insert(coordinate, digest.clone());
            }
        }
        digests.insert(digest);
    }
    Ok(digests)
}

fn recover_resume_disposition(
    events: &[ReplayedEventRecord],
    tools: &RecoveredToolState,
) -> Result<ResumeDisposition> {
    if let Some(RecoveredLifecycleSettlement::Succeeded { outputs, .. }) =
        tools.terminal_lifecycle_settlement.as_ref()
    {
        return Ok(ResumeDisposition::FinalizeDirectiveReturn {
            outputs: outputs.clone(),
        });
    }
    let Some((last_cognition_index, last_cognition)) = events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, event)| final_cognition_turn(event).is_ok_and(|turn| turn.is_some()))
    else {
        return Ok(ResumeDisposition::ContinueProvider);
    };

    let later_cognition_input = events[last_cognition_index + 1..]
        .iter()
        .any(|event| event.event_type == "cognition_in");

    if let Some(batch) = tools.current_batch.as_ref()
        && let Some(settlement) = batch.lifecycle_settlement.as_ref()
    {
        match settlement {
            RecoveredLifecycleSettlement::Succeeded {
                event_index,
                outputs,
            } => {
                if events[*event_index + 1..]
                    .iter()
                    .any(|event| event.event_type == "cognition_in")
                {
                    anyhow::bail!(
                        "resume: cognition input appeared after successful directive_return"
                    );
                }
                return Ok(ResumeDisposition::FinalizeDirectiveReturn {
                    outputs: outputs.clone(),
                });
            }
            RecoveredLifecycleSettlement::Failed { event_index } => {
                let _ = event_index;
                return Ok(if later_cognition_input {
                    ResumeDisposition::ContinueProvider
                } else {
                    ResumeDisposition::CheckContinuation
                });
            }
        }
    }

    if !tools.pending.is_empty() {
        if later_cognition_input {
            anyhow::bail!(
                "resume: cognition input appeared before the retained tool proposal settled"
            );
        }
        return Ok(ResumeDisposition::ContinueProvider);
    }

    if later_cognition_input {
        return Ok(ResumeDisposition::ContinueProvider);
    }
    if last_cognition
        .payload
        .get("interrupted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(ResumeDisposition::ContinueProvider);
    }
    if let Some(batch) = tools.current_batch.as_ref() {
        if batch.contains_lifecycle {
            return Ok(ResumeDisposition::CheckContinuation);
        }
        return Ok(ResumeDisposition::AfterSettledToolBatch { turn: batch.turn });
    }
    Ok(ResumeDisposition::ParseRetainedResponse)
}

fn replayed_provider_record(event: &ReplayedEventRecord) -> Result<Option<&str>> {
    let Some(value) = event
        .payload
        .get("provider_accounting")
        .and_then(|accounting| accounting.get("replayed_from"))
    else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let record_hash = value.as_str().ok_or_else(|| {
        anyhow::anyhow!("resume: provider_accounting.replayed_from is not a string or null")
    })?;
    if !lillux::valid_hash(record_hash) {
        anyhow::bail!(
            "resume: provider_accounting.replayed_from is not a canonical lowercase SHA-256 digest"
        );
    }
    Ok(Some(record_hash))
}

/// Recover the last cumulative token/spend snapshot and prove every completed
/// cognition lacking a same-turn settlement was a daemon-proven provider
/// replay. This permits a zero-cost replay-only first turn without inventing a
/// zero settlement for a live provider response whose usage is unknown.
fn recover_thread_usage(
    events: &[ReplayedEventRecord],
    turn_progress: RecoveredTurnProgress,
) -> Result<Option<ThreadUsage>> {
    let mut latest: Option<ThreadUsage> = None;
    let mut unconsumed_settlement_turn: Option<u64> = None;
    for event in events {
        if event.event_type == "thread_usage" {
            let usage = serde_json::from_value::<ThreadUsage>(event.payload.clone())
                .context("resume: invalid thread_usage event")?;
            usage
                .validate()
                .context("resume: invalid thread_usage event")?;
            if let Some(previous) = latest.as_ref()
                && (usage.completed_turns < previous.completed_turns
                    || usage.input_tokens < previous.input_tokens
                    || usage.output_tokens < previous.output_tokens
                    || usage.spend_usd < previous.spend_usd
                    || usage.spawns_used < previous.spawns_used
                    || usage.last_settled_turn_seq < previous.last_settled_turn_seq)
            {
                anyhow::bail!("resume: thread_usage cumulative authority regressed");
            }
            unconsumed_settlement_turn = Some(usage.last_settled_turn_seq);
            latest = Some(usage);
            continue;
        }

        let Some(turn) = final_cognition_turn(event)? else {
            continue;
        };
        if event
            .payload
            .get("interrupted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if unconsumed_settlement_turn == Some(u64::from(turn)) {
                unconsumed_settlement_turn = None;
            }
            continue;
        }
        if replayed_provider_record(event)?.is_none()
            && unconsumed_settlement_turn != Some(u64::from(turn))
        {
            anyhow::bail!(
                "resume: completed live cognition turn {turn} has no exact cumulative usage settlement"
            );
        }
        if unconsumed_settlement_turn == Some(u64::from(turn)) {
            unconsumed_settlement_turn = None;
        }
    }

    if let Some(usage) = latest.as_ref()
        && u64::from(usage.completed_turns) > u64::from(turn_progress.completed) + 1
    {
        anyhow::bail!(
            "resume: cumulative usage is more than one provider response ahead of the durable transcript"
        );
    }
    if let Some(usage) = latest.as_ref()
        && usage.last_settled_turn_seq > u64::from(turn_progress.last_coordinate) + 1
    {
        anyhow::bail!(
            "resume: cumulative usage is more than one provider-turn coordinate ahead of the durable transcript"
        );
    }
    Ok(latest)
}

fn replayed_tool_calls(payload: &Value) -> Result<Option<Vec<crate::directive::ToolCall>>> {
    let items = match payload.get("tool_calls") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => {
            anyhow::bail!("malformed cognition_out replay: 'tool_calls' is not an array or null")
        }
    };
    items
        .iter()
        .map(|item| {
            let id = match item.get("id") {
                None | Some(Value::Null) => None,
                Some(Value::String(id)) => Some(id.clone()),
                Some(_) => {
                    anyhow::bail!("malformed tool_call in replay: 'id' is not a string or null")
                }
            };
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "malformed tool_call in replay: missing or non-string 'name' field"
                    )
                })?
                .to_string();
            let arguments = item.get("arguments").cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "malformed tool_call in replay: missing 'arguments' field on tool '{name}'"
                )
            })?;
            Ok(crate::directive::ToolCall {
                id,
                name,
                arguments,
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// Fold the chain's transcript-bearing events into provider messages. The
/// substrate vocabulary is the cognition transcript — there is no "user":
///
/// - `cognition_in`     → an input/stimulus to cognition (the rendered prompt);
///   the bare `{turn}` turn-boundary markers carry no content and are skipped;
/// - `cognition_out`    → the cognition's output (content + tool_calls + reasoning);
/// - `tool_call_result` → a tool result, keyed back by `call_id`.
///
/// Every other chain event (lifecycle, usage settlement, streaming deltas,
/// tool-dispatch starts, graph milestones) carries no message and is skipped —
/// folding a chain is lossy by design, not an error. `role` is the provider-wire
/// mapping applied here, not a substrate concept.
fn reconstruct_messages(events: &[ReplayedEventRecord]) -> Result<Vec<ProviderMessage>> {
    let mut messages = Vec::new();
    let mut cognition_in = ryeos_runtime::CognitionInAssembler::default();

    for event in events {
        validate_replayed_cognition_out(event)?;
        if cognition_in.has_pending() && event.event_type != "cognition_in" {
            anyhow::bail!(
                "resume: chunked cognition_in was interrupted by event '{}'",
                event.event_type
            );
        }
        match event.event_type.as_str() {
            // An inline or completely reassembled cognition_in is exactly one
            // stimulus. Marker-only payloads and intermediate chunks add no
            // provider turn.
            "cognition_in" => {
                if let ryeos_runtime::CognitionInAssembly::Complete(content) =
                    cognition_in.push(&event.payload)?
                {
                    messages.push(ProviderMessage {
                        role: "user".to_string(),
                        content: Some(serde_json::json!(content)),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                }
            }
            "cognition_out"
                if event.payload.get("delta").is_some()
                    || event.payload.get("tool_use").is_some()
                    || event.payload.get("tool_use_partial").is_some() =>
            {
                // Streaming fragments are UI/event-log data, not replayable
                // provider history. The final transcript-bearing
                // cognition_out emitted at turn completion is folded below.
            }
            "cognition_out" => {
                let tool_calls = replayed_tool_calls(&event.payload)?;

                messages.push(ProviderMessage {
                    role: "assistant".to_string(),
                    content: event.payload.get("content").cloned(),
                    tool_calls,
                    tool_call_id: None,
                    reasoning_content: event
                        .payload
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                });
            }
            "tool_call_result" => {
                let call_id = event
                    .payload
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                // The retained string is exactly what the live runner put in
                // the provider message. Never parse JSON-looking content:
                // doing so changes both OpenAI-style wire content and custom
                // schema template rendering after a crash.
                let content = Some(serde_json::json!(replayed_result_text(event)?));

                messages.push(ProviderMessage {
                    role: "tool".to_string(),
                    content,
                    tool_calls: None,
                    tool_call_id: call_id,
                    reasoning_content: None,
                });
            }
            // Non-conversational chain events carry no message — skip.
            _ => {}
        }
    }
    cognition_in
        .finish()
        .context("resume: incomplete chunked cognition_in")?;

    Ok(messages)
}

fn count_turns(messages: &[ProviderMessage]) -> u32 {
    messages.iter().filter(|m| m.role == "assistant").count() as u32
}

/// Keep only the most recent `carry_turns` assistant turns plus the context
/// after the preceding assistant. This is the directive-level continuation
/// policy (`continuation.carry_turns`, resolved/clamped by runtime config),
/// applied before the token-budget backstop. If the cut lands immediately before
/// tool results, drop those leading tool messages rather than orphaning them
/// without the assistant tool-call that owns them; provider APIs reject that
/// shape.
fn trim_to_recent_turns(
    mut messages: Vec<ProviderMessage>,
    carry_turns: u32,
) -> Vec<ProviderMessage> {
    if carry_turns == 0 || messages.is_empty() {
        return Vec::new();
    }
    let total_turns = count_turns(&messages);
    if total_turns <= carry_turns {
        return messages;
    }

    let turns_to_drop = total_turns - carry_turns;
    let mut seen = 0u32;
    let mut cutoff = 0usize;
    for (idx, msg) in messages.iter().enumerate() {
        if msg.role == "assistant" {
            seen += 1;
            if seen == turns_to_drop {
                cutoff = idx + 1;
                break;
            }
        }
    }
    let mut trimmed = messages.split_off(cutoff);
    while trimmed.first().is_some_and(|msg| msg.role == "tool") {
        trimmed.remove(0);
    }
    trimmed
}

fn trim_to_token_budget(
    mut messages: Vec<ProviderMessage>,
    max_tokens: u64,
    pin_last_assistant: bool,
) -> Result<Vec<ProviderMessage>> {
    if messages.is_empty() {
        return Ok(messages);
    }

    loop {
        let total: u64 = messages.iter().map(estimate_tokens).sum();
        if total <= max_tokens {
            return Ok(messages);
        }

        let assistant_count = messages
            .iter()
            .filter(|message| message.role == "assistant")
            .count();
        if assistant_count == 0 || (pin_last_assistant && assistant_count == 1) {
            anyhow::bail!(
                "resume: required provider transcript suffix exceeds the {max_tokens}-token recovery budget"
            );
        }

        let assistant_index = messages
            .iter()
            .position(|message| message.role == "assistant")
            .expect("assistant_count proved one exists");
        let mut group_end = assistant_index + 1;
        while messages
            .get(group_end)
            .is_some_and(|message| message.role == "tool")
        {
            group_end += 1;
        }
        messages.drain(..group_end);
        while messages
            .first()
            .is_some_and(|message| message.role == "tool")
        {
            messages.remove(0);
        }
    }
}

fn estimate_tokens(msg: &ProviderMessage) -> u64 {
    let mut count = estimate_tokens_from_value(&msg.content);
    for tc in msg.tool_calls.iter().flatten() {
        count += estimate_tokens_from_value(&Some(tc.arguments.clone()));
    }
    count
}

fn estimate_tokens_from_value(v: &Option<Value>) -> u64 {
    match v {
        Some(Value::String(s)) => (s.len() as u64) / 4,
        Some(Value::Number(_)) => 1,
        Some(Value::Bool(_)) => 1,
        Some(Value::Null) | None => 0,
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| estimate_tokens_from_value(&Some(v.clone())))
            .sum(),
        Some(Value::Object(obj)) => obj
            .values()
            .map(|v| estimate_tokens_from_value(&Some(v.clone())))
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ryeos_runtime::ReplayedEventRecord;
    use ryeos_runtime::callback::{
        CallbackError, DispatchActionRequest, ReplayResponse, RuntimeCallbackAPI,
        TerminalCompletion,
    };
    use serde_json::json;
    use std::sync::Arc;

    struct MockCallback {
        events: Vec<ReplayedEventRecord>,
    }

    #[async_trait]
    impl RuntimeCallbackAPI for MockCallback {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            _: TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            // load_resume_state resolves the chain root from the predecessor's
            // thread detail before paging the chain.
            Ok(json!({"thread": {"chain_root_id": "C-test-chain"}}))
        }
        async fn request_continuation(
            &self,
            _: &str,
            _: Option<&str>,
            _: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: Value) -> Result<Value, CallbackError> {
            Ok(serde_json::to_value(ReplayResponse {
                events: self.events.clone(),
                next_cursor: None,
            })
            .unwrap())
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: i64,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
    }

    fn make_callback(events: Vec<ReplayedEventRecord>) -> CallbackClient {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(MockCallback { events });
        CallbackClient::from_inner(inner, "T-test", "/tmp/test", "tat-test")
    }

    fn usage_event(completed_turns: u32, last_settled_turn_seq: u64) -> ReplayedEventRecord {
        ReplayedEventRecord {
            event_type: "thread_usage".to_string(),
            payload: json!({
                "completed_turns": completed_turns,
                "input_tokens": completed_turns,
                "output_tokens": completed_turns,
                "spend_usd": "0",
                "spawns_used": 0,
                "started_at": "2026-08-31T00:00:00Z",
                "settled_at": "2026-08-31T00:00:01Z",
                "last_settled_turn_seq": last_settled_turn_seq,
                "elapsed_ms": 1,
                "provider_id": "fixture",
                "model": "fixture"
            }),
        }
    }

    #[tokio::test]
    async fn load_empty_replay_returns_empty() {
        let callback = make_callback(vec![]);
        let state = load_resume_state(&callback, "nonexistent", 8, ResumeMode::NativeSameThread)
            .await
            .unwrap();
        assert!(state.messages.is_empty());
        assert_eq!(state.turns_completed, 0);
        assert!(state.thread_usage.is_none());
    }

    #[tokio::test]
    async fn continuation_refuses_an_unresolved_predecessor_tool_batch() {
        let callback = make_callback(vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 1,
                "content": null,
                "tool_calls": [
                    {"id":"call-a", "name":"tool:a", "arguments":{"a":1}}
                ]
            }),
        }]);
        let error = load_resume_state(&callback, "T-predecessor", 8, ResumeMode::Continuation)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("continuation predecessor contains an unresolved tool batch")
        );
    }

    fn proposed_tool_batch() -> (Vec<ReplayedEventRecord>, Vec<String>, String, String) {
        let source = "T-source".to_string();
        let first = directive_tool_operation_id(&source, 3, 0);
        let second = directive_tool_operation_id(&source, 3, 1);
        let events = vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 3,
                "content": null,
                "tool_calls": [
                    {"id":"call-a", "name":"tool:a", "arguments":{"a":1}},
                    {"id":"call-b", "name":"tool:b", "arguments":{"b":2}}
                ]
            }),
        }];
        (events, vec![source], first, second)
    }

    #[test]
    fn directive_tool_operation_identity_binds_source_turn_and_order() {
        let original = directive_tool_operation_id("T-one", 4, 2);
        assert!(lillux::valid_hash(&original));
        assert_eq!(original, directive_tool_operation_id("T-one", 4, 2));
        assert_ne!(original, directive_tool_operation_id("T-two", 4, 2));
        assert_ne!(original, directive_tool_operation_id("T-one", 5, 2));
        assert_ne!(original, directive_tool_operation_id("T-one", 4, 3));
    }

    #[test]
    fn replay_requires_plain_starts_to_be_a_proposal_order_prefix() {
        let (mut events, mut sources, first, second) = proposed_tool_batch();
        let proposed = recover_tool_state(&events, &sources).unwrap();
        assert_eq!(proposed.pending.len(), 2);
        assert_eq!(proposed.pending[0].operation_id, first);
        assert!(!proposed.pending[0].start_recorded);
        assert!(!proposed.pending[0].counted);
        assert!(proposed.started.is_empty());

        events.push(ReplayedEventRecord {
            event_type: "tool_call_start".to_string(),
            payload: json!({
                "operation_id": second,
                "call_id": "call-b",
                "tool": "tool:b"
            }),
        });
        sources.push("T-source".to_string());
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("proposal-order prefix")
        );

        let (mut events, mut sources, first, second) = proposed_tool_batch();
        for (operation_id, call_id, tool) in [
            (first.clone(), "call-a", "tool:a"),
            (second.clone(), "call-b", "tool:b"),
        ] {
            events.push(ReplayedEventRecord {
                event_type: "tool_call_start".to_string(),
                payload: json!({
                    "operation_id": operation_id,
                    "call_id": call_id,
                    "tool": tool
                }),
            });
            sources.push("T-source".to_string());
        }
        let started = recover_tool_state(&events, &sources).unwrap();
        assert_eq!(started.pending.len(), 2);
        assert!(started.pending[0].start_recorded);
        assert!(started.pending[1].start_recorded);
        assert!(!started.pending[1].counted);
        assert_eq!(started.started.len(), 2);
        assert_eq!(started.started[0].operation_id, first);
        assert_eq!(started.started[1].operation_id, second);
    }

    #[test]
    fn replay_folds_settled_prefix_and_returns_only_unfinished_suffix() {
        let (mut events, mut sources, first, second) = proposed_tool_batch();
        for (event_type, payload) in [
            (
                "tool_call_start",
                json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
            ),
            (
                "tool_call_start",
                json!({"operation_id":second,"call_id":"call-b","tool":"tool:b"}),
            ),
            (
                "tool_call_result",
                json!({
                    "operation_id":first,
                    "call_id":"call-a",
                    "tool":"tool:a",
                    "result_text":"{\"ok\":true}"
                }),
            ),
        ] {
            events.push(ReplayedEventRecord {
                event_type: event_type.to_string(),
                payload,
            });
            sources.push("T-source".to_string());
        }
        let recovered = recover_tool_state(&events, &sources).unwrap();
        assert_eq!(recovered.pending.len(), 1);
        assert_eq!(recovered.pending[0].operation_id, second);
        assert!(recovered.pending[0].start_recorded);
        assert_eq!(recovered.started.len(), 2);
    }

    fn lifecycle_batch_events(
        return_index: usize,
        succeeded: bool,
    ) -> (Vec<ReplayedEventRecord>, Vec<String>) {
        let source = "T-lifecycle".to_string();
        let calls = vec![
            json!({"id":"call-a", "name":"tool:a", "arguments":{}}),
            json!({"id":"call-return", "name":DIRECTIVE_RETURN_TOOL, "arguments":{"answer":"done"}}),
            json!({"id":"call-b", "name":"tool:b", "arguments":{}}),
        ];
        let selected = if return_index == 0 {
            vec![calls[1].clone(), calls[2].clone()]
        } else {
            calls
        };
        let mut events = vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({"turn":1, "content":null, "tool_calls":selected}),
        }];
        let mut sources = vec![source.clone()];
        if return_index == 1 {
            let first = directive_tool_operation_id(&source, 1, 0);
            events.extend([
                ReplayedEventRecord {
                    event_type: "tool_call_start".to_string(),
                    payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
                },
                ReplayedEventRecord {
                    event_type: "tool_call_result".to_string(),
                    payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a","truncated":false,"result_text":"{}"}),
                },
            ]);
            sources.extend([source.clone(), source.clone()]);
        }
        let return_operation = directive_tool_operation_id(&source, 1, return_index);
        events.push(ReplayedEventRecord {
            event_type: "tool_call_start".to_string(),
            payload: json!({"operation_id":return_operation,"call_id":"call-return","tool":DIRECTIVE_RETURN_TOOL}),
        });
        events.push(ReplayedEventRecord {
            event_type: "tool_call_result".to_string(),
            payload: if succeeded {
                json!({"operation_id":return_operation,"call_id":"call-return","tool":DIRECTIVE_RETURN_TOOL,"truncated":false,"result_text":"{\"answer\":\"done\"}"})
            } else {
                json!({"operation_id":return_operation,"call_id":"call-return","tool":DIRECTIVE_RETURN_TOOL,"truncated":false,"truncated_reason":"error_envelope","result_text":"{\"error\":\"invalid\"}"})
            },
        });
        sources.extend([source.clone(), source]);
        (events, sources)
    }

    #[test]
    fn successful_lifecycle_result_is_terminal_and_abandons_later_siblings() {
        for return_index in [0, 1] {
            let (events, sources) = lifecycle_batch_events(return_index, true);
            let recovered = recover_tool_state(&events, &sources).unwrap();
            assert!(recovered.pending.is_empty());
            assert_eq!(
                recover_resume_disposition(&events, &recovered).unwrap(),
                ResumeDisposition::FinalizeDirectiveReturn {
                    outputs: json!({"answer":"done"}),
                }
            );
            assert_eq!(recovered.started.len(), return_index);
        }
    }

    #[test]
    fn lifecycle_failure_checks_continuation_and_abandons_later_siblings() {
        let (events, sources) = lifecycle_batch_events(0, false);
        let recovered = recover_tool_state(&events, &sources).unwrap();
        assert!(recovered.pending.is_empty());
        assert_eq!(
            recover_resume_disposition(&events, &recovered).unwrap(),
            ResumeDisposition::CheckContinuation
        );
    }

    #[test]
    fn evidence_after_settled_lifecycle_return_is_rejected() {
        let (mut events, mut sources) = lifecycle_batch_events(0, true);
        let later = directive_tool_operation_id("T-lifecycle", 1, 1);
        events.push(ReplayedEventRecord {
            event_type: "tool_call_start".to_string(),
            payload: json!({"operation_id":later,"call_id":"call-b","tool":"tool:b"}),
        });
        sources.push("T-lifecycle".to_string());
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("after successful directive_return")
        );
    }

    #[test]
    fn successful_lifecycle_return_rejects_later_cognition_evidence() {
        let (mut events, mut sources) = lifecycle_batch_events(0, true);
        events.push(ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 2,
                "content": "a later process must not reopen a terminal directive",
                "interrupted": true
            }),
        });
        sources.push("T-lifecycle".to_string());

        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("after successful directive_return")
        );
    }

    #[test]
    fn settled_plain_tool_batch_recovers_after_step_disposition() {
        let (mut events, mut sources, first, second) = proposed_tool_batch();
        let calls = [(first, "call-a", "tool:a"), (second, "call-b", "tool:b")];
        for (operation_id, call_id, tool) in &calls {
            events.push(ReplayedEventRecord {
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":operation_id,"call_id":call_id,"tool":tool}),
            });
            sources.push("T-source".to_string());
        }
        for (operation_id, call_id, tool) in calls {
            events.push(ReplayedEventRecord {
                event_type: "tool_call_result".to_string(),
                payload: json!({"operation_id":operation_id,"call_id":call_id,"tool":tool,"truncated":false,"result_text":"{}"}),
            });
            sources.push("T-source".to_string());
        }
        let recovered = recover_tool_state(&events, &sources).unwrap();
        assert_eq!(
            recover_resume_disposition(&events, &recovered).unwrap(),
            ResumeDisposition::AfterSettledToolBatch { turn: 3 }
        );
    }

    #[test]
    fn replay_rejects_input_interleaved_with_an_unsettled_tool_batch() {
        let (mut events, mut sources, first, _) = proposed_tool_batch();
        events.extend([
            ReplayedEventRecord {
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content":"cannot interleave"}),
            },
        ]);
        sources.extend(["T-source".to_string(), "T-source".to_string()]);
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("before every retained tool occurrence settled")
        );
    }

    #[test]
    fn replay_rejects_orphan_mismatched_and_duplicate_tool_evidence() {
        let (mut events, mut sources, first, _) = proposed_tool_batch();
        events.push(ReplayedEventRecord {
            event_type: "tool_call_result".to_string(),
            payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
        });
        sources.push("T-source".to_string());
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("precedes tool_call_start")
        );

        let (mut events, mut sources, first, _) = proposed_tool_batch();
        events.push(ReplayedEventRecord {
            event_type: "tool_call_start".to_string(),
            payload: json!({"operation_id":first,"call_id":"wrong","tool":"tool:a"}),
        });
        sources.push("T-source".to_string());
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("call_id does not match")
        );

        let (mut events, mut sources, first, _) = proposed_tool_batch();
        for _ in 0..2 {
            events.push(ReplayedEventRecord {
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
            });
            sources.push("T-source".to_string());
        }
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("duplicate tool_call_start")
        );

        let orphan = vec![ReplayedEventRecord {
            event_type: "tool_call_start".to_string(),
            payload: json!({
                "operation_id":"9".repeat(64),
                "call_id":"call-x",
                "tool":"tool:x"
            }),
        }];
        assert!(
            recover_tool_state(&orphan, &["T-source".to_string()])
                .unwrap_err()
                .to_string()
                .contains("without an open cognition proposal")
        );

        let (mut events, mut sources, _, _) = proposed_tool_batch();
        events.push(ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({"turn": 4, "content": "skipped unresolved tools"}),
        });
        sources.push("T-source".to_string());
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("before every prior tool occurrence settled")
        );
    }

    #[test]
    fn replay_rejects_successor_thread_settlement_of_predecessor_proposal() {
        let (mut events, mut sources, first, _) = proposed_tool_batch();
        events.push(ReplayedEventRecord {
            event_type: "tool_call_start".to_string(),
            payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
        });
        sources.push("T-successor".to_string());
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("source thread differs from its cognition proposal")
        );

        let (mut events, mut sources, first, _) = proposed_tool_batch();
        events.extend([
            ReplayedEventRecord {
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
            },
            ReplayedEventRecord {
                event_type: "tool_call_result".to_string(),
                payload: json!({
                    "operation_id":first,
                    "call_id":"call-a",
                    "tool":"tool:a",
                    "result_text":"{}"
                }),
            },
        ]);
        sources.extend(["T-source".to_string(), "T-successor".to_string()]);
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("source thread differs from its cognition proposal")
        );
    }

    #[test]
    fn replay_only_turn_needs_no_fabricated_usage_settlement() {
        let events = vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 1,
                "content": "replayed",
                "provider_accounting": {"replayed_from": "ab".repeat(32)}
            }),
        }];
        let progress = recover_turn_progress(&events).unwrap();
        assert_eq!(progress.completed, 1);
        assert!(recover_thread_usage(&events, progress).unwrap().is_none());
    }

    #[test]
    fn live_turn_without_exact_usage_settlement_fails_closed() {
        let events = vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({"turn": 1, "content": "live"}),
        }];
        let progress = recover_turn_progress(&events).unwrap();
        assert!(
            recover_thread_usage(&events, progress)
                .unwrap_err()
                .to_string()
                .contains("no exact cumulative usage settlement")
        );
    }

    #[test]
    fn interrupted_settlement_cannot_authorize_a_later_live_retry() {
        let events = vec![
            usage_event(0, 1),
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 1, "content": "partial", "interrupted": true}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 2, "content": "retry"}),
            },
        ];
        let progress = recover_turn_progress(&events).unwrap();
        assert_eq!(progress.last_coordinate, 2);
        assert_eq!(progress.completed, 1);
        assert!(
            recover_thread_usage(&events, progress)
                .unwrap_err()
                .to_string()
                .contains("no exact cumulative usage settlement")
        );
    }

    #[test]
    fn usage_may_be_one_turn_ahead_at_pre_cognition_crash_cut() {
        let events = vec![usage_event(1, 1)];
        let progress = recover_turn_progress(&events).unwrap();
        assert_eq!(progress.completed, 0);
        let usage = recover_thread_usage(&events, progress).unwrap().unwrap();
        assert_eq!(usage.completed_turns, 1);
        assert_eq!(usage.last_settled_turn_seq, 1);
    }

    #[test]
    fn interrupted_cognition_advances_coordinate_but_not_completed_budget() {
        let events = vec![
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 1, "content": "partial", "interrupted": true}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({
                    "turn": 2,
                    "content": "completed replay",
                    "provider_accounting": {"replayed_from": "cd".repeat(32)}
                }),
            },
        ];
        assert_eq!(
            recover_turn_progress(&events).unwrap(),
            RecoveredTurnProgress {
                last_coordinate: 2,
                completed: 1,
            }
        );
    }

    #[test]
    fn interrupted_or_mixed_stream_cognition_cannot_authorize_tool_calls() {
        let interrupted = vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 1,
                "interrupted": true,
                "tool_calls": [{"id":"c1","name":"tool:a","arguments":{}}]
            }),
        }];
        assert!(
            recover_tool_state(&interrupted, &["T-source".to_string()])
                .unwrap_err()
                .to_string()
                .contains("cannot authorize completed tool calls")
        );

        let mixed = vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 1,
                "delta": "live",
                "tool_calls": [{"id":"c1","name":"tool:a","arguments":{}}]
            }),
        }];
        assert!(
            recover_tool_state(&mixed, &["T-source".to_string()])
                .unwrap_err()
                .to_string()
                .contains("streaming cognition_out also carries final field")
        );
    }

    #[test]
    fn malformed_retained_tool_calls_cannot_disappear_during_replay() {
        let event = ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({"turn": 1, "content": "contradictory", "tool_calls": {}}),
        };
        let error = recover_tool_state(&[event], &["T-source".to_string()]).unwrap_err();
        assert!(error.to_string().contains("not an array or null"));
    }

    #[test]
    fn malformed_optional_tool_call_id_cannot_be_downgraded_to_absent() {
        let event = ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 1,
                "tool_calls": [{"id": {}, "name": "tool:a", "arguments": {}}]
            }),
        };
        let error = recover_tool_state(&[event], &["T-source".to_string()]).unwrap_err();
        assert!(error.to_string().contains("'id' is not a string or null"));
    }

    #[test]
    fn retained_terminal_cognition_is_parsed_without_another_provider_call() {
        let events = vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({"turn":1,"content":"done"}),
        }];
        let recovered = recover_tool_state(&events, &["T-source".to_string()]).unwrap();
        assert_eq!(
            recover_resume_disposition(&events, &recovered).unwrap(),
            ResumeDisposition::ParseRetainedResponse
        );

        let mut redirected = events.clone();
        redirected.push(ReplayedEventRecord {
            event_type: "cognition_in".to_string(),
            payload: json!({"content":"steer"}),
        });
        let recovered = recover_tool_state(
            &redirected,
            &["T-source".to_string(), "T-source".to_string()],
        )
        .unwrap();
        assert_eq!(
            recover_resume_disposition(&redirected, &recovered).unwrap(),
            ResumeDisposition::ContinueProvider
        );
    }

    #[tokio::test]
    async fn oversized_pending_proposal_fails_closed_before_dispatch() {
        let callback = make_callback(vec![ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn":1,
                "content":null,
                "tool_calls":[{
                    "id":"large-call",
                    "name":"tool:large",
                    "arguments":{"payload":"x".repeat(70_000)}
                }],
                "provider_accounting":{"replayed_from":"ef".repeat(32)}
            }),
        }]);
        let error = load_resume_state(&callback, "nonexistent", 0, ResumeMode::NativeSameThread)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("required provider transcript suffix exceeds")
        );
    }

    #[test]
    fn reconstruct_messages_from_cognition_transcript() {
        let events = vec![
            // turn-boundary marker (no content) — skipped
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Hello"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 1, "content": "Hi there!"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Do something"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({
                    "turn": 2,
                    "content": null,
                    "tool_calls": [{"id": "c1", "name": "read_file", "arguments": {"path": "/tmp"}}]
                }),
            },
            ReplayedEventRecord {
                event_type: "tool_call_result".to_string(),
                payload: json!({"call_id": "c1", "tool": "read_file", "result_text": "file contents"}),
            },
        ];

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "user");
        assert!(messages[3].tool_calls.is_some());
        assert_eq!(messages[4].role, "tool");
        assert_eq!(messages[4].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn replay_preserves_json_looking_tool_content_as_the_exact_provider_string() {
        let exact = r#"{"ok":true,"nested":{"value":7}}"#;
        let events = vec![ReplayedEventRecord {
            event_type: "tool_call_result".to_string(),
            payload: json!({
                "call_id": "call-exact",
                "tool": "tool:exact",
                "result_text": exact,
                "truncated": false,
            }),
        }];

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(
            messages[0].content.as_ref().and_then(Value::as_str),
            Some(exact)
        );
        let (wire, system) = crate::provider_adapter::messages::convert_messages(&messages, &None);
        assert!(system.is_none());
        assert_eq!(wire[0]["content"].as_str(), Some(exact));
        assert!(wire[0]["content"].is_string());
    }

    #[test]
    fn replay_rejects_tool_results_without_exact_string_content() {
        for payload in [
            json!({"call_id":"c","tool":"tool:t"}),
            json!({"call_id":"c","tool":"tool:t","result_text":{"ok":true}}),
            json!({"call_id":"c","tool":"tool:t","result_text":"{}","result":{}}),
        ] {
            let error = reconstruct_messages(&[ReplayedEventRecord {
                event_type: "tool_call_result".to_string(),
                payload,
            }])
            .unwrap_err();
            assert!(
                error.to_string().contains("result_text")
                    || error
                        .to_string()
                        .contains("obsolete parsed result authority")
            );
        }
    }

    #[test]
    fn native_replay_rebuilds_result_deduplication_in_exact_event_order() {
        let content = "x".repeat(1024);
        let hash = lillux::sha256_hex(content.as_bytes());
        let marker = format!("[duplicate result omitted — hash {}]", &hash[..16]);
        let events = vec![
            ReplayedEventRecord {
                event_type: "tool_call_result".to_string(),
                payload: json!({
                    "tool":"tool:t",
                    "result_text":content,
                    "truncated":false,
                }),
            },
            ReplayedEventRecord {
                event_type: "tool_call_result".to_string(),
                payload: json!({
                    "tool":"tool:t",
                    "result_text":marker,
                    "truncated":false,
                    "deduplicated":true,
                    "duplicate_of":hash,
                }),
            },
        ];
        let sources = vec!["T-current".to_string(), "T-current".to_string()];
        let mut guard = recover_result_guard(&events, &sources, "T-current").unwrap();
        let next = guard.process(&content);
        assert_eq!(next.duplicate_of.as_deref(), Some(hash.as_str()));
        assert_eq!(next.content, marker);
    }

    #[test]
    fn malformed_optional_result_metadata_cannot_be_downgraded_to_absent() {
        for (field, value, expected) in [
            ("deduplicated", json!({}), "not a boolean"),
            ("truncated_reason", json!(false), "not a string"),
        ] {
            let mut payload = json!({
                "tool":"tool:t",
                "result_text":"{}",
                "truncated":false,
            });
            payload[field] = value;
            let error = recover_result_guard(
                &[ReplayedEventRecord {
                    event_type: "tool_call_result".to_string(),
                    payload,
                }],
                &["T-current".to_string()],
                "T-current",
            )
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn reconstruct_messages_folds_chunked_cognition_input_once() {
        let content = "large ARC evidence\n".repeat(20_000);
        let events = ryeos_runtime::encode_cognition_in_payloads(&content)
            .unwrap()
            .into_iter()
            .map(|payload| ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload,
            })
            .collect::<Vec<_>>();

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(
            messages[0].content.as_ref().and_then(Value::as_str),
            Some(content.as_str())
        );
    }

    #[test]
    fn reconstruct_messages_skips_streaming_cognition_out_fragments() {
        let events = vec![
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Hello"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"delta": "H"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"tool_use_partial": {"id": "partial"}}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"tool_use": {"id": "call-1", "name": "read_file"}}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 1, "content": "Hi there!"}),
            },
        ];

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].content.as_ref().and_then(|v| v.as_str()),
            Some("Hi there!")
        );

        let trimmed = trim_to_recent_turns(messages, 1);
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].role, "user");
        assert_eq!(trimmed[1].role, "assistant");
    }

    #[test]
    fn interrupted_seal_folds_as_assistant_then_redirect() {
        // A live-interrupt seal is a content-bearing cognition_out (with
        // interrupted:true, no tool_calls). Resume must fold it as an assistant
        // message in order — the redirect input follows as the next user turn,
        // and the fresh cognition's output after that. The `interrupted` marker is
        // ignored by the fold (only content/tool_calls/reasoning are read).
        let events = vec![
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "start the long task"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 1, "content": "working on it, step on", "interrupted": true}),
            },
            // The operator's redirect, folded as a durable cognition_in by the
            // daemon's poll-and-persist.
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "stop — do X instead"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({"turn": 2, "content": "doing X"}),
            },
        ];
        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(
            messages[1].content.as_ref().and_then(|c| c.as_str()),
            Some("working on it, step on"),
            "interrupted partial content is preserved"
        );
        assert!(
            messages[1].tool_calls.is_none(),
            "an interrupted seal carries no tool_calls — no unpaired tool call"
        );
        assert_eq!(messages[2].role, "user");
        assert_eq!(
            messages[2].content.as_ref().and_then(|c| c.as_str()),
            Some("stop — do X instead")
        );
        assert_eq!(messages[3].role, "assistant");
    }

    #[test]
    fn non_conversational_events_are_skipped() {
        // Lifecycle / usage / turn-marker / streaming events carry no message;
        // folding skips them rather than failing.
        let events = vec![
            ReplayedEventRecord {
                event_type: "thread_created".to_string(),
                payload: json!({}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1}),
            },
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Hello"}),
            },
            ReplayedEventRecord {
                event_type: "thread_usage".to_string(),
                payload: json!({"completed_turns": 1}),
            },
            ReplayedEventRecord {
                event_type: "tool_call_start".to_string(),
                payload: json!({"tool": "read_file"}),
            },
        ];
        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn count_turns_correct() {
        let messages = vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("hi")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("again")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("there")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
        ];
        assert_eq!(count_turns(&messages), 2);
    }

    #[test]
    fn lifecycle_return_start_does_not_consume_dispatch_attempt_budget() {
        let source = "T-source".to_string();
        let operation_id = directive_tool_operation_id(&source, 1, 0);
        let events = vec![
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({
                    "turn": 1,
                    "tool_calls": [{
                        "id": "call-return",
                        "name": DIRECTIVE_RETURN_TOOL,
                        "arguments": {"answer": "done"}
                    }]
                }),
            },
            ReplayedEventRecord {
                event_type: "tool_call_start".to_string(),
                payload: json!({
                    "operation_id": operation_id,
                    "call_id": "call-return",
                    "tool": DIRECTIVE_RETURN_TOOL
                }),
            },
        ];
        let recovered = recover_tool_state(&events, &[source.clone(), source]).unwrap();
        assert!(recovered.started.is_empty());
    }

    #[test]
    fn trim_to_token_budget_works() {
        let mut messages = Vec::new();
        for i in 0..100 {
            messages.push(ProviderMessage {
                role: "user".to_string(),
                content: Some(json!(format!("message {} with some content here", i))),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
            messages.push(ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!(format!("answer {i} with some content here"))),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            });
        }
        let trimmed = trim_to_token_budget(messages, 200, true).unwrap();
        assert!(trimmed.len() < 200);
        assert!(!trimmed.is_empty());
    }

    fn retry_advance(
        turn: u32,
        failed_attempt_number: u32,
        tag: &str,
    ) -> ryeos_accounting::ProviderRetryAdvance {
        ryeos_accounting::ProviderRetryAdvance::build(
            format!("A-{tag}"),
            lillux::sha256_hex(format!("request-{tag}").as_bytes()),
            lillux::sha256_hex(b"provider-coordinate"),
            ryeos_accounting::ProviderRetryDecision {
                turn,
                failed_attempt_number,
                next_attempt_number: failed_attempt_number + 1,
                backoff_ms: 250,
                reason: ryeos_accounting::ProviderRetryReason::Status { code: 503 },
                failure_digest: ryeos_accounting::HexDigest::new(lillux::sha256_hex(
                    format!("failure-{tag}").as_bytes(),
                ))
                .unwrap(),
                retry_policy_digest: ryeos_accounting::HexDigest::new(lillux::sha256_hex(
                    b"retry-policy",
                ))
                .unwrap(),
            },
            1_000,
        )
        .unwrap()
    }

    fn retry_event(advance: &ryeos_accounting::ProviderRetryAdvance) -> ReplayedEventRecord {
        ReplayedEventRecord {
            event_type: "provider_retry".to_string(),
            payload: json!({
                "turn": advance.decision.turn,
                "attempt": advance.decision.failed_attempt_number,
                "max_retries": 3,
                "backoff_ms": advance.decision.backoff_ms,
                "decision_digest": advance.decision_digest,
                "retry_advance": advance,
            }),
        }
    }

    #[test]
    fn native_resume_recovers_only_ordered_retry_testimony_inside_the_active_turn() {
        let advance = retry_advance(1, 1, "one");
        let mut events = vec![
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1}),
            },
            retry_event(&advance),
        ];
        let mut sources = vec!["T-current".to_string(); events.len()];
        assert_eq!(
            recover_active_provider_turn(
                &events,
                &sources,
                "T-current",
                ResumeMode::NativeSameThread,
            )
            .unwrap(),
            Some(1)
        );
        assert_eq!(
            recover_provider_retry_testimonies(
                &events,
                &sources,
                "T-current",
                ResumeMode::NativeSameThread,
            )
            .unwrap(),
            std::collections::BTreeSet::from([advance.decision_digest.as_str().to_owned()])
        );

        events.push(ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({"turn": 1, "content": "settled"}),
        });
        sources.push("T-current".to_string());
        assert_eq!(
            recover_active_provider_turn(
                &events,
                &sources,
                "T-current",
                ResumeMode::NativeSameThread,
            )
            .unwrap(),
            None
        );
        recover_provider_retry_testimonies(
            &events,
            &sources,
            "T-current",
            ResumeMode::NativeSameThread,
        )
        .unwrap();

        let skipped = vec![
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1}),
            },
            retry_event(&retry_advance(1, 2, "skipped")),
        ];
        let error = recover_provider_retry_testimonies(
            &skipped,
            &["T-current".to_string(), "T-current".to_string()],
            "T-current",
            ResumeMode::NativeSameThread,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not follow expected attempt 1")
        );

        let orphan = vec![retry_event(&advance)];
        let error = recover_provider_retry_testimonies(
            &orphan,
            &["T-current".to_string()],
            "T-current",
            ResumeMode::NativeSameThread,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside its active provider turn")
        );
    }

    #[tokio::test]
    async fn full_roundtrip_via_callback() {
        let events = vec![
            ReplayedEventRecord {
                event_type: "cognition_in".to_string(),
                payload: json!({"content": "Do task"}),
            },
            ReplayedEventRecord {
                event_type: "cognition_out".to_string(),
                payload: json!({
                    "turn": 1,
                    "content": "Done!",
                    "provider_accounting": {"replayed_from": "ab".repeat(32)}
                }),
            },
        ];
        let callback = make_callback(events);
        let state = load_resume_state(&callback, "T-prev", 8, ResumeMode::NativeSameThread)
            .await
            .unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.turns_completed, 1);
        assert!(state.started_tool_occurrences.is_empty());
    }

    /// A chain where turn T2 continues T1, and T1 also has a non-continuation
    /// child `T1-child` sharing the chain root that emits its own
    /// `cognition_out`. Resume must fold ONLY the linear continuation path
    /// (T1, T2), never the child — proving the path-scoped (per-thread) fold.
    struct PathMock;

    #[async_trait]
    impl RuntimeCallbackAPI for PathMock {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            _: TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, thread_id: &str) -> Result<Value, CallbackError> {
            // T2's continuation predecessor is T1; T1 is the root (no upstream).
            let upstream = if thread_id == "T2" { Some("T1") } else { None };
            Ok(json!({"thread": {"chain_root_id": "T1", "upstream_thread_id": upstream}}))
        }
        async fn request_continuation(
            &self,
            _: &str,
            _: Option<&str>,
            _: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, params: Value) -> Result<Value, CallbackError> {
            let tid = params
                .get("thread_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ev = |t: &str, p: Value| ReplayedEventRecord {
                event_type: t.to_string(),
                payload: p,
            };
            let usage = |turn: u32| {
                json!({
                    "completed_turns": turn,
                    "input_tokens": turn,
                    "output_tokens": turn,
                    "spend_usd": "0",
                    "spawns_used": 0,
                    "started_at": "2026-08-31T00:00:00Z",
                    "settled_at": "2026-08-31T00:00:01Z",
                    "last_settled_turn_seq": turn,
                    "elapsed_ms": turn,
                    "provider_id": "fixture",
                    "model": "fixture"
                })
            };
            let events = match tid {
                "T1" => vec![
                    ev("cognition_in", json!({"content": "turn1 in"})),
                    ev("thread_usage", usage(1)),
                    ev("cognition_out", json!({"turn": 1, "content": "turn1 out"})),
                ],
                "T2" => vec![
                    ev("cognition_in", json!({"content": "turn2 in"})),
                    ev("thread_usage", usage(2)),
                    ev("cognition_out", json!({"turn": 2, "content": "turn2 out"})),
                ],
                // Non-continuation child sharing the chain root — must NOT fold.
                "T1-child" => vec![ev("cognition_out", json!({"content": "POLLUTION"}))],
                _ => vec![],
            };
            Ok(serde_json::to_value(ReplayResponse {
                events,
                next_cursor: None,
            })
            .unwrap())
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: i64,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
    }

    #[tokio::test]
    async fn resume_folds_only_continuation_path_not_chain_children() {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(PathMock);
        let callback = CallbackClient::from_inner(inner, "T3", "/tmp/test", "tat-test");
        let state = load_resume_state(&callback, "T2", 8, ResumeMode::Continuation)
            .await
            .unwrap();

        let contents: Vec<String> = state
            .messages
            .iter()
            .filter_map(|m| {
                m.content
                    .as_ref()
                    .and_then(|c| c.as_str())
                    .map(String::from)
            })
            .collect();
        // Both turns, root-first, in order; the chain-sharing child is excluded.
        assert_eq!(
            contents,
            vec!["turn1 in", "turn1 out", "turn2 in", "turn2 out"]
        );
        assert!(
            !contents.iter().any(|c| c.contains("POLLUTION")),
            "non-continuation child events must not be folded"
        );
        assert_eq!(state.turns_completed, 2);
        assert_eq!(state.thread_usage.unwrap().completed_turns, 2);
    }

    #[tokio::test]
    async fn zero_carry_still_preserves_cumulative_turn_coordinate() {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(PathMock);
        let callback = CallbackClient::from_inner(inner, "T3", "/tmp/test", "tat-test");
        let state = load_resume_state(&callback, "T2", 0, ResumeMode::Continuation)
            .await
            .unwrap();
        assert!(state.messages.is_empty());
        assert_eq!(state.turns_completed, 2);
    }

    #[test]
    fn trim_to_recent_turns_keeps_last_n_assistant_turns_with_context() {
        let messages = vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("u1")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("a1")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("u2")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("a2")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "tool".to_string(),
                content: Some(json!("tool2")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("u3")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("a3")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
        ];

        let trimmed = trim_to_recent_turns(messages, 2);
        let contents: Vec<_> = trimmed
            .iter()
            .filter_map(|m| m.content.as_ref().and_then(|v| v.as_str()))
            .collect();
        assert_eq!(contents, vec!["u2", "a2", "tool2", "u3", "a3"]);
    }

    #[test]
    fn trim_to_recent_turns_drops_leading_orphan_tool_results() {
        let messages = vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("u1")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("a1 tool call")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "tool".to_string(),
                content: Some(json!("orphan if kept")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("u2")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("a2")),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
            },
        ];

        let trimmed = trim_to_recent_turns(messages, 1);
        let contents: Vec<_> = trimmed
            .iter()
            .filter_map(|m| m.content.as_ref().and_then(|v| v.as_str()))
            .collect();
        assert_eq!(contents, vec!["u2", "a2"]);
    }
}
