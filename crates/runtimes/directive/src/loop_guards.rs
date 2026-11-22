//! Detection of a run that repeats itself or stops making progress.
//!
//! The detectors are pure functions over per-turn digests, which are plain
//! data built from the recorded turns. They carry fingerprints, never content,
//! and they hold no state of their own: a recovered run rebuilds the same
//! digests from its record and reaches the same findings. A compaction replaces
//! old turns by a summary, so it carries the digests the detectors still need
//! in its record (`GuardCarry`); the runner detects over those followed by the
//! digests of the live conversation. The runner decides what a finding means
//! (warn, then stop); hooks can later observe a [`Finding`] without changing
//! this module.

use crate::directive::{LoopGuardsConfig, ProviderMessage};
use ryeos_provider_contract::{Part, Role};
use ryeos_runtime::cognition::{GuardCall, GuardCarry, GuardTurn, GuardWrapUp};
use std::collections::{BTreeMap, BTreeSet};

pub use ryeos_runtime::cognition::FindingKind;

/// One finished turn reduced to fingerprints.
pub type TurnDigest = GuardTurn;

/// The findings the model was warned about, each with the position of its
/// warning: the number of turns the history held when the warning was given.
/// The history is the carried digests followed by the live conversation.
pub type WarnedAt = BTreeMap<FindingKind, u32>;

/// A 48-bit fingerprint of some text: the first 12 hex digits of its SHA-256.
pub fn fingerprint(text: &str) -> u64 {
    let hex = lillux::sha256_hex(text.as_bytes());
    u64::from_str_radix(&hex[..12], 16).unwrap_or(0)
}

/// The digests of the turns a conversation holds: one per assistant message,
/// with the results of the tool messages that answer its calls. Built from
/// the messages alone, so a recovered run derives the same digests the live
/// run had.
pub fn digests_from_messages(messages: &[ProviderMessage]) -> Vec<TurnDigest> {
    let mut digests: Vec<TurnDigest> = Vec::new();
    let mut call_ids: Vec<Vec<String>> = Vec::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                let mut calls = Vec::new();
                let mut ids = Vec::new();
                for part in &message.parts {
                    if let Part::ToolCall(call) = part {
                        let args = serde_json::to_string(&call.arguments).unwrap_or_default();
                        calls.push(GuardCall {
                            signature: fingerprint(&format!("{}|{args}", call.name)),
                            // Unanswered until a tool message answers it.
                            result: 0,
                        });
                        ids.push(call.call_id.clone());
                    }
                }
                let text = message.joined_text();
                digests.push(GuardTurn {
                    calls,
                    text: (!text.trim().is_empty()).then(|| fingerprint(&text)),
                });
                call_ids.push(ids);
            }
            Role::Tool => {
                let (Some(digest), Some(ids)) = (digests.last_mut(), call_ids.last()) else {
                    continue;
                };
                for part in &message.parts {
                    let Part::ToolResult(result) = part else {
                        continue;
                    };
                    let text: String = result
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            Part::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect();
                    if let Some(index) = ids.iter().position(|id| id == &result.call_id) {
                        digest.calls[index].result = fingerprint(&text);
                    }
                }
            }
            Role::System | Role::User => {}
        }
    }
    digests
}

