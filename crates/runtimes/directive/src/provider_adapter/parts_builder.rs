//! One builder for the parts of an assistant answer, shared by every stream
//! assembly loop. Parts are kept in the order the stream gave them: adjacent
//! text fragments merge into one text part, adjacent reasoning fragments into
//! one reasoning part, and a tool call is a part of its own at the point it
//! completed.

use ryeos_provider_contract::{Artefact, Part, ReasoningPart, Role};
use serde_json::Value;

use super::reasoning_capture::envelope;
use crate::directive::{ProviderMessage, ToolCall, ToolCallArtefact, synthesised_call_id};

#[derive(Debug, Default)]
pub struct PartsBuilder {
    parts: Vec<Part>,
    /// The compat group stamped on every artefact this builder records.
    group: Option<String>,
}

impl PartsBuilder {
    /// A builder whose artefacts carry `group`, the compat group the provider
    /// entry declares.
    pub fn new(group: Option<&str>) -> Self {
        Self {
            parts: Vec::new(),
            group: group.map(str::to_string),
        }
    }

    /// A completed opaque reasoning block, as a part of its own. Reasoning
    /// text stays in adjacent text-only parts, so the two never fuse.
    pub fn push_reasoning_artefact(&mut self, capture_id: &str, value: Value) {
        self.parts.push(Part::Reasoning(ReasoningPart {
            text: None,
            artefact: Some(Artefact(envelope(capture_id, self.group.as_deref(), value))),
        }));
    }

