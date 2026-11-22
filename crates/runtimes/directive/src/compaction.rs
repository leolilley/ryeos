//! Compaction of a conversation: choosing the cut, and the record that makes
//! the cut the fold base of every later rebuild.
//!
//! A compaction replaces the older part of the conversation by a summary and
//! keeps the newest turns whole. The pinned inputs survive unchanged. The
//! cut is always at an assistant turn, so a tool call is never split from its
//! result, and the first kept message is an assistant message that follows the
//! summary directly.

use anyhow::Result;
use ryeos_directive_definition::ImageAcceptConfig;
use ryeos_provider_contract::Part;
use ryeos_runtime::{CognitionInput, CompactionPayload};

use crate::directive::{CompactionConfig, ProviderMessage, ProviderMessageExt};
use crate::resume::{CompactionBasis, FoldedMessage};

/// Where a conversation is cut.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Index of the first kept message.
    pub cut: usize,
    pub kept_from_turn: u32,
    pub replaces_from_turn: u32,
    pub replaces_to_turn: u32,
    /// The pinned inputs before the cut, in order; they are carried by the
    /// record.
    pub pinned_inputs: Vec<CognitionInput>,
    /// Messages before the cut that the summary replaces.
    pub replaced_messages: u32,
}

/// What stands for the replaced range in the rebuilt conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub parts: Vec<Part>,
    /// A model wrote it, in the summary call at `turn`.
    pub model_call: bool,
    pub turn: u32,
    /// Messages the summary call's bounded input left out.
    pub omitted_from_input: u32,
}

/// Choose the cut: keep the newest `keep_turns` assistant turns whole and
/// replace everything unpinned before them. `None` when the conversation holds
/// no more turns than that, so there is nothing to replace.
pub fn plan(folded: &[FoldedMessage], keep_turns: u32) -> Option<Plan> {
    let keep = usize::try_from(keep_turns.max(1)).unwrap_or(usize::MAX);
    let turn_indices: Vec<usize> = folded
        .iter()
        .enumerate()
        .filter(|(_, message)| message.turn.is_some())
        .map(|(index, _)| index)
        .collect();
    if turn_indices.len() <= keep {
        return None;
    }
    let replaced_turns = &turn_indices[..turn_indices.len() - keep];
    let cut = turn_indices[turn_indices.len() - keep];
    let kept_from_turn = folded[cut].turn?;
    let replaces_from_turn = folded[*replaced_turns.first()?].turn?;
    let replaces_to_turn = folded[*replaced_turns.last()?].turn?;
    let pinned_inputs: Vec<CognitionInput> = folded[..cut]
        .iter()
        .filter(|message| message.pinned)
        .filter_map(|message| message.input.clone())
        .collect();
    let replaced_messages = folded[..cut]
        .iter()
        .filter(|message| !message.pinned)
        .count();
    Some(Plan {
        cut,
        kept_from_turn,
        replaces_from_turn,
        replaces_to_turn,
        pinned_inputs,
        replaced_messages: u32::try_from(replaced_messages).unwrap_or(u32::MAX),
    })
}

/// The summary a compaction without a model call carries: a fixed note naming
/// how much was removed.
pub fn fixed_note(plan: &Plan) -> Summary {
    Summary {
        parts: vec![Part::text(format!(
            "{} earlier messages of this conversation, from turn {} to turn {}, were \
             removed to keep it within the context window. The pinned inputs above \
             and the most recent turns below are unchanged.",
            plan.replaced_messages, plan.replaces_from_turn, plan.replaces_to_turn
        ))],
        model_call: false,
        turn: 0,
        omitted_from_input: 0,
    }
}

/// The conversation a compaction leaves: the pinned inputs, the summary, the
/// kept messages.
#[cfg(test)]
pub fn compacted_messages(
    folded: &[FoldedMessage],
    plan: &Plan,
    summary: &Summary,
) -> Vec<ProviderMessage> {
    let mut messages = crate::launch_records::messages_of(&plan.pinned_inputs);
    messages.push(ProviderMessage::new(
        ryeos_provider_contract::Role::User,
        summary.parts.clone(),
    ));
    messages.extend(folded[plan.cut..].iter().map(|kept| kept.message.clone()));
    messages
}

