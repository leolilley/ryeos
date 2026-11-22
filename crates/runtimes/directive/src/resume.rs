use anyhow::{Context, Result};
use serde_json::Value;

use ryeos_runtime::ReplayedEventRecord;
use ryeos_runtime::callback_client::{CallbackClient, RuntimeReplayBudget};
use ryeos_state::ThreadUsage;

use crate::directive::{
    DIRECTIVE_RETURN_TOOL, ProviderMessage, ProviderMessageExt, TIME_LEFT_TOOL,
};
use crate::result_guard::ResultGuard;
use crate::runner::ToolOccurrence;
use crate::tool_images::ImageRunState;
use ryeos_directive_definition::ImageAcceptConfig;
use ryeos_provider_contract::ToolResultStatus;
use ryeos_runtime::{
    CognitionFramePayload, CognitionOutPayload, FINISH_INTERRUPTED, ToolResultPayload,
};

#[derive(Debug)]
pub struct ResumeState {
    pub messages: Vec<ProviderMessage>,
    /// Model-proposed calls that do not yet have a matching durable result.
    /// These resume before another provider request and reuse their exact
    /// logical operation identities.
    pub pending_tools: Vec<ToolOccurrence>,
    /// Calls after a failed `directive_return` in the same batch that have no
    /// refusal result yet. Each is owed one, and is never run.
    pub owed_siblings: Vec<ToolOccurrence>,
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
    /// Final turns in a row, ending at the latest recorded turn of this exact
    /// native thread, that ended at the provider's output limit.
    pub trailing_length_turns: u32,
    /// Malformed replies in a row, ending at the newest recorded one that the
    /// run has not yet moved past: the position its correction names.
    pub trailing_malformed_replies: u32,
    /// Empty replies in a row, ending at the newest recorded one that the run
    /// has not yet moved past: the position its correction names.
    pub trailing_empty_replies: u32,
    /// Whether the newest final turn of this exact native thread ended at a
    /// normal stop finish.
    pub last_final_stopped: bool,
    /// The latest frame recorded on the path: what the model was given besides
    /// the conversation. `None` when the path recorded none.
    pub frame: Option<CognitionFramePayload>,
    /// Which wrap-up reasons the path's wrap-up inputs already gave, so a
    /// recovered or continued segment does not tell the model twice.
    pub wrap_up_delivered: WrapUpDelivered,
    /// The loop findings the model was already warned about, each with the
    /// number of turns the history held when the warning was given. The
    /// history is `guard_prefix` followed by the conversation.
    pub loop_warned: crate::loop_guards::WarnedAt,
    /// The turns before the conversation that the loop detectors still read:
    /// a compaction's carried digests, or the turns of the predecessors a
    /// successor's start did not carry. Oldest first, at most
    /// `GuardCarry::MAX_DIGESTS`.
    pub guard_prefix: Vec<crate::loop_guards::TurnDigest>,
    /// How complete `guard_prefix` is, for the run's current settings to judge.
    pub guard_history: GuardHistory,
    /// What the path has stored of tool images, against the run's limits.
    pub images: ImageRunState,
    /// Tool starts and child spawns counted before the replay window of the
    /// latest compaction; zero when the path holds no compaction.
    pub tool_starts_before_window: u32,
    pub spawns_before_window: u32,
    /// The latest compaction on the path, as far as the live run needs it.
    pub compaction: Option<CompactionMemo>,
    /// The trigger of the compaction summary call this thread had in flight
    /// when its process was lost (a same-thread recovery re-enters it).
    pub active_summary_trigger: Option<String>,
    /// The code of the refusal the path's latest provider turn recorded, when
    /// no completed turn or compaction follows it: the request that was
    /// refused is still to be retried.
    pub refused_code: Option<String>,
    /// Continuation predecessors this thread has on its path.
    pub chain_depth: u32,
    /// A compacted start that is still over the continuation threshold: the
    /// run compacts again before its first request when its policy allows.
    pub start_over_ceiling: bool,
}

/// What a run keeps in memory of the latest compaction on its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionMemo {
    pub settled_turns: u32,
    pub ineffective: bool,
    pub size_after: u64,
    pub compactions: u32,
    pub overflow_recoveries: u32,
    pub trigger: String,
}

impl CompactionMemo {
    pub fn of(record: &ryeos_runtime::CompactionPayload) -> Self {
        Self {
            settled_turns: record.settled_turns,
            ineffective: record.ineffective,
            size_after: record.size_after,
            compactions: record.compactions,
            overflow_recoveries: record.overflow_recoveries,
            trigger: record.trigger.clone(),
        }
    }
}

impl ResumeState {
    /// A thread that was marked running and then lost its process before it
    /// wrote any launch record: nothing recorded, nothing to rebuild. Its
    /// resume is a fresh launch.
    pub fn has_no_launch_records(&self) -> bool {
        self.frame.is_none() && self.messages.is_empty() && self.last_turn_coordinate == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// Re-spawn of the same unfinished thread after process/daemon failure.
    NativeSameThread,
    /// A new continuation thread folding a settled predecessor chain.
    Continuation,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum ResumeDisposition {
    #[default]
    ContinueProvider,
    ParseRetainedResponse,
    AfterSettledToolBatch {
        turn: u32,
    },
    CheckContinuation,
    /// A failed `directive_return` settled and later calls of its batch are
    /// still owed their refusal results (`ResumeState::owed_siblings`).
    SettleOwedSiblings,
    FinalizeDirectiveReturn {
        outputs: Value,
    },
    /// A compaction's summary call settled but its record was not written:
    /// write it from the retained summary. No second model call is made.
    FinishCompaction {
        trigger: String,
        summary: Vec<ryeos_provider_contract::Part>,
    },
    /// The thread recorded the refusal of a request the compaction policy
    /// answers by continuing; only the handoff to the successor is left.
    ForkAfterRefusal,
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

/// Code of the refusal result a call gets when an earlier `directive_return`
/// call of the same batch failed, so the call was never run.
pub(crate) const TOOL_NOT_RUN_AFTER_FAILED_RETURN: &str = "tool_not_run_after_failed_return";

/// The model-visible body of the refusal a call after a failed
/// `directive_return` gets. Fixed text: the runtime writes it and recovery
/// compares against it exactly, so no tool output can pass as the refusal.
pub(crate) fn failed_return_refusal_body() -> Value {
    serde_json::json!({
        "error": {
            "code": TOOL_NOT_RUN_AFTER_FAILED_RETURN,
            "message": "not run: an earlier directive_return call in the same batch failed; call it again if still needed",
        }
    })
}

/// Whether a recorded tool result is exactly the refusal a call after a
/// failed `directive_return` is owed: an error result whose whole body is the
/// fixed envelope, with no extra field or other text.
fn is_failed_return_refusal(result: &ToolResultPayload) -> bool {
    result.status == ToolResultStatus::Error
        && serde_json::from_str::<Value>(&result.text())
            .is_ok_and(|body| body == failed_return_refusal_body())
}

/// Backstop on the continuation-path walk — far above any real conversation
/// length. Exceeding it is treated as a runaway/cyclic chain and errors loudly
/// rather than silently truncating (which would drop conversation history).
const MAX_CONTINUATION_PATH: usize = 10_000;

/// The event types a recovery reads. Everything else a thread records, such
/// as launch audit and accounting transitions, is neither fetched nor counted
/// against the replay budget.
const RECOVERY_EVENT_TYPES: [&str; 8] = [
    "cognition_compaction",
    "cognition_frame",
    "cognition_in",
    "cognition_out",
    "tool_call_start",
    "tool_call_result",
    "thread_usage",
    "provider_retry",
];

/// `images` is the model's image cost declaration, so the carry trim counts an
/// image as the request will.
pub async fn load_resume_state(
    callback: &CallbackClient,
    previous_thread_id: &str,
    carry: CarryPolicy,
    mode: ResumeMode,
    images: Option<&ImageAcceptConfig>,
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
    let chain_depth = count_machine_edges(callback, &path).await?;

    // A path that holds a compaction replays from the compaction's kept turns
    // on, not from its start: the compaction carries what came before.
    let ReplayWindow {
        events,
        source_thread_ids,
        anchor,
    } = replay_window(callback, &path).await?;
    refuse_markers_without_purpose(&events)?;
    let seed = anchor
        .as_ref()
        .map(|anchor| anchor.payload.seed.clone())
        .unwrap_or_default();

    let own_start = match mode {
        // The current thread's events are the suffix of its own path.
        ResumeMode::NativeSameThread => source_thread_ids
            .iter()
            .position(|source| source == previous_thread_id)
            .unwrap_or(events.len()),
        // Every thread of a successor's path is a predecessor.
        ResumeMode::Continuation => events.len(),
    };
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
                return Err(ResumeFoldError::UnsettledToolBatch {
                    detail: "continuation predecessor contains an unresolved tool batch"
                        .to_string(),
                }
                .into());
            }
            ResumeMode::NativeSameThread => {}
        }
    }
    if !recovered_tools.owed.is_empty() {
        let owed_source = recovered_tools
            .owed_source_thread_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("resume: owed refusals lost their source thread"))?;
        match mode {
            ResumeMode::NativeSameThread if owed_source != previous_thread_id => {
                anyhow::bail!(
                    "resume: unsettled refusals belong to predecessor thread {owed_source}, not native-resume thread {previous_thread_id}"
                );
            }
            ResumeMode::Continuation => {
                return Err(ResumeFoldError::UnsettledToolBatch {
                    detail: "continuation predecessor contains an unresolved tool batch"
                        .to_string(),
                }
                .into());
            }
            ResumeMode::NativeSameThread => {}
        }
    }
    let result_guard = if mode == ResumeMode::NativeSameThread {
        recover_result_guard(&events, &source_thread_ids, previous_thread_id)?
    } else {
        ResultGuard::new()
    };
    let turn_progress = recover_turn_progress_from(&events, &seed)?;
    let unfinished = recover_unfinished_provider_turn(
        &events,
        &source_thread_ids,
        previous_thread_id,
        seed.last_coordinate,
    )?;
    // A settled predecessor with a turn that never completed is a record
    // defect: a refusal the policy answers is recorded as a final turn.
    if mode == ResumeMode::Continuation && unfinished.is_some() {
        anyhow::bail!("resume: continuation predecessor contains an unfinished provider turn");
    }
    let active_provider_turn = unfinished.map(|turn| turn.turn);
    let active_summary_trigger = unfinished
        .and_then(|turn| turn.purpose.summary_trigger())
        .map(str::to_string);
    let refused_code = recover_refused_code(&events);
    let provider_retry_testimony_digests =
        recover_provider_retry_testimonies(&events, &source_thread_ids, previous_thread_id, mode)?;
    let thread_usage = recover_thread_usage(&events, turn_progress)?;
    let disposition = if mode == ResumeMode::NativeSameThread {
        recover_resume_disposition(&events, &recovered_tools)?
    } else {
        ResumeDisposition::ContinueProvider
    };
    // The conversation a recovery rebuilds. Same-thread recovery never trims
    // the current thread's own events: it rebuilds everything the thread
    // recorded. A predecessor thread's part of the path is what a successor
    // started from, so it is carried the way the successor's start carried it:
    // pinned inputs, then the most recent turns.
    let trailing_length_turns = trailing_length_turns(&events[own_start..])?;
    let trailing_malformed_replies = trailing_malformed_replies(&events[own_start..])?;
    let trailing_empty_replies = trailing_empty_replies(&events[own_start..])?;
    let last_final_stopped = last_final_stopped(&events[own_start..])?;
    let frame = latest_frame(&events)?;
    let (messages, wrap_up_delivered, loop_warned, start_over_ceiling, guard_prefix, guard_history) =
        match anchor.as_ref() {
            // A compacted conversation is the compaction's own fold; it is not cut
            // again. A successor's start must still fit the window.
            Some(anchor) => {
                let folded = fold_messages_anchored(&events, Some(anchor.chain_seq))?;
                let kept_at_record = turns_before_event(&events, anchor.chain_seq)?;
                let signals = anchored_signals(
                    mode,
                    anchor.payload.guard_carry.as_ref(),
                    kept_at_record,
                    &folded,
                );
                let messages: Vec<ProviderMessage> =
                    folded.into_iter().map(|folded| folded.message).collect();
                let over = mode == ResumeMode::Continuation
                    && crate::estimate::estimate_tokens(&messages, images) > carry.ceiling_tokens;
                (
                    messages,
                    signals.delivered,
                    signals.warned,
                    over,
                    signals.prefix,
                    signals.history,
                )
            }
            None => {
                let predecessor = fold_messages(&events[..own_start])?;
                let own = fold_messages(&events[own_start..])?;
                let predecessor_digests = crate::loop_guards::digests_from_messages(
                    &predecessor
                        .iter()
                        .map(|folded| folded.message.clone())
                        .collect::<Vec<_>>(),
                );
                let predecessor_notices: Vec<(ProviderMessage, WrapUpDelivered)> = predecessor
                    .iter()
                    .filter_map(|folded| {
                        wrap_up_notice_reasons(folded).map(|r| (folded.message.clone(), r))
                    })
                    .collect();
                let mut wrap_up_delivered = own
                    .iter()
                    .filter_map(wrap_up_notice_reasons)
                    .fold(WrapUpDelivered::default(), WrapUpDelivered::union);
                let mut messages = carry_into_successor(predecessor, &carry, images)?;
                // A notice counts as delivered only if the conversation the model
                // is given still holds it: a successor whose carried start trimmed
                // it away is told again, with the time left now.
                for (notice, reasons) in &predecessor_notices {
                    if messages.contains(notice) {
                        wrap_up_delivered = wrap_up_delivered.union(*reasons);
                    }
                }
                // The predecessor turns the carried start left out are still
                // history the loop detectors read. A successor is made by the
                // runtime (context window, a refused request) or by an operator,
                // never by a model action, but a looping run fills its context
                // and is continued, so detection must not start over there.
                let carried = crate::loop_guards::digests_from_messages(&messages).len();
                let prefix = successor_prefix(&predecessor_digests, carried);
                // A loop warning is positioned among the turns of the whole
                // history: the prefix, the carried part, then this thread's own
                // messages.
                let mut loop_warned = crate::loop_guards::WarnedAt::new();
                let mut turns_so_far = (prefix.len() + carried) as u32;
                for folded in &own {
                    if folded.message.role == ryeos_provider_contract::Role::Assistant {
                        turns_so_far += 1;
                    }
                    if let Some(kind) = loop_warning_kind(folded) {
                        loop_warned.insert(kind, turns_so_far);
                    }
                }
                messages.extend(own.into_iter().map(|folded| folded.message));
                (
                    messages,
                    wrap_up_delivered,
                    loop_warned,
                    false,
                    prefix,
                    GuardHistory::Complete,
                )
            }
        };
    // The whole rebuilt conversation, predecessors' carry and own part, must
    // be one a provider accepts. A failed check is a typed refusal, not a
    // request that would be sent and rejected.
    // Calls unanswered at the very end are tolerated only when the record
    // accounts for them: a pending tool batch (still to run), calls a
    // successful `directive_return` abandoned, or calls a failed one still
    // owes a refusal. Any other unanswered call is a record defect and is
    // refused.
    let tolerated_open: Vec<String> = recovered_tools
        .pending
        .iter()
        .chain(recovered_tools.owed.iter())
        .map(|occurrence| occurrence.call.id.clone().unwrap_or_default())
        .chain(recovered_tools.abandoned_call_ids.iter().cloned())
        .collect();
    check_well_formed(&messages, &tolerated_open)?;

    Ok(ResumeState {
        messages,
        pending_tools: recovered_tools.pending,
        owed_siblings: recovered_tools.owed,
        disposition,
        last_turn_coordinate: turn_progress.last_coordinate,
        active_provider_turn,
        turns_completed: turn_progress.completed,
        started_tool_occurrences: recovered_tools.started,
        thread_usage,
        result_guard,
        provider_retry_testimony_digests,
        trailing_length_turns,
        trailing_malformed_replies,
        trailing_empty_replies,
        last_final_stopped,
        frame,
        wrap_up_delivered,
        loop_warned,
        guard_prefix,
        guard_history,
        images: recover_image_run_from(&events, &seed)?,
        tool_starts_before_window: seed.tool_starts,
        spawns_before_window: seed.spawns,
        compaction: anchor
            .as_ref()
            .map(|anchor| CompactionMemo::of(&anchor.payload)),
        active_summary_trigger,
        refused_code,
        chain_depth,
        start_over_ceiling,
    })
}

/// The events a recovery reads, in path order, each with the thread that
/// recorded it, and the compaction the replay started from, if the path holds
/// one.
struct ReplayWindow {
    events: Vec<ReplayedEventRecord>,
    source_thread_ids: Vec<String>,
    anchor: Option<AnchorRecord>,
}

/// The latest compaction of a path.
#[derive(Debug, Clone)]
pub(crate) struct AnchorRecord {
    pub chain_seq: i64,
    pub payload: ryeos_runtime::CompactionPayload,
}

/// Read the path's events. Without a compaction that is every recovery event
/// of every thread. With one it is the frames recorded before the window, then
/// every recovery event from the first kept turn's marker on: what came
/// before is in the compaction.
async fn replay_window(callback: &CallbackClient, path: &[String]) -> Result<ReplayWindow> {
    let located = locate_window(callback, path).await?;
    let (first_thread, after_seq, anchor) = match located {
        Some((first_thread, marker_seq, anchor)) => {
            (first_thread, Some(marker_seq - 1), Some(anchor))
        }
        None => (0, None, None),
    };
    let mut events: Vec<ReplayedEventRecord> = Vec::new();
    let mut source_thread_ids = Vec::new();
    let mut replay_budget = RuntimeReplayBudget::default();
    if let Some(after_seq) = after_seq {
        // The frame in force may have been recorded before the window opens.
        for (index, thread_id) in path.iter().enumerate().take(first_thread + 1) {
            let page = callback
                .replay_thread_with_budget(
                    thread_id,
                    Some(&["cognition_frame"]),
                    &mut replay_budget,
                )
                .await?;
            for event in page.events.into_iter().filter(|event| {
                event.event_type == "cognition_frame"
                    && (index < first_thread || event.chain_seq <= after_seq)
            }) {
                source_thread_ids.push(thread_id.clone());
                events.push(event);
            }
        }
    }
    for (index, thread_id) in path.iter().enumerate().skip(first_thread) {
        let page = match after_seq.filter(|_| index == first_thread) {
            Some(after_seq) => {
                callback
                    .replay_thread_after(
                        thread_id,
                        Some(&RECOVERY_EVENT_TYPES),
                        after_seq,
                        &mut replay_budget,
                    )
                    .await?
            }
            None => {
                callback
                    .replay_thread_with_budget(
                        thread_id,
                        Some(&RECOVERY_EVENT_TYPES),
                        &mut replay_budget,
                    )
                    .await?
            }
        };
        source_thread_ids.extend(std::iter::repeat_n(thread_id.clone(), page.events.len()));
        events.extend(page.events);
    }
    Ok(ReplayWindow {
        events,
        source_thread_ids,
        anchor,
    })
}

/// Find the path's latest compaction and where its replay window opens: the
/// thread index and chain sequence of the turn marker of its first kept turn.
async fn locate_window(
    callback: &CallbackClient,
    path: &[String],
) -> Result<Option<(usize, i64, AnchorRecord)>> {
    let mut found = None;
    for (index, thread_id) in path.iter().enumerate().rev() {
        let page = callback
            .replay_thread_with_budget(
                thread_id,
                Some(&["cognition_compaction"]),
                &mut RuntimeReplayBudget::default(),
            )
            .await?;
        if let Some(event) = page
            .events
            .iter()
            .rev()
            .find(|event| event.event_type == "cognition_compaction")
        {
            let payload: ryeos_runtime::CompactionPayload =
                serde_json::from_value(event.payload.clone())
                    .context("resume: cognition_compaction is not a recorded compaction")?;
            payload
                .validate()
                .context("resume: invalid cognition_compaction")?;
            found = Some((
                index,
                AnchorRecord {
                    chain_seq: event.chain_seq,
                    payload,
                },
            ));
            break;
        }
    }
    let Some((anchor_thread, anchor)) = found else {
        return Ok(None);
    };
    let kept = anchor.payload.kept_from_turn;
    // Every `cognition_in` of a thread (turn markers and inputs) is read to
    // find the first kept turn's marker; the first kept turn is recent, so the
    // search stops in the anchor's thread or the one before it. A thread with
    // more of them than the replay ceiling allows fails typed
    // (`replay_ceiling_exceeded`) before its window is read.
    for index in (0..=anchor_thread).rev() {
        let page = callback
            .replay_thread_with_budget(
                &path[index],
                Some(&["cognition_in"]),
                &mut RuntimeReplayBudget::default(),
            )
            .await?;
        if let Some(marker) = page.events.iter().rev().find(|event| {
            event.event_type == "cognition_in"
                && event.chain_seq < anchor.chain_seq
                && turn_start_coordinate(event).ok().flatten() == Some(kept)
        }) {
            return Ok(Some((index, marker.chain_seq, anchor)));
        }
    }
    anyhow::bail!(
        "resume: the latest compaction keeps turn {kept} but no turn marker for it is on the path"
    )
}

