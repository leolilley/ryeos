//! The one size estimate over a conversation's parts. The live continuation
//! check and the recovery trim both call it, so they cannot disagree about
//! how big a conversation is.
//!
//! It counts characters, divided by four: deliberately provider-agnostic, it
//! only needs to track the context window, not bill. Text and reasoning text
//! count by length; a tool call counts its name and its arguments' JSON text;
//! a tool result counts its text parts; a provider artefact counts its JSON
//! text. An image counts by the provider entry's declared cost for its
//! recorded dimensions, and only the newest images a request carries count,
//! the same rule the request is built with. With no image declaration an image
//! counts nothing: such a request is refused before it is sent.

use ryeos_directive_definition::ImageAcceptConfig;
use ryeos_provider_contract::Part;

use crate::directive::ProviderMessage;
use crate::provider_adapter::images::retained_images;

/// Estimated tokens of the whole message list: the sum of every part's
/// characters, divided by four, plus the declared cost of each image a
/// request would carry.
pub fn estimate_tokens(messages: &[ProviderMessage], images: Option<&ImageAcceptConfig>) -> u64 {
    let chars: usize = messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .map(part_chars)
        .sum();
    let image_tokens: u64 = images.map_or(0, |accepts| {
        retained_images(messages, accepts.max_images_per_request)
            .into_iter()
            .map(|image| accepts.cost.tokens(image.width, image.height))
            .sum()
    });
    (chars as u64) / 4 + image_tokens
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
        assert_eq!(estimate_tokens(&[assistant], None), (100 + 60 + 4 + 9) / 4);
        let result = ProviderMessage::tool_result("c1", "x".repeat(40));
        assert_eq!(estimate_tokens(&[result], None), 10);
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
            estimate_tokens(&[message], None),
            ((len + 1 + 2 + len) / 4) as u64
        );
    }

    #[test]
    fn text_is_counted_by_length_not_by_its_json_encoding() {
        let messages = vec![
            ProviderMessage::user("a".repeat(400)),
            ProviderMessage::assistant_text("b".repeat(400)),
        ];
        assert_eq!(estimate_tokens(&messages, None), 200);
    }

    fn tool_result_with_images(count: usize, width: u32, height: u32) -> ProviderMessage {
        let mut parts = vec![Part::text("")];
        for index in 0..count {
            parts.push(Part::Image(ryeos_provider_contract::ImagePart {
                media_type: "image/png".to_string(),
                attachment: format!("{index:064x}"),
                byte_length: 10,
                width: Some(width),
                height: Some(height),
            }));
        }
        ProviderMessage::new(
            ryeos_provider_contract::Role::Tool,
            vec![Part::tool_result(
                "c",
                parts,
                ryeos_provider_contract::ToolResultStatus::Ok,
            )],
        )
    }

    fn accepts(max: u32) -> ImageAcceptConfig {
        serde_json::from_value(json!({
            "media_types": ["image/png"],
            "max_bytes_per_image": 1000,
            "max_images_per_request": max,
            "text_part_template": {"type": "text", "text": "{text}"},
            "image_part_template": {"type": "image_url", "image_url": {"url": "{data_url}"}},
            "tool_result_note": "[{count}]",
            "deferred_label": "{index} {call_id}",
            "omitted_note": "[omitted]",
            "cost": {"pixels_per_token": 100, "max_tokens": 50, "unknown_dimensions_tokens": 50}
        }))
        .unwrap()
    }

    #[test]
    fn an_image_counts_by_the_declared_cost_and_only_the_newest_count() {
        let message = tool_result_with_images(4, 10, 10);
        // 10x10 pixels at 100 per token is one token each.
        assert_eq!(estimate_tokens(&[message.clone()], Some(&accepts(10))), 4);
        assert_eq!(estimate_tokens(&[message.clone()], Some(&accepts(2))), 2);
        // Dimensions past the cap count the cap.
        let big = tool_result_with_images(1, 1000, 1000);
        assert_eq!(estimate_tokens(&[big], Some(&accepts(10))), 50);
        // No declaration: nothing to count.
        assert_eq!(estimate_tokens(&[message], None), 0);
    }

    #[test]
    fn the_estimate_sums_characters_before_dividing() {
        // Two messages of 3 characters each: 6 / 4 = 1, not 0 + 0.
        let messages = vec![ProviderMessage::user("abc"), ProviderMessage::user("def")];
        assert_eq!(estimate_tokens(&messages, None), 1);
    }
}
