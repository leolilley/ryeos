//! Generic replay of captured reasoning onto later requests, driven only by
//! the signed `schemas.reasoning.replay` placements. Nothing here knows a
//! field name or a provider: a placement names the capture it replays, the
//! key it writes, the messages it applies to and what to do when a message
//! that owes the value does not hold it.
//!
//! An artefact travels as the envelope the capture side wrote:
//! `{"capture": <rule id>, "group": <compat group>, "value": <captured JSON>}`.
//! It replays only when its capture id matches the placement and its group
//! equals the compat group the active entry declares, which is what keeps one
//! provider's signed blocks from reaching another.

use ryeos_directive_definition::{
    OnMissing, ReasoningCaptureRule, ReasoningSchemaConfig, ReplayPlacement, ReplayScope,
    ReplayTarget,
};
use ryeos_provider_contract::{Artefact, Part, ReasoningPart, Role, ToolCallPart};
use serde_json::{Value, json};

use crate::directive::{ProviderMessage, ProviderMessageExt};

/// Stable failure code for a turn that owes a replayable value and has none.
pub const ARTEFACT_MISSING_CODE: &str = "reasoning_artefact_missing";

/// A message in scope that proposes a tool call and holds no artefact for a
/// placement whose `on_missing` is `refuse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingArtefact {
    pub message_index: usize,
    pub capture_id: String,
    pub key: String,
}

impl std::fmt::Display for MissingArtefact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{ARTEFACT_MISSING_CODE}: assistant message {} proposes a tool call but holds no \
             replayable artefact of capture {:?} for field {:?}; the signed provider entry \
             declares on_missing refuse",
            self.message_index, self.capture_id, self.key
        )
    }
}

impl std::error::Error for MissingArtefact {}

/// The index of the first message of the open loop: the one after the last
/// user message, or 0 when there is none.
pub fn open_loop_start(messages: &[ProviderMessage]) -> usize {
    messages
        .iter()
        .rposition(|message| message.role == Role::User)
        .map_or(0, |index| index + 1)
}

/// The first message that owes a replayable value under a `refuse`
/// placement and holds none. `None` when no placement refuses or every owed
/// value is present.
pub fn owed_missing(
    messages: &[ProviderMessage],
    config: &ReasoningSchemaConfig,
) -> Option<MissingArtefact> {
    let plan = ReplayPlan::new(messages, config)?;
    messages.iter().enumerate().find_map(|(index, message)| {
        plan.placements().find_map(|(placement, rule)| {
            let refuses = matches!(placement.on_missing, OnMissing::Refuse);
            (refuses
                && plan.owes(index, message, placement)
                && plan.is_missing(message, placement, rule))
            .then(|| MissingArtefact {
                message_index: index,
                capture_id: placement.from.clone(),
                key: placement.into.key().to_string(),
            })
        })
    })
}

/// Whether a freshly completed assistant `message`, about to follow
/// `history`, owes a replayable value under a `refuse` placement and holds
/// none. This is the turn-completion check: it runs before anything the turn
/// proposed is dispatched.
pub fn owed_missing_in_turn(
    history: &[ProviderMessage],
    message: &ProviderMessage,
    config: &ReasoningSchemaConfig,
) -> Option<MissingArtefact> {
    let plan = ReplayPlan::new(history, config)?;
    let index = history.len();
    plan.placements().find_map(|(placement, rule)| {
        let refuses = matches!(placement.on_missing, OnMissing::Refuse);
        (refuses
            && plan.owes(index, message, placement)
            && plan.is_missing(message, placement, rule))
        .then(|| MissingArtefact {
            message_index: index,
            capture_id: placement.from.clone(),
            key: placement.into.key().to_string(),
        })
    })
}