/// The summary call's input: the replaced range as labelled text, bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// Messages the bound left out. The text says so.
    pub omitted: u32,
}

/// A tool result longer than this is rendered as its head and tail.
const RENDERED_RESULT_HEAD_TAIL_BYTES: usize = 1_000;

fn clip_middle(text: &str) -> String {
    let limit = RENDERED_RESULT_HEAD_TAIL_BYTES * 2;
    if text.len() <= limit {
        return text.to_string();
    }
    let mut head = RENDERED_RESULT_HEAD_TAIL_BYTES;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - RENDERED_RESULT_HEAD_TAIL_BYTES;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... {} bytes not shown ...]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn render_parts(parts: &[Part], out: &mut String) {
    for part in parts {
        match part {
            Part::Text(text) => {
                out.push_str(&clip_middle(&text.text));
                out.push('\n');
            }
            Part::Image(_) => out.push_str("[an image was shown here]\n"),
            Part::Reasoning(_) => {}
            Part::ToolCall(call) => {
                out.push_str(&format!("[called {} with {}]\n", call.name, call.arguments));
            }
            Part::ToolResult(result) => render_parts(&result.parts, out),
        }
    }
}

fn render_message(message: &ProviderMessage) -> String {
    let label = match message.role() {
        "assistant" => "assistant",
        "tool" => "tool result",
        _ => "user",
    };
    let mut out = format!("[{label}]\n");
    render_parts(&message.parts, &mut out);
    out
}

/// Render the replaced range for the summary call, deterministically. A
/// previous summary always comes first. When the range is larger than
/// `budget_tokens` (estimated), the oldest other messages are left out and the
/// text says how many: the record still holds them.
pub fn render_for_summary(replaced: &[FoldedMessage], budget_tokens: u64) -> Rendered {
    let blocks: Vec<(bool, String)> = replaced
        .iter()
        .filter(|message| !message.pinned)
        .map(|message| {
            (
                message.kind.as_deref() == Some(ryeos_runtime::cognition::KIND_COMPACTION_SUMMARY),
                render_message(&message.message),
            )
        })
        .collect();
    let budget_bytes = usize::try_from(budget_tokens.saturating_mul(4)).unwrap_or(usize::MAX);
    let mut used: usize = blocks
        .iter()
        .filter(|(previous, _)| *previous)
        .map(|(_, text)| text.len())
        .sum();
    let mut included = vec![false; blocks.len()];
    for (index, (previous, _)) in blocks.iter().enumerate() {
        included[index] = *previous;
    }
    for index in (0..blocks.len()).rev() {
        if included[index] {
            continue;
        }
        let size = blocks[index].1.len();
        if used.saturating_add(size) > budget_bytes {
            break;
        }
        used += size;
        included[index] = true;
    }
    let omitted = included.iter().filter(|included| !**included).count();
    let mut text = String::new();
    if omitted > 0 {
        text.push_str(&format!("[{omitted} earlier messages are not shown]\n\n"));
    }
    for ((_, block), included) in blocks.iter().zip(&included) {
        if *included {
            text.push_str(block);
            text.push('\n');
        }
    }
    Rendered {
        text,
        omitted: u32::try_from(omitted).unwrap_or(u32::MAX),
    }
}

/// Whether the pinned inputs, with room for a summary of up to
/// `max_output_tokens`, fit the bound on a compaction record. Checked before
/// the paid summary call, so a call whose result could not be recorded is not
/// made.
pub fn pinned_inputs_fit_a_record(
    pinned: &[CognitionInput],
    max_output_tokens: u64,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(pinned).map_or(usize::MAX, |bytes| bytes.len());
    let reserve = usize::try_from(max_output_tokens.saturating_mul(8))
        .unwrap_or(usize::MAX)
        .saturating_add(16 * 1024);
    if bytes.saturating_add(reserve) > ryeos_runtime::cognition::MAX_COMPACTION_RECORD_BYTES {
        return Err(format!(
            "the pinned inputs are {bytes} bytes, too large for a compaction record to carry"
        ));
    }
    Ok(())
}

