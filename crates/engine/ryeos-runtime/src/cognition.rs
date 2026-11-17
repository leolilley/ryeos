//! Typed payloads of the conversation events a directive records.
//!
//! A conversation is recorded as parts (the provider contract's neutral turn)
//! in four event shapes:
//!
//! - `cognition_in` input: `{origin, parts, kind?, ack?, pinned?}`, or a turn
//!   marker `{turn, tools_offered?}`;
//! - `cognition_out`: `{turn, parts, finish, usage?, provider_accounting?}`;
//! - `tool_call_result`: `{operation_id, call_id, tool, status, parts, guard}`.
//!
//! Writers build these types and serialise them; readers parse payloads into
//! them. Both sides therefore share one definition of every field.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_provider_contract::{Message, Part, Role, ToolResultStatus};

/// The `finish` value of a turn that a live interrupt cut short.
pub const FINISH_INTERRUPTED: &str = "interrupted";

/// Longest origin, kind or finish token.
pub const MAX_TOKEN_BYTES: usize = 64;

/// An open, bounded vocabulary token: lowercase ASCII letters, digits and
/// underscores, starting with a letter.
pub fn validate_token(label: &str, value: &str) -> Result<()> {
    let mut bytes = value.bytes();
    let starts_ok = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase());
    if !starts_ok
        || value.len() > MAX_TOKEN_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bail!(
            "{label} {value:?} must be a lowercase token of at most {MAX_TOKEN_BYTES} bytes \
             (letters, digits, underscores; starting with a letter)"
        );
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One input to cognition: what the model is given as a user message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionInput {
    /// Where the input came from (`launch`, `operator`, `harness`, `context`,
    /// ...). An open token; readers that do not know a value treat it as
    /// any other input.
    pub origin: String,
    pub parts: Vec<Part>,
    /// Which harness input this is, for origins that have several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The acknowledgement the runtime wrote after this input. The fold
    /// emits it as an assistant message directly after the input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<String>,
    /// Survives every trim of an old conversation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
}

impl CognitionInput {
    pub fn text(origin: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            origin: origin.into(),
            parts: vec![Part::text(text)],
            kind: None,
            ack: None,
            pinned: false,
        }
    }

    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    pub fn with_ack(mut self, ack: impl Into<String>) -> Self {
        self.ack = Some(ack.into());
        self
    }

    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }

    pub fn validate(&self) -> Result<()> {
        validate_token("cognition_in origin", &self.origin)?;
        if let Some(kind) = &self.kind {
            validate_token("cognition_in kind", kind)?;
        }
        if self.parts.is_empty() {
            bail!("cognition_in carries no parts");
        }
        Message::new(Role::User, self.parts.clone()).validate()
    }

    /// The input as a user message.
    pub fn message(&self) -> Message {
        Message::new(Role::User, self.parts.clone())
    }

    /// Joined text of the text parts.
    pub fn joined_text(&self) -> String {
        self.message().joined_text()
    }

    /// The single text part's text, when the input is exactly one text part.
    /// Only such an input may be split into chunks.
    pub(crate) fn single_text(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [Part::Text(text)] => Some(&text.text),
            _ => None,
        }
    }
}

/// The `cognition_in` payload that opens a provider turn. It carries no
/// conversation content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnMarker {
    pub turn: u32,
    /// The tool names offered on this turn, recorded only when the offered
    /// set differs from the tool inventory in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_offered: Option<Vec<String>>,
}

impl TurnMarker {
    pub fn validate(&self) -> Result<()> {
        if self.turn == 0 {
            bail!("cognition_in turn marker has turn 0");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A completed or interrupted turn: the assistant message the model produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionOutPayload {
    pub turn: u32,
    pub parts: Vec<Part>,
    /// The normalised finish reason, or [`FINISH_INTERRUPTED`].
    pub finish: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<CognitionUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_accounting: Option<Value>,
}

impl CognitionOutPayload {
    pub fn is_interrupted(&self) -> bool {
        self.finish == FINISH_INTERRUPTED
    }

    pub fn message(&self) -> Message {
        Message::new(Role::Assistant, self.parts.clone())
    }

    pub fn validate(&self) -> Result<()> {
        if self.turn == 0 {
            bail!("cognition_out has turn 0");
        }
        validate_token("cognition_out finish", &self.finish)?;
        let message = self.message();
        message.validate()?;
        if self.is_interrupted() && message.tool_calls().next().is_some() {
            bail!("an interrupted cognition_out cannot carry tool calls");
        }
        Ok(())
    }
}

/// What the result guard decided when the result was recorded, so a rebuild
/// reproduces the request without deciding again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultGuard {
    /// Size of the result before the guard acted.
    pub result_size_bytes: u64,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
    /// The hash of the earlier result this one repeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultPayload {
    pub operation_id: String,
    pub call_id: String,
    pub tool: String,
    pub status: ToolResultStatus,
    pub parts: Vec<Part>,
    pub guard: ToolResultGuard,
}

impl ToolResultPayload {
    /// Joined text of the result's text parts: exactly the model-visible
    /// string the live run put in the provider message.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The tool message this result becomes.
    pub fn message(&self) -> Message {
        Message::new(
            Role::Tool,
            vec![Part::tool_result(
                self.call_id.clone(),
                self.parts.clone(),
                self.status,
            )],
        )
    }

    pub fn validate(&self) -> Result<()> {
        if !crate::callback::valid_action_operation_id(&self.operation_id) {
            bail!("tool result operation_id is not a canonical lowercase SHA-256 digest");
        }
        if self.call_id.is_empty() {
            bail!("tool result call_id is empty");
        }
        if self.guard.truncated && self.guard.truncated_reason.is_none() {
            bail!("tool result guard is truncated without a reason");
        }
        if self.guard.duplicate_of.is_some() && self.status == ToolResultStatus::Error {
            bail!("an error tool result cannot be a duplicate");
        }
        self.message().validate()
    }
}

/// Serialise a typed payload to the JSON an event carries.
pub fn to_payload<T: Serialize>(payload: &T) -> Result<Value> {
    Ok(serde_json::to_value(payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_serialises_without_empty_optionals() {
        let input = CognitionInput::text("launch", "hi").pinned();
        assert_eq!(
            to_payload(&input).unwrap(),
            json!({"origin": "launch", "parts": [{"type": "text", "text": "hi"}], "pinned": true})
        );
        let plain = CognitionInput::text("operator", "steer");
        assert_eq!(
            to_payload(&plain).unwrap(),
            json!({"origin": "operator", "parts": [{"type": "text", "text": "steer"}]})
        );
    }

    #[test]
    fn origin_must_be_a_token() {
        assert!(CognitionInput::text("Launch", "x").validate().is_err());
        assert!(CognitionInput::text("", "x").validate().is_err());
        assert!(CognitionInput::text("context", "x").validate().is_ok());
    }

    #[test]
    fn interrupted_turn_cannot_carry_calls() {
        let payload = CognitionOutPayload {
            turn: 1,
            parts: vec![Part::tool_call("c1", "tool", json!({}))],
            finish: FINISH_INTERRUPTED.to_string(),
            usage: None,
            provider_accounting: None,
        };
        assert!(payload.validate().is_err());
    }

    #[test]
    fn duplicate_call_ids_in_one_answer_are_refused() {
        let payload = CognitionOutPayload {
            turn: 1,
            parts: vec![
                Part::tool_call("c1", "tool", json!({})),
                Part::tool_call("c1", "tool", json!({})),
            ],
            finish: "tool_calls".to_string(),
            usage: None,
            provider_accounting: None,
        };
        assert!(payload.validate().is_err());
    }
}
