//! The neutral turn: a message is a role and an ordered list of parts.
//!
//! The same shape is held in memory by the directive runtime, recorded in
//! thread events, and stored in provider-call effect records. Part order is
//! the order the provider gave or the runtime built, and it is kept.
//!
//! The set of part types is closed because each needs code. Anything a
//! provider requires back unchanged travels as an opaque [`Artefact`] on a
//! reasoning or tool-call part; this crate never reads inside one.

use std::collections::BTreeSet;

use anyhow::{Context as _, bail};
use ryeos_effect_contract::{canonical_value_digest, require_hex64, validate_identifier};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Upper bound on the parts of one message, and on the parts of one tool
/// result. A message past it is refused rather than truncated.
pub const MAX_MESSAGE_PARTS: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "system" => Self::System,
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "tool" => Self::Tool,
            other => bail!("unknown message role {other:?}"),
        })
    }
}

/// A value a provider requires back unchanged. Stored and replayed without
/// being read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Artefact(pub Value);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextPart {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePart {
    pub media_type: String,
    /// Object hash of the stored attachment holding the image bytes.
    pub attachment: String,
    pub byte_length: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningPart {
    /// What the provider showed, when it showed anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artefact: Option<Artefact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallPart {
    /// The id used on the wire, including one the runtime synthesised
    /// because the provider issued none. A tool result names its call by it.
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artefact: Option<Artefact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultPart {
    pub call_id: String,
    /// Text and image parts only.
    pub parts: Vec<Part>,
    pub status: ToolResultStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text(TextPart),
    Image(ImagePart),
    Reasoning(ReasoningPart),
    ToolCall(ToolCallPart),
    ToolResult(ToolResultPart),
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextPart { text: text.into() })
    }

    pub fn tool_call(
        call_id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self::ToolCall(ToolCallPart {
            call_id: call_id.into(),
            name: name.into(),
            arguments,
            artefact: None,
        })
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        parts: Vec<Part>,
        status: ToolResultStatus,
    ) -> Self {
        Self::ToolResult(ToolResultPart {
            call_id: call_id.into(),
            parts,
            status,
        })
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Image(_) => "image",
            Self::Reasoning(_) => "reasoning",
            Self::ToolCall(_) => "tool_call",
            Self::ToolResult(_) => "tool_result",
        }
    }

    fn allowed_in(&self, role: Role) -> bool {
        match self {
            Self::Text(_) => role != Role::Tool,
            Self::Image(_) => role == Role::User,
            Self::Reasoning(_) | Self::ToolCall(_) => role == Role::Assistant,
            Self::ToolResult(_) => role == Role::Tool,
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Text(_) => Ok(()),
            Self::Image(image) => image.validate(),
            Self::Reasoning(reasoning) => {
                if reasoning.text.is_none() && reasoning.artefact.is_none() {
                    bail!("reasoning part carries neither text nor an artefact");
                }
                Ok(())
            }
            Self::ToolCall(call) => {
                validate_identifier("tool call id", &call.call_id)?;
                validate_identifier("tool call name", &call.name)
            }
            Self::ToolResult(result) => result.validate(),
        }
    }
}

impl ImagePart {
    fn validate(&self) -> anyhow::Result<()> {
        validate_media_type(&self.media_type)?;
        if !self.media_type.starts_with("image/") {
            bail!(
                "image part media type {:?} is not an image/ type",
                self.media_type
            );
        }
        require_hex64("image attachment", &self.attachment)?;
        if self.byte_length == 0 {
            bail!("image part has zero byte length");
        }
        if self.width == Some(0) || self.height == Some(0) {
            bail!("image part has a zero dimension");
        }
        Ok(())
    }
}