/// Read the tool results of `messages` (the kept turns, oldest first) under
/// `rule`: every result, except one already spilled to an attachment, but the newest `keep_recent` whose text is longer than
/// `over_bytes` becomes its head, a marker naming what was left out, and its
/// tail. A pure function of the messages and the rule, so a live run and a
/// rebuild from the record read the same bytes.
pub fn clip_tool_results(messages: &mut [ProviderMessage], rule: &ryeos_runtime::ClipRule) {
    let tool_messages: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role() == "tool")
        .map(|(index, _)| index)
        .collect();
    let keep = usize::try_from(rule.keep_recent).unwrap_or(usize::MAX);
    let clipped = tool_messages.len().saturating_sub(keep);
    for &index in &tool_messages[..clipped] {
        for part in &mut messages[index].parts {
            let Part::ToolResult(result) = part else {
                continue;
            };
            // A spilled result is already a bounded preview with its own
            // marker; clipping it again would cut the marker's account of
            // where the full text lives.
            let spilled = result.parts.iter().any(|inner| {
                matches!(inner, Part::Text(text)
                    if text.text.starts_with(ryeos_runtime::TOOL_RESULT_PARTIAL_PREFIX))
            });
            if spilled {
                continue;
            }
            for inner in &mut result.parts {
                if let Part::Text(text) = inner {
                    text.text = clip_text(&text.text, rule);
                }
            }
        }
    }
}

fn clip_text(text: &str, rule: &ryeos_runtime::ClipRule) -> String {
    let over = usize::try_from(rule.over_bytes).unwrap_or(usize::MAX);
    if text.len() <= over {
        return text.to_string();
    }
    let mut head = usize::try_from(rule.head_bytes)
        .unwrap_or(0)
        .min(text.len());
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let tail_len = usize::try_from(rule.tail_bytes)
        .unwrap_or(0)
        .min(text.len() - head);
    let mut tail = text.len() - tail_len;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[... {} of {} bytes of this result were clipped when the conversation was compacted ...]\n{}",
        &text[..head],
        tail - head,
        text.len(),
        &text[tail..]
    )
}

/// The run's loop-guard state at a compaction, for the record to carry.
pub struct GuardSnapshot<'a> {
    pub config: &'a crate::directive::LoopGuardsConfig,
    /// The history already carried before this compaction.
    pub prefix: &'a [ryeos_runtime::GuardTurn],
    pub warned: &'a crate::loop_guards::WarnedAt,
    pub wrap_up: ryeos_runtime::GuardWrapUp,
}

