//! Typed payloads of the conversation events a directive records.
//!
//! A conversation is recorded as parts (the provider contract's neutral turn)
//! in four event shapes:
//!
//! - `cognition_in` input: `{origin, parts, kind?, ack?, pinned?}`, or a turn
//!   marker `{turn, tools_offered?}`;
//! - `cognition_out`: `{turn, parts, finish, usage?, provider_accounting?,
//!   malformed_calls?}`;
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

/// Kind of the harness input that tells the model its run is nearly out of
/// time. Readers that do not know it treat it as any other harness input.
pub const KIND_DEADLINE_WRAP_UP: &str = "deadline_wrap_up";

/// Kind of the harness input that tells the model the next turn is the last
/// the run's turn limit allows.
pub const KIND_LIMIT_WRAP_UP: &str = "limit_wrap_up";

/// Kind of the single harness input that gives both reasons at once, when the
/// run deadline's wrap-up and the turn limit's fall on the same turn.
pub const KIND_DEADLINE_LIMIT_WRAP_UP: &str = "deadline_limit_wrap_up";

/// The kind of the wrap-up input that gives these reasons, if any is due.
pub fn wrap_up_kind(deadline: bool, turn_limit: bool) -> Option<&'static str> {
    match (deadline, turn_limit) {
        (true, true) => Some(KIND_DEADLINE_LIMIT_WRAP_UP),
        (true, false) => Some(KIND_DEADLINE_WRAP_UP),
        (false, true) => Some(KIND_LIMIT_WRAP_UP),
        (false, false) => None,
    }
}

/// The reasons a wrap-up input of this kind gave, as (deadline, turn limit);
/// `None` for any other kind. A recovered run reads its delivered flags from
/// these.
pub fn wrap_up_reasons(kind: &str) -> Option<(bool, bool)> {
    match kind {
        KIND_DEADLINE_WRAP_UP => Some((true, false)),
        KIND_LIMIT_WRAP_UP => Some((false, true)),
        KIND_DEADLINE_LIMIT_WRAP_UP => Some((true, true)),
        _ => None,
    }
}

/// Prefix of the kind of a harness input that warns the model its run is
/// looping; the finding's code follows it.
pub const KIND_LOOP_WARNING_PREFIX: &str = "loop_warning_";

/// Prefix of the kind of the harness input that replaces a malformed reply.
/// The kind names the turn it corrects and the reply's position in the run of
/// malformed replies: `malformed_reply_turn_<turn>_streak_<position>`.
pub const KIND_MALFORMED_REPLY_PREFIX: &str = "malformed_reply_turn_";

const MALFORMED_REPLY_STREAK_SEPARATOR: &str = "_streak_";

/// The kind of the input that corrects `turn`, the `streak`th malformed reply
/// in a row (counting from 1).
pub fn malformed_reply_kind(turn: u32, streak: u32) -> String {
    format!("{KIND_MALFORMED_REPLY_PREFIX}{turn}{MALFORMED_REPLY_STREAK_SEPARATOR}{streak}")
}

/// The turn a malformed-reply kind corrects and its position in the run, or
/// `None` for any other kind.
pub fn malformed_reply_of_kind(kind: &str) -> Option<(u32, u32)> {
    correction_position(kind.strip_prefix(KIND_MALFORMED_REPLY_PREFIX)?)
}

/// Prefix of the kind of the harness input that replaces an empty reply (no
/// tool call, no text). The kind names the turn it corrects and the reply's
/// position in the run of empty replies: `empty_reply_turn_<turn>_streak_<position>`.
pub const KIND_EMPTY_REPLY_PREFIX: &str = "empty_reply_turn_";

/// The kind of the input that corrects `turn`, the `streak`th empty reply in
/// a row (counting from 1).
pub fn empty_reply_kind(turn: u32, streak: u32) -> String {
    format!("{KIND_EMPTY_REPLY_PREFIX}{turn}{MALFORMED_REPLY_STREAK_SEPARATOR}{streak}")
}

/// The turn an empty-reply kind corrects and its position in the run, or
/// `None` for any other kind.
pub fn empty_reply_of_kind(kind: &str) -> Option<(u32, u32)> {
    correction_position(kind.strip_prefix(KIND_EMPTY_REPLY_PREFIX)?)
}

