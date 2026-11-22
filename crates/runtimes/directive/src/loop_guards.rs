//! Detection of a run that repeats itself or stops making progress.
//!
//! The detectors are pure functions over per-turn digests, which are plain
//! data built from the recorded turns. They carry hashes, never content, and
//! they hold no state of their own: a recovered run rebuilds the same digests
//! from its record and reaches the same findings. The runner decides what a
//! finding means (warn, then stop); hooks can later observe a [`Finding`]
//! without changing this module.

use crate::directive::{LoopGuardsConfig, ProviderMessage};
use ryeos_provider_contract::{Part, Role};
use std::collections::BTreeSet;

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
                        calls.push((call.name.clone(), lillux::sha256_hex(args.as_bytes())));
                        ids.push(call.call_id.clone());
                    }
                }
                let text = message.joined_text();
                digests.push(TurnDigest {
                    turn: digests.len() as u32 + 1,
                    results: vec![String::new(); calls.len()],
                    calls,
                    text: (!text.trim().is_empty()).then(|| lillux::sha256_hex(text.as_bytes())),
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
                        digest.results[index] = lillux::sha256_hex(text.as_bytes());
                    }
                }
            }
            Role::System | Role::User => {}
        }
    }
    digests
}

/// One finished turn, reduced to hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnDigest {
    pub turn: u32,
    /// Each call as (tool name, hash of its arguments), in call order.
    pub calls: Vec<(String, String)>,
    /// The hash of each call's result text, in call order.
    pub results: Vec<String>,
    /// The hash of the assistant's text, when it wrote any.
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    Repetition,
    PingPong,
    NoProgress,
    Idle,
}

impl FindingKind {
    pub const ALL: [FindingKind; 4] = [
        Self::Repetition,
        Self::PingPong,
        Self::NoProgress,
        Self::Idle,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::Repetition => "repetition",
            Self::PingPong => "ping_pong",
            Self::NoProgress => "no_progress",
            Self::Idle => "idle",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.code() == code)
    }
}

/// A detector's verdict: which pattern and the turns it spans. No content.
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