impl ToolResultPart {
    fn validate(&self) -> anyhow::Result<()> {
        validate_identifier("tool result call id", &self.call_id)?;
        if self.parts.len() > MAX_MESSAGE_PARTS {
            bail!("tool result has more than {MAX_MESSAGE_PARTS} parts");
        }
        for (index, part) in self.parts.iter().enumerate() {
            match part {
                Part::Text(_) => {}
                Part::Image(image) => image
                    .validate()
                    .with_context(|| format!("tool result part {index}"))?,
                other => bail!(
                    "tool result part {index} is {}; only text and image parts may appear in a tool result",
                    other.type_name()
                ),
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn new(role: Role, parts: Vec<Part>) -> Self {
        Self { role, parts }
    }

    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::new(role, vec![Part::text(text)])
    }

    /// Checks the part placement rules and every part's own fields.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.parts.len() > MAX_MESSAGE_PARTS {
            bail!("message has more than {MAX_MESSAGE_PARTS} parts");
        }
        if self.role == Role::Tool && self.parts.is_empty() {
            bail!("tool message has no tool result part");
        }
        let mut call_ids = BTreeSet::new();
        for (index, part) in self.parts.iter().enumerate() {
            if !part.allowed_in(self.role) {
                bail!(
                    "part {index} is {}, which may not appear in a {} message",
                    part.type_name(),
                    self.role.as_str()
                );
            }
            part.validate()
                .with_context(|| format!("message part {index}"))?;
            let call_id = match part {
                Part::ToolCall(call) => Some(&call.call_id),
                Part::ToolResult(result) => Some(&result.call_id),
                _ => None,
            };
            if let Some(call_id) = call_id
                && !call_ids.insert(call_id.as_str())
            {
                bail!("call id {call_id:?} appears twice in one message");
            }
        }
        Ok(())
    }

    /// Joined text of the message's text parts, in order.
    pub fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The tool-call parts, in order. A call's position in this list is its
    /// `call_index` within the turn.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCallPart> {
        self.parts.iter().filter_map(|part| match part {
            Part::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// Canonical digest of the message's serialised form.
    pub fn digest(&self) -> anyhow::Result<String> {
        canonical_value_digest(&serde_json::to_value(self)?)
    }
}

fn validate_media_type(value: &str) -> anyhow::Result<()> {
    let Some((kind, subtype)) = value.split_once('/') else {
        bail!("media type {value:?} is not type/subtype");
    };
    let token = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.".contains(c))
    };
    if !token(kind) || !token(subtype) {
        bail!("media type {value:?} is not a lowercase type/subtype token");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn image() -> Part {
        Part::Image(ImagePart {
            media_type: "image/png".to_string(),
            attachment: HASH.to_string(),
            byte_length: 12,
            width: Some(4),
            height: None,
        })
    }

    #[test]
    fn parts_serialise_with_a_type_tag_and_omit_absent_options() {
        let message = Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning(ReasoningPart {
                    text: Some("think".to_string()),
                    artefact: None,
                }),
                Part::text("answer"),
                Part::tool_call("call_1", "read_file", json!({"path": "a"})),
            ],
        );
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(
            value,
            json!({
                "role": "assistant",
                "parts": [
                    {"type": "reasoning", "text": "think"},
                    {"type": "text", "text": "answer"},
                    {"type": "tool_call", "call_id": "call_1", "name": "read_file", "arguments": {"path": "a"}},
                ]
            })
        );
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(back, message);
        message.validate().unwrap();
    }

    #[test]
    fn an_artefact_round_trips_unread() {
        let artefact = json!({"signature": "opaque", "nested": [1, {"x": null}]});
        let part = Part::Reasoning(ReasoningPart {
            text: None,
            artefact: Some(Artefact(artefact.clone())),
        });
        let value = serde_json::to_value(&part).unwrap();
        assert_eq!(value["artefact"], artefact);
        assert_eq!(serde_json::from_value::<Part>(value).unwrap(), part);
    }