/// The compaction record for `plan`, with the sizes before and after and the
/// seed every recovery reader starts from. `guard` is the run's loop-guard
/// state, or `None` when the loop guards are off, which the record carries as
/// `null`.
pub fn build_record(
    basis: &CompactionBasis,
    plan: &Plan,
    summary: Summary,
    trigger: &str,
    policy: &CompactionConfig,
    images: Option<&ImageAcceptConfig>,
    guard: Option<&GuardSnapshot<'_>>,
) -> Result<CompactionPayload> {
    let before: Vec<ProviderMessage> = basis
        .folded
        .iter()
        .map(|folded| folded.message.clone())
        .collect();
    let clip = (policy.clip.tool_results == crate::directive::ClipMode::Clip).then_some({
        ryeos_runtime::ClipRule {
            over_bytes: policy.clip.over_bytes,
            head_bytes: policy.clip.head_bytes,
            tail_bytes: policy.clip.tail_bytes,
            keep_recent: policy.clip.keep_recent,
        }
    });
    // The estimate is over the bytes the rebuild reads: the pinned inputs, the
    // summary, and the kept turns as the raw events give them, clipped once.
    let mut kept = basis.raw_kept_messages(plan.kept_from_turn)?;
    if let Some(rule) = &clip {
        clip_tool_results(&mut kept, rule);
    }
    let mut after = crate::launch_records::messages_of(&plan.pinned_inputs);
    after.push(ProviderMessage::new(
        ryeos_provider_contract::Role::User,
        summary.parts.clone(),
    ));
    after.extend(kept);
    let size_before = crate::estimate::estimate_tokens(&before, images);
    let size_after = crate::estimate::estimate_tokens(&after, images);
    let ineffective =
        (size_after as f64) > (size_before as f64) * (1.0 - policy.min_reduction_ratio);
    let previous = basis.anchor();
    let seed = basis.seed_before_turn(plan.kept_from_turn)?;
    let settled_turns = basis.completed_turns;
    let guard_carry = guard.map(|guard| {
        let conversation = crate::loop_guards::digests_from_messages(&before);
        let replaced_turns = crate::loop_guards::digests_from_messages(&before[..plan.cut]).len();
        // A flag the previous record carried stays set.
        let earlier = previous
            .and_then(|record| record.guard_carry.as_ref())
            .map(|carry| carry.wrap_up)
            .unwrap_or_default();
        crate::loop_guards::build_carry(
            guard.config,
            guard.prefix,
            &conversation,
            replaced_turns,
            guard.warned,
            ryeos_runtime::GuardWrapUp {
                deadline: guard.wrap_up.deadline || earlier.deadline,
                turn_limit: guard.wrap_up.turn_limit || earlier.turn_limit,
            },
            // The same bound `CompactionPayload::validate` applies.
            settled_turns.saturating_sub(seed.completed),
        )
    });
    let record = CompactionPayload {
        turn: summary.turn,
        trigger: trigger.to_string(),
        replaces_from_turn: plan.replaces_from_turn,
        replaces_to_turn: plan.replaces_to_turn,
        kept_from_turn: plan.kept_from_turn,
        summary: summary.parts,
        summary_model_call: summary.model_call,
        omitted_from_input: summary.omitted_from_input,
        pinned: plan.pinned_inputs.clone(),
        seed,
        settled_turns,
        size_before,
        size_after,
        ineffective,
        compactions: previous
            .map_or(0, |record| record.compactions)
            .saturating_add(1),
        overflow_recoveries: previous
            .map_or(0, |record| record.overflow_recoveries)
            .saturating_add(u32::from(trigger == "overflow")),
        clip,
        guard_carry,
    };
    record.validate()?;
    let bytes = serde_json::to_vec(&record)?.len();
    if bytes > ryeos_runtime::cognition::MAX_COMPACTION_RECORD_BYTES {
        anyhow::bail!(
            "compaction record would be {bytes} bytes, over the {} byte bound; \
             the pinned inputs are too large to carry",
            ryeos_runtime::cognition::MAX_COMPACTION_RECORD_BYTES
        );
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::ProviderMessageExt;

    fn message(role: &str, text: &str, turn: Option<u32>, pinned: bool) -> FoldedMessage {
        let message = match role {
            "assistant" => ProviderMessage::assistant_text(text),
            _ => ProviderMessage::user(text),
        };
        FoldedMessage {
            message,
            pinned,
            kind: None,
            turn,
            input: (pinned && role == "user")
                .then(|| CognitionInput::text("launch", text).pinned()),
        }
    }

    fn conversation(turns: u32) -> Vec<FoldedMessage> {
        let mut folded = vec![message("user", "the task", None, true)];
        for turn in 1..=turns {
            folded.push(message("user", &format!("step {turn}"), None, false));
            folded.push(message(
                "assistant",
                &format!("did {turn}"),
                Some(turn),
                false,
            ));
        }
        folded
    }

    #[test]
    fn a_conversation_within_the_kept_turns_has_nothing_to_replace() {
        assert_eq!(plan(&conversation(3), 3), None);
        assert_eq!(plan(&conversation(2), 6), None);
    }

    #[test]
    fn the_cut_keeps_the_newest_turns_and_pinned_inputs_before_it() {
        let folded = conversation(8);
        let plan = plan(&folded, 3).expect("a cut");
        assert_eq!(plan.kept_from_turn, 6);
        assert_eq!(plan.replaces_from_turn, 1);
        assert_eq!(plan.replaces_to_turn, 5);
        // The cut lands on an assistant message.
        assert_eq!(folded[plan.cut].message.role(), "assistant");
        assert_eq!(plan.pinned_inputs.len(), 1);
        // Eleven unpinned messages precede the cut: six inputs, five answers.
        assert_eq!(plan.replaced_messages, 11);
    }

    #[test]
    fn the_rendering_is_bounded_keeps_a_previous_summary_first_and_says_what_it_left_out() {
        let mut folded = vec![
            message("user", "the task", None, true),
            FoldedMessage {
                kind: Some(ryeos_runtime::cognition::KIND_COMPACTION_SUMMARY.to_string()),
                ..message("user", "earlier summary text", None, false)
            },
        ];
        for turn in 1..=20 {
            folded.push(message(
                "user",
                &format!("input {turn} {}", "x".repeat(200)),
                None,
                false,
            ));
            folded.push(message(
                "assistant",
                &format!("answer {turn}"),
                Some(turn),
                false,
            ));
        }
        // Room for a few messages only (budget in tokens: bytes / 4).
        let rendered = render_for_summary(&folded, 200);
        assert!(rendered.omitted > 0);
        assert!(rendered.text.starts_with(&format!(
            "[{} earlier messages are not shown]",
            rendered.omitted
        )));
        let summary_at = rendered
            .text
            .find("earlier summary text")
            .expect("previous summary kept");
        let newest_at = rendered
            .text
            .find("answer 20")
            .expect("newest message kept");
        assert!(summary_at < newest_at);
        assert!(!rendered.text.contains("the task"));
        // Deterministic.
        assert_eq!(rendered, render_for_summary(&folded, 200));
        // Unbounded keeps everything.
        let all = render_for_summary(&folded, u64::MAX / 8);
        assert_eq!(all.omitted, 0);
        assert!(all.text.contains("input 1 "));
    }

    #[test]
    fn older_large_tool_results_are_clipped_and_the_newest_are_not() {
        let big = "x".repeat(1000);
        let mut messages: Vec<ProviderMessage> = (0..4)
            .map(|n| ProviderMessage::tool_result(format!("c{n}"), format!("{n}{big}")))
            .collect();
        let rule = ryeos_runtime::ClipRule {
            over_bytes: 100,
            head_bytes: 10,
            tail_bytes: 10,
            keep_recent: 2,
        };
        clip_tool_results(&mut messages, &rule);
        let texts: Vec<String> = messages.iter().map(|m| m.text().unwrap()).collect();
        assert!(texts[0].contains("bytes of this result were clipped"));
        assert!(texts[1].contains("bytes of this result were clipped"));
        assert!(texts[0].starts_with('0'));
        assert_eq!(texts[2].len(), 1001);
        assert_eq!(texts[3].len(), 1001);
        // Deterministic.
        let mut again: Vec<ProviderMessage> = (0..4)
            .map(|n| ProviderMessage::tool_result(format!("c{n}"), format!("{n}{big}")))
            .collect();
        clip_tool_results(&mut again, &rule);
        assert_eq!(again, messages);
    }

    #[test]
    fn a_spilled_tool_result_is_never_clipped_again() {
        let big = "x".repeat(1000);
        let spilled = format!(
            "{}9000 bytes, full result stored as attachment abc; the full result is not available to you]\n{big}",
            ryeos_runtime::TOOL_RESULT_PARTIAL_PREFIX
        );
        let mut messages = vec![
            ProviderMessage::tool_result("c0".to_string(), spilled.clone()),
            ProviderMessage::tool_result("c1".to_string(), format!("1{big}")),
            ProviderMessage::tool_result("c2".to_string(), format!("2{big}")),
        ];
        let rule = ryeos_runtime::ClipRule {
            over_bytes: 100,
            head_bytes: 10,
            tail_bytes: 10,
            keep_recent: 1,
        };
        clip_tool_results(&mut messages, &rule);
        let texts: Vec<String> = messages.iter().map(|m| m.text().unwrap()).collect();
        assert_eq!(texts[0], spilled);
        assert!(texts[1].contains("bytes of this result were clipped"));
        assert_eq!(texts[2].len(), 1001);
    }

    #[test]
    fn the_open_loop_of_a_compacted_conversation_starts_after_the_summary() {
        let folded = conversation(8);
        let plan = plan(&folded, 3).expect("a cut");
        let rebuilt = compacted_messages(&folded, &plan, &fixed_note(&plan));
        // The summary is a user message, so every kept assistant turn is inside
        // the open loop and a replaced turn's provider artefact is out of scope.
        let start = crate::provider_adapter::reasoning_replay::open_loop_start(&rebuilt);
        assert!(
            start >= 2,
            "the open loop never reaches back past the summary"
        );
        assert!(rebuilt[start..].iter().any(|m| m.role() == "assistant"));
    }

    #[test]
    fn a_compaction_record_without_one_of_its_fields_is_refused() {
        let folded = conversation(8);
        let plan = plan(&folded, 3).expect("a cut");
        let record = ryeos_runtime::CompactionPayload {
            turn: 0,
            trigger: "pressure".to_string(),
            replaces_from_turn: plan.replaces_from_turn,
            replaces_to_turn: plan.replaces_to_turn,
            kept_from_turn: plan.kept_from_turn,
            summary: fixed_note(&plan).parts,
            summary_model_call: false,
            omitted_from_input: 0,
            pinned: plan.pinned_inputs.clone(),
            seed: ryeos_runtime::WindowSeed::default(),
            settled_turns: 8,
            size_before: 10,
            size_after: 5,
            ineffective: false,
            compactions: 1,
            overflow_recoveries: 0,
            clip: None,
            guard_carry: None,
        };
        let whole = serde_json::to_value(&record).unwrap();
        assert!(serde_json::from_value::<ryeos_runtime::CompactionPayload>(whole.clone()).is_ok());
        for field in [
            "clip",
            "guard_carry",
            "omitted_from_input",
            "ineffective",
            "summary_model_call",
        ] {
            let mut cut = whole.clone();
            cut.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<ryeos_runtime::CompactionPayload>(cut).is_err(),
                "{field} is required"
            );
        }
    }

    #[test]
    fn pinned_inputs_over_the_record_bound_are_refused_before_a_paid_call() {
        let small = vec![CognitionInput::text("launch", "task").pinned()];
        assert!(pinned_inputs_fit_a_record(&small, 2_000).is_ok());
        let huge = vec![CognitionInput::text("launch", "x".repeat(200 * 1024)).pinned()];
        assert!(pinned_inputs_fit_a_record(&huge, 2_000).is_err());
    }

    #[test]
    fn the_fixed_note_names_the_range_and_the_rebuilt_conversation_leads_with_the_pinned_input() {
        let folded = conversation(8);
        let plan = plan(&folded, 3).expect("a cut");
        let note = fixed_note(&plan);
        assert!(!note.model_call);
        let rebuilt = compacted_messages(&folded, &plan, &note);
        // Pinned task, summary, then the three kept turns' answers and the
        // inputs between them.
        assert_eq!(rebuilt[0].text().as_deref(), Some("the task"));
        assert_eq!(rebuilt[1].role(), "user");
        assert!(
            rebuilt[1]
                .text()
                .is_some_and(|text| text.contains("turn 1 to turn 5"))
        );
        assert_eq!(rebuilt[2].role(), "assistant");
        assert_eq!(rebuilt.len(), 2 + 5);
    }
}