/// The bytes of reasoning values a request carries because of the declared
/// placements: every placement key found on a rendered assistant message or
/// on one of its rendered tool calls.
pub fn replayed_bytes(converted: &[Value], config: &ReasoningSchemaConfig) -> u64 {
    let Some(replay) = config.replay.as_ref() else {
        return 0;
    };
    let len = |value: &Value| serde_json::to_vec(value).map_or(0, |bytes| bytes.len() as u64);
    converted
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .map(|message| {
            replay
                .placements
                .iter()
                .map(|placement| match &placement.into {
                    ReplayTarget::MessageField(key) => message.get(key).map_or(0, len),
                    ReplayTarget::ToolCallField(key) => message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .map_or(0, |calls| {
                            calls.iter().filter_map(|call| call.get(key)).map(len).sum()
                        }),
                })
                .sum::<u64>()
        })
        .sum()
}

/// The placements of one request, resolved against its history.
pub struct ReplayPlan<'a> {
    config: &'a ReasoningSchemaConfig,
    group: Option<&'a str>,
    open_loop_start: usize,
}

impl<'a> ReplayPlan<'a> {
    /// `None` when the entry declares no replay.
    pub fn new(messages: &[ProviderMessage], config: &'a ReasoningSchemaConfig) -> Option<Self> {
        let replay = config.replay.as_ref()?;
        Some(Self {
            config,
            group: replay.compat_group.as_deref(),
            open_loop_start: open_loop_start(messages),
        })
    }

    fn placements(&self) -> impl Iterator<Item = (&'a ReplayPlacement, &'a ReasoningCaptureRule)> {
        let config = self.config;
        config.replay.iter().flat_map(move |replay| {
            replay.placements.iter().filter_map(move |placement| {
                config
                    .capture
                    .iter()
                    .find(|rule| rule.id() == placement.from)
                    .map(|rule| (placement, rule))
            })
        })
    }

    fn in_scope(&self, index: usize, message: &ProviderMessage, scope: ReplayScope) -> bool {
        if message.role != Role::Assistant {
            return false;
        }
        match scope {
            ReplayScope::All => true,
            ReplayScope::ToolCallTurns => message.has_tool_calls(),
            ReplayScope::OpenLoop => index >= self.open_loop_start,
        }
    }

    /// A message owes a placement's value when it is in scope and proposes a
    /// tool call.
    fn owes(&self, index: usize, message: &ProviderMessage, placement: &ReplayPlacement) -> bool {
        self.in_scope(index, message, placement.scope) && message.has_tool_calls()
    }

