//! One builder for the parts of an assistant answer, shared by every stream
//! assembly loop. Parts are kept in the order the stream gave them: adjacent
//! text fragments merge into one text part, adjacent reasoning fragments into
//! one reasoning part, and a tool call is a part of its own at the point it
//! completed.

use ryeos_provider_contract::{Part, ReasoningPart, Role};

use crate::directive::{ProviderMessage, ToolCall, synthesised_call_id};

#[derive(Debug, Default)]
pub struct PartsBuilder {
    parts: Vec<Part>,
}

impl PartsBuilder {
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
        self.parts.push(Part::tool_call(
            call.id.unwrap_or_default(),
            call.name,
            call.arguments,
        ));
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
}