fn correction_position(rest: &str) -> Option<(u32, u32)> {
    let (turn, streak) = rest.split_once(MALFORMED_REPLY_STREAK_SEPARATOR)?;
    let turn = turn.parse::<u32>().ok().filter(|turn| *turn > 0)?;
    let streak = streak.parse::<u32>().ok().filter(|streak| *streak > 0)?;
    Some((turn, streak))
}

/// The closed set of reply corrections. Each has its own kind prefix, its own
/// policy and its own streak; the prefixes are matched exactly, so a kind of
/// one never reads as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReplyCorrection {
    Malformed,
    Empty,
}

/// Which correction a kind is, the turn it names and its position in that
/// correction's run, or `None` for any other kind.
pub fn reply_correction_of_kind(kind: &str) -> Option<(ReplyCorrection, u32, u32)> {
    if let Some((turn, streak)) = malformed_reply_of_kind(kind) {
        Some((ReplyCorrection::Malformed, turn, streak))
    } else {
        empty_reply_of_kind(kind).map(|(turn, streak)| (ReplyCorrection::Empty, turn, streak))
    }
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

/// A tool call whose streamed argument text did not parse as JSON. The call
/// part carries a recovered empty object; this record is the fact that the
/// call must not be dispatched, so a recovery rebuilds the same decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MalformedCall {
    /// Ordinal of the call among the `tool_call` parts of the turn.
    pub call_index: u32,
    /// Byte length of the unparsable argument text.
    pub raw_len: u64,
    /// SHA-256 (lowercase hex) of the unparsable argument text.
    pub sha256: String,
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
    /// Calls of this turn whose arguments were not valid JSON. Recorded in the
    /// same event as the turn, so a crash cannot separate them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub malformed_calls: Vec<MalformedCall>,
}

impl CognitionOutPayload {
    /// The malformed-argument record for the call at `call_index`, if any.
    pub fn malformed_call(&self, call_index: usize) -> Option<&MalformedCall> {
        self.malformed_calls
            .iter()
            .find(|call| usize::try_from(call.call_index).ok() == Some(call_index))
    }

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
        let call_count = message.tool_calls().count();
        let mut previous: Option<u32> = None;
        for malformed in &self.malformed_calls {
            if usize::try_from(malformed.call_index).map_or(true, |index| index >= call_count) {
                bail!(
                    "cognition_out malformed call index {} does not name a tool_call part",
                    malformed.call_index
                );
            }
            if previous.is_some_and(|prior| prior >= malformed.call_index) {
                bail!("cognition_out malformed calls must be sorted by unique call index");
            }
            previous = Some(malformed.call_index);
            if malformed.sha256.len() != 64
                || !malformed
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                bail!("cognition_out malformed call sha256 must be 64 lowercase hex digits");
            }
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
    /// The images of the result that were not used, each with its stable
    /// code. The model-visible text ends with a note for each; they are kept
    /// apart from the guard's own truncation so a rebuild can tell the guarded
    /// text from the notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_images: Vec<DroppedImage>,
    /// Set when the full result was stored beside this event and the model saw
    /// a bounded preview. The preview is the recorded text; the stored bytes
    /// are reachable through `attachment_refs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill: Option<ToolResultSpill>,
}

/// The `truncated_reason` of a result whose full text was stored and replaced
/// by a preview.
pub const TOOL_RESULT_SPILLED: &str = "result_spilled";

/// The first line of the model-visible text of a spilled result, up to the
/// byte count. Context folding recognises a spilled result by this prefix.
pub const TOOL_RESULT_PARTIAL_PREFIX: &str = "[tool result partial: ";

/// Where the full text of a spilled tool result lives and what the preview
/// kept of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultSpill {
    /// Object hash of the stored attachment; the last entry of the result's
    /// `attachment_refs`.
    pub attachment: String,
    /// Bytes of the result the attachment holds; less than `total_bytes` when
    /// the result exceeded the node's attachment size limit.
    pub stored_bytes: u64,
    /// Size of the result before the guard acted.
    pub total_bytes: u64,
    /// Leading bytes of the result shown in the preview.
    pub preview_head_bytes: u64,
    /// Trailing bytes of the result shown in the preview.
    pub preview_tail_bytes: u64,
}