fn call_keys(digest: &TurnDigest) -> Vec<String> {
    digest
        .calls
        .iter()
        .enumerate()
        .map(|(index, (tool, args))| {
            let result = digest.results.get(index).map_or("", String::as_str);
            format!("{tool}|{args}|{result}")
        })
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
    let mut seen: std::collections::BTreeMap<String, (u32, u32, u32)> = Default::default();
    for digest in window {
        for key in call_keys(digest) {
            if !latest.contains(&key) {
                continue;
            }
            let entry = seen.entry(key).or_insert((0, digest.turn, digest.turn));
            entry.0 += 1;
            entry.2 = digest.turn;
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
fn signature(digest: &TurnDigest) -> String {
    digest
        .calls
        .iter()
        .map(|(tool, args)| format!("{tool}|{args}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// An alternation of distinct calling turns, repeated `pingpong_cycles` whole
/// cycles, whose later cycles brought no result the first cycle did not.
fn ping_pong(digests: &[TurnDigest], config: &LoopGuardsConfig) -> Option<Finding> {
    for period in 2..=config.pingpong_max_period as usize {
        let needed = period * config.pingpong_cycles as usize;
        if digests.len() < needed || needed > config.window_turns as usize {
            continue;
        }
        let window = &digests[digests.len() - needed..];
        let signatures: Vec<String> = window.iter().map(signature).collect();
        let cycle = &signatures[..period];
        if cycle.iter().any(String::is_empty) {
            continue;
        }
        let distinct: BTreeSet<&String> = cycle.iter().collect();
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
        let first_results: BTreeSet<&String> = window[..period]
            .iter()
            .flat_map(|digest| digest.results.iter())
            .collect();
        let nothing_new = window[period..]
            .iter()
            .flat_map(|digest| digest.results.iter())
            .all(|result| first_results.contains(result));
        if nothing_new {
            return Some(Finding {
                kind: FindingKind::PingPong,
                first_turn: window[0].turn,
                last_turn: window[needed - 1].turn,
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
    let (earlier, window) = digests.split_at(digests.len() - span);
    let mut known: BTreeSet<&String> = earlier
        .iter()
        .flat_map(|digest| digest.results.iter().chain(digest.text.iter()))
        .collect();
    for digest in window {
        let items: Vec<&String> = digest.results.iter().chain(digest.text.iter()).collect();
        if items.is_empty() || items.iter().any(|item| !known.contains(*item)) {
            return None;
        }
        known.extend(items);
    }
    Some(Finding {
        kind: FindingKind::NoProgress,
        first_turn: window[0].turn,
        last_turn: window[span - 1].turn,
    })
}

/// `idle_turns` consecutive turns with neither a call nor text.
fn idle(digests: &[TurnDigest], config: &LoopGuardsConfig) -> Option<Finding> {
    let span = config.idle_turns as usize;
    if digests.len() < span {
        return None;
    }
    let window = &digests[digests.len() - span..];
    window
        .iter()
        .all(|digest| digest.calls.is_empty() && digest.text.is_none())
        .then(|| Finding {
            kind: FindingKind::Idle,
            first_turn: window[0].turn,
            last_turn: window[span - 1].turn,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(turn: u32, tool: &str, args: &str, result: &str) -> TurnDigest {
        TurnDigest {
            turn,
            calls: vec![(tool.to_string(), args.to_string())],
            results: vec![result.to_string()],
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
        let digests: Vec<_> = (1..=5).map(|turn| call(turn, "read", "a", "r")).collect();
        let found = detect(&digests, &LoopGuardsConfig::default());
        assert_eq!(found[0].kind, FindingKind::Repetition);
        assert_eq!((found[0].first_turn, found[0].last_turn), (1, 5));
        // One fewer is not enough.
        assert!(!kinds(&digests[..4]).contains(&FindingKind::Repetition));
    }

    #[test]
    fn the_same_call_with_changing_results_is_not_a_repetition() {
        let digests: Vec<_> = (1..=8)
            .map(|turn| call(turn, "poll", "a", &format!("r{turn}")))
            .collect();
        assert!(kinds(&digests).is_empty());
    }

    #[test]
    fn an_alternation_with_nothing_new_is_a_ping_pong() {
        let digests: Vec<_> = (1..=8)
            .map(|turn| {
                if turn % 2 == 1 {
                    call(turn, "edit", "x", "r1")
                } else {
                    call(turn, "test", "y", "r2")
                }
            })
            .collect();
        let found = kinds(&digests);
        assert!(found.contains(&FindingKind::PingPong), "{found:?}");
    }

    #[test]
    fn an_alternation_that_keeps_finding_new_results_is_not() {
        let digests: Vec<_> = (1..=8)
            .map(|turn| {
                let tool = if turn % 2 == 1 { "edit" } else { "test" };
                call(turn, tool, "x", &format!("r{turn}"))
            })
            .collect();
        assert!(!kinds(&digests).contains(&FindingKind::PingPong));
    }

    #[test]
    fn turns_that_only_show_what_was_seen_are_no_progress() {
        let mut digests: Vec<_> = (1..=3)
            .map(|turn| call(turn, "read", &format!("f{turn}"), &format!("r{turn}")))
            .collect();
        // Eight more turns, each re-reading a result already seen.
        for turn in 4..=11 {
            digests.push(call(
                turn,
                "read",
                &format!("g{turn}"),
                &format!("r{}", (turn % 3) + 1),
            ));
        }
        let found = detect(&digests, &LoopGuardsConfig::default());
        assert!(
            found.iter().any(|f| f.kind == FindingKind::NoProgress),
            "{found:?}"
        );
        // One new result inside the window ends it.
        digests[9] = call(10, "read", "h", "fresh");
        assert!(!kinds(&digests).contains(&FindingKind::NoProgress));
    }

    #[test]
    fn turns_with_neither_a_call_nor_text_are_idle() {
        let empty = |turn| TurnDigest {
            turn,
            calls: vec![],
            results: vec![],
            text: None,
        };
        let digests = vec![call(1, "read", "a", "r"), empty(2), empty(3), empty(4)];
        assert!(kinds(&digests).contains(&FindingKind::Idle));
        let mut spoke = digests.clone();
        spoke[3].text = Some("hello".to_string());
        assert!(!kinds(&spoke).contains(&FindingKind::Idle));
    }

    #[test]
    fn a_disabled_guard_finds_nothing() {
        let digests: Vec<_> = (1..=10).map(|turn| call(turn, "read", "a", "r")).collect();
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
        assert_eq!(digests[0].turn, 1);
        assert_eq!(digests[0].calls, digests[1].calls);
        assert_eq!(digests[0].results, digests[1].results);
        assert!(digests[0].text.is_none());
        assert!(digests[2].calls.is_empty() && digests[2].text.is_some());
        // Rebuilding from the same messages gives the same digests.
        assert_eq!(digests_from_messages(&messages), digests);
    }

    #[test]
    fn finding_codes_round_trip() {
        for kind in FindingKind::ALL {
            assert_eq!(FindingKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(FindingKind::from_code("other"), None);
    }
}