    pub fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.parts.last_mut() {
            Some(Part::Text(last)) => last.text.push_str(text),
            _ => self.parts.push(Part::text(text)),
        }
    }

    pub fn push_reasoning(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.parts.last_mut() {
            Some(Part::Reasoning(ReasoningPart {
                text: Some(last), ..
            })) => last.push_str(text),
            _ => self.parts.push(Part::Reasoning(ReasoningPart {
                text: Some(text.to_string()),
                artefact: None,
            })),
        }
    }

    /// A completed tool call. A call the provider issued without an id keeps
    /// an empty id until [`Self::finish`] gives it one.
    pub fn push_call(&mut self, call: ToolCall) {
        self.push_call_with_artefact(call, None);
    }

    /// A completed tool call with the value a capture rule collected for it.
    pub fn push_call_with_artefact(&mut self, call: ToolCall, artefact: Option<ToolCallArtefact>) {
        let mut part = Part::tool_call(call.id.unwrap_or_default(), call.name, call.arguments);
        if let (Part::ToolCall(call_part), Some(artefact)) = (&mut part, artefact) {
            call_part.artefact = Some(Artefact(envelope(
                &artefact.capture_id,
                self.group.as_deref(),
                artefact.value,
            )));
        }
        self.parts.push(part);
    }

    /// Joined text of every text part.
    pub fn text(&self) -> Option<String> {
        joined(self.parts.iter().filter_map(|part| match part {
            Part::Text(text) => Some(text.text.as_str()),
            _ => None,
        }))
    }

    /// Joined text of every reasoning part.
    pub fn reasoning_text(&self) -> Option<String> {
        joined(self.parts.iter().filter_map(|part| match part {
            Part::Reasoning(ReasoningPart {
                text: Some(text), ..
            }) => Some(text.as_str()),
            _ => None,
        }))
    }

    /// The calls as the stream gave them: a call without an id has none.
    pub fn calls(&self) -> Vec<ToolCall> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolCall(call) => Some(ToolCall {
                    id: (!call.call_id.is_empty()).then(|| call.call_id.clone()),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// The parts of a turn an interrupt cut short: the complete text and
    /// reasoning parts. A tool call is never recorded for an interrupted turn.
    pub fn interrupted_parts(self) -> Vec<Part> {
        self.parts
            .into_iter()
            .filter(|part| matches!(part, Part::Text(_) | Part::Reasoning(_)))
            .collect()
    }

    /// The finished assistant message. A call the provider issued without an
    /// id gets one derived from the turn and the call's position, so a replay
    /// and a recovery see the id the live run used.
    pub fn finish(mut self, turn: u32) -> ProviderMessage {
        let mut call_index = 0usize;
        for part in &mut self.parts {
            if let Part::ToolCall(call) = part {
                if call.call_id.is_empty() {
                    call.call_id = synthesised_call_id(turn, call_index);
                }
                call_index += 1;
            }
        }
        ProviderMessage::new(Role::Assistant, self.parts)
    }
}

fn joined<'a>(fragments: impl Iterator<Item = &'a str>) -> Option<String> {
    let text: String = fragments.collect();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: Option<&str>, name: &str) -> ToolCall {
        ToolCall {
            id: id.map(str::to_string),
            name: name.to_string(),
            arguments: json!({}),
        }
    }

    #[test]
    fn parts_keep_the_order_the_stream_gave() {
        let mut builder = PartsBuilder::default();
        builder.push_call(call(Some("a"), "first"));
        builder.push_text("hel");
        builder.push_text("lo");
        builder.push_reasoning("think");
        builder.push_call(call(Some("b"), "second"));
        builder.push_text("bye");
        let message = builder.finish(1);
        let kinds: Vec<_> = message.parts.iter().map(|part| part.type_name()).collect();
        assert_eq!(
            kinds,
            ["tool_call", "text", "reasoning", "tool_call", "text"]
        );
        assert_eq!(message.joined_text(), "hellobye");
        message.validate().unwrap();
    }

    #[test]
    fn adjacent_fragments_merge_and_empty_ones_add_nothing() {
        let mut builder = PartsBuilder::default();
        builder.push_text("");
        builder.push_reasoning("");
        builder.push_reasoning("a");
        builder.push_reasoning("b");
        assert_eq!(builder.text(), None);
        assert_eq!(builder.reasoning_text().as_deref(), Some("ab"));
        assert_eq!(builder.finish(1).parts.len(), 1);
    }

    #[test]
    fn missing_ids_are_synthesised_by_call_position() {
        let mut builder = PartsBuilder::default();
        builder.push_text("x");
        builder.push_call(call(None, "a"));
        builder.push_call(call(Some("kept"), "b"));
        builder.push_call(call(None, "c"));
        assert_eq!(builder.calls()[0].id, None);
        let ids: Vec<_> = builder
            .finish(4)
            .tool_calls()
            .map(|call| call.call_id.clone())
            .collect();
        assert_eq!(ids, ["call_t4_0", "kept", "call_t4_2"]);
    }

    #[test]
    fn an_interrupted_turn_keeps_text_and_reasoning_but_no_call() {
        let mut builder = PartsBuilder::default();
        builder.push_reasoning("why");
        builder.push_text("partial");
        builder.push_call(call(Some("a"), "tool"));
        let parts = builder.interrupted_parts();
        let kinds: Vec<_> = parts.iter().map(|part| part.type_name()).collect();
        assert_eq!(kinds, ["reasoning", "text"]);
    }

    #[test]
    fn artefacts_are_parts_of_their_own_stamped_with_the_group() {
        let mut builder = PartsBuilder::new(Some("g1"));
        builder.push_reasoning("shown");
        builder.push_reasoning_artefact("blocks", json!({"n": 1}));
        builder.push_reasoning("more");
        builder.push_call_with_artefact(
            call(Some("a"), "tool"),
            Some(ToolCallArtefact {
                capture_id: "sig".to_string(),
                value: json!({"s": 1}),
            }),
        );
        let message = builder.finish(1);
        let envelope = |capture: &str, value: Value| json!({"capture": capture, "group": "g1", "value": value});
        let parts = serde_json::to_value(&message.parts).unwrap();
        assert_eq!(
            parts,
            json!([
                {"type": "reasoning", "text": "shown"},
                {"type": "reasoning", "artefact": envelope("blocks", json!({"n": 1}))},
                {"type": "reasoning", "text": "more"},
                {"type": "tool_call", "call_id": "a", "name": "tool", "arguments": {},
                 "artefact": envelope("sig", json!({"s": 1}))},
            ])
        );
    }

    #[test]
    fn a_builder_without_a_group_stamps_none() {
        let mut builder = PartsBuilder::default();
        builder.push_reasoning_artefact("blocks", json!(1));
        let parts = serde_json::to_value(&builder.finish(1).parts).unwrap();
        assert!(parts[0]["artefact"].get("group").is_none());
    }

    #[test]
    fn an_interrupted_turn_keeps_complete_artefact_parts() {
        let mut builder = PartsBuilder::new(Some("g"));
        builder.push_reasoning_artefact("blocks", json!(1));
        builder.push_call(call(Some("a"), "tool"));
        let kept = builder.interrupted_parts();
        assert_eq!(kept.len(), 1);
        assert!(matches!(&kept[0], Part::Reasoning(_)));
    }
}
