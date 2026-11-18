//! What a launch records and how a recorded input becomes messages.
//!
//! The launch records are one atomic batch in the order the model is given
//! them: the frame (system text, tool inventory, request options, provider
//! configuration), the `before` context, the stimulus, the `after` context.
//! The live run and the fold both turn a recorded input into messages through
//! [`input_messages`], so a rebuilt conversation is the one the live run had.

use anyhow::{Context as _, Result};
use ryeos_directive_definition::{ReasoningConfig, SamplingConfig};
use ryeos_runtime::cognition::{CognitionFramePayload, FrameComponent};
use ryeos_runtime::{CognitionInput, FrameRecord, FrameRequestOptions};

use crate::directive::{ProviderMessage, ProviderMessageExt, ToolSchema};

/// The acknowledgement that follows the `before` context.
pub const BEFORE_CONTEXT_ACK: &str = "Understood. I will use this context.";
/// The acknowledgement that follows the `after` context.
pub const AFTER_CONTEXT_ACK: &str = "Noted. I will apply this guidance.";

/// The origin token of a context input.
pub const CONTEXT_ORIGIN: &str = "context";
/// The origin token of the launch stimulus.
pub const LAUNCH_ORIGIN: &str = "launch";

/// What a frame is built from.
pub struct FrameSource<'a> {
    pub system_prompt: Option<&'a str>,
    /// The tools the launch offers the model, with their schemas.
    pub inventory: &'a [ToolSchema],
    pub model: &'a str,
    pub reasoning: Option<&'a ReasoningConfig>,
    pub sampling: Option<&'a SamplingConfig>,
    /// The configured output limit per turn; zero means none.
    pub max_output_tokens: u64,
    pub provider_config_hash: &'a str,
}

/// Builds the frame at position `frame_seq`. The inventory attachment is the
/// canonical JSON of the tool list, so equal inventories have equal hashes.
pub fn build_frame(frame_seq: u32, source: &FrameSource<'_>) -> Result<FrameRecord> {
    let inventory = lillux::canonical_json(&serde_json::to_value(source.inventory)?)
        .context("canonicalise the tool inventory")?;
    FrameRecord::new(
        frame_seq,
        source.system_prompt,
        FrameRequestOptions {
            model: source.model.to_string(),
            reasoning: source
                .reasoning
                .map(serde_json::to_value)
                .transpose()
                .context("serialise reasoning options")?,
            sampling: source
                .sampling
                .map(serde_json::to_value)
                .transpose()
                .context("serialise sampling options")?,
            max_output_tokens: (source.max_output_tokens > 0).then_some(source.max_output_tokens),
        },
        source.provider_config_hash,
        inventory.into_bytes(),
    )
}

/// The launch inputs in the order the model is given them. A context input
/// carries the acknowledgement the runtime writes after it; the stimulus is
/// pinned.
pub fn launch_inputs(
    before: Option<&str>,
    stimulus: &str,
    after: Option<&str>,
) -> Vec<CognitionInput> {
    let mut inputs = Vec::new();
    if let Some(before) = before {
        inputs.push(CognitionInput::text(CONTEXT_ORIGIN, before).with_ack(BEFORE_CONTEXT_ACK));
    }
    inputs.push(CognitionInput::text(LAUNCH_ORIGIN, stimulus).pinned());
    if let Some(after) = after {
        inputs.push(CognitionInput::text(CONTEXT_ORIGIN, after).with_ack(AFTER_CONTEXT_ACK));
    }
    inputs
}

/// The messages a recorded input becomes: the user message, then the
/// acknowledgement as an assistant message when the input has one.
pub fn input_messages(input: &CognitionInput) -> Vec<ProviderMessage> {
    let mut messages = vec![input.message()];
    if let Some(ack) = &input.ack {
        messages.push(ProviderMessage::assistant_text(ack.as_str()));
    }
    messages
}