/// The whole conversation of a running thread as its record holds it, for
/// choosing a compaction's cut. The predecessors are not trimmed here: the
/// compaction summarises what the record has, not what a successor carried.
pub(crate) struct CompactionBasis {
    pub folded: Vec<FoldedMessage>,
    events: Vec<ReplayedEventRecord>,
    source_thread_ids: Vec<String>,
    seed: ryeos_runtime::WindowSeed,
    anchor: Option<AnchorRecord>,
    /// Completed model turns on the path.
    pub completed_turns: u32,
}

impl CompactionBasis {
    /// The latest compaction already on the path.
    pub fn anchor(&self) -> Option<&ryeos_runtime::CompactionPayload> {
        self.anchor.as_ref().map(|anchor| &anchor.payload)
    }

    /// What a compaction keeping turn `kept_turn` first must remember of the run
    /// before that turn's marker: the replay window will open there.
    pub fn seed_before_turn(&self, kept_turn: u32) -> Result<ryeos_runtime::WindowSeed> {
        let marker = self
            .events
            .iter()
            .rposition(|event| turn_start_coordinate(event).ok().flatten() == Some(kept_turn))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "compaction: no turn marker for kept turn {kept_turn} in the window"
                )
            })?;
        let prefix = &self.events[..marker];
        let sources = &self.source_thread_ids[..marker];
        let progress = recover_turn_progress_from(prefix, &self.seed)?;
        let tools = recover_tool_state(prefix, sources)?;
        let tool_starts = u32::try_from(tools.started.len())
            .ok()
            .and_then(|starts| starts.checked_add(self.seed.tool_starts))
            .ok_or_else(|| anyhow::anyhow!("compaction: tool-start count exceeds u32"))?;
        let mut spawns = self.seed.spawns;
        for event in prefix
            .iter()
            .filter(|event| event.event_type == "thread_usage")
        {
            if let Some(used) = event
                .payload
                .get("spawns_used")
                .and_then(Value::as_u64)
                .and_then(|used| u32::try_from(used).ok())
            {
                spawns = used;
            }
        }
        let images = recover_image_run_from(prefix, &self.seed)?;
        Ok(ryeos_runtime::WindowSeed {
            last_coordinate: progress.last_coordinate,
            completed: progress.completed,
            tool_starts,
            spawns,
            images: images.images,
            image_bytes: images.bytes,
            image_stored: images.stored.into_iter().collect(),
        })
    }
}

pub(crate) async fn load_compaction_basis(
    callback: &CallbackClient,
    thread_id: &str,
) -> Result<CompactionBasis> {
    let path = continuation_path(callback, thread_id).await?;
    let ReplayWindow {
        events,
        source_thread_ids,
        anchor,
    } = replay_window(callback, &path).await?;
    CompactionBasis::from_window(events, source_thread_ids, anchor)
}

impl CompactionBasis {
    fn from_window(
        events: Vec<ReplayedEventRecord>,
        source_thread_ids: Vec<String>,
        anchor: Option<AnchorRecord>,
    ) -> Result<Self> {
        refuse_markers_without_purpose(&events)?;
        let seed = anchor
            .as_ref()
            .map(|anchor| anchor.payload.seed.clone())
            .unwrap_or_default();
        let progress = recover_turn_progress_from(&events, &seed)?;
        let folded =
            fold_messages_anchored(&events, anchor.as_ref().map(|anchor| anchor.chain_seq))?;
        Ok(Self {
            folded,
            events,
            source_thread_ids,
            seed,
            anchor,
            completed_turns: progress.completed,
        })
    }

    /// The messages the rebuild after a compaction keeping `kept_turn` first
    /// reads for the kept turns, before any clipping: the raw events from that
    /// turn's marker on. A compaction's size estimate is taken over these, so it
    /// agrees with the bytes the fold produces.
    pub fn raw_kept_messages(&self, kept_turn: u32) -> Result<Vec<ProviderMessage>> {
        let marker = self
            .events
            .iter()
            .rposition(|event| turn_start_coordinate(event).ok().flatten() == Some(kept_turn))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "compaction: no turn marker for kept turn {kept_turn} in the window"
                )
            })?;
        Ok(fold_messages_anchored(&self.events[marker..], None)?
            .into_iter()
            .map(|folded| folded.message)
            .collect())
    }
}

/// Consecutive machine continuation edges that end at the path's last thread,
/// newest first. An edge into a thread that began with an operator input (a
/// follow-up) is not one, and ends the count: the daemon exempts those edges
/// from the chain depth limit, and so does this. A live steer inside a thread
/// also carries the operator origin, so it can only make the count shorter.
async fn count_machine_edges(callback: &CallbackClient, path: &[String]) -> Result<u32> {
    let mut count = 0u32;
    for thread_id in path.iter().skip(1).rev() {
        let page = callback
            .replay_thread_with_budget(
                thread_id,
                Some(&["cognition_in"]),
                &mut RuntimeReplayBudget::default(),
            )
            .await?;
        let operator_input = page.events.iter().any(|event| {
            event.event_type == "cognition_in"
                && event.payload.get("origin").and_then(Value::as_str) == Some("operator")
        });
        if !machine_edge(operator_input) {
            break;
        }
        count = count.saturating_add(1);
    }
    Ok(count)
}

/// Whether the edge into a thread is a machine continuation, given whether the
/// thread opened with an operator input.
fn machine_edge(operator_input: bool) -> bool {
    !operator_input
}

/// What the whole path has stored of tool images: how many were admitted and
/// how many distinct bytes they hold, and which attachments the node therefore
/// already stores. Seeds the run's limits and keeps a repeated image from
/// being uploaded again.
fn recover_image_run_from(
    events: &[ReplayedEventRecord],
    seed: &ryeos_runtime::WindowSeed,
) -> Result<ImageRunState> {
    let mut run = ImageRunState {
        images: seed.images,
        bytes: seed.image_bytes,
        stored: seed.image_stored.iter().cloned().collect(),
    };
    for event in events
        .iter()
        .filter(|event| event.event_type == "tool_call_result")
    {
        for part in replayed_tool_result(event)?.parts {
            if let ryeos_provider_contract::Part::Image(image) = part {
                run.record(&image);
            }
        }
    }
    Ok(run)
}

/// Which wrap-up reasons a conversation's wrap-up inputs have given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WrapUpDelivered {
    pub deadline: bool,
    pub turn_limit: bool,
}

impl WrapUpDelivered {
    pub(crate) fn union(self, other: Self) -> Self {
        Self {
            deadline: self.deadline || other.deadline,
            turn_limit: self.turn_limit || other.turn_limit,
        }
    }
}

/// The wrap-up reasons already given and the loop findings already warned
/// about in a compacted conversation, each warning positioned by the assistant
/// turns before it in the conversation the model is given.
fn compacted_signals(folded: &[FoldedMessage]) -> (WrapUpDelivered, crate::loop_guards::WarnedAt) {
    let delivered = folded
        .iter()
        .filter_map(wrap_up_notice_reasons)
        .fold(WrapUpDelivered::default(), WrapUpDelivered::union);
    let mut warned = std::collections::BTreeMap::new();
    let mut turns_so_far = 0u32;
    for message in folded {
        if message.message.role == ryeos_provider_contract::Role::Assistant {
            turns_so_far += 1;
        }
        if let Some(kind) = loop_warning_kind(message) {
            warned.insert(kind, turns_so_far);
        }
    }
    (delivered, warned)
}

/// How much of the history the loop guards are configured to read the path
/// still holds, as the record it was rebuilt from says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardHistory {
    /// No compaction on the path: every turn is in the conversation or the
    /// carried predecessor digests.
    Complete,
    /// A compaction record carried this much history (`carry_len` turns).
    Carried { carry_len: u32 },
    /// A compaction record written with the loop guards off: no history was
    /// carried, so the turns it replaced were not measured.
    NotRecorded,
}

impl GuardHistory {
    /// When the loop-guard settings now in force want more history than the
    /// record carried (or any when it carried none), the history the detectors
    /// read is shorter than configured: a quiet detector does not mean a clean
    /// run. Returns the turns the record carried.
    pub fn shortfall(self, config: &crate::directive::LoopGuardsConfig) -> Option<u32> {
        if !config.enabled {
            return None;
        }
        let wanted = crate::loop_guards::carry_len(config);
        match self {
            Self::Complete => None,
            Self::Carried { carry_len } => (carry_len < wanted).then_some(carry_len),
            Self::NotRecorded => Some(0),
        }
    }
}

/// What a rebuilt compacted conversation tells the guards.
struct AnchoredSignals {
    delivered: WrapUpDelivered,
    warned: crate::loop_guards::WarnedAt,
    prefix: Vec<crate::loop_guards::TurnDigest>,
    history: GuardHistory,
}

/// The guard state of a conversation rebuilt from a compaction.
///
/// A compaction record carries the guard state the replaced turns held: the
/// recent digests (the prefix the detectors read before the conversation), the
/// wrap-up flags and each warning's age. A same-thread recovery takes the
/// union of that with what the compacted conversation itself still holds
/// (inputs recorded since, and notices a predecessor gave that the
/// conversation carries). A successor is a new conversation: it keeps the
/// prefix, but counts a notice or warning only if the conversation it was
/// given still holds it. `kept_at_record` is the turns the record kept whole.
fn anchored_signals(
    mode: ResumeMode,
    carry: Option<&ryeos_runtime::GuardCarry>,
    kept_at_record: u32,
    folded: &[FoldedMessage],
) -> AnchoredSignals {
    let prefix = carry
        .map(|carry| carry.recent_digests.clone())
        .unwrap_or_default();
    let (mut delivered, mut warned) = compacted_signals(folded);
    // Positions in the conversation count after the prefix.
    let offset = u32::try_from(prefix.len()).unwrap_or(u32::MAX);
    for position in warned.values_mut() {
        *position = position.saturating_add(offset);
    }
    if mode == ResumeMode::NativeSameThread
        && let Some(carry) = carry
    {
        delivered = delivered.union(WrapUpDelivered {
            deadline: carry.wrap_up.deadline,
            turn_limit: carry.wrap_up.turn_limit,
        });
        for (kind, position) in crate::loop_guards::warned_from_carry(carry, kept_at_record) {
            // A finding in both keeps the earlier position: the earlier stop.
            warned
                .entry(kind)
                .and_modify(|existing| *existing = (*existing).min(position))
                .or_insert(position);
        }
    }
    let history = match carry {
        Some(carry) => GuardHistory::Carried {
            carry_len: carry.carry_len,
        },
        None => GuardHistory::NotRecorded,
    };
    AnchoredSignals {
        delivered,
        warned,
        prefix,
        history,
    }
}

/// The turns the window holds before the compaction event at `anchor_seq`: the
/// kept turns the record kept whole. Counts the same finals the fold turns into
/// assistant messages, so a reply a correction removed is not a turn: the live
/// run popped it before it measured how long ago a warning was given.
fn turns_before_event(events: &[ReplayedEventRecord], anchor_seq: i64) -> Result<u32> {
    let corrected = corrected_replies(events)?;
    let end = events
        .iter()
        .position(|event| {
            event.event_type == "cognition_compaction" && event.chain_seq == anchor_seq
        })
        .unwrap_or(events.len());
    let turns = events[..end]
        .iter()
        .enumerate()
        .filter(|(index, event)| {
            event.event_type == "cognition_out"
                && !is_stream_fragment(event)
                && !is_compaction_summary_turn(event)
                && !is_refusal_final(event)
                && !corrected.contains(index)
        })
        .count();
    Ok(u32::try_from(turns).unwrap_or(u32::MAX))
}

/// The history before a successor's conversation: the predecessors' turns its
/// carried start left out, newest `GuardCarry::MAX_DIGESTS` at most.
/// `carried_turns` is how many of the predecessors' last turns the start holds.
fn successor_prefix(
    predecessor: &[crate::loop_guards::TurnDigest],
    carried_turns: usize,
) -> Vec<crate::loop_guards::TurnDigest> {
    let left_out = predecessor.len().saturating_sub(carried_turns);
    let start = left_out.saturating_sub(ryeos_runtime::GuardCarry::MAX_DIGESTS as usize);
    predecessor[start..left_out].to_vec()
}

/// The reasons a folded message gave, when it is a wrap-up notice.
fn wrap_up_notice_reasons(folded: &FoldedMessage) -> Option<WrapUpDelivered> {
    let (deadline, turn_limit) =
        ryeos_runtime::cognition::wrap_up_reasons(folded.kind.as_deref()?)?;
    Some(WrapUpDelivered {
        deadline,
        turn_limit,
    })
}

/// The finding a folded message warned about, when it is a loop warning.
fn loop_warning_kind(folded: &FoldedMessage) -> Option<crate::loop_guards::FindingKind> {
    let kind = folded.kind.as_deref()?;
    crate::loop_guards::FindingKind::from_code(
        kind.strip_prefix(ryeos_runtime::cognition::KIND_LOOP_WARNING_PREFIX)?,
    )
}

/// Count the final turns, newest first, that ended at the output limit,
/// stopping at the first final turn that did not. Derived from the record so
/// resume needs no separate counter.
fn trailing_length_turns(events: &[ReplayedEventRecord]) -> Result<u32> {
    let mut count = 0u32;
    for event in events.iter().rev() {
        // A compaction's summary call is not a turn of the conversation.
        if final_cognition_turn(event)?.is_none()
            || is_compaction_summary_turn(event)
            || is_refusal_final(event)
        {
            continue;
        }
        if event.payload.get("finish").and_then(Value::as_str) != Some("length") {
            break;
        }
        count = count.saturating_add(1);
    }
    Ok(count)
}

/// The correction, turn and run position a `cognition_in` event's kind names,
/// when the event is the correction of a reply.
fn reply_correction_input(
    event: &ReplayedEventRecord,
) -> Option<(ryeos_runtime::cognition::ReplyCorrection, u32, u32)> {
    if event.event_type != "cognition_in" {
        return None;
    }
    ryeos_runtime::cognition::reply_correction_of_kind(
        event.payload.get("kind").and_then(Value::as_str)?,
    )
}

/// Whether a final turn is an empty reply the empty-reply correction applies
/// to: a normal stop finish with nothing to act on.
fn final_is_correctable_empty(event: &ReplayedEventRecord) -> Result<bool> {
    if event.payload.get("finish").and_then(Value::as_str) != Some("stop") {
        return Ok(false);
    }
    Ok(crate::directive::is_empty_reply(
        &replayed_cognition_out(event)?.message(),
    ))
}

/// The run position of the newest correction of `which`, while the run has not
/// moved past it. At most one final turn may follow it, and only one that
/// proposed no tool call and is the kind of reply `which` corrects: that turn
/// is the reply the run was about to judge, so the correction still counts.
/// Any later turn means the run went on and the run starts over. Derived from
/// the record so resume needs no separate counter.
fn trailing_corrections(
    events: &[ReplayedEventRecord],
    which: ryeos_runtime::cognition::ReplyCorrection,
) -> Result<u32> {
    use ryeos_runtime::cognition::ReplyCorrection;
    let mut finals_after = 0u32;
    let mut calls_after = false;
    let mut shape_ok = true;
    for event in events.iter().rev() {
        if let Some((kind, _, position)) = reply_correction_input(event)
            && kind == which
        {
            let still_counts = finals_after == 0 || (finals_after == 1 && !calls_after && shape_ok);
            return Ok(if still_counts { position } else { 0 });
        }
        // A compaction's summary call and a recorded refusal are not replies
        // the run judged: a compaction landing after a correction leaves the
        // run where it was.
        if final_cognition_turn(event)?.is_some()
            && !is_compaction_summary_turn(event)
            && !is_refusal_final(event)
        {
            finals_after = finals_after.saturating_add(1);
            calls_after |= replayed_cognition_out(event)?
                .message()
                .tool_calls()
                .next()
                .is_some();
            let empty = final_is_correctable_empty(event)?;
            // An empty reply is never malformed, and a malformed reply is
            // never empty, so the reply after a correction must be of the
            // corrected shape for its run to continue.
            shape_ok = match which {
                ReplyCorrection::Empty => empty,
                ReplyCorrection::Malformed => !empty,
            };
        }
    }
    Ok(0)
}

/// Whether the newest final turn ended at a normal stop finish.
fn last_final_stopped(events: &[ReplayedEventRecord]) -> Result<bool> {
    for event in events.iter().rev() {
        if final_cognition_turn(event)?.is_some() {
            return Ok(event.payload.get("finish").and_then(Value::as_str) == Some("stop"));
        }
    }
    Ok(false)
}

fn trailing_malformed_replies(events: &[ReplayedEventRecord]) -> Result<u32> {
    trailing_corrections(events, ryeos_runtime::cognition::ReplyCorrection::Malformed)
}

fn trailing_empty_replies(events: &[ReplayedEventRecord]) -> Result<u32> {
    trailing_corrections(events, ryeos_runtime::cognition::ReplyCorrection::Empty)
}

/// The positions of the final `cognition_out` events that a later reply
/// correction names. The live run removed such a reply from the conversation,
/// so a fold must not include it. A correction names its turn, and only the
/// latest final turn before it can be that turn; an empty-reply correction
/// also removes only a reply that is an empty normal stop, so a record that
/// names any other reply leaves it in.
fn corrected_replies(events: &[ReplayedEventRecord]) -> Result<std::collections::HashSet<usize>> {
    use ryeos_runtime::cognition::ReplyCorrection;
    let mut corrected = std::collections::HashSet::new();
    let mut latest_final: Option<(usize, u32)> = None;
    for (index, event) in events.iter().enumerate() {
        if let Some(turn) = final_cognition_turn(event)? {
            latest_final = Some((index, turn));
        } else if let Some((kind, named, _)) = reply_correction_input(event)
            && let Some((final_index, turn)) = latest_final
            && turn == named
            && (kind == ReplyCorrection::Malformed
                || final_is_correctable_empty(&events[final_index])?)
        {
            corrected.insert(final_index);
        }
    }
    Ok(corrected)
}

/// Parse a replayed `tool_call_result` into its typed payload. A payload in
/// any other shape is refused; there is no reader for an earlier shape.
fn replayed_tool_result(event: &ReplayedEventRecord) -> Result<ToolResultPayload> {
    let result: ToolResultPayload = serde_json::from_value(event.payload.clone())
        .context("resume: tool_call_result is not a recorded tool result")?;
    result
        .validate()
        .context("resume: invalid tool_call_result")?;
    let text_bytes: usize = result.text().len();
    if text_bytes > ryeos_runtime::callback_client::TOOL_RESULT_INLINE_MAX_BYTES {
        anyhow::bail!("resume: tool_call_result text exceeds the runtime event bound");
    }
    Ok(result)
}

/// Parse a replayed final `cognition_out` into its typed payload.
fn replayed_cognition_out(event: &ReplayedEventRecord) -> Result<CognitionOutPayload> {
    let payload: CognitionOutPayload = serde_json::from_value(event.payload.clone())
        .context("resume: cognition_out is not a recorded turn")?;
    payload
        .validate()
        .context("resume: invalid cognition_out")?;
    Ok(payload)
}

/// Whether a final `cognition_out` is a compaction's summary call.
/// Whether a final `cognition_out` records the refusal of a request the
/// compaction policy answers by continuing.
fn is_refusal_final(event: &ReplayedEventRecord) -> bool {
    refusal_code_of(event).is_some()
}

fn refusal_code_of(event: &ReplayedEventRecord) -> Option<&str> {
    if event.event_type != "cognition_out" {
        return None;
    }
    event
        .payload
        .get("finish")
        .and_then(Value::as_str)
        .and_then(ryeos_runtime::cognition::provider_refusal_code)
}

/// The refusal the path's latest provider turn or compaction recorded, when
/// the latest is a refusal: nothing has completed or compacted since.
fn recover_refused_code(events: &[ReplayedEventRecord]) -> Option<String> {
    for event in events.iter().rev() {
        if event.event_type == "cognition_compaction" {
            return None;
        }
        if final_cognition_turn(event).ok().flatten().is_none() {
            continue;
        }
        return refusal_code_of(event).map(str::to_string);
    }
    None
}

/// A turn marker recorded before markers carried their purpose is refused: no
/// reader parses the old shape.
fn refuse_markers_without_purpose(events: &[ReplayedEventRecord]) -> Result<()> {
    for event in events {
        if event.event_type == "cognition_in"
            && event.payload.get("turn").is_some()
            && event.payload.get("purpose").is_none()
        {
            return Err(ResumeFoldError::MarkerWithoutPurpose.into());
        }
    }
    Ok(())
}