    #[test]
    fn unknown_part_fields_and_types_are_refused() {
        assert!(
            serde_json::from_value::<Part>(json!({"type": "text", "text": "a", "extra": 1}))
                .is_err()
        );
        assert!(serde_json::from_value::<Part>(json!({"type": "audio"})).is_err());
        assert!(
            serde_json::from_value::<Message>(json!({"role": "user", "parts": [], "x": 1}))
                .is_err()
        );
    }

    #[test]
    fn placement_rules_follow_the_role() {
        Message::new(Role::User, vec![Part::text("look"), image()])
            .validate()
            .unwrap();
        assert!(
            Message::new(Role::Assistant, vec![image()])
                .validate()
                .is_err()
        );
        assert!(
            Message::new(Role::User, vec![Part::tool_call("c", "t", json!({}))])
                .validate()
                .is_err()
        );
        assert!(
            Message::new(Role::Tool, vec![Part::text("bare")])
                .validate()
                .is_err()
        );
        assert!(Message::new(Role::Tool, vec![]).validate().is_err());
        Message::new(
            Role::Tool,
            vec![Part::tool_result(
                "c",
                vec![Part::text("ok"), image()],
                ToolResultStatus::Ok,
            )],
        )
        .validate()
        .unwrap();
    }

    #[test]
    fn a_tool_result_holds_only_text_and_images() {
        let nested = Part::tool_result(
            "c",
            vec![Part::tool_call("d", "t", json!({}))],
            ToolResultStatus::Error,
        );
        assert!(Message::new(Role::Tool, vec![nested]).validate().is_err());
    }

    #[test]
    fn call_ids_are_unique_within_a_message() {
        let message = Message::new(
            Role::Assistant,
            vec![
                Part::tool_call("same", "a", json!({})),
                Part::tool_call("same", "b", json!({})),
            ],
        );
        assert!(message.validate().is_err());
    }

    #[test]
    fn image_fields_are_checked() {
        let mut bad = ImagePart {
            media_type: "image/png".to_string(),
            attachment: HASH.to_string(),
            byte_length: 1,
            width: None,
            height: None,
        };
        bad.media_type = "IMAGE/PNG".to_string();
        assert!(bad.validate().is_err());
        bad.media_type = "image".to_string();
        assert!(bad.validate().is_err());
        bad.media_type = "application/pdf".to_string();
        assert!(bad.validate().is_err());
        bad.media_type = "image/webp".to_string();
        bad.attachment = "short".to_string();
        assert!(bad.validate().is_err());
        bad.attachment = HASH.to_string();
        bad.byte_length = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn empty_reasoning_is_refused() {
        let message = Message::new(
            Role::Assistant,
            vec![Part::Reasoning(ReasoningPart {
                text: None,
                artefact: None,
            })],
        );
        assert!(message.validate().is_err());
    }

    #[test]
    fn accessors_read_text_and_calls_in_order() {
        let message = Message::new(
            Role::Assistant,
            vec![
                Part::text("a"),
                Part::tool_call("c1", "x", json!({})),
                Part::text("b"),
                Part::tool_call("c2", "y", json!({})),
            ],
        );
        assert_eq!(message.joined_text(), "ab");
        let ids: Vec<_> = message.tool_calls().map(|c| c.call_id.as_str()).collect();
        assert_eq!(ids, ["c1", "c2"]);
    }

    #[test]
    fn the_digest_depends_on_part_order() {
        let a = Message::new(Role::User, vec![Part::text("x"), Part::text("y")]);
        let b = Message::new(Role::User, vec![Part::text("y"), Part::text("x")]);
        assert_ne!(a.digest().unwrap(), b.digest().unwrap());
        assert_eq!(a.digest().unwrap(), a.clone().digest().unwrap());
    }

    #[test]
    fn roles_parse_their_own_names() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            assert_eq!(Role::parse(role.as_str()).unwrap(), role);
        }
        assert!(Role::parse("developer").is_err());
    }
}