/// One image of a tool result that was dropped instead of used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DroppedImage {
    /// One-based position among the result's image parts.
    pub position: u32,
    pub code: String,
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
    /// The object hashes of the attachments the image parts name, distinct and
    /// in order of first use. Absent when the result carries no image. The
    /// daemon links each to the event, so the bytes stay reachable from the
    /// signed chain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_refs: Vec<String>,
}

impl ToolResultPayload {
    /// The distinct attachment hashes of a result's image parts, in order of
    /// first use: what `attachment_refs` must list.
    pub fn image_attachment_hashes(parts: &[Part]) -> Vec<String> {
        let mut hashes: Vec<String> = Vec::new();
        for part in parts {
            if let Part::Image(image) = part
                && !hashes.contains(&image.attachment)
            {
                hashes.push(image.attachment.clone());
            }
        }
        hashes
    }

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
        let mut expected = Self::image_attachment_hashes(&self.parts);
        if let Some(spill) = &self.guard.spill {
            if !self.guard.truncated
                || self.guard.truncated_reason.as_deref() != Some(TOOL_RESULT_SPILLED)
            {
                bail!(
                    "a spilled tool result must be truncated with reason `{TOOL_RESULT_SPILLED}`"
                );
            }
            if self.guard.duplicate_of.is_some() {
                bail!("a spilled tool result cannot be a duplicate");
            }
            if spill.stored_bytes > spill.total_bytes
                || spill.preview_head_bytes + spill.preview_tail_bytes > spill.total_bytes
            {
                bail!("tool result spill byte counts contradict each other");
            }
            expected.push(spill.attachment.clone());
        }
        if self.attachment_refs != expected {
            bail!(
                "tool result attachment_refs do not match the attachments of its image parts and its spill"
            );
        }
        self.message().validate()
    }
}

/// How the provider request is configured for the whole conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameRequestOptions {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Value>,
    /// The configured output limit per turn; absent when none is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// What the model is given that is not a conversation message and that is
/// stable across turns: the system parts, the tool inventory, the request
/// options and the identity of the provider configuration in force.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionFramePayload {
    /// The frame's position among the frames of a chain, starting at 1.
    pub frame_seq: u32,
    pub system: Vec<Part>,
    /// The object hash of the attachment holding the tool inventory.
    pub tool_inventory: String,
    pub request_options: FrameRequestOptions,
    /// The hash of the provider configuration the request is built from.
    pub provider_config: String,
    /// The attachments the frame names; always the tool inventory.
    pub attachment_refs: Vec<String>,
}

impl CognitionFramePayload {
    pub fn validate(&self) -> Result<()> {
        if self.frame_seq == 0 {
            bail!("cognition_frame has frame_seq 0");
        }
        if self.request_options.model.is_empty() {
            bail!("cognition_frame request_options has no model");
        }
        ryeos_effect_contract::require_hex64(
            "cognition_frame tool_inventory",
            &self.tool_inventory,
        )?;
        ryeos_effect_contract::require_hex64(
            "cognition_frame provider_config",
            &self.provider_config,
        )?;
        if self.attachment_refs != [self.tool_inventory.clone()] {
            bail!("cognition_frame attachment_refs must name exactly its tool inventory");
        }
        if self
            .system
            .iter()
            .any(|part| !matches!(part, Part::Text(_)))
        {
            bail!("cognition_frame system parts must be text");
        }
        Ok(())
    }

    /// Joined text of the system parts; `None` when the frame has no system
    /// text.
    pub fn system_text(&self) -> Option<String> {
        let text: String = self
            .system
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        (!text.is_empty()).then_some(text)
    }

    /// Whether two frames give the model the same thing. The position in the
    /// chain is not part of it.
    pub fn same_as(&self, other: &Self) -> bool {
        self.system == other.system
            && self.tool_inventory == other.tool_inventory
            && self.request_options == other.request_options
            && self.provider_config == other.provider_config
    }

    /// The frame components in which `other` differs from this frame, in the
    /// order of [`FrameComponent`]. `system` compares the system parts,
    /// `tools` the tool inventory, `reasoning` the request's reasoning
    /// policy. Other differences (model, sampling, output limit, provider
    /// configuration) are not components a provider can refuse.
    pub fn differing_components(&self, other: &Self) -> Vec<FrameComponent> {
        let mut differing = Vec::new();
        if self.system != other.system {
            differing.push(FrameComponent::System);
        }
        if self.tool_inventory != other.tool_inventory {
            differing.push(FrameComponent::Tools);
        }
        if self.request_options.reasoning != other.request_options.reasoning {
            differing.push(FrameComponent::Reasoning);
        }
        differing
    }
}