fn is_compaction_summary_turn(event: &ReplayedEventRecord) -> bool {
    event
        .payload
        .get("finish")
        .and_then(Value::as_str)
        .is_some_and(|finish| {
            ryeos_runtime::cognition::compaction_summary_trigger(finish).is_some()
        })
}

/// Whether a final `cognition_out` records a turn cut short by an interrupt.
fn is_interrupted_turn(event: &ReplayedEventRecord) -> bool {
    event.payload.get("finish").and_then(Value::as_str) == Some(FINISH_INTERRUPTED)
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
        let result = replayed_tool_result(event)?;
        if result.tool == DIRECTIVE_RETURN_TOOL || result.status == ToolResultStatus::Error {
            if result.guard.duplicate_of.is_some() {
                anyhow::bail!("resume: unguarded tool result claims duplicate metadata");
            }
            continue;
        }
        // The notes for dropped images follow the guard's text and were never
        // part of what it hashed.
        let text = result.text();
        let dropped: Vec<(usize, &str)> = result
            .guard
            .dropped_images
            .iter()
            .map(|image| (image.position as usize, image.code.as_str()))
            .collect();
        let guarded = if dropped.is_empty() {
            text.as_str()
        } else {
            crate::tool_images::strip_drop_notes(&text, &dropped).ok_or_else(|| {
                anyhow::anyhow!(
                    "resume: tool result text does not end with its dropped-image notes"
                )
            })?
        };
        guard
            .restore_result(guarded, result.guard.duplicate_of.as_deref())
            .map_err(|error| anyhow::anyhow!("resume: invalid result-guard evidence: {error}"))?;
    }
    Ok(guard)
}