    /// The artefact value an envelope holds for `capture_id`, when the
    /// envelope was captured under this entry's compat group.
    fn value_of<'v>(&self, artefact: &'v Artefact, capture_id: &str) -> Option<&'v Value> {
        let envelope = artefact.0.as_object()?;
        let group = envelope.get("group")?.as_str()?;
        (envelope.get("capture")?.as_str()? == capture_id && Some(group) == self.group)
            .then(|| envelope.get("value"))
            .flatten()
    }

    fn message_values<'m>(&self, message: &'m ProviderMessage, capture_id: &str) -> Vec<&'m Value> {
        message
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Reasoning(ReasoningPart {
                    artefact: Some(artefact),
                    ..
                }) => self.value_of(artefact, capture_id),
                _ => None,
            })
            .collect()
    }

    fn call_values<'m>(
        &self,
        message: &'m ProviderMessage,
        capture_id: &str,
    ) -> Vec<Option<&'m Value>> {
        message
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolCall(ToolCallPart { artefact, .. }) => {
                    Some(artefact.as_ref().and_then(|a| self.value_of(a, capture_id)))
                }
                _ => None,
            })
            .collect()
    }

    fn is_missing(
        &self,
        message: &ProviderMessage,
        placement: &ReplayPlacement,
        rule: &ReasoningCaptureRule,
    ) -> bool {
        match rule {
            // Visible text is never owed.
            ReasoningCaptureRule::TextDelta { .. } => false,
            ReasoningCaptureRule::Blocks { .. } => {
                self.message_values(message, &placement.from).is_empty()
            }
            ReasoningCaptureRule::ToolCallField { .. } => self
                .call_values(message, &placement.from)
                .iter()
                .all(Option::is_none),
        }
    }

    /// Sets the tool-call placements on the rendered calls of one assistant
    /// message. `calls` holds one rendered call per call of the message, in
    /// order.
    pub fn apply_to_calls(&self, index: usize, message: &ProviderMessage, calls: &mut [Value]) {
        for (placement, rule) in self.placements() {
            let ReplayTarget::ToolCallField(key) = &placement.into else {
                continue;
            };
            if !self.in_scope(index, message, placement.scope) {
                continue;
            }
            let values = self.call_values(message, &placement.from);
            let mut set_any = false;
            for (call, value) in calls.iter_mut().zip(&values) {
                if let (Some(value), Some(call)) = (value, call.as_object_mut()) {
                    call.insert(key.clone(), (*value).clone());
                    set_any = true;
                }
            }
            if !set_any
                && self.owes(index, message, placement)
                && self.is_missing(message, placement, rule)
                && let OnMissing::Fill(fill) = &placement.on_missing
                && let Some(first) = calls.first_mut().and_then(Value::as_object_mut)
            {
                first.insert(key.clone(), fill.clone());
            }
        }
    }

    /// Sets the message-level placements on one rendered assistant message.
    pub fn apply_to_message(&self, index: usize, message: &ProviderMessage, rendered: &mut Value) {
        let Some(object) = rendered.as_object_mut() else {
            return;
        };
        for (placement, rule) in self.placements() {
            let ReplayTarget::MessageField(key) = &placement.into else {
                continue;
            };
            if !self.in_scope(index, message, placement.scope) {
                continue;
            }
            match rule {
                ReasoningCaptureRule::TextDelta { .. } => {
                    if let Some(text) = message.reasoning_text().filter(|text| !text.is_empty()) {
                        object.insert(key.clone(), json!(text));
                    }
                }
                ReasoningCaptureRule::Blocks { .. } => {
                    let values = self.message_values(message, &placement.from);
                    if !values.is_empty() {
                        object.insert(
                            key.clone(),
                            Value::Array(values.into_iter().cloned().collect()),
                        );
                    } else if self.owes(index, message, placement)
                        && let OnMissing::Fill(fill) = &placement.on_missing
                    {
                        object.insert(key.clone(), fill.clone());
                    }
                }
                ReasoningCaptureRule::ToolCallField { .. } => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_provider_contract::Role;

    #[test]
    fn a_fresh_turn_is_checked_against_the_loop_it_joins() {
        let history = [user("go")];
        let unsigned = assistant(vec![call_part("a", None)]);
        let missing = owed_missing_in_turn(&history, &unsigned, &claude()).expect("owed");
        assert_eq!(missing.message_index, 1);
        let signed = thinking_turn("a", "g1", "t");
        assert!(owed_missing_in_turn(&history, &signed, &claude()).is_none());
        let answer = assistant(vec![Part::text("done")]);
        assert!(owed_missing_in_turn(&history, &answer, &claude()).is_none());
    }

    #[test]
    fn replayed_bytes_count_only_declared_keys() {
        let rendered = json!([
            {"role": "user", "thinking_blocks": [1]},
            {"role": "assistant", "thinking_blocks": [{"a": 1}], "content": "x"}
        ]);
        let bytes = replayed_bytes(rendered.as_array().unwrap(), &claude());
        assert_eq!(bytes, br#"[{"a":1}]"#.len() as u64);
    }

    fn config(replay: Value, capture: Value) -> ReasoningSchemaConfig {
        serde_json::from_value(json!({"capture": capture, "replay": replay})).unwrap()
    }

    fn claude() -> ReasoningSchemaConfig {
        config(
            json!({
                "compat_group": "g1",
                "placements": [{
                    "from": "blocks", "into": {"message_field": "thinking_blocks"},
                    "scope": "open_loop", "on_missing": "refuse"
                }]
            }),
            json!([
                {"id": "text", "kind": "text_delta", "fields": ["reasoning_content"]},
                {"id": "blocks", "kind": "blocks", "field": "thinking_blocks",
                 "merge_keys": ["thinking"], "close_key": "signature"}
            ]),
        )
    }

    fn gemini(on_missing: Value) -> ReasoningSchemaConfig {
        config(
            json!({
                "compat_group": "g2",
                "placements": [{
                    "from": "sig", "into": {"tool_call_field": "extra_content"},
                    "scope": "open_loop", "on_missing": on_missing
                }]
            }),
            json!([{"id": "sig", "kind": "tool_call_field", "field": "extra_content",
                    "merge": "last"}]),
        )
    }

    fn envelope(capture: &str, group: &str, value: Value) -> Artefact {
        Artefact(json!({"capture": capture, "group": group, "value": value}))
    }

    fn block_part(group: &str, value: Value) -> Part {
        Part::Reasoning(ReasoningPart {
            text: None,
            artefact: Some(envelope("blocks", group, value)),
        })
    }

    fn call_part(id: &str, artefact: Option<Artefact>) -> Part {
        Part::ToolCall(ToolCallPart {
            call_id: id.to_string(),
            name: "t".to_string(),
            arguments: json!({}),
            artefact,
        })
    }

    fn assistant(parts: Vec<Part>) -> ProviderMessage {
        ProviderMessage::new(Role::Assistant, parts)
    }

    fn user(text: &str) -> ProviderMessage {
        ProviderMessage::user(text)
    }

    fn tool_result(id: &str) -> ProviderMessage {
        ProviderMessage::tool_result(id, "ok")
    }

    fn thinking_turn(call: &str, group: &str, thinking: &str) -> ProviderMessage {
        assistant(vec![
            Part::Reasoning(ReasoningPart {
                text: Some("shown".to_string()),
                artefact: None,
            }),
            block_part(
                group,
                json!({"type": "thinking", "thinking": thinking, "signature": "S"}),
            ),
            call_part(call, None),
        ])
    }

    fn render(cfg: &ReasoningSchemaConfig, messages: &[ProviderMessage]) -> Vec<Value> {
        let plan = ReplayPlan::new(messages, cfg).expect("replay declared");
        messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == Role::Assistant)
            .map(|(index, message)| {
                let mut calls: Vec<Value> = message.calls().iter().map(|_| json!({})).collect();
                plan.apply_to_calls(index, message, &mut calls);
                let mut rendered = json!({"role": "assistant", "tool_calls": calls});
                plan.apply_to_message(index, message, &mut rendered);
                rendered
            })
            .collect()
    }

    #[test]
    fn open_loop_starts_after_the_last_user_message() {
        let messages = [
            user("a"),
            assistant(vec![]),
            tool_result("c"),
            user("b"),
            assistant(vec![]),
        ];
        assert_eq!(open_loop_start(&messages), 4);
        assert_eq!(open_loop_start(&[assistant(vec![])]), 0);
    }

    #[test]
    fn blocks_replay_in_part_order_without_text_echo() {
        let messages = [
            user("go"),
            assistant(vec![
                block_part("g1", json!({"n": 1, "u": "é\u{1F600}", "z": 1.5})),
                block_part("g1", json!({"n": 2})),
                call_part("c", None),
            ]),
        ];
        let rendered = render(&claude(), &messages);
        assert_eq!(
            rendered[0]["thinking_blocks"],
            json!([{"n": 1, "u": "é\u{1F600}", "z": 1.5}, {"n": 2}])
        );
        assert!(rendered[0].get("reasoning_content").is_none());
    }

    #[test]
    fn a_foreign_group_is_never_replayed() {
        let messages = [user("go"), thinking_turn("c", "other", "x")];
        let rendered = render(&claude(), &messages);
        assert!(rendered[0].get("thinking_blocks").is_none());
        assert!(owed_missing(&messages, &claude()).is_some());
    }

    #[test]
    fn open_loop_scope_skips_turns_before_the_last_user_message() {
        let messages = [
            user("one"),
            thinking_turn("c1", "g1", "old"),
            tool_result("c1"),
            user("two"),
            thinking_turn("c2", "g1", "new"),
        ];
        let rendered = render(&claude(), &messages);
        assert!(rendered[0].get("thinking_blocks").is_none());
        assert_eq!(rendered[1]["thinking_blocks"][0]["thinking"], "new");
        assert!(owed_missing(&messages, &claude()).is_none());
    }

    #[test]
    fn a_summary_user_message_mid_loop_resets_the_loop() {
        let messages = [
            user("task"),
            thinking_turn("c1", "g1", "a"),
            tool_result("c1"),
            user("summary of earlier turns"),
            assistant(vec![call_part("c2", None)]),
        ];
        // The turn after the summary owes blocks and has none.
        let missing = owed_missing(&messages, &claude()).expect("owed");
        assert_eq!(missing.message_index, 4);
        assert!(missing.to_string().starts_with(ARTEFACT_MISSING_CODE));
    }

    #[test]
    fn refuse_only_applies_to_messages_with_a_tool_call() {
        let messages = [user("go"), assistant(vec![Part::text("final answer")])];
        assert!(owed_missing(&messages, &claude()).is_none());
    }

    #[test]
    fn tool_call_values_go_on_their_own_call_and_fill_goes_on_the_first() {
        let sig = Some(envelope("sig", "g2", json!({"s": "abc"})));
        let messages = [
            user("go"),
            assistant(vec![call_part("a", sig.clone()), call_part("b", None)]),
        ];
        let cfg = gemini(json!("refuse"));
        let rendered = render(&cfg, &messages);
        assert_eq!(
            rendered[0]["tool_calls"][0]["extra_content"],
            json!({"s": "abc"})
        );
        assert!(rendered[0]["tool_calls"][1].get("extra_content").is_none());
        assert!(owed_missing(&messages, &cfg).is_none());

        let unsigned = [
            user("go"),
            assistant(vec![call_part("a", None), call_part("b", None)]),
        ];
        assert!(owed_missing(&unsigned, &cfg).is_some());
        let filled = render(&gemini(json!({"fill": {"s": "dummy"}})), &unsigned);
        assert_eq!(
            filled[0]["tool_calls"][0]["extra_content"],
            json!({"s": "dummy"})
        );
        assert!(filled[0]["tool_calls"][1].get("extra_content").is_none());
        let omitted = render(&gemini(json!("omit")), &unsigned);
        assert!(omitted[0]["tool_calls"][0].get("extra_content").is_none());
    }

    #[test]
    fn omit_never_refuses_and_fill_never_refuses() {
        let unsigned = [user("go"), assistant(vec![call_part("a", None)])];
        assert!(owed_missing(&unsigned, &gemini(json!("omit"))).is_none());
        assert!(owed_missing(&unsigned, &gemini(json!({"fill": 1}))).is_none());
    }

    #[test]
    fn text_placement_joins_reasoning_text_under_its_scope() {
        let cfg = config(
            json!({"placements": [{
                "from": "text", "into": {"message_field": "reasoning_content"},
                "scope": "tool_call_turns"
            }]}),
            json!([{"id": "text", "kind": "text_delta", "fields": ["reasoning"]}]),
        );
        let with_calls = assistant(vec![
            Part::Reasoning(ReasoningPart {
                text: Some("a".into()),
                artefact: None,
            }),
            Part::Reasoning(ReasoningPart {
                text: Some("b".into()),
                artefact: None,
            }),
            call_part("c", None),
        ]);
        let plain = assistant(vec![
            Part::Reasoning(ReasoningPart {
                text: Some("x".into()),
                artefact: None,
            }),
            Part::text("answer"),
        ]);
        let messages = [user("go"), with_calls, plain];
        let rendered = render(&cfg, &messages);
        assert_eq!(rendered[0]["reasoning_content"], "ab");
        assert!(rendered[1].get("reasoning_content").is_none());
    }

    #[test]
    fn no_replay_block_means_no_plan_and_nothing_is_sent() {
        let cfg: ReasoningSchemaConfig = serde_json::from_value(json!({
            "capture": [{"id": "text", "kind": "text_delta", "fields": ["reasoning_content"]}]
        }))
        .unwrap();
        let messages = [user("go"), thinking_turn("c", "g1", "x")];
        assert!(ReplayPlan::new(&messages, &cfg).is_none());
        assert!(owed_missing(&messages, &cfg).is_none());
    }

    #[test]
    fn the_same_history_renders_the_same_bytes() {
        let messages = [user("go"), thinking_turn("c", "g1", "x")];
        let a = serde_json::to_vec(&render(&claude(), &messages)).unwrap();
        let b = serde_json::to_vec(&render(&claude(), &messages)).unwrap();
        assert_eq!(a, b);
    }
}