/// How many turns of history the detectors and the grace that follows a
/// warning can reach back over: the longest window any detector needs, plus
/// the grace turns. A compaction carries this many replaced turns. The
/// no-progress detector looks at `no_progress_turns` turns against the ones
/// before them, so it needs one more.
pub fn carry_len(config: &LoopGuardsConfig) -> u32 {
    let longest = [
        config.window_turns,
        config.no_progress_turns.saturating_add(1),
        config
            .pingpong_cycles
            .saturating_mul(config.pingpong_max_period),
        config.repeat_threshold,
        config.idle_turns,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    longest.saturating_add(config.grace_turns)
}

/// The history the detectors read: the carried digests, then the live
/// conversation's.
pub fn history(prefix: &[TurnDigest], conversation: &[TurnDigest]) -> Vec<TurnDigest> {
    prefix.iter().chain(conversation).cloned().collect()
}

/// The guard state a compaction record carries, built from the runner's state
/// as the compaction is about to replace the first `replaced_turns` turns of
/// `conversation`. `prefix` is the history already carried before it.
/// `kept_bound` bounds how far back a warning age may reach over the kept turns
/// (see `CompactionPayload::validate`).
pub fn build_carry(
    config: &LoopGuardsConfig,
    prefix: &[TurnDigest],
    conversation: &[TurnDigest],
    replaced_turns: usize,
    warned: &WarnedAt,
    wrap_up: GuardWrapUp,
    kept_bound: u32,
) -> GuardCarry {
    let carry = carry_len(config);
    let full = history(prefix, conversation);
    let replaced = (prefix.len() + replaced_turns).min(full.len());
    let kept = full.len() - replaced;
    let start = replaced.saturating_sub(carry as usize);
    let recent: Vec<TurnDigest> = full[start..replaced].to_vec();
    let reach = u32::try_from(recent.len())
        .unwrap_or(u32::MAX)
        .saturating_add(u32::try_from(kept).unwrap_or(u32::MAX).min(kept_bound));
    let loop_warned_turns_ago = warned
        .iter()
        .map(|(kind, position)| {
            let ago = u32::try_from(full.len())
                .unwrap_or(u32::MAX)
                .saturating_sub(*position);
            (*kind, ago.min(reach))
        })
        .collect();
    GuardCarry {
        carry_len: carry,
        recent_digests: recent,
        wrap_up,
        loop_warned_turns_ago,
    }
}

/// The position, in the history of the carried digests followed by the live
/// conversation, of each warning a record carries. `kept_at_record` is the
/// number of turns the record kept whole.
pub fn warned_from_carry(carry: &GuardCarry, kept_at_record: u32) -> WarnedAt {
    let end = u32::try_from(carry.recent_digests.len())
        .unwrap_or(u32::MAX)
        .saturating_add(kept_at_record);
    carry
        .loop_warned_turns_ago
        .iter()
        .map(|(kind, ago)| (*kind, end.saturating_sub(*ago)))
        .collect()
}

/// A detector's verdict: which pattern and the turns it spans, numbered from 1
/// over the history. No content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    pub first_turn: u32,
    pub last_turn: u32,
}

