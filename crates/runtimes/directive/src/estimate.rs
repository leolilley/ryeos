//! The one size estimate over a conversation's parts. The live continuation
//! check and the recovery trim both call it, so they cannot disagree about
//! how big a conversation is.
//!
//! It counts characters, divided by four: deliberately provider-agnostic, it
//! only needs to track the context window, not bill. Text and reasoning text
//! count by length; a tool call counts its name and its arguments' JSON text;
//! a tool result counts its text parts; a provider artefact counts its JSON
//! text. An image counts nothing yet: its cost is a per-model fact that is
//! not recorded.

use ryeos_provider_contract::Part;

use crate::directive::ProviderMessage;

/// Estimated tokens of the whole message list: the sum of every part's
/// characters, divided by four.
pub fn estimate_tokens(messages: &[ProviderMessage]) -> u64 {
    let chars: usize = messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .map(part_chars)
        .sum();
    (chars as u64) / 4
}

fn part_chars(part: &Part) -> usize {
    match part {
        Part::Text(text) => text.text.len(),
        Part::Image(_) => 0,
        Part::Reasoning(reasoning) => {
            reasoning.text.as_ref().map_or(0, String::len)
                + reasoning
                    .artefact
                    .as_ref()
                    .map_or(0, |artefact| artefact.0.to_string().len())
        }
        Part::ToolCall(call) => {
            call.name.len()
                + call.arguments.to_string().len()
                + call
                    .artefact
                    .as_ref()
                    .map_or(0, |artefact| artefact.0.to_string().len())
        }
        Part::ToolResult(result) => result.parts.iter().map(part_chars).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::ProviderMessageExt;
    use serde_json::json;

    #[test]
    fn counts_text_reasoning_calls_and_results_once_each() {
        let assistant = ProviderMessage::assistant(
            Some("a".repeat(100)),
            vec![crate::directive::ToolCall {
                id: Some("c1".to_string()),
                name: "tool".to_string(),
                arguments: json!({"k": "v"}),
            }],
            Some("r".repeat(60)),
        );
        // 100 text + 60 reasoning + 4 name + 9 arguments {"k":"v"}.
        assert_eq!(estimate_tokens(&[assistant]), (100 + 60 + 4 + 9) / 4);
        let result = ProviderMessage::tool_result("c1", "x".repeat(40));
        assert_eq!(estimate_tokens(&[result]), 10);
    }

    #[test]
    fn artefacts_count_by_their_compact_json_on_reasoning_parts_and_calls() {
        let artefact = ryeos_provider_contract::Artefact(json!({"k": "0123456789"}));
        let len = artefact.0.to_string().len();
        let message = ProviderMessage::new(
            ryeos_provider_contract::Role::Assistant,
            vec![
                Part::Reasoning(ryeos_provider_contract::ReasoningPart {
                    text: None,
                    artefact: Some(artefact.clone()),
                }),
                Part::ToolCall(ryeos_provider_contract::ToolCallPart {
                    call_id: "c".to_string(),
                    name: "t".to_string(),
                    arguments: json!({}),
                    artefact: Some(artefact),
                }),
            ],
        );
        assert_eq!(
            estimate_tokens(&[message]),
            ((len + 1 + 2 + len) / 4) as u64
        );
    }

    #[test]
    fn text_is_counted_by_length_not_by_its_json_encoding() {
        let messages = vec![
            ProviderMessage::user("a".repeat(400)),
            ProviderMessage::assistant_text("b".repeat(400)),
        ];
        assert_eq!(estimate_tokens(&messages), 200);
    }

    #[test]
    fn the_estimate_sums_characters_before_dividing() {
        // Two messages of 3 characters each: 6 / 4 = 1, not 0 + 0.
        let messages = vec![ProviderMessage::user("abc"), ProviderMessage::user("def")];
        assert_eq!(estimate_tokens(&messages), 1);
    }
}