/// The messages of a list of inputs, in order.
pub fn messages_of(inputs: &[CognitionInput]) -> Vec<ProviderMessage> {
    inputs.iter().flat_map(input_messages).collect()
}

/// The first frame component the launch changes that the provider entry
/// refuses to see change within one conversation, or `None` when the launch
/// may record its frame as the next one.
pub fn refused_frame_change(
    recorded: &CognitionFramePayload,
    launch: &CognitionFramePayload,
    refuse_change_of: &[FrameComponent],
) -> Option<FrameComponent> {
    recorded
        .differing_components(launch)
        .into_iter()
        .find(|component| refuse_change_of.contains(component))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_inputs_read_in_the_order_the_model_is_given_them() {
        let inputs = launch_inputs(Some("before"), "go", Some("after"));
        let origins: Vec<_> = inputs.iter().map(|input| input.origin.as_str()).collect();
        assert_eq!(origins, ["context", "launch", "context"]);
        assert!(!inputs[0].pinned && inputs[1].pinned && !inputs[2].pinned);
        let texts: Vec<_> = messages_of(&inputs)
            .iter()
            .map(|message| {
                (
                    message.role().to_string(),
                    message.text().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            texts,
            [
                ("user".to_string(), "before".to_string()),
                ("assistant".to_string(), BEFORE_CONTEXT_ACK.to_string()),
                ("user".to_string(), "go".to_string()),
                ("user".to_string(), "after".to_string()),
                ("assistant".to_string(), AFTER_CONTEXT_ACK.to_string()),
            ]
        );
    }

    #[test]
    fn a_launch_with_no_context_is_the_stimulus_alone() {
        let inputs = launch_inputs(None, "go", None);
        assert_eq!(inputs.len(), 1);
        assert_eq!(messages_of(&inputs).len(), 1);
    }

    #[test]
    fn equal_inventories_make_equal_frames() {
        let tool = ToolSchema {
            name: "t".to_string(),
            item_id: "tool:t".to_string(),
            description: None,
            input_schema: None,
        };
        let source = |inventory: &[ToolSchema]| {
            build_frame(
                1,
                &FrameSource {
                    system_prompt: Some("sys"),
                    inventory,
                    model: "m",
                    reasoning: None,
                    sampling: None,
                    max_output_tokens: 0,
                    provider_config_hash: &"ab".repeat(32),
                },
            )
            .unwrap()
        };
        let one = source(std::slice::from_ref(&tool));
        let again = source(std::slice::from_ref(&tool));
        assert!(one.payload.same_as(&again.payload));
        assert!(!one.payload.same_as(&source(&[]).payload));
        assert_eq!(one.payload.request_options.max_output_tokens, None);
    }

    #[test]
    fn only_a_listed_component_makes_a_frame_change_refused() {
        let frame = |system: &str, reasoning: Option<ReasoningConfig>| {
            build_frame(
                1,
                &FrameSource {
                    system_prompt: Some(system),
                    inventory: &[],
                    model: "m",
                    reasoning: reasoning.as_ref(),
                    sampling: None,
                    max_output_tokens: 0,
                    provider_config_hash: &"ab".repeat(32),
                },
            )
            .unwrap()
            .payload
        };
        let high = ReasoningConfig {
            mode: ryeos_directive_definition::ReasoningMode::Enabled,
            effort: Some("high".to_string()),
        };
        let recorded = frame("sys", None);
        let changed_effort = frame("sys", Some(high));
        assert_eq!(
            refused_frame_change(&recorded, &changed_effort, &[FrameComponent::Reasoning]),
            Some(FrameComponent::Reasoning)
        );
        assert_eq!(
            refused_frame_change(&recorded, &changed_effort, &[FrameComponent::System]),
            None
        );
        assert_eq!(refused_frame_change(&recorded, &changed_effort, &[]), None);
        assert_eq!(
            refused_frame_change(&recorded, &recorded.clone(), &[FrameComponent::System]),
            None
        );
    }
}