#[derive(Debug)]
struct RecoveredToolState {
    pending: Vec<ToolOccurrence>,
    /// Call ids of calls a successful lifecycle call abandoned: they were
    /// never started and never get a result, by design. A successful return is
    /// terminal, so nothing follows them.
    abandoned_call_ids: Vec<String>,
    /// Calls after a failed lifecycle call in its batch that have no result
    /// yet. The run answers each with a refusal and never runs it.
    owed: Vec<ToolOccurrence>,
    owed_source_thread_id: Option<String>,
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
    /// Settled by a successful `directive_return` ahead of it: never started,
    /// never answered.
    abandoned: bool,
    /// Follows a failed `directive_return` in its batch: owed a refusal result.
    owed: bool,
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
            return Err(ResumeFoldError::UnsettledToolBatch {
                detail: "cognition input appeared before every retained tool occurrence settled"
                    .to_string(),
            }
            .into());
        }
        match event.event_type.as_str() {
            "cognition_out"
                if event.payload.get("delta").is_some()
                    || event.payload.get("tool_use").is_some()
                    || event.payload.get("tool_use_partial").is_some() => {}
            "cognition_out" => {
                if open.iter().any(|entry| !entry.settled && !entry.abandoned) {
                    return Err(ResumeFoldError::UnsettledToolBatch { detail: "a new cognition_out appeared before every prior tool occurrence settled".to_string() }.into());
                }
                current_batch = None;
                let cognition = replayed_cognition_out(event)?;
                let tool_calls = cognition.message().calls();
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
                            turn,
                            call_index,
                            start_recorded: false,
                            counted: false,
                            spawn_counted: false,
                            malformed: cognition.malformed_call(call_index).cloned(),
                        },
                        source_thread_id: source_thread_id.clone(),
                        settled: false,
                        abandoned: false,
                        owed: false,
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
                validate_replayed_tool_event(event, &open[occurrence_index].occurrence.call)?;
                open[occurrence_index].occurrence.start_recorded = true;
                // The lifecycle return, the in-process time-left tool and a
                // refusal owed after a failed return take no tool-call budget,
                // so their starts never count against it.
                if !open[occurrence_index].owed
                    && open[occurrence_index].occurrence.call.name != DIRECTIVE_RETURN_TOOL
                    && open[occurrence_index].occurrence.call.name != TIME_LEFT_TOOL
                {
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
                validate_replayed_tool_event(event, &open[occurrence_index].occurrence.call)?;
                if open[occurrence_index].owed
                    && !is_failed_return_refusal(&replayed_tool_result(event)?)
                {
                    anyhow::bail!(
                        "resume: tool_call_result for operation {operation_id} after a failed directive_return is not the {TOOL_NOT_RUN_AFTER_FAILED_RETURN} refusal"
                    );
                }
                // An owed `directive_return` (a second return after a failed
                // one) is an ordinary refusal, not a lifecycle settlement.
                if open[occurrence_index].occurrence.call.name == DIRECTIVE_RETURN_TOOL
                    && !open[occurrence_index].owed
                {
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
                    // A successful return is terminal: the calls after it are
                    // abandoned and nothing follows. A failed one is not, so
                    // the calls after it are each owed a refusal.
                    let succeeded =
                        matches!(settlement, RecoveredLifecycleSettlement::Succeeded { .. });
                    if succeeded {
                        terminal_lifecycle_settlement = Some(settlement.clone());
                    }
                    batch.lifecycle_settlement = Some(settlement);
                    for entry in &mut open[occurrence_index + 1..] {
                        if succeeded {
                            entry.abandoned = true;
                        } else {
                            entry.owed = true;
                        }
                    }
                }
                open[occurrence_index].settled = true;
            }
            _ => {}
        }
    }

    let abandoned_call_ids: Vec<String> = open
        .iter()
        .filter(|entry| !entry.settled && entry.abandoned)
        .map(|entry| entry.occurrence.call.id.clone().unwrap_or_default())
        .collect();
    let pending_source_thread_id = open
        .iter()
        .find(|entry| !entry.settled && !entry.abandoned && !entry.owed)
        .map(|entry| entry.source_thread_id.clone());
    let owed_source_thread_id = open
        .iter()
        .find(|entry| !entry.settled && entry.owed)
        .map(|entry| entry.source_thread_id.clone());
    Ok(RecoveredToolState {
        pending: open
            .iter()
            .filter(|entry| !entry.settled && !entry.abandoned && !entry.owed)
            .map(|entry| entry.occurrence.clone())
            .collect(),
        owed: open
            .iter()
            .filter(|entry| !entry.settled && entry.owed)
            .map(|entry| entry.occurrence.clone())
            .collect(),
        owed_source_thread_id,
        abandoned_call_ids,
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
    let result = replayed_tool_result(event)?;
    match result.status {
        ToolResultStatus::Error => Ok(RecoveredLifecycleSettlement::Failed { event_index }),
        ToolResultStatus::Ok => {
            if result.guard.truncated {
                anyhow::bail!("resume: successful directive_return result is truncated");
            }
            let outputs: Value = serde_json::from_str(&result.text()).context(
                "resume: successful directive_return result text is not exact JSON outputs",
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
) -> Result<()> {
    let observed_tool = event.payload.get("tool").and_then(Value::as_str);
    if observed_tool != Some(call.name.as_str()) {
        anyhow::bail!(
            "resume: {} tool identity does not match its cognition proposal",
            event.event_type
        );
    }
    let expected_call_id = call.id.as_deref();
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
        for final_key in ["parts", "finish", "usage", "provider_accounting"] {
            if event.payload.get(final_key).is_some() {
                anyhow::bail!(
                    "resume: streaming cognition_out also carries final field {final_key:?}"
                );
            }
        }
        return Ok(());
    }
    replayed_cognition_out(event)?;
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
#[cfg(test)]
fn recover_turn_progress(events: &[ReplayedEventRecord]) -> Result<RecoveredTurnProgress> {
    recover_turn_progress_from(events, &ryeos_runtime::WindowSeed::default())
}

/// [`recover_turn_progress`] for a replay window that opens after the seed's
/// progress. A compaction's summary call takes a coordinate but is not a
/// completed model turn.
fn recover_turn_progress_from(
    events: &[ReplayedEventRecord],
    seed: &ryeos_runtime::WindowSeed,
) -> Result<RecoveredTurnProgress> {
    let mut progress = RecoveredTurnProgress {
        last_coordinate: seed.last_coordinate,
        completed: seed.completed,
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
        if is_interrupted_turn(event)
            || is_compaction_summary_turn(event)
            || is_refusal_final(event)
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

#[cfg(test)]
fn recover_active_provider_turn(
    events: &[ReplayedEventRecord],
    source_thread_ids: &[String],
    current_thread_id: &str,
    mode: ResumeMode,
) -> Result<Option<u32>> {
    let unfinished =
        recover_unfinished_provider_turn(events, source_thread_ids, current_thread_id, 0)?;
    Ok((mode == ResumeMode::NativeSameThread)
        .then_some(unfinished)
        .flatten()
        .map(|turn| turn.turn))
}

/// The turn coordinate the path's last thread started and never completed.
/// `last_final_seed` is the last completed coordinate before the replay window.
/// For the thread of this process generation it is a turn in flight; for a
/// settled predecessor it is a turn whose request was refused.
fn recover_unfinished_provider_turn(
    events: &[ReplayedEventRecord],
    source_thread_ids: &[String],
    current_thread_id: &str,
    last_final_seed: u32,
) -> Result<Option<UnfinishedTurn>> {
    if events.len() != source_thread_ids.len() {
        anyhow::bail!("resume: replay source/event cardinality mismatch");
    }
    let mut last_final = last_final_seed;
    let mut current_active: Option<u32> = None;
    let mut current_purpose = ryeos_runtime::cognition::TurnPurpose::Turn;
    for (event, source_thread_id) in events.iter().zip(source_thread_ids) {
        if source_thread_id == current_thread_id
            && let Some(turn) = turn_start_coordinate(event)?
        {
            current_purpose = turn_marker_purpose(event)?;
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
    Ok(current_active.map(|turn| UnfinishedTurn {
        turn,
        purpose: current_purpose,
    }))
}

/// A turn that started and has no final yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnfinishedTurn {
    turn: u32,
    purpose: ryeos_runtime::cognition::TurnPurpose,
}

/// The purpose a turn marker records; a marker without it is refused.
fn turn_marker_purpose(
    event: &ReplayedEventRecord,
) -> Result<ryeos_runtime::cognition::TurnPurpose> {
    let marker: ryeos_runtime::TurnMarker = match serde_json::from_value(event.payload.clone()) {
        Ok(marker) => marker,
        Err(_) if event.payload.get("purpose").is_none() => {
            return Err(ResumeFoldError::MarkerWithoutPurpose.into());
        }
        Err(error) => {
            return Err(
                anyhow::Error::new(error).context("resume: invalid cognition_in turn marker")
            );
        }
    };
    Ok(marker.purpose)
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
        if event.payload.get("supersedes")
            != Some(&crate::runner::retry_supersedes(&advance.decision))
        {
            anyhow::bail!("resume: provider_retry supersedes contradicts its exact advancement");
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

    if is_refusal_final(last_cognition) {
        return Ok(ResumeDisposition::ForkAfterRefusal);
    }
    if is_compaction_summary_turn(last_cognition)
        && !events[last_cognition_index + 1..]
            .iter()
            .any(|event| event.event_type == "cognition_compaction")
    {
        let out = replayed_cognition_out(last_cognition)?;
        let trigger = ryeos_runtime::cognition::compaction_summary_trigger(&out.finish)
            .unwrap_or_default()
            .to_string();
        return Ok(ResumeDisposition::FinishCompaction {
            trigger,
            summary: out.parts,
        });
    }

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
                if !tools.owed.is_empty() {
                    return Ok(ResumeDisposition::SettleOwedSiblings);
                }
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
            return Err(ResumeFoldError::UnsettledToolBatch {
                detail: "cognition input appeared before the retained tool proposal settled"
                    .to_string(),
            }
            .into());
        }
        return Ok(ResumeDisposition::ContinueProvider);
    }

    if later_cognition_input {
        return Ok(ResumeDisposition::ContinueProvider);
    }
    if is_interrupted_turn(last_cognition) {
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
        if is_interrupted_turn(event) || is_refusal_final(event) {
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
/// The latest `cognition_frame` on the path. Frames are numbered from 1 and
/// each is the previous number plus one; anything else is refused.
fn latest_frame(events: &[ReplayedEventRecord]) -> Result<Option<CognitionFramePayload>> {
    let mut latest: Option<CognitionFramePayload> = None;
    for event in events
        .iter()
        .filter(|event| event.event_type == "cognition_frame")
    {
        let frame: CognitionFramePayload = serde_json::from_value(event.payload.clone())
            .context("resume: cognition_frame is not a recorded frame")?;
        frame
            .validate()
            .context("resume: invalid cognition_frame")?;
        let expected = latest.as_ref().map_or(1, |previous| previous.frame_seq + 1);
        if frame.frame_seq != expected {
            anyhow::bail!(
                "resume: cognition_frame {} does not follow frame {}",
                frame.frame_seq,
                expected - 1
            );
        }
        latest = Some(frame);
    }
    Ok(latest)
}

/// One message of a fold, with whether the input that produced it is pinned.
#[derive(Debug, Clone)]
pub(crate) struct FoldedMessage {
    pub(crate) message: ProviderMessage,
    pub(crate) pinned: bool,
    /// The kind of the harness input that produced this message, when it is an
    /// input's own user message.
    pub(crate) kind: Option<String>,
    /// The turn coordinate of an assistant turn's message.
    pub(crate) turn: Option<u32>,
    /// The recorded input whose own user message this is.
    pub(crate) input: Option<ryeos_runtime::CognitionInput>,
}

fn fold_messages(events: &[ReplayedEventRecord]) -> Result<Vec<FoldedMessage>> {
    fold_messages_anchored(events, None)
}

/// [`fold_messages`] over a replay window that holds a compaction: at the
/// compaction with chain sequence `anchor_seq` the conversation so far (the
/// kept turns) gets the pinned inputs and the summary in front of it. Other
/// compactions in the window are older and add nothing. A compaction's summary
/// call is not a conversation turn.
pub(crate) fn fold_messages_anchored(
    events: &[ReplayedEventRecord],
    anchor_seq: Option<i64>,
) -> Result<Vec<FoldedMessage>> {
    let mut messages = Vec::new();
    let mut cognition_in = ryeos_runtime::CognitionInAssembler::default();
    let corrected = corrected_replies(events)?;
    let unpinned = |message: ProviderMessage| FoldedMessage {
        message,
        pinned: false,
        kind: None,
        turn: None,
        input: None,
    };

    for (index, event) in events.iter().enumerate() {
        validate_replayed_cognition_out(event)?;
        if cognition_in.has_pending() && event.event_type != "cognition_in" {
            anyhow::bail!(
                "resume: chunked cognition_in was interrupted by event '{}'",
                event.event_type
            );
        }
        match event.event_type.as_str() {
            // An inline or completely reassembled cognition_in is exactly one
            // input. Turn markers and intermediate chunks add no message.
            "cognition_in" => {
                if let ryeos_runtime::CognitionInAssembly::Complete(input) =
                    cognition_in.push(&event.payload)?
                {
                    messages.extend(
                        crate::launch_records::input_messages(&input)
                            .into_iter()
                            .enumerate()
                            .map(|(index, message)| FoldedMessage {
                                message,
                                pinned: input.pinned,
                                // The input's own message comes first; its
                                // acknowledgement follows and has no kind.
                                kind: if index == 0 { input.kind.clone() } else { None },
                                turn: None,
                                input: (index == 0).then(|| input.clone()),
                            }),
                    );
                }
            }
            // Streaming fragments are UI/event-log data, not replayable
            // provider history. The final transcript-bearing cognition_out
            // emitted at turn completion is folded below.
            "cognition_out" if is_stream_fragment(event) => {}
            "cognition_out" if is_compaction_summary_turn(event) || is_refusal_final(event) => {}
            // A reply the run corrected as malformed was removed from the
            // conversation when it was corrected.
            "cognition_out" if corrected.contains(&index) => {}
            "cognition_out" => {
                let out = replayed_cognition_out(event)?;
                messages.push(FoldedMessage {
                    turn: Some(out.turn),
                    ..unpinned(out.message())
                });
            }
            "cognition_compaction" if anchor_seq == Some(event.chain_seq) => {
                let record: ryeos_runtime::CompactionPayload =
                    serde_json::from_value(event.payload.clone())
                        .context("resume: cognition_compaction is not a recorded compaction")?;
                record
                    .validate()
                    .context("resume: invalid cognition_compaction")?;
                let mut prefix = Vec::new();
                for input in &record.pinned {
                    prefix.extend(
                        crate::launch_records::input_messages(input)
                            .into_iter()
                            .enumerate()
                            .map(|(index, message)| FoldedMessage {
                                message,
                                pinned: true,
                                kind: if index == 0 { input.kind.clone() } else { None },
                                turn: None,
                                input: (index == 0).then(|| input.clone()),
                            }),
                    );
                }
                prefix.push(FoldedMessage {
                    kind: Some(ryeos_runtime::cognition::KIND_COMPACTION_SUMMARY.to_string()),
                    ..unpinned(record.summary_message())
                });
                // The kept turns read as the record says: older large tool
                // results clipped, the events themselves untouched.
                if let Some(rule) = &record.clip {
                    let mut kept: Vec<ProviderMessage> = messages
                        .iter()
                        .map(|folded| folded.message.clone())
                        .collect();
                    crate::compaction::clip_tool_results(&mut kept, rule);
                    for (folded, clipped) in messages.iter_mut().zip(kept) {
                        folded.message = clipped;
                    }
                }
                messages.splice(0..0, prefix);
            }
            "tool_call_result" => {
                // The retained text is exactly what the live runner put in the
                // provider message. Never parse JSON-looking content: doing so
                // changes both OpenAI-style wire content and custom schema
                // template rendering after a crash.
                messages.push(unpinned(replayed_tool_result(event)?.message()));
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

#[cfg(test)]
fn reconstruct_messages(events: &[ReplayedEventRecord]) -> Result<Vec<ProviderMessage>> {
    Ok(fold_messages(events)?
        .into_iter()
        .map(|folded| folded.message)
        .collect())
}

/// The default token budget of the conversation a successor starts from.
pub const DEFAULT_SUCCESSOR_START_TOKENS: u64 = 16_000;

/// How a successor's start is cut from its predecessors' part of the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarryPolicy {
    /// Recent turns carried after the pinned inputs.
    pub carry_turns: u32,
    /// Estimated tokens the start (pinned inputs plus recent turns) should
    /// fit. Pinned inputs over this budget are carried alone.
    pub start_tokens: u64,
    /// Estimated tokens above which even the pinned inputs alone are refused:
    /// the model window times the continuation threshold ratio.
    pub ceiling_tokens: u64,
}

impl CarryPolicy {
    /// A policy with the default start budget and no ceiling, for tests that
    /// exercise only the turn count.
    #[cfg(test)]
    fn turns(carry_turns: u32) -> Self {
        Self {
            carry_turns,
            start_tokens: DEFAULT_SUCCESSOR_START_TOKENS,
            ceiling_tokens: u64::MAX,
        }
    }
}

/// A rebuilt conversation that a provider would reject. Each variant has a
/// stable code that is the prefix of its message, which is how the failure
/// reaches the thread's failure record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeFoldError {
    /// The conversation does not begin with a user message.
    DoesNotStartWithUser { role: String },
    /// An assistant tool call has no result before the conversation moves on.
    UnansweredToolCall { call_id: String },
    /// A tool result names a call that is not in the conversation.
    OrphanToolResult { call_id: String },
    /// The pinned inputs alone are larger than the ceiling.
    PinnedInputsExceedWindow { ceiling: u64 },
    /// A compacted conversation is still larger than the ceiling.
    CompactedStartExceedsWindow { ceiling: u64 },
    /// A provider turn marker without its `purpose`: the record was written
    /// before markers carried it, and there is no reader for that shape.
    MarkerWithoutPurpose,
    /// The record moves on while calls of a tool batch have no result.
    /// Records written before a failed `directive_return` answered the calls
    /// after it (they were left with no start and no result) read this way
    /// when the run went on, and a continuation predecessor with calls still
    /// owed a result reads this way too. Such records are refused: there is no
    /// reader for the old shape.
    UnsettledToolBatch { detail: String },
}

impl ResumeFoldError {
    /// The stable failure code of this refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::DoesNotStartWithUser { .. } => "resume_start_not_user",
            Self::UnansweredToolCall { .. } => "resume_unanswered_tool_call",
            Self::OrphanToolResult { .. } => "resume_orphan_tool_result",
            Self::PinnedInputsExceedWindow { .. } => "resume_pinned_exceeds_window",
            Self::CompactedStartExceedsWindow { .. } => "resume_compacted_start_exceeds_window",
            Self::MarkerWithoutPurpose => "resume_marker_without_purpose",
            Self::UnsettledToolBatch { .. } => "resume_unsettled_tool_batch",
        }
    }
}

impl std::fmt::Display for ResumeFoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.code();
        match self {
            Self::DoesNotStartWithUser { role } => write!(
                f,
                "{code}: the rebuilt conversation begins with a {role} message, not a user message"
            ),
            Self::UnansweredToolCall { call_id } => write!(
                f,
                "{code}: tool call {call_id:?} in the rebuilt conversation has no result"
            ),
            Self::OrphanToolResult { call_id } => write!(
                f,
                "{code}: tool result {call_id:?} in the rebuilt conversation has no call"
            ),
            Self::PinnedInputsExceedWindow { ceiling } => write!(
                f,
                "{code}: pinned inputs alone exceed the {ceiling}-token ceiling of the model window"
            ),
            Self::MarkerWithoutPurpose => write!(
                f,
                "{code}: a provider turn marker has no purpose; the record predates the marker's purpose and is not read"
            ),
            Self::CompactedStartExceedsWindow { ceiling } => write!(
                f,
                "{code}: the compacted conversation still exceeds the {ceiling}-token ceiling of the model window"
            ),
            Self::UnsettledToolBatch { detail } => write!(f, "{code}: {detail}"),
        }
    }
}

impl std::error::Error for ResumeFoldError {}

/// The conversation a successor starts from, taken from its predecessors'
/// part of the path: the pinned inputs in their original order, then the most
/// recent `carry_turns` turns and as many of them as fit the start budget.
/// When nothing needs dropping the conversation keeps its original order.
/// Pinned inputs larger than the start budget are carried alone (no recent
/// turns); pinned inputs larger than the ceiling are refused. The result
/// begins with a user message and every tool call in it has its result, or
/// the rebuild fails with a [`ResumeFoldError`].
fn carry_into_successor(
    folded: Vec<FoldedMessage>,
    policy: &CarryPolicy,
    images: Option<&ImageAcceptConfig>,
) -> Result<Vec<ProviderMessage>> {
    if folded.is_empty() {
        return Ok(Vec::new());
    }
    let whole: Vec<ProviderMessage> = folded.iter().map(|f| f.message.clone()).collect();
    let (pinned, rest): (Vec<_>, Vec<_>) = folded.into_iter().partition(|f| f.pinned);
    let pinned: Vec<ProviderMessage> = pinned.into_iter().map(|f| f.message).collect();
    let rest: Vec<ProviderMessage> = rest.into_iter().map(|f| f.message).collect();
    let rest_len = rest.len();

    let pinned_tokens = crate::estimate::estimate_tokens(&pinned, images);
    if pinned_tokens > policy.ceiling_tokens {
        return Err(ResumeFoldError::PinnedInputsExceedWindow {
            ceiling: policy.ceiling_tokens,
        }
        .into());
    }
    let recent = if pinned_tokens > policy.start_tokens {
        Vec::new()
    } else {
        let recent = trim_to_recent_turns(rest, policy.carry_turns);
        trim_to_token_budget(recent, policy.start_tokens - pinned_tokens, images)?
    };

    let mut messages = if recent.len() == rest_len {
        whole
    } else {
        let mut messages = pinned;
        messages.extend(recent);
        messages
    };
    // A conversation with no pinned input can still begin on an assistant or
    // tool message after a trim: drop those until a user message leads.
    while messages
        .first()
        .is_some_and(|message| message.role() != "user")
    {
        messages.remove(0);
    }
    check_well_formed(&messages, &[])?;
    Ok(messages)
}

/// Checks that a conversation begins with a user message and that every tool
/// call is answered by a result and every result has its call. Calls still
/// unanswered at the very end are accepted only when their ids are in
/// `tolerated_open`: a tool batch a same-thread recovery has yet to settle, or
/// calls a settled lifecycle call abandoned. The end of
/// the conversation is otherwise unchecked, because a rebuild legitimately
/// ends on an assistant message (a retained answer or a settled return).
fn check_well_formed(
    messages: &[ProviderMessage],
    tolerated_open: &[String],
) -> Result<(), ResumeFoldError> {
    if let Some(first) = messages.first()
        && first.role() != "user"
    {
        return Err(ResumeFoldError::DoesNotStartWithUser {
            role: first.role().to_string(),
        });
    }
    let mut open: Vec<String> = Vec::new();
    for message in messages {
        if message.role() == "tool" {
            for part in &message.parts {
                if let ryeos_provider_contract::Part::ToolResult(result) = part {
                    match open.iter().position(|id| *id == result.call_id) {
                        Some(index) => {
                            open.remove(index);
                        }
                        None => {
                            return Err(ResumeFoldError::OrphanToolResult {
                                call_id: result.call_id.clone(),
                            });
                        }
                    }
                }
            }
            continue;
        }
        if let Some(call_id) = open.iter().find(|id| !tolerated_open.contains(id)) {
            return Err(ResumeFoldError::UnansweredToolCall {
                call_id: call_id.clone(),
            });
        }
        open.clear();
        open.extend(message.tool_calls().map(|call| call.call_id.clone()));
    }
    if let Some(call_id) = open.iter().find(|id| !tolerated_open.contains(id)) {
        return Err(ResumeFoldError::UnansweredToolCall {
            call_id: call_id.clone(),
        });
    }
    Ok(())
}

fn count_turns(messages: &[ProviderMessage]) -> u32 {
    messages.iter().filter(|m| m.role() == "assistant").count() as u32
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
        if msg.role() == "assistant" {
            seen += 1;
            if seen == turns_to_drop {
                cutoff = idx + 1;
                break;
            }
        }
    }
    let mut trimmed = messages.split_off(cutoff);
    while trimmed.first().is_some_and(|msg| msg.role() == "tool") {
        trimmed.remove(0);
    }
    trimmed
}

fn trim_to_token_budget(
    mut messages: Vec<ProviderMessage>,
    max_tokens: u64,
    images: Option<&ImageAcceptConfig>,
) -> Result<Vec<ProviderMessage>> {
    if messages.is_empty() {
        return Ok(messages);
    }

    loop {
        let total = crate::estimate::estimate_tokens(&messages, images);
        if total <= max_tokens {
            return Ok(messages);
        }

        let assistant_count = messages
            .iter()
            .filter(|message| message.role() == "assistant")
            .count();
        if assistant_count == 0 {
            anyhow::bail!(
                "resume: required provider transcript suffix exceeds the {max_tokens}-token recovery budget"
            );
        }

        let assistant_index = messages
            .iter()
            .position(|message| message.role() == "assistant")
            .expect("assistant_count proved one exists");
        let mut group_end = assistant_index + 1;
        while messages
            .get(group_end)
            .is_some_and(|message| message.role() == "tool")
        {
            group_end += 1;
        }
        messages.drain(..group_end);
        while messages
            .first()
            .is_some_and(|message| message.role() == "tool")
        {
            messages.remove(0);
        }
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
        async fn attach_process(&self, _: &str) -> Result<Value, CallbackError> {
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
        async fn append_event(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(
            &self,
            _: &str,
            _: Vec<Value>,
            _: ryeos_runtime::callback::EventAppendOptions,
        ) -> Result<Value, CallbackError> {
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

    fn text_parts(text: &str) -> Value {
        json!([{"type": "text", "text": text}])
    }

    /// A recorded operator input.
    fn input(text: &str) -> Value {
        json!({"origin": "operator", "parts": text_parts(text)})
    }

    /// A completed text-only turn.
    fn out(turn: u32, text: &str) -> Value {
        json!({"turn": turn, "parts": text_parts(text), "finish": "stop"})
    }

    /// A completed text-only turn that carries its provider record, as a
    /// replayed turn does.
    fn accounted_out(turn: u32, text: &str) -> Value {
        let mut payload = out(turn, text);
        payload["provider_accounting"] = json!({"replayed_from": "ab".repeat(32)});
        payload
    }

    /// A turn cut short by an interrupt.
    fn out_interrupted(turn: u32, text: &str) -> Value {
        json!({"turn": turn, "parts": text_parts(text), "finish": "interrupted"})
    }

    /// A turn that proposes tool calls, given as `{id, name, arguments}`
    /// objects.
    fn out_calls(turn: u32, calls: Value) -> Value {
        let parts: Vec<Value> = calls
            .as_array()
            .expect("calls are an array")
            .iter()
            .map(|call| {
                json!({
                    "type": "tool_call",
                    "call_id": call["id"],
                    "name": call["name"],
                    "arguments": call["arguments"],
                })
            })
            .collect();
        json!({"turn": turn, "parts": parts, "finish": "tool_calls"})
    }

    fn result_with(
        operation_id: &str,
        call_id: &str,
        tool: &str,
        text: &str,
        status: &str,
        duplicate_of: Option<&str>,
    ) -> Value {
        let mut guard = json!({"result_size_bytes": text.len(), "truncated": false});
        if let Some(hash) = duplicate_of {
            guard["duplicate_of"] = json!(hash);
        }
        json!({
            "operation_id": operation_id,
            "call_id": call_id,
            "tool": tool,
            "status": status,
            "parts": text_parts(text),
            "guard": guard,
        })
    }

    fn result(operation_id: &str, call_id: &str, tool: &str, text: &str) -> Value {
        result_with(operation_id, call_id, tool, text, "ok", None)
    }

    fn error_result(operation_id: &str, call_id: &str, tool: &str, text: &str) -> Value {
        result_with(operation_id, call_id, tool, text, "error", None)
    }

    fn make_callback(events: Vec<ReplayedEventRecord>) -> CallbackClient {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(MockCallback { events });
        CallbackClient::from_inner(inner, "T-test", "/tmp/test", "tat-test")
    }

    fn usage_event(completed_turns: u32, last_settled_turn_seq: u64) -> ReplayedEventRecord {
        ReplayedEventRecord {
            chain_seq: 0,
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
        let state = load_resume_state(
            &callback,
            "nonexistent",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert!(state.messages.is_empty());
        assert_eq!(state.turns_completed, 0);
        assert!(state.thread_usage.is_none());
    }

    #[tokio::test]
    async fn continuation_refuses_an_unresolved_predecessor_tool_batch() {
        let callback = make_callback(vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out_calls(
                1,
                json!([
                    {"id":"call-a", "name":"tool:a", "arguments":{"a":1}}
                ]),
            ),
        }]);
        let error = load_resume_state(
            &callback,
            "T-predecessor",
            CarryPolicy::turns(8),
            ResumeMode::Continuation,
            None,
        )
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
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out_calls(
                3,
                json!([
                    {"id":"call-a", "name":"tool:a", "arguments":{"a":1}},
                    {"id":"call-b", "name":"tool:b", "arguments":{"b":2}}
                ]),
            ),
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
            chain_seq: 0,
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
                chain_seq: 0,
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
    fn recovered_occurrences_carry_their_turn_and_call_index_and_accept_the_recorded_keys() {
        let (mut events, mut sources, first, second) = proposed_tool_batch();
        for (operation_id, call_id, tool, call_index) in [
            (first.clone(), "call-a", "tool:a", 0),
            (second.clone(), "call-b", "tool:b", 1),
        ] {
            events.push(ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_start".to_string(),
                payload: json!({
                    "operation_id": operation_id,
                    "call_id": call_id,
                    "tool": tool,
                    "item_ref": tool,
                    "turn": 3,
                    "call_index": call_index,
                }),
            });
            sources.push("T-source".to_string());
        }
        let recovered = recover_tool_state(&events, &sources).unwrap();
        assert_eq!(recovered.pending.len(), 2);
        for (index, occurrence) in recovered.pending.iter().enumerate() {
            assert!(occurrence.start_recorded);
            assert_eq!(occurrence.turn, 3);
            assert_eq!(occurrence.call_index, index);
        }
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
                result(&first, "call-a", "tool:a", "{\"ok\":true}"),
            ),
        ] {
            events.push(ReplayedEventRecord {
                chain_seq: 0,
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
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out_calls(1, json!(selected)),
        }];
        let mut sources = vec![source.clone()];
        if return_index == 1 {
            let first = directive_tool_operation_id(&source, 1, 0);
            events.extend([
                ReplayedEventRecord {
                    chain_seq: 0,
                    event_type: "tool_call_start".to_string(),
                    payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
                },
                ReplayedEventRecord {
                    chain_seq: 0,
                    event_type: "tool_call_result".to_string(),
                    payload: result(&first, "call-a", "tool:a", "{}"),
                },
            ]);
            sources.extend([source.clone(), source.clone()]);
        }
        let return_operation = directive_tool_operation_id(&source, 1, return_index);
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "tool_call_start".to_string(),
            payload: json!({"operation_id":return_operation,"call_id":"call-return","tool":DIRECTIVE_RETURN_TOOL}),
        });
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "tool_call_result".to_string(),
            payload: if succeeded {
                result(
                    &return_operation,
                    "call-return",
                    DIRECTIVE_RETURN_TOOL,
                    "{\"answer\":\"done\"}",
                )
            } else {
                error_result(
                    &return_operation,
                    "call-return",
                    DIRECTIVE_RETURN_TOOL,
                    "{\"error\":\"invalid\"}",
                )
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

    /// The refusal a call after a failed `directive_return` is owed.
    fn refusal_text() -> String {
        failed_return_refusal_body().to_string()
    }

    /// The batch of `lifecycle_batch_events(0, false)` (a failed return, then
    /// one sibling call) with the sibling's start, and its result when
    /// `settled`.
    fn failed_return_batch(
        sibling_start: bool,
        sibling_result: Option<String>,
    ) -> (Vec<ReplayedEventRecord>, Vec<String>) {
        let (mut events, mut sources) = lifecycle_batch_events(0, false);
        let sibling = directive_tool_operation_id("T-lifecycle", 1, 1);
        if sibling_start {
            events.push(ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":sibling,"call_id":"call-b","tool":"tool:b"}),
            });
            sources.push("T-lifecycle".to_string());
        }
        if let Some(text) = sibling_result {
            events.push(ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload: error_result(&sibling, "call-b", "tool:b", &text),
            });
            sources.push("T-lifecycle".to_string());
        }
        (events, sources)
    }

    #[test]
    fn a_failed_lifecycle_return_owes_its_later_siblings_a_refusal() {
        let (events, sources) = lifecycle_batch_events(0, false);
        let recovered = recover_tool_state(&events, &sources).unwrap();
        assert!(recovered.pending.is_empty());
        assert!(recovered.abandoned_call_ids.is_empty());
        assert_eq!(recovered.owed.len(), 1);
        assert_eq!(recovered.owed[0].call.id.as_deref(), Some("call-b"));
        assert!(!recovered.owed[0].start_recorded);
        assert_eq!(
            recovered.owed_source_thread_id.as_deref(),
            Some("T-lifecycle")
        );
        assert_eq!(
            recover_resume_disposition(&events, &recovered).unwrap(),
            ResumeDisposition::SettleOwedSiblings
        );
        // A start already recorded is carried so only the result is owed.
        let (events, sources) = failed_return_batch(true, None);
        let recovered = recover_tool_state(&events, &sources).unwrap();
        assert_eq!(recovered.owed.len(), 1);
        assert!(recovered.owed[0].start_recorded);
        // The refusal's start takes no tool-call budget.
        assert!(recovered.started.is_empty());
    }

    #[test]
    fn a_failed_lifecycle_return_with_every_refusal_recorded_checks_continuation() {
        let (events, sources) = failed_return_batch(true, Some(refusal_text()));
        let recovered = recover_tool_state(&events, &sources).unwrap();
        assert!(recovered.pending.is_empty());
        assert!(recovered.owed.is_empty());
        assert!(recovered.abandoned_call_ids.is_empty());
        assert_eq!(
            recover_resume_disposition(&events, &recovered).unwrap(),
            ResumeDisposition::CheckContinuation
        );
    }

    #[test]
    fn a_call_owed_a_refusal_accepts_nothing_else() {
        // Any result for it other than the refusal is a record defect.
        let (events, sources) = failed_return_batch(true, Some("{\"ok\":true}".to_string()));
        assert!(
            recover_tool_state(&events, &sources)
                .unwrap_err()
                .to_string()
                .contains("is not the tool_not_run_after_failed_return refusal")
        );
        // An ok-status result carrying the code is not the refusal either.
        let (mut events, sources) = failed_return_batch(true, Some(refusal_text()));
        let last = events.len() - 1;
        events[last].payload["status"] = json!("ok");
        assert!(recover_tool_state(&events, &sources).is_err());
        // The next proposal cannot begin while a refusal is still owed.
        let (mut events, mut sources) = failed_return_batch(true, None);
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out(2, "next"),
        });
        sources.push("T-lifecycle".to_string());
        assert!(recover_tool_state(&events, &sources).is_err());
    }

    #[test]
    fn only_the_exact_fixed_envelope_is_the_refusal() {
        let body = failed_return_refusal_body();
        let mut with_extra = body.clone();
        with_extra["error"]["detail"] = json!("smuggled tool output");
        let mut other_message = body.clone();
        other_message["error"]["message"] = json!("something else");
        for text in [with_extra.to_string(), other_message.to_string()] {
            let (events, sources) = failed_return_batch(true, Some(text));
            assert!(recover_tool_state(&events, &sources).is_err());
        }
    }

    /// A batch `[failing return, (tool:x,) return]` whose calls after the
    /// failed first return were each refused; `answered` holds how many of
    /// them have their start and refusal recorded.
    fn double_return_events(
        with_tool_between: bool,
        answered: usize,
    ) -> (Vec<ReplayedEventRecord>, Vec<String>) {
        let source = "T-lifecycle".to_string();
        let mut calls = vec![json!({"id":"call-r1", "name":DIRECTIVE_RETURN_TOOL, "arguments":{}})];
        if with_tool_between {
            calls.push(json!({"id":"call-x", "name":"tool:x", "arguments":{}}));
        }
        calls.push(
            json!({"id":"call-r2", "name":DIRECTIVE_RETURN_TOOL, "arguments":{"answer":"done"}}),
        );
        let mut events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("task"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: {
                    let mut out = out_calls(1, json!(calls));
                    out["provider_accounting"] = json!({"replayed_from": "ab".repeat(32)});
                    out
                },
            },
        ];
        let first = directive_tool_operation_id(&source, 1, 0);
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "tool_call_start".to_string(),
            payload: json!({"operation_id":first,"call_id":"call-r1","tool":DIRECTIVE_RETURN_TOOL}),
        });
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "tool_call_result".to_string(),
            payload: error_result(
                &first,
                "call-r1",
                DIRECTIVE_RETURN_TOOL,
                "{\"error\":\"invalid\"}",
            ),
        });
        for (offset, call) in calls.iter().enumerate().skip(1).take(answered) {
            let operation = directive_tool_operation_id(&source, 1, offset);
            let id = call["id"].as_str().unwrap();
            let tool = call["name"].as_str().unwrap();
            events.push(ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":operation,"call_id":id,"tool":tool}),
            });
            events.push(ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload: error_result(&operation, id, tool, &refusal_text()),
            });
        }
        let sources = vec![source; events.len()];
        (events, sources)
    }

    #[test]
    fn a_second_return_after_a_failed_one_is_an_ordinary_owed_refusal() {
        for with_tool in [false, true] {
            let total = if with_tool { 2 } else { 1 };
            let (events, sources) = double_return_events(with_tool, total);
            let recovered = recover_tool_state(&events, &sources).unwrap();
            assert!(recovered.pending.is_empty() && recovered.owed.is_empty());
            assert!(recovered.terminal_lifecycle_settlement.is_none());
            assert_eq!(
                recover_resume_disposition(&events, &recovered).unwrap(),
                ResumeDisposition::CheckContinuation
            );
            // Cut before any refusal: every later call is owed one.
            let (events, sources) = double_return_events(with_tool, 0);
            let recovered = recover_tool_state(&events, &sources).unwrap();
            assert_eq!(recovered.owed.len(), total);
            assert_eq!(
                recover_resume_disposition(&events, &recovered).unwrap(),
                ResumeDisposition::SettleOwedSiblings
            );
        }
    }

    #[tokio::test]
    async fn a_batch_with_two_returns_rebuilds_and_a_successor_starts_from_it() {
        for with_tool in [false, true] {
            let total = if with_tool { 2 } else { 1 };
            let (events, _) = double_return_events(with_tool, total);
            let callback = make_callback(events);
            for mode in [ResumeMode::NativeSameThread, ResumeMode::Continuation] {
                let state =
                    load_resume_state(&callback, "T-lifecycle", CarryPolicy::turns(8), mode, None)
                        .await
                        .unwrap();
                let tools = state.messages.iter().filter(|m| m.role() == "tool").count();
                assert_eq!(tools, total + 1);
            }
        }
    }

    #[tokio::test]
    async fn the_untyped_unsettled_batch_refusals_carry_their_stable_code() {
        // A continuation from a tail that still owes refusals.
        let callback = make_callback(failed_return_events_before_refusal());
        let error = load_resume_state(
            &callback,
            "T-lifecycle",
            CarryPolicy::turns(8),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<ResumeFoldError>()
                .map(ResumeFoldError::code),
            Some("resume_unsettled_tool_batch")
        );
        // A record written before the fix: the failed return's sibling has no
        // start and no result and the run went on.
        let mut events = failed_return_events_before_refusal();
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload: input("later"),
        });
        let sources = vec!["T-lifecycle".to_string(); events.len()];
        let error = recover_tool_state(&events, &sources).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<ResumeFoldError>()
                .map(ResumeFoldError::code),
            Some("resume_unsettled_tool_batch")
        );
    }

    #[test]
    fn a_successful_return_still_abandons_and_accepts_no_evidence_for_its_siblings() {
        let (mut events, mut sources) = lifecycle_batch_events(0, true);
        let recovered = recover_tool_state(&events, &sources).unwrap();
        assert_eq!(recovered.abandoned_call_ids, ["call-b"]);
        assert!(recovered.owed.is_empty());
        let sibling = directive_tool_operation_id("T-lifecycle", 1, 1);
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "tool_call_start".to_string(),
            payload: json!({"operation_id":sibling,"call_id":"call-b","tool":"tool:b"}),
        });
        sources.push("T-lifecycle".to_string());
        assert!(recover_tool_state(&events, &sources).is_err());
    }

    /// The launch input, then a batch whose `directive_return` failed
    /// validation and whose sibling call after it was answered with the
    /// refusal.
    fn failed_return_events() -> Vec<ReplayedEventRecord> {
        let (mut batch, _) = failed_return_batch(true, Some(refusal_text()));
        batch[0].payload["provider_accounting"] = json!({"replayed_from": "ab".repeat(32)});
        let mut events = vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload: input("task"),
        }];
        events.extend(batch);
        events
    }

    /// As `failed_return_events`, cut before the sibling's start and result.
    fn failed_return_events_before_refusal() -> Vec<ReplayedEventRecord> {
        let mut events = failed_return_events();
        events.truncate(events.len() - 2);
        events
    }

    #[tokio::test]
    async fn a_same_thread_rebuild_hands_back_the_refusals_a_crash_left_owed() {
        let callback = make_callback(failed_return_events_before_refusal());
        let state = load_resume_state(
            &callback,
            "T-lifecycle",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert_eq!(state.disposition, ResumeDisposition::SettleOwedSiblings);
        assert!(state.pending_tools.is_empty());
        assert_eq!(state.owed_siblings.len(), 1);
        let roles: Vec<&str> = state.messages.iter().map(|m| m.role()).collect();
        assert_eq!(roles, ["user", "assistant", "tool"]);
    }

    #[tokio::test]
    async fn a_same_thread_rebuild_of_a_fully_answered_failed_return_checks_continuation() {
        let callback = make_callback(failed_return_events());
        let state = load_resume_state(
            &callback,
            "T-lifecycle",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert_eq!(state.disposition, ResumeDisposition::CheckContinuation);
        assert!(state.owed_siblings.is_empty());
        let roles: Vec<&str> = state.messages.iter().map(|m| m.role()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "tool"]);
    }

    #[tokio::test]
    async fn a_successor_starts_from_a_tail_whose_failed_return_siblings_were_answered() {
        // Every call of the predecessor's last assistant turn has a result: the
        // failed return's error and the sibling's refusal. A successor carries
        // that turn whole and the rebuilt conversation is well formed.
        let callback = make_callback(failed_return_events());
        let state = load_resume_state(
            &callback,
            "T-lifecycle",
            CarryPolicy::turns(8),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap();
        let roles: Vec<&str> = state.messages.iter().map(|m| m.role()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "tool"]);
    }

    #[tokio::test]
    async fn a_successor_cannot_start_while_a_failed_return_still_owes_refusals() {
        let callback = make_callback(failed_return_events_before_refusal());
        let error = load_resume_state(
            &callback,
            "T-lifecycle",
            CarryPolicy::turns(8),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("unresolved tool batch"));
    }

    #[tokio::test]
    async fn an_interrupted_turn_still_resumes_in_place() {
        // An interrupted turn is sealed with its text and reasoning only, no
        // tool calls, so a conversation that ends on it is well formed.
        let mut interrupted = out_interrupted(1, "partial");
        interrupted["provider_accounting"] = json!({"replayed_from": "ab".repeat(32)});
        let callback = make_callback(vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("task"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: interrupted,
            },
        ]);
        let state = load_resume_state(
            &callback,
            "T-int",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert_eq!(state.disposition, ResumeDisposition::ContinueProvider);
        assert_eq!(state.messages.len(), 2);
    }

    #[test]
    fn evidence_after_settled_lifecycle_return_is_rejected() {
        let (mut events, mut sources) = lifecycle_batch_events(0, true);
        let later = directive_tool_operation_id("T-lifecycle", 1, 1);
        events.push(ReplayedEventRecord {
            chain_seq: 0,
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
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out_interrupted(2, "a later process must not reopen a terminal directive"),
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
                chain_seq: 0,
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":operation_id,"call_id":call_id,"tool":tool}),
            });
            sources.push("T-source".to_string());
        }
        for (operation_id, call_id, tool) in calls {
            events.push(ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload: result(&operation_id, call_id, tool, "{}"),
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
                chain_seq: 0,
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("cannot interleave"),
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
            chain_seq: 0,
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
            chain_seq: 0,
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
                chain_seq: 0,
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
            chain_seq: 0,
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
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out(4, "skipped unresolved tools"),
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
            chain_seq: 0,
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
                chain_seq: 0,
                event_type: "tool_call_start".to_string(),
                payload: json!({"operation_id":first,"call_id":"call-a","tool":"tool:a"}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload: result(&first, "call-a", "tool:a", "{}"),
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
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: {
                let mut payload = out(1, "replayed");
                payload["provider_accounting"] = json!({"replayed_from": "ab".repeat(32)});
                payload
            },
        }];
        let progress = recover_turn_progress(&events).unwrap();
        assert_eq!(progress.completed, 1);
        assert!(recover_thread_usage(&events, progress).unwrap().is_none());
    }

    #[test]
    fn live_turn_without_exact_usage_settlement_fails_closed() {
        let events = vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out(1, "live"),
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
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out_interrupted(1, "partial"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out(2, "retry"),
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
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out_interrupted(1, "partial"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: {
                    let mut payload = out(2, "completed replay");
                    payload["provider_accounting"] = json!({"replayed_from": "cd".repeat(32)});
                    payload
                },
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
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: {
                let mut payload = out_calls(1, json!([{"id":"c1","name":"tool:a","arguments":{}}]));
                payload["finish"] = json!("interrupted");
                payload
            },
        }];
        assert!(
            format!(
                "{:#}",
                recover_tool_state(&interrupted, &["T-source".to_string()]).unwrap_err()
            )
            .contains("cannot carry tool calls")
        );

        let mixed = vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 1,
                "delta": "live",
                "parts": [{"type": "tool_call", "call_id": "c1", "name": "tool:a", "arguments": {}}]
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
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 1,
                "parts": {"type": "tool_call"},
                "finish": "tool_calls"
            }),
        };
        let error = recover_tool_state(&[event], &["T-source".to_string()]).unwrap_err();
        assert!(error.to_string().contains("not a recorded turn"));
    }

    #[test]
    fn a_tool_call_part_without_a_string_id_is_refused() {
        let event = ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": 1,
                "parts": [{"type": "tool_call", "call_id": {}, "name": "tool:a", "arguments": {}}],
                "finish": "tool_calls"
            }),
        };
        let error = recover_tool_state(&[event], &["T-source".to_string()]).unwrap_err();
        assert!(error.to_string().contains("not a recorded turn"));
    }

    #[test]
    fn retained_terminal_cognition_is_parsed_without_another_provider_call() {
        let events = vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out(1, "done"),
        }];
        let recovered = recover_tool_state(&events, &["T-source".to_string()]).unwrap();
        assert_eq!(
            recover_resume_disposition(&events, &recovered).unwrap(),
            ResumeDisposition::ParseRetainedResponse
        );

        let mut redirected = events.clone();
        redirected.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload: input("steer"),
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
    async fn a_large_pending_proposal_is_kept_whole_by_same_thread_recovery() {
        let callback = make_callback(vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("Do task"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: {
                    let mut payload = out_calls(
                        1,
                        json!([{
                            "id":"large-call",
                            "name":"tool:large",
                            "arguments":{"payload":"x".repeat(70_000)}
                        }]),
                    );
                    payload["provider_accounting"] = json!({"replayed_from":"ef".repeat(32)});
                    payload
                },
            },
        ]);
        // The thread's own turns are never trimmed, so the proposal that the
        // old recovery budget would have refused is rebuilt with its call
        // still pending.
        let state = load_resume_state(
            &callback,
            "nonexistent",
            CarryPolicy::turns(0),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert_eq!(state.messages.len(), 2);
        assert!(state.messages[1].has_tool_calls());
        assert_eq!(state.pending_tools.len(), 1);
    }

    #[test]
    fn reconstruct_messages_from_cognition_transcript() {
        let events = vec![
            // turn-boundary marker (no content) — skipped
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1, "purpose": "turn"}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("Hello"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out(1, "Hi there!"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("Do something"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out_calls(
                    2,
                    json!([{"id": "c1", "name": "read_file", "arguments": {"path": "/tmp"}}]),
                ),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload: result(&"1".repeat(64), "c1", "read_file", "file contents"),
            },
        ];

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role(), "user");
        assert_eq!(messages[1].role(), "assistant");
        assert_eq!(messages[2].role(), "user");
        assert!(messages[3].has_tool_calls());
        assert_eq!(messages[4].role(), "tool");
        assert_eq!(messages[4].tool_call_id(), Some("c1"));
    }

    #[test]
    fn replay_preserves_json_looking_tool_content_as_the_exact_provider_string() {
        let exact = r#"{"ok":true,"nested":{"value":7}}"#;
        let events = vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "tool_call_result".to_string(),
            payload: result(&"2".repeat(64), "call-exact", "tool:exact", exact),
        }];

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages[0].text().as_deref(), Some(exact));
        let (wire, system) = crate::provider_adapter::messages::convert_messages(
            &messages,
            &None,
            &Default::default(),
        );
        assert!(system.is_none());
        assert_eq!(wire[0]["content"].as_str(), Some(exact));
        assert!(wire[0]["content"].is_string());
    }

    #[test]
    fn replay_rejects_tool_results_in_any_other_shape() {
        let operation_id = "3".repeat(64);
        let mut missing_status = result(&operation_id, "c", "tool:t", "{}");
        missing_status.as_object_mut().unwrap().remove("status");
        let mut old_text_field = result(&operation_id, "c", "tool:t", "{}");
        old_text_field["result_text"] = json!("{}");
        let mut no_call_id = result(&operation_id, "c", "tool:t", "{}");
        no_call_id["call_id"] = json!("");
        for payload in [
            json!({"call_id":"c","tool":"tool:t","result_text":"{}","truncated":false}),
            missing_status,
            old_text_field,
            no_call_id,
        ] {
            let error = reconstruct_messages(&[ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload,
            }])
            .unwrap_err();
            assert!(error.to_string().contains("tool_call_result"), "{error:#}");
        }
    }

    #[test]
    fn native_replay_rebuilds_result_deduplication_in_exact_event_order() {
        let content = "x".repeat(1024);
        let hash = lillux::sha256_hex(content.as_bytes());
        let marker = format!("[duplicate result omitted — hash {}]", &hash[..16]);
        let events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload: result(&"4".repeat(64), "c1", "tool:t", &content),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload: result_with(
                    &"5".repeat(64),
                    "c2",
                    "tool:t",
                    &marker,
                    "ok",
                    Some(hash.as_str()),
                ),
            },
        ];
        let sources = vec!["T-current".to_string(), "T-current".to_string()];
        let mut guard = recover_result_guard(&events, &sources, "T-current").unwrap();
        let next = guard.process(
            &content,
            &crate::directive::ToolOutputConfig::default(),
            crate::result_guard::SpillMode::Disabled(crate::result_guard::HARD_CUT_REASON),
        );
        assert_eq!(next.duplicate_of.as_deref(), Some(hash.as_str()));
        assert_eq!(next.content, marker);
    }

    #[test]
    fn dropped_image_notes_are_not_part_of_the_recovered_guard_text() {
        let content = "x".repeat(1024);
        let hash = lillux::sha256_hex(content.as_bytes());
        let dropped = [(1usize, "image_oversize")];
        let with_notes = |text: &str| crate::tool_images::with_drop_notes(text, &dropped);
        let marker = format!("[duplicate result omitted — hash {}]", &hash[..16]);
        let record = |operation: &str, call: &str, text: String, duplicate: Option<&str>| {
            let mut payload = result_with(operation, call, "tool:t", &text, "ok", duplicate);
            payload["guard"]["dropped_images"] = json!([{"position": 1, "code": "image_oversize"}]);
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_result".to_string(),
                payload,
            }
        };
        let events = vec![
            record(&"4".repeat(64), "c1", with_notes(&content), None),
            record(
                &"5".repeat(64),
                "c2",
                with_notes(&marker),
                Some(hash.as_str()),
            ),
        ];
        let sources = vec!["T-current".to_string(); 2];
        let mut guard = recover_result_guard(&events, &sources, "T-current").unwrap();
        let next = guard.process(
            &content,
            &crate::directive::ToolOutputConfig::default(),
            crate::result_guard::SpillMode::Disabled(crate::result_guard::HARD_CUT_REASON),
        );
        assert_eq!(next.duplicate_of.as_deref(), Some(hash.as_str()));
        // Text that does not end with its recorded notes is refused.
        let mut broken = events.clone();
        broken[0].payload["parts"] = text_parts(&content);
        let error = recover_result_guard(&broken, &sources, "T-current").unwrap_err();
        assert!(
            format!("{error:#}").contains("dropped-image notes"),
            "{error:#}"
        );
    }

    #[test]
    fn error_results_never_enter_the_result_guard() {
        let content = "x".repeat(1024);
        let events = vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "tool_call_result".to_string(),
            payload: error_result(&"6".repeat(64), "c1", "tool:t", &content),
        }];
        let mut guard =
            recover_result_guard(&events, &["T-current".to_string()], "T-current").unwrap();
        // The error text was never seen by the guard, so the same bytes from
        // a successful call are not a duplicate.
        assert!(
            guard
                .process(
                    &content,
                    &crate::directive::ToolOutputConfig::default(),
                    crate::result_guard::SpillMode::Disabled(crate::result_guard::HARD_CUT_REASON)
                )
                .duplicate_of
                .is_none()
        );

        let mut duplicate = events.clone();
        duplicate[0].payload["guard"]["duplicate_of"] = json!("a".repeat(64));
        let error =
            recover_result_guard(&duplicate, &["T-current".to_string()], "T-current").unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot be a duplicate"),
            "{error:#}"
        );
    }

    /// The events of an operator input then a wrap-up notice (turn markers
    /// carry no message), folded.
    fn wrap_up_events() -> Vec<ReplayedEventRecord> {
        let record = |payload: Value| ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload,
        };
        let mut events = vec![
            record(json!({"turn": 1, "purpose": "turn"})),
            record(input("Deadline notice: about 5 seconds remain")),
        ];
        events.extend(
            ryeos_runtime::encode_cognition_in_payloads(
                &ryeos_runtime::CognitionInput::text("harness", "wrap up")
                    .with_kind(ryeos_runtime::cognition::KIND_DEADLINE_WRAP_UP),
            )
            .unwrap()
            .into_iter()
            .map(record),
        );
        events
    }

    fn kinded_input(kind: &str) -> Vec<ReplayedEventRecord> {
        ryeos_runtime::encode_cognition_in_payloads(
            &ryeos_runtime::CognitionInput::text("harness", "notice").with_kind(kind),
        )
        .unwrap()
        .into_iter()
        .map(|payload| ReplayedEventRecord {
            event_type: "cognition_in".to_string(),
            payload,
            chain_seq: 0,
        })
        .collect()
    }

    #[test]
    fn each_wrap_up_kind_recovers_exactly_the_reasons_it_gave() {
        use ryeos_runtime::cognition::{
            KIND_DEADLINE_LIMIT_WRAP_UP, KIND_DEADLINE_WRAP_UP, KIND_LIMIT_WRAP_UP,
        };
        let reasons = |kind: &str| {
            let folded = fold_messages(&kinded_input(kind)).unwrap();
            wrap_up_notice_reasons(&folded[0])
        };
        assert_eq!(
            reasons(KIND_DEADLINE_WRAP_UP),
            Some(WrapUpDelivered {
                deadline: true,
                turn_limit: false
            })
        );
        assert_eq!(
            reasons(KIND_LIMIT_WRAP_UP),
            Some(WrapUpDelivered {
                deadline: false,
                turn_limit: true
            })
        );
        assert_eq!(
            reasons(KIND_DEADLINE_LIMIT_WRAP_UP),
            Some(WrapUpDelivered {
                deadline: true,
                turn_limit: true
            })
        );
        assert_eq!(reasons("operator_note"), None);
    }

    #[test]
    fn a_loop_warning_is_recognised_by_its_kind() {
        let folded = fold_messages(&kinded_input("loop_warning_ping_pong")).unwrap();
        assert_eq!(
            loop_warning_kind(&folded[0]),
            Some(crate::loop_guards::FindingKind::PingPong)
        );
        let other = fold_messages(&kinded_input("loop_warning_unknown")).unwrap();
        assert_eq!(loop_warning_kind(&other[0]), None);
        let wrap = fold_messages(&kinded_input("limit_wrap_up")).unwrap();
        assert_eq!(loop_warning_kind(&wrap[0]), None);
    }

    #[test]
    fn the_fold_marks_only_the_wrap_up_notice_with_its_kind() {
        let folded = fold_messages(&wrap_up_events()).unwrap();
        let notices: Vec<bool> = folded
            .iter()
            .map(|f| wrap_up_notice_reasons(f).is_some())
            .collect();
        assert_eq!(notices, [false, true]);
        // The notice is deliberately not pinned.
        assert!(folded.iter().all(|f| !f.pinned));
    }

    #[tokio::test]
    async fn a_successor_is_told_again_when_its_carried_start_dropped_the_notice() {
        // T1 recorded the task, a turn, the notice and a later turn. A
        // successor with a zero carry keeps pinned inputs only, so the notice
        // is gone and it is not delivered; a successor carrying everything
        // still holds it; a same-thread resume keeps its own events whole.
        let mut events = vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload: serde_json::to_value(
                ryeos_runtime::CognitionInput::text("launch", "task").pinned(),
            )
            .unwrap(),
        }];
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: accounted_out(1, "a1"),
        });
        events.extend(wrap_up_events().into_iter().skip(2));
        events.push(ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: accounted_out(2, "a2"),
        });
        let callback = make_callback(events);
        let delivered = |carry, mode| {
            let callback = &callback;
            async move {
                load_resume_state(callback, "T1", CarryPolicy::turns(carry), mode, None)
                    .await
                    .unwrap()
                    .wrap_up_delivered
                    .deadline
            }
        };
        assert!(!delivered(0, ResumeMode::Continuation).await);
        assert!(delivered(8, ResumeMode::Continuation).await);
        assert!(delivered(0, ResumeMode::NativeSameThread).await);
    }

    #[test]
    fn reconstruct_messages_folds_chunked_cognition_input_once() {
        let content = "large ARC evidence\n".repeat(20_000);
        let events = ryeos_runtime::encode_cognition_in_payloads(
            &ryeos_runtime::CognitionInput::text("launch", content.as_str()),
        )
        .unwrap()
        .into_iter()
        .map(|payload| ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload,
        })
        .collect::<Vec<_>>();

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role(), "user");
        assert_eq!(messages[0].text().as_deref(), Some(content.as_str()));
    }

    #[test]
    fn reconstruct_messages_skips_streaming_cognition_out_fragments() {
        let events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("Hello"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: json!({"delta": "H"}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: json!({"tool_use_partial": {"id": "partial"}}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: json!({"tool_use": {"id": "call-1", "name": "read_file"}}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out(1, "Hi there!"),
            },
        ];

        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role(), "user");
        assert_eq!(messages[1].role(), "assistant");
        assert_eq!(messages[1].text().as_deref(), Some("Hi there!"));

        let trimmed = trim_to_recent_turns(messages, 1);
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].role(), "user");
        assert_eq!(trimmed[1].role(), "assistant");
    }

    #[test]
    fn interrupted_seal_folds_as_assistant_then_redirect() {
        // A live-interrupt seal is a content-bearing cognition_out (with
        // finish "interrupted", no tool calls). Resume must fold it as an assistant
        // message in order — the redirect input follows as the next user turn,
        // and the fresh cognition's output after that. The `interrupted` marker is
        // ignored by the fold (only content/tool_calls/reasoning are read).
        let events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("start the long task"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out_interrupted(1, "working on it, step on"),
            },
            // The operator's redirect, folded as a durable cognition_in by the
            // daemon's poll-and-persist.
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("stop — do X instead"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out(2, "doing X"),
            },
        ];
        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role(), "user");
        assert_eq!(messages[1].role(), "assistant");
        assert_eq!(
            messages[1].text().as_deref(),
            Some("working on it, step on"),
            "interrupted partial content is preserved"
        );
        assert!(
            !messages[1].has_tool_calls(),
            "an interrupted seal carries no tool_calls — no unpaired tool call"
        );
        assert_eq!(messages[2].role(), "user");
        assert_eq!(messages[2].text().as_deref(), Some("stop — do X instead"));
        assert_eq!(messages[3].role(), "assistant");
    }

    #[test]
    fn non_conversational_events_are_skipped() {
        // Lifecycle / usage / turn-marker / streaming events carry no message;
        // folding skips them rather than failing.
        let events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "thread_created".to_string(),
                payload: json!({}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1, "purpose": "turn"}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("Hello"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "thread_usage".to_string(),
                payload: json!({"completed_turns": 1}),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "tool_call_start".to_string(),
                payload: json!({"tool": "read_file"}),
            },
        ];
        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role(), "user");
    }

    #[test]
    fn count_turns_correct() {
        let messages = vec![
            ProviderMessage::user("hello"),
            ProviderMessage::assistant_text("hi"),
            ProviderMessage::user("again"),
            ProviderMessage::assistant_text("there"),
        ];
        assert_eq!(count_turns(&messages), 2);
    }

    #[test]
    fn lifecycle_return_start_does_not_consume_dispatch_attempt_budget() {
        let source = "T-source".to_string();
        let operation_id = directive_tool_operation_id(&source, 1, 0);
        let events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: out_calls(
                    1,
                    json!([{
                        "id": "call-return",
                        "name": DIRECTIVE_RETURN_TOOL,
                        "arguments": {"answer": "done"}
                    }]),
                ),
            },
            ReplayedEventRecord {
                chain_seq: 0,
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
            messages.push(ProviderMessage::user(format!(
                "message {} with some content here",
                i
            )));
            messages.push(ProviderMessage::assistant_text(format!(
                "answer {i} with some content here"
            )));
        }
        let trimmed = trim_to_token_budget(messages, 200, None).unwrap();
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
                retry_after_ms: None,
            },
            1_000,
        )
        .unwrap()
    }

    fn retry_event(advance: &ryeos_accounting::ProviderRetryAdvance) -> ReplayedEventRecord {
        ReplayedEventRecord {
            chain_seq: 0,
            event_type: "provider_retry".to_string(),
            payload: json!({
                "turn": advance.decision.turn,
                "attempt": advance.decision.failed_attempt_number,
                "max_retries": 3,
                "backoff_ms": advance.decision.backoff_ms,
                "decision_digest": advance.decision_digest,
                "supersedes": crate::runner::retry_supersedes(&advance.decision),
                "retry_advance": advance,
            }),
        }
    }

    #[test]
    fn native_resume_recovers_only_ordered_retry_testimony_inside_the_active_turn() {
        let advance = retry_advance(1, 1, "one");
        let mut events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1, "purpose": "turn"}),
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
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload: out(1, "settled"),
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
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: json!({"turn": 1, "purpose": "turn"}),
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

    #[test]
    fn trailing_length_turns_are_derived_from_the_newest_finals() {
        let event = |event_type: &str, payload: Value| ReplayedEventRecord {
            chain_seq: 0,
            event_type: event_type.to_string(),
            payload,
        };
        let length = |turn: u32| {
            event(
                "cognition_out",
                json!({"turn": turn, "parts": text_parts("cut"), "finish": "length"}),
            )
        };
        let stop = |turn: u32| event("cognition_out", out(turn, "done"));
        let harness_input = event(
            "cognition_in",
            json!({"origin": "harness", "kind": "output_limit", "parts": text_parts("continue")}),
        );
        let fragment = event("cognition_out", json!({"turn": 3, "delta": "partial"}));

        assert_eq!(trailing_length_turns(&[]).unwrap(), 0);
        assert_eq!(trailing_length_turns(&[stop(1)]).unwrap(), 0);
        assert_eq!(
            trailing_length_turns(&[stop(1), length(2), harness_input.clone()]).unwrap(),
            1
        );
        assert_eq!(
            trailing_length_turns(&[
                stop(1),
                length(2),
                harness_input.clone(),
                fragment,
                length(3)
            ])
            .unwrap(),
            2
        );
        // A clean turn after the cuts ends the run of them.
        assert_eq!(
            trailing_length_turns(&[length(1), harness_input, stop(2)]).unwrap(),
            0
        );
    }

    fn malformed_fixture() -> (
        impl Fn(&str, Value) -> ReplayedEventRecord,
        impl Fn(u32, u32) -> ReplayedEventRecord,
    ) {
        let event = |event_type: &str, payload: Value| ReplayedEventRecord {
            chain_seq: 0,
            event_type: event_type.to_string(),
            payload,
        };
        let correction = |turn: u32, position: u32| ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload: json!({
                "origin": "harness",
                "kind": ryeos_runtime::cognition::malformed_reply_kind(turn, position),
                "parts": text_parts("fix it"),
            }),
        };
        (event, correction)
    }

    #[test]
    fn the_fold_drops_a_reply_a_later_correction_names() {
        let (event, correction) = malformed_fixture();
        let events = vec![
            event("cognition_in", input("Do task")),
            event("cognition_out", out(1, "<|channel|>leak")),
            correction(1, 1),
            event("cognition_out", out(2, "the answer")),
        ];
        let folded = reconstruct_messages(&events).unwrap();
        let texts: Vec<String> = folded.iter().map(|message| message.joined_text()).collect();
        assert_eq!(texts, vec!["Do task", "fix it", "the answer"]);

        // A correction that names another turn removes nothing.
        let other = vec![
            event("cognition_in", input("Do task")),
            event("cognition_out", out(1, "kept")),
            correction(5, 1),
        ];
        assert_eq!(reconstruct_messages(&other).unwrap().len(), 3);
    }

    #[test]
    fn the_malformed_run_position_is_derived_from_the_record() {
        let (event, correction) = malformed_fixture();
        let leak = |turn: u32| event("cognition_out", out(turn, "<|channel|>leak"));
        let call = |turn: u32| {
            event(
                "cognition_out",
                json!({
                    "turn": turn,
                    "parts": [{"type": "tool_call", "call_id": "c1", "name": "t", "arguments": {}}],
                    "finish": "tool_calls",
                }),
            )
        };

        assert_eq!(trailing_malformed_replies(&[]).unwrap(), 0);
        assert_eq!(trailing_malformed_replies(&[leak(1)]).unwrap(), 0);
        assert_eq!(
            trailing_malformed_replies(&[leak(1), correction(1, 1)]).unwrap(),
            1
        );
        assert_eq!(
            trailing_malformed_replies(&[leak(1), correction(1, 1), leak(2), correction(2, 2)])
                .unwrap(),
            2
        );
        // The next reply, not yet judged, still counts the correction before it.
        assert_eq!(
            trailing_malformed_replies(&[leak(1), correction(1, 1), leak(2)]).unwrap(),
            1
        );
        // A later turn that made a call, or two later turns, ends the run.
        assert_eq!(
            trailing_malformed_replies(&[leak(1), correction(1, 1), call(2)]).unwrap(),
            0
        );
        assert_eq!(
            trailing_malformed_replies(&[leak(1), correction(1, 1), leak(2), leak(3)]).unwrap(),
            0
        );
        // A compaction's summary call, or a recorded refusal, between the
        // correction and the next reply does not move the run on.
        let summary_final = |turn: u32| {
            event(
                "cognition_out",
                json!({
                    "turn": turn,
                    "parts": [{"type": "text", "text": "SUMMARY"}],
                    "finish": "compaction_summary_pressure",
                }),
            )
        };
        assert_eq!(
            trailing_malformed_replies(&[leak(1), correction(1, 1), summary_final(2), leak(3)])
                .unwrap(),
            1
        );
        let refusal_final = |turn: u32| {
            event(
                "cognition_out",
                json!({
                    "turn": turn,
                    "parts": [],
                    "finish": ryeos_runtime::cognition::provider_refused_finish("context_length"),
                }),
            )
        };
        assert_eq!(
            trailing_malformed_replies(&[leak(1), correction(1, 1), refusal_final(2), leak(3)])
                .unwrap(),
            1
        );
    }

    fn empty_correction(turn: u32, position: u32) -> ReplayedEventRecord {
        ReplayedEventRecord {
            event_type: "cognition_in".to_string(),
            payload: json!({
                "origin": "harness",
                "kind": ryeos_runtime::cognition::empty_reply_kind(turn, position),
                "parts": text_parts("answer or call"),
            }),
            chain_seq: 0,
        }
    }

    /// A reply holding only reasoning, ending as `finish`.
    fn reasoning_only(turn: u32, finish: &str) -> ReplayedEventRecord {
        ReplayedEventRecord {
            event_type: "cognition_out".to_string(),
            payload: json!({
                "turn": turn,
                "parts": [{"type": "reasoning", "text": "thinking"}],
                "finish": finish,
            }),
            chain_seq: 0,
        }
    }

    fn texts_of(events: &[ReplayedEventRecord]) -> Vec<String> {
        reconstruct_messages(events)
            .unwrap()
            .iter()
            .map(|message| message.joined_text())
            .collect()
    }

    #[test]
    fn the_fold_drops_an_empty_reply_a_later_correction_names() {
        let event = |event_type: &str, payload: Value| ReplayedEventRecord {
            event_type: event_type.to_string(),
            payload,
            chain_seq: 0,
        };
        let events = vec![
            event("cognition_in", input("Do task")),
            reasoning_only(1, "stop"),
            empty_correction(1, 1),
            event("cognition_out", out(2, "the answer")),
        ];
        assert_eq!(
            texts_of(&events),
            vec!["Do task", "answer or call", "the answer"]
        );

        // A whitespace-only text reply is empty too.
        let mut spaced = events.clone();
        spaced[1] = event("cognition_out", out(1, "  \n "));
        assert_eq!(texts_of(&spaced).len(), 3);

        // A correction naming another turn removes nothing.
        let other = vec![
            event("cognition_in", input("Do task")),
            reasoning_only(1, "stop"),
            empty_correction(5, 1),
        ];
        assert_eq!(texts_of(&other).len(), 3);

        // An empty-reply correction never removes a reply that is not an empty
        // normal stop: a text reply, or an empty one with another finish.
        let text_reply = vec![
            event("cognition_in", input("Do task")),
            event("cognition_out", out(1, "kept")),
            empty_correction(1, 1),
        ];
        assert!(texts_of(&text_reply).contains(&"kept".to_string()));
        for finish in ["content_filter", "other", "length"] {
            let filtered = vec![
                event("cognition_in", input("Do task")),
                reasoning_only(1, finish),
                empty_correction(1, 1),
            ];
            // The reply stays in the fold: it carries no text, so the visible
            // difference is the message count (task, reply, correction).
            assert_eq!(
                reconstruct_messages(&filtered).unwrap().len(),
                3,
                "{finish}"
            );
        }
    }

    #[test]
    fn an_empty_run_position_is_derived_from_the_record() {
        let event = |event_type: &str, payload: Value| ReplayedEventRecord {
            event_type: event_type.to_string(),
            payload,
            chain_seq: 0,
        };
        let empty = |turn: u32| reasoning_only(turn, "stop");
        assert_eq!(trailing_empty_replies(&[]).unwrap(), 0);
        assert_eq!(trailing_empty_replies(&[empty(1)]).unwrap(), 0);
        assert_eq!(
            trailing_empty_replies(&[empty(1), empty_correction(1, 1)]).unwrap(),
            1
        );
        assert_eq!(
            trailing_empty_replies(&[
                empty(1),
                empty_correction(1, 1),
                empty(2),
                empty_correction(2, 2)
            ])
            .unwrap(),
            2
        );
        // The next reply, not yet judged, still counts the correction before it.
        assert_eq!(
            trailing_empty_replies(&[empty(1), empty_correction(1, 1), empty(2)]).unwrap(),
            1
        );
        // A later reply that is not an empty stop (text, or another finish)
        // ends the run, as does a second later turn.
        assert_eq!(
            trailing_empty_replies(&[
                empty(1),
                empty_correction(1, 1),
                event("cognition_out", out(2, "text"))
            ])
            .unwrap(),
            0
        );
        assert_eq!(
            trailing_empty_replies(&[
                empty(1),
                empty_correction(1, 1),
                reasoning_only(2, "content_filter")
            ])
            .unwrap(),
            0
        );
        assert_eq!(
            trailing_empty_replies(&[empty(1), empty_correction(1, 1), empty(2), empty(3)])
                .unwrap(),
            0
        );
        // The last final's finish is read for the retained-response path.
        assert!(!last_final_stopped(&[]).unwrap());
        assert!(last_final_stopped(&[empty(1)]).unwrap());
        assert!(!last_final_stopped(&[empty(1), reasoning_only(2, "content_filter")]).unwrap());
        assert!(!last_final_stopped(&[empty(1), reasoning_only(2, "other")]).unwrap());
    }

    #[test]
    fn both_correction_kinds_in_one_run_fold_and_count_independently() {
        let (event, malformed_correction) = malformed_fixture();
        let events = vec![
            event("cognition_in", input("Do task")),
            event("cognition_out", out(1, "<|channel|>leak")),
            malformed_correction(1, 1),
            reasoning_only(2, "stop"),
            empty_correction(2, 1),
            event("cognition_out", out(3, "the answer")),
        ];
        // Each named reply is dropped; the other kind's reply is not touched.
        assert_eq!(
            texts_of(&events),
            vec!["Do task", "fix it", "answer or call", "the answer"]
        );
        // Streaks are derived per kind: the empty correction ends the
        // malformed run, and the malformed reply never counted for the empty.
        assert_eq!(trailing_malformed_replies(&events[..3]).unwrap(), 1);
        assert_eq!(trailing_malformed_replies(&events[..5]).unwrap(), 0);
        assert_eq!(trailing_empty_replies(&events[..5]).unwrap(), 1);
        assert_eq!(trailing_empty_replies(&events[..3]).unwrap(), 0);
        // The reply after an empty correction is malformed: the empty run
        // is over, and the malformed correction after it starts its own.
        let mixed = vec![
            reasoning_only(1, "stop"),
            empty_correction(1, 1),
            event("cognition_out", out(2, "<|channel|>leak")),
            malformed_correction(2, 1),
        ];
        assert_eq!(trailing_malformed_replies(&mixed).unwrap(), 1);
        assert_eq!(trailing_empty_replies(&mixed).unwrap(), 0);
        // A malformed correction naming the empty reply's turn removes the
        // latest final only when the turn matches, and an empty correction
        // naming a malformed reply's turn leaves the reply in.
        let crossed = vec![
            event("cognition_in", input("Do task")),
            event("cognition_out", out(1, "<|channel|>leak")),
            empty_correction(1, 1),
        ];
        assert_eq!(texts_of(&crossed).len(), 3);
    }

    #[test]
    fn retry_supersedes_counts_live_fragments_and_recovery_checks_it() {
        let mut advance = retry_advance(1, 1, "cut");
        advance.decision.reason = ryeos_accounting::ProviderRetryReason::MidStream {
            live_output_events_emitted: 7,
        };
        assert_eq!(
            crate::runner::retry_supersedes(&advance.decision),
            json!({"turn": 1, "attempt": 1, "output_events": 7})
        );
        let status = retry_advance(1, 1, "status");
        assert_eq!(
            crate::runner::retry_supersedes(&status.decision)["output_events"],
            json!(0)
        );

        let prelude = ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload: json!({"turn": 1, "purpose": "turn"}),
        };
        let sources = vec!["T-current".to_string(); 2];
        let mut event = retry_event(&status);
        let accepted = recover_provider_retry_testimonies(
            &[prelude.clone(), event.clone()],
            &sources,
            "T-current",
            ResumeMode::NativeSameThread,
        );
        assert!(accepted.is_ok());

        event.payload["supersedes"]["output_events"] = json!(3);
        let error = recover_provider_retry_testimonies(
            &[prelude.clone(), event],
            &sources,
            "T-current",
            ResumeMode::NativeSameThread,
        )
        .unwrap_err();
        assert!(error.to_string().contains("supersedes contradicts"));

        let mut missing = retry_event(&status);
        missing
            .payload
            .as_object_mut()
            .unwrap()
            .remove("supersedes");
        assert!(
            recover_provider_retry_testimonies(
                &[prelude, missing],
                &sources,
                "T-current",
                ResumeMode::NativeSameThread,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn full_roundtrip_via_callback() {
        let events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("Do task"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: {
                    let mut payload = out(1, "Done!");
                    payload["provider_accounting"] = json!({"replayed_from": "ab".repeat(32)});
                    payload
                },
            },
        ];
        let callback = make_callback(events);
        let state = load_resume_state(
            &callback,
            "T-prev",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
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
        async fn attach_process(&self, _: &str) -> Result<Value, CallbackError> {
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
        async fn append_event(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(
            &self,
            _: &str,
            _: Vec<Value>,
            _: ryeos_runtime::callback::EventAppendOptions,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, params: Value) -> Result<Value, CallbackError> {
            let tid = params
                .get("thread_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ev = |t: &str, p: Value| ReplayedEventRecord {
                chain_seq: 0,
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
                    ev("cognition_in", input("turn1 in")),
                    ev("thread_usage", usage(1)),
                    ev("cognition_out", out(1, "turn1 out")),
                ],
                "T2" => vec![
                    ev("cognition_in", input("turn2 in")),
                    ev("thread_usage", usage(2)),
                    ev("cognition_out", out(2, "turn2 out")),
                ],
                // Non-continuation child sharing the chain root — must NOT fold.
                "T1-child" => vec![ev("cognition_out", out(1, "POLLUTION"))],
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
        let state = load_resume_state(
            &callback,
            "T2",
            CarryPolicy::turns(8),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap();

        let contents: Vec<String> = state.messages.iter().filter_map(|m| m.text()).collect();
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
    async fn same_thread_recovery_keeps_every_event_of_the_current_thread() {
        // T2 crashed and is resumed in place. Its own conversation is rebuilt
        // whole even with a carry of zero turns; only the predecessor's part
        // of the path is carried the way T2's start carried it.
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(PathMock);
        let callback = CallbackClient::from_inner(inner, "T2", "/tmp/test", "tat-test");
        for carry in [0, 1, 8] {
            let state = load_resume_state(
                &callback,
                "T2",
                CarryPolicy::turns(carry),
                ResumeMode::NativeSameThread,
                None,
            )
            .await
            .unwrap();
            let texts: Vec<String> = state.messages.iter().filter_map(|m| m.text()).collect();
            let own = ["turn2 in".to_string(), "turn2 out".to_string()];
            assert!(texts.ends_with(&own), "carry {carry}: {texts:?}");
        }
        let all = load_resume_state(
            &callback,
            "T2",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert_eq!(all.messages.len(), 4);
        let own_only = load_resume_state(
            &callback,
            "T2",
            CarryPolicy::turns(0),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert_eq!(own_only.messages.len(), 2);
    }

    fn folded(message: ProviderMessage, pinned: bool) -> FoldedMessage {
        FoldedMessage {
            message,
            pinned,
            kind: None,
            turn: None,
            input: None,
        }
    }

    #[test]
    fn a_successor_start_keeps_pinned_inputs_and_the_recent_turns() {
        let conversation = vec![
            folded(ProviderMessage::user("launch"), true),
            folded(ProviderMessage::assistant_text("a1"), false),
            folded(ProviderMessage::user("u2"), false),
            folded(ProviderMessage::assistant_text("a2"), false),
            folded(ProviderMessage::user("u3"), false),
            folded(ProviderMessage::assistant_text("a3"), false),
        ];
        let carried = carry_into_successor(conversation, &CarryPolicy::turns(1), None).unwrap();
        let texts: Vec<String> = carried.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, ["launch", "u3", "a3"]);
        assert_eq!(carried[0].role(), "user");
    }

    /// A twenty-turn path that starts with the launch batch (before, task,
    /// after, each pinned with its acknowledgement) and an operator follow-up
    /// input in the middle.
    fn long_path_with_context() -> Vec<FoldedMessage> {
        let mut path: Vec<FoldedMessage> = Vec::new();
        for input in crate::launch_records::launch_inputs(Some("before"), "task", Some("after")) {
            for message in crate::launch_records::input_messages(&input) {
                path.push(folded(message, input.pinned));
            }
        }
        for turn in 1..=20 {
            if turn == 10 {
                let follow_up = crate::launch_records::operator_follow_up_input("follow-up");
                path.push(folded(follow_up.message(), follow_up.pinned));
            } else {
                path.push(folded(ProviderMessage::user(format!("u{turn}")), false));
            }
            path.push(folded(
                ProviderMessage::assistant_text(format!("a{turn}")),
                false,
            ));
        }
        path
    }

    #[test]
    fn a_successor_start_is_the_pinned_prefix_then_the_recent_turns_in_path_order() {
        let carried =
            carry_into_successor(long_path_with_context(), &CarryPolicy::turns(2), None).unwrap();
        let texts: Vec<String> = carried.iter().filter_map(|m| m.text()).collect();
        let ack_before = crate::launch_records::BEFORE_CONTEXT_ACK;
        let ack_after = crate::launch_records::AFTER_CONTEXT_ACK;
        assert_eq!(
            texts,
            [
                "before",
                ack_before,
                "task",
                "after",
                ack_after,
                "follow-up",
                "u19",
                "a19",
                "u20",
                "a20"
            ]
        );
        assert_eq!(carried[0].role(), "user");
    }

    #[test]
    fn a_zero_carry_keeps_the_whole_pinned_prefix() {
        let carried =
            carry_into_successor(long_path_with_context(), &CarryPolicy::turns(0), None).unwrap();
        let texts: Vec<String> = carried.iter().filter_map(|m| m.text()).collect();
        let ack_before = crate::launch_records::BEFORE_CONTEXT_ACK;
        let ack_after = crate::launch_records::AFTER_CONTEXT_ACK;
        assert_eq!(
            texts,
            [
                "before",
                ack_before,
                "task",
                "after",
                ack_after,
                "follow-up"
            ]
        );
    }

    fn assistant_with_artefact(text: &str, artefact: Value) -> ProviderMessage {
        ProviderMessage::new(
            ryeos_provider_contract::Role::Assistant,
            vec![
                ryeos_provider_contract::Part::Reasoning(ryeos_provider_contract::ReasoningPart {
                    text: None,
                    artefact: Some(ryeos_provider_contract::Artefact(artefact)),
                }),
                ryeos_provider_contract::Part::text(text),
            ],
        )
    }

    fn artefacts_of(messages: &[ProviderMessage]) -> Vec<Value> {
        messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                ryeos_provider_contract::Part::Reasoning(reasoning) => reasoning
                    .artefact
                    .as_ref()
                    .map(|artefact| artefact.0.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_successor_keeps_the_artefacts_of_kept_turns_whole_and_drops_the_rest_with_their_turns() {
        let envelope = |n: u32| json!({"capture": "blocks", "group": "g", "value": {"n": n, "text": "é\u{1F600}"}});
        let conversation = vec![
            folded(ProviderMessage::user("launch"), true),
            folded(assistant_with_artefact("a1", envelope(1)), false),
            folded(ProviderMessage::user("u2"), false),
            folded(assistant_with_artefact("a2", envelope(2)), false),
        ];
        let all = carry_into_successor(conversation.clone(), &CarryPolicy::turns(8), None).unwrap();
        assert_eq!(artefacts_of(&all), [envelope(1), envelope(2)]);
        let recent = carry_into_successor(conversation, &CarryPolicy::turns(1), None).unwrap();
        assert_eq!(artefacts_of(&recent), [envelope(2)]);
    }

    #[test]
    fn reconstructed_messages_hold_recorded_artefact_parts_unchanged() {
        let envelope = json!({"capture": "blocks", "group": "g", "value": {"b": [1, 2.5, null]}});
        let call_envelope = json!({"capture": "sig", "group": "g", "value": {"s": "x"}});
        let events = vec![
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload: input("go"),
            },
            ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: json!({
                    "turn": 1,
                    "parts": [
                        {"type": "reasoning", "text": "shown"},
                        {"type": "reasoning", "artefact": envelope},
                        {"type": "tool_call", "call_id": "c1", "name": "t",
                         "arguments": {}, "artefact": call_envelope},
                    ],
                    "finish": "tool_calls",
                }),
            },
        ];
        let messages = reconstruct_messages(&events).unwrap();
        assert_eq!(artefacts_of(&messages), [envelope]);
        let call_artefact = messages[1]
            .parts
            .iter()
            .find_map(|part| match part {
                ryeos_provider_contract::Part::ToolCall(call) => call.artefact.clone(),
                _ => None,
            })
            .expect("the call keeps its artefact");
        assert_eq!(call_artefact.0, call_envelope);
    }

    #[test]
    fn the_signed_rebind_minimum_is_the_successor_starting_budget() {
        // An operator continuation may bind a different model. The runtime
        // contract refuses one whose context window is smaller than the
        // conversation a successor starts from, so the signed number and this
        // budget must be the same number.
        let descriptor =
            include_str!("../../../../bundles/directive/.ai/runtimes/directive-runtime.yaml");
        let runtime: ryeos_engine::runtime_registry::RuntimeYaml =
            serde_yaml::from_str(&lillux::signature::strip_signature_lines(descriptor))
                .expect("shipped directive runtime descriptor");
        assert_eq!(
            runtime.launch_contract.runtime_facts["context_window"].rebind_minimum,
            Some(DEFAULT_SUCCESSOR_START_TOKENS)
        );
    }

    #[test]
    fn a_zero_carry_still_keeps_pinned_inputs() {
        let conversation = vec![
            folded(ProviderMessage::user("launch"), true),
            folded(ProviderMessage::assistant_text("a1"), false),
        ];
        let carried = carry_into_successor(conversation, &CarryPolicy::turns(0), None).unwrap();
        let texts: Vec<String> = carried.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, ["launch"]);
    }

    #[test]
    fn nothing_dropped_means_nothing_reordered() {
        let conversation = vec![
            folded(ProviderMessage::user("u1"), false),
            folded(ProviderMessage::assistant_text("a1"), false),
            folded(ProviderMessage::user("pinned later"), true),
            folded(ProviderMessage::assistant_text("a2"), false),
        ];
        let carried = carry_into_successor(conversation, &CarryPolicy::turns(8), None).unwrap();
        let texts: Vec<String> = carried.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, ["u1", "a1", "pinned later", "a2"]);
    }

    #[test]
    fn without_a_pinned_input_the_start_is_cut_to_a_user_message() {
        let conversation = vec![
            folded(ProviderMessage::user("u1"), false),
            folded(ProviderMessage::assistant_text("a1"), false),
            folded(ProviderMessage::user("u2"), false),
            folded(ProviderMessage::assistant_text("a2"), false),
        ];
        let carried = carry_into_successor(conversation, &CarryPolicy::turns(1), None).unwrap();
        let texts: Vec<String> = carried.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, ["u2", "a2"]);
    }

    #[test]
    fn a_start_with_a_broken_tool_pairing_fails_with_a_typed_error() {
        let call = crate::directive::ToolCall {
            id: Some("c1".to_string()),
            name: "tool:a".to_string(),
            arguments: json!({}),
        };
        let unanswered = vec![
            ProviderMessage::user("go"),
            ProviderMessage::assistant(None, vec![call], None),
            ProviderMessage::user("and then"),
        ];
        assert_eq!(
            check_well_formed(&unanswered, &[]),
            Err(ResumeFoldError::UnansweredToolCall {
                call_id: "c1".to_string()
            })
        );
        let orphan = vec![
            ProviderMessage::user("go"),
            ProviderMessage::tool_result("c9", "result"),
        ];
        assert_eq!(
            check_well_formed(&orphan, &[]),
            Err(ResumeFoldError::OrphanToolResult {
                call_id: "c9".to_string()
            })
        );
        let assistant_first = vec![ProviderMessage::assistant_text("hi")];
        assert_eq!(
            check_well_formed(&assistant_first, &[]),
            Err(ResumeFoldError::DoesNotStartWithUser {
                role: "assistant".to_string()
            })
        );
        // A typed error survives the anyhow boundary.
        let error: anyhow::Error = ResumeFoldError::PinnedInputsExceedWindow { ceiling: 1 }.into();
        assert!(error.downcast_ref::<ResumeFoldError>().is_some());
    }

    #[test]
    fn each_refusal_has_a_stable_code_that_prefixes_its_message() {
        let errors = [
            (
                ResumeFoldError::DoesNotStartWithUser {
                    role: "assistant".to_string(),
                },
                "resume_start_not_user",
            ),
            (
                ResumeFoldError::UnansweredToolCall {
                    call_id: "c".to_string(),
                },
                "resume_unanswered_tool_call",
            ),
            (
                ResumeFoldError::OrphanToolResult {
                    call_id: "c".to_string(),
                },
                "resume_orphan_tool_result",
            ),
            (
                ResumeFoldError::PinnedInputsExceedWindow { ceiling: 9 },
                "resume_pinned_exceeds_window",
            ),
            (
                ResumeFoldError::UnsettledToolBatch {
                    detail: "d".to_string(),
                },
                "resume_unsettled_tool_batch",
            ),
        ];
        for (error, code) in errors {
            assert_eq!(error.code(), code);
            assert!(error.to_string().starts_with(&format!("{code}: ")));
        }
    }

    #[test]
    fn an_unanswered_call_at_the_end_is_accepted_only_for_a_pending_batch() {
        let call = crate::directive::ToolCall {
            id: Some("c1".to_string()),
            name: "tool:a".to_string(),
            arguments: json!({}),
        };
        let open_tail = vec![
            ProviderMessage::user("go"),
            ProviderMessage::assistant(None, vec![call], None),
        ];
        assert_eq!(
            check_well_formed(&open_tail, &[]),
            Err(ResumeFoldError::UnansweredToolCall {
                call_id: "c1".to_string()
            })
        );
        assert_eq!(check_well_formed(&open_tail, &["c1".to_string()]), Ok(()));
        // An assistant message may end a rebuilt conversation.
        let answered = vec![
            ProviderMessage::user("go"),
            ProviderMessage::assistant_text("done"),
        ];
        assert_eq!(check_well_formed(&answered, &[]), Ok(()));
    }

    /// A pinned launch input of `chars` characters (four per estimated token)
    /// followed by two turns.
    fn conversation_with_pinned_chars(chars: usize) -> Vec<FoldedMessage> {
        vec![
            folded(ProviderMessage::user("x".repeat(chars)), true),
            folded(ProviderMessage::assistant_text("a1"), false),
            folded(ProviderMessage::user("u2"), false),
            folded(ProviderMessage::assistant_text("a2"), false),
        ]
    }

    #[test]
    fn pinned_inputs_within_the_start_budget_carry_the_recent_turns_too() {
        let policy = CarryPolicy {
            carry_turns: 1,
            start_tokens: 1_000,
            ceiling_tokens: 10_000,
        };
        let carried =
            carry_into_successor(conversation_with_pinned_chars(400), &policy, None).unwrap();
        let roles: Vec<&str> = carried.iter().map(|m| m.role()).collect();
        assert_eq!(roles, ["user", "user", "assistant"]);
    }

    #[test]
    fn pinned_inputs_over_the_start_budget_but_within_the_ceiling_are_carried_alone() {
        let policy = CarryPolicy {
            carry_turns: 8,
            start_tokens: 100,
            ceiling_tokens: 10_000,
        };
        let carried =
            carry_into_successor(conversation_with_pinned_chars(4_000), &policy, None).unwrap();
        assert_eq!(carried.len(), 1);
        assert_eq!(carried[0].role(), "user");
    }

    #[test]
    fn pinned_inputs_over_the_ceiling_are_refused_with_their_code() {
        let policy = CarryPolicy {
            carry_turns: 8,
            start_tokens: 100,
            ceiling_tokens: 500,
        };
        let error =
            carry_into_successor(conversation_with_pinned_chars(4_000), &policy, None).unwrap_err();
        let typed = error
            .downcast_ref::<ResumeFoldError>()
            .expect("a typed refusal");
        assert_eq!(typed.code(), "resume_pinned_exceeds_window");
    }

    #[tokio::test]
    async fn a_whole_conversation_that_does_not_start_with_a_user_message_is_refused() {
        let mut payload = out(1, "orphan answer");
        payload["provider_accounting"] = json!({"replayed_from": "ab".repeat(32)});
        let callback = make_callback(vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_out".to_string(),
            payload,
        }]);
        let error = load_resume_state(
            &callback,
            "T-own",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap_err();
        let typed = error
            .downcast_ref::<ResumeFoldError>()
            .expect("a typed refusal");
        assert_eq!(typed.code(), "resume_start_not_user");
    }

    fn frame_event(frame_seq: u32, system: &str) -> ReplayedEventRecord {
        let mut frame = crate::launch_records::build_frame(
            1,
            &crate::launch_records::FrameSource {
                system_prompt: Some(system),
                inventory: &[],
                model: "m",
                reasoning: None,
                sampling: None,
                max_output_tokens: 0,
                provider_config_hash: &"ab".repeat(32),
            },
        )
        .unwrap();
        frame.payload.frame_seq = frame_seq;
        ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_frame".to_string(),
            payload: ryeos_runtime::cognition::to_payload(&frame.payload).unwrap(),
        }
    }

    fn input_events(inputs: &[ryeos_runtime::CognitionInput]) -> Vec<ReplayedEventRecord> {
        inputs
            .iter()
            .flat_map(|input| ryeos_runtime::encode_cognition_in_payloads(input).unwrap())
            .map(|payload| ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_in".to_string(),
                payload,
            })
            .collect()
    }

    #[test]
    fn recorded_launch_records_fold_to_the_messages_the_live_run_had() {
        let inputs = crate::launch_records::launch_inputs(Some("before"), "go", Some("after"));
        let mut events = vec![frame_event(1, "system text")];
        events.extend(input_events(&inputs));

        let folded = fold_messages(&events).unwrap();
        let messages: Vec<ProviderMessage> = folded.iter().map(|f| f.message.clone()).collect();
        assert_eq!(messages, crate::launch_records::messages_of(&inputs));
        // All three inputs are pinned; the acknowledgements follow their input.
        let pinned: Vec<bool> = folded.iter().map(|f| f.pinned).collect();
        assert_eq!(pinned, [true; 5]);

        let frame = latest_frame(&events).unwrap().unwrap();
        assert_eq!(frame.system_text().as_deref(), Some("system text"));
    }

    #[test]
    fn frames_are_numbered_without_gaps_and_the_latest_wins() {
        let events = vec![frame_event(1, "one"), frame_event(2, "two")];
        let latest = latest_frame(&events).unwrap().unwrap();
        assert_eq!(latest.frame_seq, 2);
        assert_eq!(latest.system_text().as_deref(), Some("two"));

        let gap = vec![frame_event(1, "one"), frame_event(3, "three")];
        assert!(latest_frame(&gap).is_err());
        let starts_late = vec![frame_event(2, "two")];
        assert!(latest_frame(&starts_late).is_err());
        assert!(latest_frame(&[]).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_thread_that_recorded_nothing_resumes_as_a_fresh_launch() {
        let empty = make_callback(vec![]);
        let state = load_resume_state(
            &empty,
            "T-empty",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert!(state.has_no_launch_records());

        let with_frame = make_callback(vec![frame_event(1, "system text")]);
        let state = load_resume_state(
            &with_frame,
            "T-framed",
            CarryPolicy::turns(8),
            ResumeMode::NativeSameThread,
            None,
        )
        .await
        .unwrap();
        assert!(!state.has_no_launch_records());
        assert_eq!(state.frame.unwrap().frame_seq, 1);
    }

    #[tokio::test]
    async fn zero_carry_still_preserves_cumulative_turn_coordinate() {
        let inner: Arc<dyn RuntimeCallbackAPI> = Arc::new(PathMock);
        let callback = CallbackClient::from_inner(inner, "T3", "/tmp/test", "tat-test");
        let state = load_resume_state(
            &callback,
            "T2",
            CarryPolicy::turns(0),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap();
        assert!(state.messages.is_empty());
        assert_eq!(state.turns_completed, 2);
    }

    #[test]
    fn trim_to_recent_turns_keeps_last_n_assistant_turns_with_context() {
        let messages = vec![
            ProviderMessage::user("u1"),
            ProviderMessage::assistant_text("a1"),
            ProviderMessage::user("u2"),
            ProviderMessage::assistant_text("a2"),
            ProviderMessage::tool_result("c0", "tool2"),
            ProviderMessage::user("u3"),
            ProviderMessage::assistant_text("a3"),
        ];

        let trimmed = trim_to_recent_turns(messages, 2);
        let contents: Vec<_> = trimmed
            .iter()
            .filter_map(|m| m.text())
            .collect::<Vec<String>>();
        assert_eq!(contents, vec!["u2", "a2", "tool2", "u3", "a3"]);
    }

    #[test]
    fn trim_to_recent_turns_drops_leading_orphan_tool_results() {
        let messages = vec![
            ProviderMessage::user("u1"),
            ProviderMessage::assistant_text("a1 tool call"),
            ProviderMessage::tool_result("c0", "orphan if kept"),
            ProviderMessage::user("u2"),
            ProviderMessage::assistant_text("a2"),
        ];

        let trimmed = trim_to_recent_turns(messages, 1);
        let contents: Vec<_> = trimmed
            .iter()
            .filter_map(|m| m.text())
            .collect::<Vec<String>>();
        assert_eq!(contents, vec!["u2", "a2"]);
    }

    fn event_at(event_type: &str, chain_seq: i64, payload: Value) -> ReplayedEventRecord {
        ReplayedEventRecord {
            chain_seq,
            event_type: event_type.to_string(),
            payload,
        }
    }

    /// A compaction record keeping turns 3 and on, with one pinned task.
    fn compaction_record(trigger: &str) -> ryeos_runtime::CompactionPayload {
        ryeos_runtime::CompactionPayload {
            turn: 0,
            trigger: trigger.to_string(),
            replaces_from_turn: 1,
            replaces_to_turn: 2,
            kept_from_turn: 3,
            summary: vec![ryeos_provider_contract::Part::text("THE SUMMARY")],
            summary_model_call: false,
            omitted_from_input: 0,
            pinned: vec![ryeos_runtime::CognitionInput::text("launch", "THE TASK").pinned()],
            seed: ryeos_runtime::WindowSeed {
                last_coordinate: 2,
                completed: 2,
                tool_starts: 5,
                spawns: 1,
                images: 2,
                image_bytes: 300,
                image_stored: vec!["att-1".to_string()],
            },
            settled_turns: 4,
            size_before: 1000,
            size_after: 400,
            ineffective: false,
            compactions: 1,
            overflow_recoveries: 0,
            clip: None,
            guard_carry: None,
        }
    }

    /// The window of a compaction: the kept turns 3 and 4, a summary call at
    /// turn 5, the record, then turn 6.
    fn windowed_events() -> Vec<ReplayedEventRecord> {
        vec![
            event_at("cognition_in", 20, json!({"turn": 3, "purpose": "turn"})),
            event_at("cognition_out", 21, out(3, "kept three")),
            event_at("cognition_in", 22, json!({"turn": 4, "purpose": "turn"})),
            event_at("cognition_out", 23, out(4, "kept four")),
            event_at(
                "cognition_in",
                24,
                json!({"turn": 5, "purpose": "compaction_summary_pressure"}),
            ),
            event_at(
                "cognition_out",
                25,
                json!({
                    "turn": 5,
                    "parts": text_parts("THE SUMMARY"),
                    "finish": "compaction_summary_pressure",
                }),
            ),
            event_at(
                "cognition_compaction",
                26,
                serde_json::to_value(compaction_record("pressure")).unwrap(),
            ),
            event_at("cognition_in", 27, json!({"turn": 6, "purpose": "turn"})),
            event_at("cognition_out", 28, out(6, "after")),
        ]
    }

    #[test]
    fn a_compaction_keeps_the_wrap_up_and_loop_warnings_its_kept_turns_carry() {
        let mut events = windowed_events();
        let notices: Vec<ReplayedEventRecord> = kinded_input("loop_warning_ping_pong")
            .into_iter()
            .chain(kinded_input(ryeos_runtime::cognition::KIND_LIMIT_WRAP_UP))
            .enumerate()
            .map(|(i, mut event)| {
                event.chain_seq = 100 + i as i64;
                event
            })
            .collect();
        // The notices sit between kept turn 4 and the summary call.
        events.splice(4..4, notices);
        let folded = fold_messages_anchored(&events, Some(26)).unwrap();
        let (delivered, warned) = compacted_signals(&folded);
        assert_eq!(
            delivered,
            WrapUpDelivered {
                deadline: false,
                turn_limit: true
            }
        );
        // Two assistant turns (the kept turns 3 and 4) precede the warning.
        assert_eq!(
            warned.get(&crate::loop_guards::FindingKind::PingPong),
            Some(&2)
        );
        // Without the notices nothing is carried.
        let plain = fold_messages_anchored(&windowed_events(), Some(26)).unwrap();
        let (none, empty) = compacted_signals(&plain);
        assert_eq!(none, WrapUpDelivered::default());
        assert!(empty.is_empty());
    }

    fn digest(n: u64) -> crate::loop_guards::TurnDigest {
        ryeos_runtime::GuardTurn {
            calls: vec![],
            text: Some(n),
        }
    }

    /// A record's carry: a deadline wrap-up given, two replaced turns, and a
    /// ping-pong warning three turns before the record.
    fn carry_record() -> ryeos_runtime::GuardCarry {
        ryeos_runtime::GuardCarry {
            carry_len: 18,
            recent_digests: vec![digest(1), digest(2)],
            wrap_up: ryeos_runtime::GuardWrapUp {
                deadline: true,
                turn_limit: false,
            },
            loop_warned_turns_ago: [(crate::loop_guards::FindingKind::PingPong, 3)].into(),
        }
    }

    #[test]
    fn corrected_replies_are_not_kept_turns_of_a_compaction_window() {
        // Turn 3 was a malformed reply and turn 4 an empty one; each was popped
        // by the live run. Only turn 5 is a kept turn.
        let mut events = Vec::new();
        let mut seq = 20;
        let mut push = |events: &mut Vec<ReplayedEventRecord>, kind: &str, payload: Value| {
            events.push(event_at(kind, seq, payload));
            seq += 1;
        };
        push(
            &mut events,
            "cognition_in",
            json!({"turn": 3, "purpose": "turn"}),
        );
        push(
            &mut events,
            "cognition_out",
            out(3, "a call written as text"),
        );
        for mut input in kinded_input("malformed_reply_turn_3_streak_1") {
            input.chain_seq = seq;
            seq += 1;
            events.push(input);
        }
        push(
            &mut events,
            "cognition_in",
            json!({"turn": 4, "purpose": "turn"}),
        );
        push(&mut events, "cognition_out", out(4, "  "));
        for mut input in kinded_input("empty_reply_turn_4_streak_1") {
            input.chain_seq = seq;
            seq += 1;
            events.push(input);
        }
        push(
            &mut events,
            "cognition_in",
            json!({"turn": 5, "purpose": "turn"}),
        );
        push(&mut events, "cognition_out", out(5, "kept"));
        let anchor = seq;
        push(
            &mut events,
            "cognition_compaction",
            serde_json::to_value(compaction_record("pressure")).unwrap(),
        );
        assert_eq!(turns_before_event(&events, anchor).unwrap(), 1);
        // Without the empty reply's correction, that reply is a kept turn again.
        let mut only_malformed = events.clone();
        only_malformed.remove(5);
        assert_eq!(turns_before_event(&only_malformed, anchor).unwrap(), 2);
    }

    #[test]
    fn the_record_tells_a_same_thread_recovery_what_a_compaction_dropped() {
        // The compacted conversation holds no notice and no warning (both were
        // summarised away), the record carries them.
        let window = windowed_events();
        let folded = fold_messages_anchored(&window, Some(26)).unwrap();
        assert_eq!(turns_before_event(&window, 26).unwrap(), 2);
        let signals = anchored_signals(
            ResumeMode::NativeSameThread,
            Some(&carry_record()),
            2,
            &folded,
        );
        assert_eq!(
            signals.delivered,
            WrapUpDelivered {
                deadline: true,
                turn_limit: false
            }
        );
        assert_eq!(signals.prefix, vec![digest(1), digest(2)]);
        // 2 carried + 2 kept turns, warned 3 turns before the record: position 1.
        assert_eq!(
            signals
                .warned
                .get(&crate::loop_guards::FindingKind::PingPong),
            Some(&1)
        );
        assert_eq!(signals.history, GuardHistory::Carried { carry_len: 18 });
    }

    #[test]
    fn a_successor_keeps_the_prefix_but_not_a_notice_its_conversation_lacks() {
        let folded = fold_messages_anchored(&windowed_events(), Some(26)).unwrap();
        let signals = anchored_signals(ResumeMode::Continuation, Some(&carry_record()), 2, &folded);
        // The notice and the warning are in the record only, not in the
        // conversation the successor was given: the existing rule applies.
        assert_eq!(signals.delivered, WrapUpDelivered::default());
        assert!(signals.warned.is_empty());
        // The detectors still read the history the compaction carried.
        assert_eq!(signals.prefix.len(), 2);
    }

    #[test]
    fn a_same_thread_recovery_unions_the_record_with_what_the_conversation_holds() {
        let mut window = windowed_events();
        let carried: Vec<ReplayedEventRecord> =
            kinded_input(ryeos_runtime::cognition::KIND_LIMIT_WRAP_UP)
                .into_iter()
                .chain(kinded_input("loop_warning_ping_pong"))
                .enumerate()
                .map(|(i, mut event)| {
                    event.chain_seq = 100 + i as i64;
                    event
                })
                .collect();
        window.splice(4..4, carried);
        let folded = fold_messages_anchored(&window, Some(26)).unwrap();
        let signals = anchored_signals(
            ResumeMode::NativeSameThread,
            Some(&carry_record()),
            2,
            &folded,
        );
        // The limit notice is in the conversation, the deadline one in the record.
        assert_eq!(
            signals.delivered,
            WrapUpDelivered {
                deadline: true,
                turn_limit: true
            }
        );
        // The conversation's own warning sits after the 2 carried and 2 kept
        // turns (position 4); the record says 1: the earlier stop wins.
        assert_eq!(
            signals
                .warned
                .get(&crate::loop_guards::FindingKind::PingPong),
            Some(&1)
        );
    }

    #[test]
    fn history_shorter_than_configured_is_reported() {
        let config = crate::directive::LoopGuardsConfig::default();
        let off = crate::directive::LoopGuardsConfig {
            enabled: false,
            ..Default::default()
        };
        // Written with guards on and enough history: complete.
        assert_eq!(
            GuardHistory::Carried { carry_len: 18 }.shortfall(&config),
            None
        );
        assert_eq!(GuardHistory::Complete.shortfall(&config), None);
        // Written under a shorter setting: the record's length is reported.
        assert_eq!(
            GuardHistory::Carried { carry_len: 10 }.shortfall(&config),
            Some(10)
        );
        // Written with the guards off, resumed with them on: nothing was
        // measured, which must not read as a clean history.
        assert_eq!(GuardHistory::NotRecorded.shortfall(&config), Some(0));
        // With the guards off there is nothing to claim.
        assert_eq!(GuardHistory::NotRecorded.shortfall(&off), None);
        // A record with no carry is read as not recorded.
        let folded = fold_messages_anchored(&windowed_events(), Some(26)).unwrap();
        let signals = anchored_signals(ResumeMode::NativeSameThread, None, 2, &folded);
        assert_eq!(signals.history, GuardHistory::NotRecorded);
        assert!(signals.prefix.is_empty());
    }

    #[test]
    fn a_successors_prefix_is_the_predecessor_turns_its_start_left_out() {
        let digests: Vec<_> = (0..10).map(digest).collect();
        // (carried turns, expected prefix length)
        for (carried, expected) in [(3, 7), (0, 10), (10, 0), (25, 0)] {
            assert_eq!(
                successor_prefix(&digests, carried).len(),
                expected,
                "{carried}"
            );
        }
        assert_eq!(successor_prefix(&digests, 3), digests[..7]);
        // Never more than a record could carry, newest first kept.
        let many: Vec<_> = (0..300).map(digest).collect();
        let prefix = successor_prefix(&many, 0);
        assert_eq!(prefix.len(), 256);
        assert_eq!(prefix[255], digest(299));
    }

    #[tokio::test]
    async fn a_successor_start_keeps_the_history_it_did_not_carry() {
        // The predecessor ran six turns and warned about a loop after the
        // second. A successor carrying two turns holds neither the warning nor
        // the first four turns, but detection still reads those four.
        let mut events = vec![ReplayedEventRecord {
            chain_seq: 0,
            event_type: "cognition_in".to_string(),
            payload: serde_json::to_value(
                ryeos_runtime::CognitionInput::text("launch", "task").pinned(),
            )
            .unwrap(),
        }];
        for turn in 1..=6 {
            events.push(ReplayedEventRecord {
                chain_seq: 0,
                event_type: "cognition_out".to_string(),
                payload: accounted_out(turn, &format!("a{turn}")),
            });
            if turn == 2 {
                events.extend(kinded_input("loop_warning_idle"));
            }
        }
        let callback = make_callback(events);
        let state = load_resume_state(
            &callback,
            "T1",
            CarryPolicy::turns(2),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap();
        assert_eq!(state.guard_prefix.len(), 4);
        assert_eq!(state.guard_history, GuardHistory::Complete);
        // The warning was in the turns the start left out: told again if found.
        assert!(state.loop_warned.is_empty());
        // Carrying everything leaves no prefix. A predecessor's warning is not
        // a successor's own: as before, only this thread's own inputs count.
        let whole = load_resume_state(
            &callback,
            "T1",
            CarryPolicy::turns(8),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap();
        assert!(whole.guard_prefix.is_empty());
        assert_eq!(
            whole
                .loop_warned
                .get(&crate::loop_guards::FindingKind::Idle),
            None
        );
    }

    #[test]
    fn a_wrap_up_flag_a_live_process_set_cannot_turn_off() {
        let given = WrapUpDelivered {
            deadline: true,
            turn_limit: false,
        };
        let rebuilt = WrapUpDelivered {
            deadline: false,
            turn_limit: true,
        };
        assert_eq!(
            given.union(rebuilt),
            WrapUpDelivered {
                deadline: true,
                turn_limit: true
            }
        );
        assert_eq!(given.union(WrapUpDelivered::default()), given);
    }

    #[test]
    fn a_compaction_folds_to_pinned_inputs_the_summary_and_the_kept_turns() {
        let folded = fold_messages_anchored(&windowed_events(), Some(26)).unwrap();
        let texts: Vec<String> = folded
            .iter()
            .map(|f| f.message.text().unwrap_or_default())
            .collect();
        // The summary call's own turn is not a conversation turn.
        assert_eq!(
            texts,
            [
                "THE TASK",
                "THE SUMMARY",
                "kept three",
                "kept four",
                "after"
            ]
        );
        assert!(folded[0].pinned);
        assert!(folded[0].input.is_some());
        assert_eq!(
            folded[1].kind.as_deref(),
            Some(ryeos_runtime::cognition::KIND_COMPACTION_SUMMARY)
        );
        assert_eq!(
            folded.iter().map(|f| f.turn).collect::<Vec<_>>(),
            [None, None, Some(3), Some(4), Some(6)]
        );
        // A different anchor leaves the record out: older compactions add nothing.
        let unanchored = fold_messages_anchored(&windowed_events(), Some(99)).unwrap();
        assert_eq!(unanchored.len(), 3);
    }

    #[test]
    fn recovery_readers_start_from_the_window_seed() {
        let events = windowed_events();
        let seed = compaction_record("pressure").seed;
        let progress = recover_turn_progress_from(&events, &seed).unwrap();
        // Turns 3, 4 and 6 complete; the summary call at 5 takes a coordinate
        // and is not a completed turn.
        assert_eq!(progress.last_coordinate, 6);
        assert_eq!(progress.completed, 2 + 3);
        // Without the seed the window's first turn does not follow turn 0.
        assert!(recover_turn_progress(&events).is_err());
        let images = recover_image_run_from(&events, &seed).unwrap();
        assert_eq!(images.images, 2);
        assert_eq!(images.bytes, 300);
        assert!(images.stored.contains("att-1"));
        let sources = vec!["T".to_string(); events.len()];
        assert_eq!(
            recover_unfinished_provider_turn(&events, &sources, "T", seed.last_coordinate).unwrap(),
            None
        );
        assert!(recover_unfinished_provider_turn(&events, &sources, "T", 0).is_err());
    }

    #[test]
    fn a_summary_turn_without_its_record_is_finished_from_the_retained_summary() {
        let mut events = windowed_events();
        events.truncate(6);
        let sources = vec!["T".to_string(); events.len()];
        let tools = recover_tool_state(&events, &sources).unwrap();
        match recover_resume_disposition(&events, &tools).unwrap() {
            ResumeDisposition::FinishCompaction { trigger, summary } => {
                assert_eq!(trigger, "pressure");
                assert_eq!(
                    summary,
                    vec![ryeos_provider_contract::Part::text("THE SUMMARY")]
                );
            }
            other => panic!("expected FinishCompaction, got {other:?}"),
        }
        // With the record written, the run goes on from its last turn.
        let events = windowed_events();
        let tools = recover_tool_state(&events, &vec!["T".to_string(); events.len()]).unwrap();
        assert!(!matches!(
            recover_resume_disposition(&events, &tools).unwrap(),
            ResumeDisposition::FinishCompaction { .. }
        ));
    }

    #[test]
    fn an_unfinished_marker_is_reported_with_its_purpose() {
        use ryeos_runtime::cognition::TurnPurpose;
        let events = vec![
            event_at("cognition_in", 1, json!({"turn": 1, "purpose": "turn"})),
            event_at("cognition_out", 2, out(1, "one")),
            event_at(
                "cognition_in",
                3,
                json!({"turn": 2, "purpose": "compaction_summary_overflow"}),
            ),
        ];
        let sources = vec!["T".to_string(); events.len()];
        assert_eq!(
            recover_unfinished_provider_turn(&events, &sources, "T", 0).unwrap(),
            Some(UnfinishedTurn {
                turn: 2,
                purpose: TurnPurpose::CompactionSummaryOverflow
            })
        );
    }

    #[test]
    fn a_marker_without_a_purpose_is_refused_with_its_stable_code() {
        let events = vec![event_at("cognition_in", 1, json!({"turn": 1}))];
        let error = refuse_markers_without_purpose(&events).unwrap_err();
        let typed = error.downcast_ref::<ResumeFoldError>().unwrap();
        assert_eq!(typed.code(), "resume_marker_without_purpose");
        let sources = vec!["T".to_string()];
        assert!(
            recover_unfinished_provider_turn(&events, &sources, "T", 0)
                .unwrap_err()
                .downcast_ref::<ResumeFoldError>()
                .is_some()
        );
    }

    fn refusal_events() -> Vec<ReplayedEventRecord> {
        vec![
            event_at("cognition_in", 1, json!({"turn": 1, "purpose": "turn"})),
            event_at("cognition_out", 2, out(1, "one")),
            event_at("cognition_in", 3, json!({"turn": 2, "purpose": "turn"})),
            event_at(
                "cognition_out",
                4,
                json!({
                    "turn": 2,
                    "parts": text_parts("refused"),
                    "finish": "provider_refused_provider_context_length_exceeded",
                }),
            ),
        ]
    }

    #[test]
    fn a_refusal_final_is_data_not_a_conversation_turn() {
        let events = refusal_events();
        // Not a message, and no completed turn, but the coordinate is used.
        let folded = fold_messages(&events).unwrap();
        assert_eq!(folded.len(), 1);
        let progress = recover_turn_progress(&events).unwrap();
        assert_eq!(progress.last_coordinate, 2);
        assert_eq!(progress.completed, 1);
        // The marker has its final, so nothing is unfinished.
        let sources = vec!["T".to_string(); events.len()];
        assert_eq!(
            recover_unfinished_provider_turn(&events, &sources, "T", 0).unwrap(),
            None
        );
        assert_eq!(
            recover_refused_code(&events).as_deref(),
            Some("provider_context_length_exceeded")
        );
        // A completed turn or a compaction after it ends the pending retry.
        let mut later = events.clone();
        later.push(event_at(
            "cognition_in",
            5,
            json!({"turn": 3, "purpose": "turn"}),
        ));
        later.push(event_at("cognition_out", 6, out(3, "three")));
        assert_eq!(recover_refused_code(&later), None);
        let tools = recover_tool_state(&events, &sources).unwrap();
        assert_eq!(
            recover_resume_disposition(&events, &tools).unwrap(),
            ResumeDisposition::ForkAfterRefusal
        );
    }

    /// Turns 3 to 8 each call a tool with a large result; a first compaction
    /// (kept turns 3 on) sits after turn 5.
    fn two_compaction_events() -> Vec<ReplayedEventRecord> {
        let big = "z".repeat(600);
        let mut events = Vec::new();
        let mut seq = 10;
        let mut push = |events: &mut Vec<ReplayedEventRecord>, kind: &str, payload: Value| {
            seq += 1;
            events.push(event_at(kind, seq, payload));
        };
        let turn = |events: &mut Vec<ReplayedEventRecord>,
                    push: &mut dyn FnMut(&mut Vec<ReplayedEventRecord>, &str, Value),
                    n: u32| {
            let call = format!("c{n}");
            let operation = directive_tool_operation_id("T", n, 0);
            push(
                events,
                "cognition_in",
                json!({"turn": n, "purpose": "turn"}),
            );
            push(
                events,
                "cognition_out",
                out_calls(n, json!([{"id": call, "name": "big", "arguments": {}}])),
            );
            push(
                events,
                "tool_call_start",
                json!({"operation_id": operation, "call_id": call, "tool": "big", "turn": n, "call_index": 0}),
            );
            push(
                events,
                "tool_call_result",
                result(&operation, &call, "big", &format!("{n}{big}")),
            );
        };
        for n in 3..=5 {
            turn(&mut events, &mut push, n);
        }
        let first = serde_json::to_value(compaction_record("pressure")).unwrap();
        push(&mut events, "cognition_compaction", first);
        for n in 6..=8 {
            turn(&mut events, &mut push, n);
        }
        events
    }

    #[test]
    fn a_second_compactions_size_estimate_is_the_size_of_what_the_fold_rebuilds() {
        let events = two_compaction_events();
        let sources = vec!["T".to_string(); events.len()];
        let first_seq = events
            .iter()
            .find(|event| event.event_type == "cognition_compaction")
            .unwrap()
            .chain_seq;
        let mut first = compaction_record("pressure");
        first.clip = Some(ryeos_runtime::ClipRule {
            over_bytes: 100,
            head_bytes: 10,
            tail_bytes: 10,
            keep_recent: 1,
        });
        let mut events = events;
        for event in &mut events {
            if event.event_type == "cognition_compaction" {
                event.payload = serde_json::to_value(&first).unwrap();
            }
        }
        let basis = CompactionBasis::from_window(
            events.clone(),
            sources,
            Some(AnchorRecord {
                chain_seq: first_seq,
                payload: first,
            }),
        )
        .unwrap();
        let policy = crate::directive::CompactionConfig {
            keep_turns: 2,
            summary: crate::directive::SummaryConfig {
                mode: crate::directive::SummaryMode::None,
                ..Default::default()
            },
            clip: crate::directive::ClipConfig {
                tool_results: crate::directive::ClipMode::Clip,
                over_bytes: 100,
                head_bytes: 10,
                tail_bytes: 10,
                keep_recent: 1,
            },
            ..Default::default()
        };
        let plan = crate::compaction::plan(&basis.folded, policy.keep_turns).expect("a cut");
        assert_eq!(plan.kept_from_turn, 7);
        let summary = crate::compaction::fixed_note(&plan);
        let record = crate::compaction::build_record(
            &basis, &plan, summary, "pressure", &policy, None, None,
        )
        .unwrap();
        // The rebuild reads the window from the kept turn's marker, with the
        // new record in it.
        let marker = events
            .iter()
            .rposition(|event| turn_start_coordinate(event).ok().flatten() == Some(7))
            .unwrap();
        let mut window = events[marker..].to_vec();
        window.push(event_at(
            "cognition_compaction",
            9_999,
            serde_json::to_value(&record).unwrap(),
        ));
        let rebuilt = fold_messages_anchored(&window, Some(9_999)).unwrap();
        let rebuilt: Vec<ProviderMessage> = rebuilt.into_iter().map(|f| f.message).collect();
        assert_eq!(
            record.size_after,
            crate::estimate::estimate_tokens(&rebuilt, None)
        );
        assert!(
            rebuilt
                .iter()
                .any(|m| m.text().is_some_and(|t| t.contains("were clipped")))
        );
    }

    #[tokio::test]
    async fn a_continuation_predecessor_with_an_unfinished_turn_is_refused() {
        // A marker with no final and no recorded refusal: a lost process, a
        // budget stop, an auth failure. Not a refusal the policy answers.
        let callback = make_callback(vec![
            event_at("cognition_in", 1, input_with_origin("task")),
            event_at("cognition_in", 2, json!({"turn": 1, "purpose": "turn"})),
            event_at("cognition_out", 3, out(1, "one")),
            event_at("cognition_in", 4, json!({"turn": 2, "purpose": "turn"})),
        ]);
        let error = load_resume_state(
            &callback,
            "T-test",
            CarryPolicy::turns(8),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("unfinished provider turn"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_predecessor_that_recorded_a_refusal_reports_its_code_to_the_successor() {
        let mut events = vec![event_at("cognition_in", 1, input_with_origin("task"))];
        let mut refusal = refusal_events();
        // The completed turn 1 has its usage settlement before its final.
        refusal.insert(1, usage_event(1, 1));
        events.extend(refusal);
        let callback = make_callback(events);
        let loaded = load_resume_state(
            &callback,
            "T-test",
            CarryPolicy::turns(8),
            ResumeMode::Continuation,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            loaded.refused_code.as_deref(),
            Some("provider_context_length_exceeded")
        );
        assert_eq!(loaded.last_turn_coordinate, 2);
        assert_eq!(loaded.turns_completed, 1);
    }

    fn input_with_origin(text: &str) -> Value {
        json!({"origin": "launch", "parts": text_parts(text), "pinned": true})
    }
}