/// Every pattern the digests show, in `FindingKind` order.
pub fn detect(digests: &[TurnDigest], config: &LoopGuardsConfig) -> Vec<Finding> {
    if !config.enabled {
        return Vec::new();
    }
    [
        repetition(digests, config),
        ping_pong(digests, config),
        no_progress(digests, config),
        idle(digests, config),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The 1-based turn number of `digests[index]`.
fn turn_number(index: usize) -> u32 {
    u32::try_from(index + 1).unwrap_or(u32::MAX)
}

fn call_keys(digest: &TurnDigest) -> Vec<(u64, u64)> {
    digest
        .calls
        .iter()
        .map(|call| (call.signature, call.result))
        .collect()
}

fn tail<T>(items: &[T], count: u32) -> &[T] {
    let count = count as usize;
    &items[items.len().saturating_sub(count)..]
}

/// The same call, with the same result, `repeat_threshold` times in the window,
/// including in the latest turn: a run that has since changed course is not
/// repeating itself.
fn repetition(digests: &[TurnDigest], config: &LoopGuardsConfig) -> Option<Finding> {
    let latest = call_keys(digests.last()?);
    let window = tail(digests, config.window_turns);
    let first_index = digests.len() - window.len();
    let mut seen: BTreeMap<(u64, u64), (u32, u32, u32)> = Default::default();
    for (offset, digest) in window.iter().enumerate() {
        let turn = turn_number(first_index + offset);
        for key in call_keys(digest) {
            if !latest.contains(&key) {
                continue;
            }
            let entry = seen.entry(key).or_insert((0, turn, turn));
            entry.0 += 1;
            entry.2 = turn;
        }
    }
    seen.into_values()
        .filter(|(count, _, _)| *count >= config.repeat_threshold)
        .map(|(_, first, last)| Finding {
            kind: FindingKind::Repetition,
            first_turn: first,
            last_turn: last,
        })
        .next()
}

/// A turn's calls without their results, the unit of an alternation.
fn signature(digest: &TurnDigest) -> Vec<u64> {
    digest.calls.iter().map(|call| call.signature).collect()
}

fn results(digest: &TurnDigest) -> impl Iterator<Item = u64> + '_ {
    digest.calls.iter().map(|call| call.result)
}

/// An alternation of distinct calling turns, repeated `pingpong_cycles` whole
/// cycles, whose later cycles brought no result the first cycle did not.
fn ping_pong(digests: &[TurnDigest], config: &LoopGuardsConfig) -> Option<Finding> {
    for period in 2..=config.pingpong_max_period as usize {
        let needed = period * config.pingpong_cycles as usize;
        if digests.len() < needed || needed > config.window_turns as usize {
            continue;
        }
        let first_index = digests.len() - needed;
        let window = &digests[first_index..];
        let signatures: Vec<Vec<u64>> = window.iter().map(signature).collect();
        let cycle = &signatures[..period];
        if cycle.iter().any(Vec::is_empty) {
            continue;
        }
        let distinct: BTreeSet<&Vec<u64>> = cycle.iter().collect();
        if distinct.len() != period {
            continue;
        }
        if !signatures
            .iter()
            .enumerate()
            .all(|(index, signature)| signature == &cycle[index % period])
        {
            continue;
        }
        let first_results: BTreeSet<u64> = window[..period].iter().flat_map(results).collect();
        let nothing_new = window[period..]
            .iter()
            .flat_map(results)
            .all(|result| first_results.contains(&result));
        if nothing_new {
            return Some(Finding {
                kind: FindingKind::PingPong,
                first_turn: turn_number(first_index),
                last_turn: turn_number(digests.len() - 1),
            });
        }
    }
    None
}

/// `no_progress_turns` consecutive turns that did something, every result and
/// text of which was seen in an earlier turn.
fn no_progress(digests: &[TurnDigest], config: &LoopGuardsConfig) -> Option<Finding> {
    let span = config.no_progress_turns as usize;
    if digests.len() <= span {
        return None;
    }
    let split = digests.len() - span;
    let (earlier, window) = digests.split_at(split);
    let mut known: BTreeSet<u64> = earlier
        .iter()
        .flat_map(|digest| results(digest).chain(digest.text))
        .collect();
    for digest in window {
        let items: Vec<u64> = results(digest).chain(digest.text).collect();
        if items.is_empty() || items.iter().any(|item| !known.contains(item)) {
            return None;
        }
        known.extend(items);
    }
    Some(Finding {
        kind: FindingKind::NoProgress,
        first_turn: turn_number(split),
        last_turn: turn_number(digests.len() - 1),
    })
}

/// `idle_turns` consecutive turns with neither a call nor text.
fn idle(digests: &[TurnDigest], config: &LoopGuardsConfig) -> Option<Finding> {
    let span = config.idle_turns as usize;
    if digests.len() < span {
        return None;
    }
    let first_index = digests.len() - span;
    let window = &digests[first_index..];
    window
        .iter()
        .all(|digest| digest.calls.is_empty() && digest.text.is_none())
        .then(|| Finding {
            kind: FindingKind::Idle,
            first_turn: turn_number(first_index),
            last_turn: turn_number(digests.len() - 1),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::ProviderMessageExt;

    fn call(tool: &str, args: &str, result: &str) -> TurnDigest {
        GuardTurn {
            calls: vec![GuardCall {
                signature: fingerprint(&format!("{tool}|{args}")),
                result: fingerprint(result),
            }],
            text: None,
        }
    }

    fn kinds(digests: &[TurnDigest]) -> Vec<FindingKind> {
        detect(digests, &LoopGuardsConfig::default())
            .into_iter()
            .map(|finding| finding.kind)
            .collect()
    }

    #[test]
    fn the_same_call_and_result_repeated_is_found() {
        let digests: Vec<_> = (1..=5).map(|_| call("read", "a", "r")).collect();
        let found = detect(&digests, &LoopGuardsConfig::default());
        assert_eq!(found[0].kind, FindingKind::Repetition);
        assert_eq!((found[0].first_turn, found[0].last_turn), (1, 5));
        // One fewer is not enough.
        assert!(!kinds(&digests[..4]).contains(&FindingKind::Repetition));
    }

    #[test]
    fn the_same_call_with_changing_results_is_not_a_repetition() {
        let digests: Vec<_> = (1..=8)
            .map(|turn| call("poll", "a", &format!("r{turn}")))
            .collect();
        assert!(kinds(&digests).is_empty());
    }

    fn alternation(turns: std::ops::RangeInclusive<u32>) -> Vec<TurnDigest> {
        turns
            .map(|turn| {
                if turn % 2 == 1 {
                    call("edit", "x", "r1")
                } else {
                    call("test", "y", "r2")
                }
            })
            .collect()
    }

    #[test]
    fn an_alternation_with_nothing_new_is_a_ping_pong() {
        let found = kinds(&alternation(1..=8));
        assert!(found.contains(&FindingKind::PingPong), "{found:?}");
    }

    #[test]
    fn an_alternation_that_keeps_finding_new_results_is_not() {
        let digests: Vec<_> = (1..=8)
            .map(|turn| {
                let tool = if turn % 2 == 1 { "edit" } else { "test" };
                call(tool, "x", &format!("r{turn}"))
            })
            .collect();
        assert!(!kinds(&digests).contains(&FindingKind::PingPong));
    }

    fn no_progress_run() -> Vec<TurnDigest> {
        let mut digests: Vec<_> = (1..=3)
            .map(|turn| call("read", &format!("f{turn}"), &format!("r{turn}")))
            .collect();
        // Eight more turns, each re-reading a result already seen.
        for turn in 4..=11 {
            digests.push(call(
                "read",
                &format!("g{turn}"),
                &format!("r{}", (turn % 3) + 1),
            ));
        }
        digests
    }

    #[test]
    fn turns_that_only_show_what_was_seen_are_no_progress() {
        let mut digests = no_progress_run();
        let found = detect(&digests, &LoopGuardsConfig::default());
        assert!(
            found.iter().any(|f| f.kind == FindingKind::NoProgress),
            "{found:?}"
        );
        // One new result inside the window ends it.
        digests[9] = call("read", "h", "fresh");
        assert!(!kinds(&digests).contains(&FindingKind::NoProgress));
    }

    #[test]
    fn turns_with_neither_a_call_nor_text_are_idle() {
        let empty = || GuardTurn {
            calls: vec![],
            text: None,
        };
        let digests = vec![call("read", "a", "r"), empty(), empty(), empty()];
        assert!(kinds(&digests).contains(&FindingKind::Idle));
        let mut spoke = digests.clone();
        spoke[3].text = Some(fingerprint("hello"));
        assert!(!kinds(&spoke).contains(&FindingKind::Idle));
    }

    #[test]
    fn corrected_empty_replies_leave_no_digest_so_idle_does_not_fire() {
        let task = ProviderMessage::user("Do task");
        let empty = || ProviderMessage::new(Role::Assistant, vec![]);
        // Without a correction the three empty replies stay in the
        // conversation and the guard sees them.
        let uncorrected = vec![task.clone(), empty(), empty(), empty()];
        assert!(
            detect(
                &digests_from_messages(&uncorrected),
                &LoopGuardsConfig::default()
            )
            .iter()
            .any(|finding| finding.kind == FindingKind::Idle)
        );
        // With the policy each empty reply was replaced by the correction, a
        // user message, so no digest records it.
        let corrected = vec![
            task,
            ProviderMessage::user("answer or call"),
            ProviderMessage::user("answer or call"),
            ProviderMessage::user("answer or call"),
        ];
        assert!(digests_from_messages(&corrected).is_empty());
        assert!(
            detect(
                &digests_from_messages(&corrected),
                &LoopGuardsConfig::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn a_disabled_guard_finds_nothing() {
        let digests: Vec<_> = (1..=10).map(|_| call("read", "a", "r")).collect();
        let config = LoopGuardsConfig {
            enabled: false,
            ..LoopGuardsConfig::default()
        };
        assert!(detect(&digests, &config).is_empty());
    }

    fn assistant_calling(id: &str, tool: &str, args: serde_json::Value) -> ProviderMessage {
        ProviderMessage::new(Role::Assistant, vec![Part::tool_call(id, tool, args)])
    }

    fn tool_result(id: &str, text: &str) -> ProviderMessage {
        ProviderMessage::new(
            Role::Tool,
            vec![Part::ToolResult(ryeos_provider_contract::ToolResultPart {
                call_id: id.to_string(),
                parts: vec![Part::text(text)],
                status: ryeos_provider_contract::ToolResultStatus::Ok,
            })],
        )
    }

    #[test]
    fn digests_come_from_the_messages_alone() {
        let messages = vec![
            ProviderMessage::text(Role::User, "go"),
            assistant_calling("c1", "read", serde_json::json!({"p": "a"})),
            tool_result("c1", "same"),
            assistant_calling("c2", "read", serde_json::json!({"p": "a"})),
            tool_result("c2", "same"),
            ProviderMessage::text(Role::Assistant, "done"),
        ];
        let digests = digests_from_messages(&messages);
        assert_eq!(digests.len(), 3);
        assert_eq!(digests[0].calls, digests[1].calls);
        assert!(digests[0].text.is_none());
        assert!(digests[2].calls.is_empty() && digests[2].text.is_some());
        // Rebuilding from the same messages gives the same digests.
        assert_eq!(digests_from_messages(&messages), digests);
        // Every fingerprint fits the record's bound.
        assert!(digests[0].calls[0].result < ryeos_runtime::GUARD_FINGERPRINT_BOUND);
    }

    #[test]
    fn finding_codes_round_trip() {
        for kind in FindingKind::ALL {
            assert_eq!(FindingKind::from_code(kind.code()), Some(kind));
            // The wire form is the code string, so the enum and the code agree.
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.code())
            );
        }
        assert_eq!(FindingKind::from_code("other"), None);
        assert!(serde_json::from_value::<FindingKind>(serde_json::json!("other")).is_err());
    }

    fn config(window: u32, no_progress: u32, grace: u32) -> LoopGuardsConfig {
        LoopGuardsConfig {
            window_turns: window,
            no_progress_turns: no_progress,
            grace_turns: grace,
            ..LoopGuardsConfig::default()
        }
    }

    #[test]
    fn carry_len_is_the_longest_window_plus_the_grace() {
        // (config, expected)
        let table = [
            // Defaults: window 16, ping-pong 4 x 4 = 16, no-progress 8 + 1, grace 2.
            (LoopGuardsConfig::default(), 18),
            // A wide window wins.
            (config(40, 8, 2), 42),
            // The no-progress span plus one wins over a small window.
            (
                LoopGuardsConfig {
                    window_turns: 16,
                    no_progress_turns: 30,
                    ..LoopGuardsConfig::default()
                },
                33,
            ),
            // Grace is added whichever is longest.
            (config(16, 8, 7), 23),
            // Ping-pong cycles x period can be the longest.
            (
                LoopGuardsConfig {
                    window_turns: 30,
                    pingpong_cycles: 5,
                    pingpong_max_period: 6,
                    ..LoopGuardsConfig::default()
                },
                32,
            ),
        ];
        for (config, expected) in table {
            assert_eq!(carry_len(&config), expected, "{config:?}");
        }
    }

    /// Detect as the runner does: over the carried prefix, then the live turns.
    fn detect_over(
        prefix: &[TurnDigest],
        live: &[TurnDigest],
        config: &LoopGuardsConfig,
    ) -> Vec<Finding> {
        detect(&history(prefix, live), config)
    }

    #[test]
    fn a_ping_pong_spanning_a_compaction_fires_on_the_same_turn() {
        let config = LoopGuardsConfig::default();
        let all = alternation(1..=16);
        // Without a compaction the alternation first fires at the 8th turn
        // (four cycles of period 2).
        let first_fire = (1..=all.len())
            .find(|n| {
                detect(&all[..*n], &config)
                    .iter()
                    .any(|f| f.kind == FindingKind::PingPong)
            })
            .unwrap();
        // A compaction after turn 5 replaces 2 turns and keeps 3.
        let replaced = 2;
        let carry = build_carry(
            &config,
            &[],
            &all[..5],
            replaced,
            &WarnedAt::new(),
            GuardWrapUp::default(),
            u32::MAX,
        );
        assert_eq!(carry.recent_digests, all[..replaced]);
        // The live conversation after it: the kept turns, then the rest.
        let fire_with_carry = (3..=(all.len() - replaced))
            .find(|live_turns| {
                let live = &all[replaced..replaced + live_turns];
                detect_over(&carry.recent_digests, live, &config)
                    .iter()
                    .any(|f| f.kind == FindingKind::PingPong)
            })
            .map(|live_turns| live_turns + replaced)
            .unwrap();
        assert_eq!(fire_with_carry, first_fire);
        // Without the carried digests the conversation alone has not fired by
        // then.
        let live_only = &all[replaced..first_fire];
        assert!(
            !detect(live_only, &config)
                .iter()
                .any(|f| f.kind == FindingKind::PingPong)
        );
    }

    #[test]
    fn a_no_progress_run_spanning_a_compaction_fires_on_the_same_turn() {
        let config = LoopGuardsConfig::default();
        let all = no_progress_run();
        let first_fire = (1..=all.len())
            .find(|n| {
                detect(&all[..*n], &config)
                    .iter()
                    .any(|f| f.kind == FindingKind::NoProgress)
            })
            .unwrap();
        // The compaction replaces the first 4 turns of the 11.
        let replaced = 4;
        let carry = build_carry(
            &config,
            &[],
            &all,
            replaced,
            &WarnedAt::new(),
            GuardWrapUp::default(),
            u32::MAX,
        );
        let fire_with_carry = (1..=(all.len() - replaced))
            .find(|live_turns| {
                detect_over(
                    &carry.recent_digests,
                    &all[replaced..replaced + live_turns],
                    &config,
                )
                .iter()
                .any(|f| f.kind == FindingKind::NoProgress)
            })
            .map(|live_turns| live_turns + replaced)
            .unwrap();
        assert_eq!(fire_with_carry, first_fire);
    }

    #[test]
    fn the_carry_keeps_the_newest_replaced_turns_up_to_carry_len() {
        let config = config(16, 8, 2);
        let all: Vec<_> = (0..40).map(|n| call("t", &n.to_string(), "r")).collect();
        let carry = build_carry(
            &config,
            &[],
            &all,
            30,
            &WarnedAt::new(),
            GuardWrapUp::default(),
            u32::MAX,
        );
        assert_eq!(carry.carry_len, 18);
        assert_eq!(carry.recent_digests, all[12..30]);
        // A second compaction extends the history it already carried.
        let second = build_carry(
            &config,
            &carry.recent_digests,
            &all[30..],
            4,
            &WarnedAt::new(),
            GuardWrapUp::default(),
            u32::MAX,
        );
        let expected: Vec<_> = carry.recent_digests[4..]
            .iter()
            .chain(&all[30..34])
            .cloned()
            .collect();
        assert_eq!(second.recent_digests, expected);
    }

    #[test]
    fn a_warning_keeps_its_position_across_a_compaction() {
        let config = config(16, 1, 5);
        let all: Vec<_> = (0..5).map(|n| call("t", &n.to_string(), "r")).collect();
        let mut warned = WarnedAt::new();
        // Warned when the history held 2 turns; 5 turns now.
        warned.insert(FindingKind::NoProgress, 2);
        // The compaction replaces 2 turns and keeps 3.
        let carry = build_carry(&config, &[], &all, 2, &warned, GuardWrapUp::default(), 3);
        assert_eq!(carry.loop_warned_turns_ago[&FindingKind::NoProgress], 3);
        // Read back with the 3 kept turns: position 2 + 3 - 3 = 2, unchanged,
        // so the stop lands where it would have: at position + grace.
        assert_eq!(warned_from_carry(&carry, 3), warned);
    }

    #[test]
    fn a_warning_older_than_the_carried_history_is_clamped_not_lost() {
        let config = config(16, 1, 5);
        let all: Vec<_> = (0..40).map(|n| call("t", &n.to_string(), "r")).collect();
        let mut warned = WarnedAt::new();
        warned.insert(FindingKind::Idle, 1);
        let carry = build_carry(&config, &[], &all, 30, &warned, GuardWrapUp::default(), 10);
        // 21 recent digests, 10 kept turns at most.
        assert_eq!(carry.recent_digests.len(), 21);
        assert_eq!(carry.loop_warned_turns_ago[&FindingKind::Idle], 31);
        assert!(carry.validate(10).is_ok());
    }
}