/// A part of the request frame that a provider entry can refuse to see
/// change within one conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameComponent {
    System,
    Tools,
    Reasoning,
}

impl FrameComponent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Tools => "tools",
            Self::Reasoning => "reasoning",
        }
    }
}

/// A frame ready to append: its payload and the attachment holding the tool
/// inventory the payload names.
#[derive(Debug, Clone)]
pub struct FrameRecord {
    pub payload: CognitionFramePayload,
    pub inventory: crate::callback::EventAttachment,
}

impl FrameRecord {
    /// Builds a frame whose inventory is `inventory_json`, the exact bytes of
    /// the canonical JSON tool list.
    pub fn new(
        frame_seq: u32,
        system: Option<&str>,
        request_options: FrameRequestOptions,
        provider_config: &str,
        inventory_json: Vec<u8>,
    ) -> Result<Self> {
        let inventory = crate::callback::EventAttachment::new("application/json", inventory_json)?;
        let payload = CognitionFramePayload {
            frame_seq,
            system: system.map(Part::text).into_iter().collect(),
            tool_inventory: inventory.object_hash().to_string(),
            request_options,
            provider_config: provider_config.to_string(),
            attachment_refs: vec![inventory.object_hash().to_string()],
        };
        payload.validate()?;
        Ok(Self { payload, inventory })
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
    fn a_malformed_reply_kind_names_its_turn_and_position() {
        let kind = malformed_reply_kind(7, 2);
        assert_eq!(kind, "malformed_reply_turn_7_streak_2");
        validate_token("kind", &kind).unwrap();
        assert_eq!(malformed_reply_of_kind(&kind), Some((7, 2)));
        for other in [
            "output_limit",
            "malformed_reply_turn_0_streak_1",
            "malformed_reply_turn_3_streak_0",
            "malformed_reply_turn_x_streak_1",
            "malformed_reply_turn_3",
        ] {
            assert_eq!(malformed_reply_of_kind(other), None, "{other}");
        }
    }

    #[test]
    fn an_empty_reply_kind_names_its_turn_and_position_and_is_not_malformed() {
        let kind = empty_reply_kind(7, 2);
        assert_eq!(kind, "empty_reply_turn_7_streak_2");
        validate_token("kind", &kind).unwrap();
        assert_eq!(empty_reply_of_kind(&kind), Some((7, 2)));
        assert_eq!(
            reply_correction_of_kind(&kind),
            Some((ReplyCorrection::Empty, 7, 2))
        );
        assert_eq!(malformed_reply_of_kind(&kind), None);
        let malformed = malformed_reply_kind(7, 2);
        assert_eq!(empty_reply_of_kind(&malformed), None);
        assert_eq!(
            reply_correction_of_kind(&malformed),
            Some((ReplyCorrection::Malformed, 7, 2))
        );
        for other in [
            "empty_reply_turn_0_streak_1",
            "empty_reply_turn_3_streak_0",
            "empty_reply_turn_x_streak_1",
            "empty_reply_turn_3",
            "xempty_reply_turn_3_streak_1",
            "output_limit",
        ] {
            assert_eq!(empty_reply_of_kind(other), None, "{other}");
            assert_eq!(reply_correction_of_kind(other), None, "{other}");
        }
    }

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
            malformed_calls: Vec::new(),
        };
        assert!(payload.validate().is_err());
    }

    fn spilled_result() -> ToolResultPayload {
        let attachment = "ab".repeat(32);
        ToolResultPayload {
            operation_id: "cd".repeat(32),
            call_id: "call-1".to_string(),
            tool: "t".to_string(),
            status: ToolResultStatus::Ok,
            parts: vec![Part::text(format!(
                "{TOOL_RESULT_PARTIAL_PREFIX}9000 bytes, full result stored as attachment {attachment}; the full result is not available to you]\nx"
            ))],
            guard: ToolResultGuard {
                result_size_bytes: 9000,
                truncated: true,
                truncated_reason: Some(TOOL_RESULT_SPILLED.to_string()),
                duplicate_of: None,
                dropped_images: Vec::new(),
                spill: Some(ToolResultSpill {
                    attachment: attachment.clone(),
                    stored_bytes: 9000,
                    total_bytes: 9000,
                    preview_head_bytes: 100,
                    preview_tail_bytes: 50,
                }),
            },
            attachment_refs: vec![attachment],
        }
    }

    #[test]
    fn a_spilled_result_names_its_stored_text_and_round_trips() {
        let payload = spilled_result();
        payload.validate().unwrap();
        let value = to_payload(&payload).unwrap();
        assert_eq!(value["guard"]["spill"]["total_bytes"], 9000);
        let parsed: ToolResultPayload = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, payload);
        // A result without a spill carries no `spill` key at all.
        let mut plain = spilled_result();
        plain.guard.spill = None;
        plain.guard.truncated = false;
        plain.guard.truncated_reason = None;
        plain.attachment_refs.clear();
        plain.validate().unwrap();
        assert!(to_payload(&plain).unwrap()["guard"].get("spill").is_none());
    }

    #[test]
    fn a_spilled_result_that_contradicts_itself_is_refused() {
        let mut unlisted = spilled_result();
        unlisted.attachment_refs.clear();
        assert!(unlisted.validate().is_err());
        let mut wrong_reason = spilled_result();
        wrong_reason.guard.truncated_reason = Some("result_guard".to_string());
        assert!(wrong_reason.validate().is_err());
        let mut impossible = spilled_result();
        impossible.guard.spill.as_mut().unwrap().stored_bytes = 10_000;
        assert!(impossible.validate().is_err());
        let mut preview_too_big = spilled_result();
        preview_too_big
            .guard
            .spill
            .as_mut()
            .unwrap()
            .preview_head_bytes = 9000;
        assert!(preview_too_big.validate().is_err());
    }

    fn frame(system: Option<&str>, model: &str) -> FrameRecord {
        FrameRecord::new(
            1,
            system,
            FrameRequestOptions {
                model: model.to_string(),
                reasoning: None,
                sampling: None,
                max_output_tokens: Some(1024),
            },
            &"ab".repeat(32),
            br#"[{"name":"tool"}]"#.to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn a_frame_names_its_inventory_attachment() {
        let record = frame(Some("be brief"), "m");
        assert_eq!(
            record.payload.tool_inventory,
            record.inventory.object_hash()
        );
        assert_eq!(
            record.payload.attachment_refs,
            [record.inventory.object_hash().to_string()]
        );
        assert_eq!(record.payload.system_text().as_deref(), Some("be brief"));
        let value = to_payload(&record.payload).unwrap();
        let parsed: CognitionFramePayload = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, record.payload);
        parsed.validate().unwrap();
    }

    #[test]
    fn frames_compare_without_their_position() {
        let first = frame(Some("be brief"), "m");
        let mut later = first.payload.clone();
        later.frame_seq = 2;
        assert!(first.payload.same_as(&later));
        assert!(!first.payload.same_as(&frame(Some("be long"), "m").payload));
        assert!(!first.payload.same_as(&frame(Some("be brief"), "n").payload));
        let mut other_inventory = first.payload.clone();
        other_inventory.tool_inventory = "cd".repeat(32);
        assert!(!first.payload.same_as(&other_inventory));
    }

    #[test]
    fn differing_components_name_only_what_a_provider_can_refuse() {
        let first = frame(Some("be brief"), "m").payload;
        assert!(first.differing_components(&first.clone()).is_empty());
        assert_eq!(
            first.differing_components(&frame(Some("be long"), "m").payload),
            [FrameComponent::System]
        );
        // A different model is a frame change but not a refusable component.
        assert!(
            first
                .differing_components(&frame(Some("be brief"), "n").payload)
                .is_empty()
        );
        let mut changed = first.clone();
        changed.tool_inventory = "cd".repeat(32);
        changed.request_options.reasoning = Some(json!({"effort": "high"}));
        assert_eq!(
            first.differing_components(&changed),
            [FrameComponent::Tools, FrameComponent::Reasoning]
        );
    }

    #[test]
    fn a_frame_that_does_not_name_its_inventory_is_refused() {
        let mut payload = frame(None, "m").payload;
        payload.attachment_refs = Vec::new();
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
            malformed_calls: Vec::new(),
        };
        assert!(payload.validate().is_err());
    }
}
