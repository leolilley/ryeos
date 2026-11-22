//! Generic capture of reasoning from a `delta_merge` stream, driven only by
//! the signed `schemas.reasoning.capture` rules. No field name or provider
//! shape is known here: the rules name the delta fields to read.
//!
//! Three mechanical rule kinds exist. `text_delta` reads string delta fields
//! as visible reasoning text. `blocks` assembles the items of an array field
//! into one opaque artefact per block and emits it when the block completes.
//! `tool_call_field` collects a field riding on streamed tool-call entries
//! and attaches it to the call it belongs to. Assembly state lives in the
//! parser's string state bag under the `rc_` prefix, so a stream split over
//! many events keeps its partial blocks between calls.

use std::collections::HashMap;

use ryeos_directive_definition::{ProviderConfig, ReasoningCaptureRule, ToolCallFieldMerge};
use serde_json::{Map, Value};

use crate::directive::{StreamEvent, ToolCallArtefact};

/// State key prefix of a block that has opened and not yet completed.
const OPEN_BLOCK_PREFIX: &str = "rc_open_";
/// State key prefix of a tool-call field value awaiting its call.
const CALL_FIELD_PREFIX: &str = "rc_call_";

/// Stable warning code for a block dropped because it never completed.
pub const INCOMPLETE_ARTEFACT_CODE: &str = "reasoning_artefact_incomplete";

/// The provider entry's declared capture rules; empty when it declares none.
pub fn capture_rules(provider: &ProviderConfig) -> &[ReasoningCaptureRule] {
    provider
        .schemas
        .as_ref()
        .and_then(|schemas| schemas.reasoning.as_ref())
        .map_or(&[], |reasoning| reasoning.capture.as_slice())
}

/// The compat group the entry stamps on every artefact it captures.
pub fn replay_group(provider: &ProviderConfig) -> Option<&str> {
    provider
        .schemas
        .as_ref()
        .and_then(|schemas| schemas.reasoning.as_ref())
        .and_then(|reasoning| reasoning.replay.as_ref())
        .and_then(|replay| replay.compat_group.as_deref())
}

/// Wraps a captured value in the envelope the replay rules read. `capture`
/// names the rule that produced it and `group` the compat group of the entry
/// that captured it; an artefact replays only under the same group.
pub fn envelope(capture_id: &str, group: Option<&str>, value: Value) -> Value {
    let mut envelope = Map::new();
    envelope.insert("capture".to_string(), Value::String(capture_id.to_string()));
    if let Some(group) = group {
        envelope.insert("group".to_string(), Value::String(group.to_string()));
    }
    envelope.insert("value".to_string(), value);
    Value::Object(envelope)
}

/// Reads every delta-level rule from one chunk's `delta` object: text fields
/// become reasoning text, block items assemble into artefacts.
pub fn capture_delta(
    delta: &Value,
    rules: &[ReasoningCaptureRule],
    state: &mut HashMap<String, String>,
    events: &mut Vec<StreamEvent>,
) {
    for rule in rules {
        match rule {
            ReasoningCaptureRule::TextDelta { fields, .. } => {
                if let Some(text) = fields
                    .iter()
                    .find_map(|field| delta.get(field.as_str()))
                    .and_then(Value::as_str)
                {
                    events.push(StreamEvent::ReasoningDelta(text.to_string()));
                }
            }
            ReasoningCaptureRule::Blocks { field, .. } => {
                if let Some(items) = delta.get(field.as_str()).and_then(Value::as_array) {
                    for item in items {
                        if let Value::Object(item) = item {
                            assemble_block_item(rule, item, state, events);
                        }
                    }
                }
            }
            ReasoningCaptureRule::ToolCallField { .. } => {}
        }
    }
}

fn str_key<'a>(key: Option<&'a String>, default: &'a str) -> &'a str {
    key.map(String::as_str).unwrap_or(default)
}

fn assemble_block_item(
    rule: &ReasoningCaptureRule,
    item: &Map<String, Value>,
    state: &mut HashMap<String, String>,
    events: &mut Vec<StreamEvent>,
) {
    let ReasoningCaptureRule::Blocks {
        id,
        type_key,
        merge_keys,
        close_key,
        whole_types,
        group_key,
        text_key,
        ..
    } = rule
    else {
        return;
    };
    let type_key = str_key(type_key.as_ref(), "type");
    let item_type = item.get(type_key).and_then(Value::as_str).unwrap_or("");

    if let Some(text) = text_key
        .as_deref()
        .and_then(|key| item.get(key))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        events.push(StreamEvent::ReasoningDelta(text.to_string()));
    }

    if whole_types.iter().any(|whole| whole == item_type) {
        events.push(StreamEvent::ReasoningArtefact {
            capture_id: id.clone(),
            value: Value::Object(item.clone()),
        });
        return;
    }

    let group = group_key
        .as_deref()
        .and_then(|key| item.get(key))
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "_".to_string());
    let slot = format!("{OPEN_BLOCK_PREFIX}{id}:{group}");

    let mut block: Map<String, Value> = state
        .get(&slot)
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    // Without a group key there is one open slot, so an item of another type
    // means the open block was abandoned: drop it, never replay half a block.
    // An item that names no type is a continuation chunk of the open block
    // (a bare signature, say) and never displaces it.
    if group_key.is_none()
        && !block.is_empty()
        && item.contains_key(type_key)
        && block.get(type_key).and_then(Value::as_str).unwrap_or("") != item_type
    {
        events.push(incomplete_warning(id, &block));
        block.clear();
    }
    for (key, value) in item {
        if merge_keys.contains(key)
            && let (Some(Value::String(existing)), Value::String(piece)) =
                (block.get_mut(key), value)
        {
            existing.push_str(piece);
        } else {
            block.insert(key.clone(), value.clone());
        }
    }

    let closed = close_key
        .as_deref()
        .and_then(|key| block.get(key))
        .is_some_and(|value| match value {
            Value::Null => false,
            Value::String(text) => !text.is_empty(),
            _ => true,
        });
    if closed {
        state.remove(&slot);
        events.push(StreamEvent::ReasoningArtefact {
            capture_id: id.clone(),
            value: Value::Object(block),
        });
    } else {
        state.insert(slot, Value::Object(block).to_string());
    }
}

fn incomplete_warning(id: &str, block: &Map<String, Value>) -> StreamEvent {
    StreamEvent::Warning {
        code: INCOMPLETE_ARTEFACT_CODE.to_string(),
        message: format!(
            "reasoning capture {id:?} dropped a block that never completed ({} keys seen)",
            block.len()
        ),
    }
}

/// Records the capture field of one streamed tool-call entry against its
/// accumulation slot.
pub fn capture_tool_call_entry(
    entry: &Value,
    slot: &str,
    rules: &[ReasoningCaptureRule],
    state: &mut HashMap<String, String>,
) {
    for rule in rules {
        let ReasoningCaptureRule::ToolCallField { id, field, merge } = rule else {
            continue;
        };
        let Some(value) = entry.get(field.as_str()).filter(|value| !value.is_null()) else {
            continue;
        };
        let key = format!("{CALL_FIELD_PREFIX}{id}:{slot}");
        let merged = match (
            merge,
            state
                .get(&key)
                .and_then(|raw| serde_json::from_str(raw).ok()),
        ) {
            (ToolCallFieldMerge::ObjectMerge, Some(Value::Object(mut held))) => {
                if let Value::Object(more) = value {
                    held.extend(more.clone());
                    Value::Object(held)
                } else {
                    value.clone()
                }
            }
            _ => value.clone(),
        };
        state.insert(key, merged.to_string());
    }
}

/// Takes the captured field of the call in `slot`, if the stream carried one.
pub fn take_tool_call_artefact(
    slot: &str,
    rules: &[ReasoningCaptureRule],
    state: &mut HashMap<String, String>,
) -> Option<ToolCallArtefact> {
    // A call holds one artefact, so provider validation caps an entry at one
    // `tool_call_field` rule; only the first such rule is read here.
    debug_assert!(
        rules
            .iter()
            .filter(|rule| matches!(rule, ReasoningCaptureRule::ToolCallField { .. }))
            .count()
            <= 1,
        "validation allows at most one tool_call_field rule"
    );
    rules.iter().find_map(|rule| {
        let ReasoningCaptureRule::ToolCallField { id, .. } = rule else {
            return None;
        };
        let raw = state.remove(&format!("{CALL_FIELD_PREFIX}{id}:{slot}"))?;
        let value = serde_json::from_str(&raw).ok()?;
        Some(ToolCallArtefact {
            capture_id: id.clone(),
            value,
        })
    })
}

/// Ends the stream's capture: every block still open is dropped with a
/// warning, and any tool-call value whose call never completed is discarded.
pub fn finish_capture(state: &mut HashMap<String, String>, events: &mut Vec<StreamEvent>) {
    let mut open: Vec<String> = state
        .keys()
        .filter(|key| key.starts_with(OPEN_BLOCK_PREFIX))
        .cloned()
        .collect();
    open.sort();
    for key in open {
        let block: Map<String, Value> = state
            .remove(&key)
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let id = key
            .strip_prefix(OPEN_BLOCK_PREFIX)
            .and_then(|rest| rest.split(':').next())
            .unwrap_or("");
        events.push(incomplete_warning(id, &block));
    }
    state.retain(|key, _| !key.starts_with(CALL_FIELD_PREFIX));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude_rules() -> Vec<ReasoningCaptureRule> {
        serde_json::from_value(json!([
            {"id": "text", "kind": "text_delta", "fields": ["reasoning_content", "reasoning"]},
            {"id": "blocks", "kind": "blocks", "field": "thinking_blocks",
             "merge_keys": ["thinking"], "close_key": "signature",
             "whole_types": ["redacted_thinking"]}
        ]))
        .unwrap()
    }

    fn run(
        rules: &[ReasoningCaptureRule],
        deltas: &[Value],
    ) -> (Vec<StreamEvent>, HashMap<String, String>) {
        let mut state = HashMap::new();
        let mut events = Vec::new();
        for delta in deltas {
            capture_delta(delta, rules, &mut state, &mut events);
        }
        (events, state)
    }

    fn artefacts(events: &[StreamEvent]) -> Vec<(String, Value)> {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ReasoningArtefact { capture_id, value } => {
                    Some((capture_id.clone(), value.clone()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_fields_are_read_in_declared_order() {
        let (events, _) = run(
            &claude_rules(),
            &[
                json!({"reasoning": "b"}),
                json!({"reasoning_content": "a", "reasoning": "ignored"}),
                json!({"content": "x"}),
            ],
        );
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ReasoningDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["b", "a"]);
    }

    #[test]
    fn a_block_completes_when_its_signature_arrives_in_a_final_chunk() {
        let (events, state) = run(
            &claude_rules(),
            &[
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "ab"}]}),
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "cd"}]}),
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "", "signature": "S1"}]}),
            ],
        );
        assert_eq!(
            artefacts(&events),
            [(
                "blocks".to_string(),
                json!({"type": "thinking", "thinking": "abcd", "signature": "S1"})
            )]
        );
        assert!(state.is_empty(), "completed block leaves no state");
    }

    #[test]
    fn a_signature_riding_the_last_text_piece_completes_the_block() {
        let (events, _) = run(
            &claude_rules(),
            &[
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "ab"}]}),
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "cd", "signature": "S"}]}),
            ],
        );
        assert_eq!(artefacts(&events)[0].1["thinking"], "abcd");
    }

    #[test]
    fn a_whole_type_item_is_complete_on_arrival_and_keeps_every_key() {
        let (events, _) = run(
            &claude_rules(),
            &[
                json!({"thinking_blocks": [{"type": "redacted_thinking", "data": "xx==", "extra": 1}]}),
            ],
        );
        assert_eq!(
            artefacts(&events)[0].1,
            json!({"type": "redacted_thinking", "data": "xx==", "extra": 1})
        );
    }

    #[test]
    fn an_empty_signature_does_not_close_a_block() {
        let (events, state) = run(
            &claude_rules(),
            &[json!({"thinking_blocks": [{"type": "thinking", "thinking": "a", "signature": ""}]})],
        );
        assert!(artefacts(&events).is_empty());
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn a_block_of_another_type_displaces_the_open_one_with_a_warning() {
        let (events, _) = run(
            &claude_rules(),
            &[
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "a"}]}),
                json!({"thinking_blocks": [{"type": "other", "thinking": "b", "signature": "S"}]}),
            ],
        );
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Warning { code, .. } if code == INCOMPLETE_ARTEFACT_CODE
        )));
        assert_eq!(artefacts(&events)[0].1["thinking"], "b");
    }

    #[test]
    fn a_type_less_signature_chunk_continues_the_open_block() {
        let (events, state) = run(
            &claude_rules(),
            &[
                json!({"thinking_blocks": [{"type": "thinking", "thinking": "ab"}]}),
                json!({"thinking_blocks": [{"signature": "S1"}]}),
            ],
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Warning { .. })),
            "a continuation chunk must not drop the block"
        );
        assert_eq!(
            artefacts(&events)[0].1,
            json!({"type": "thinking", "thinking": "ab", "signature": "S1"})
        );
        assert!(state.is_empty());
    }

    #[test]
    fn an_open_block_at_stream_end_is_dropped_with_a_warning() {
        let (mut events, mut state) = run(
            &claude_rules(),
            &[json!({"thinking_blocks": [{"type": "thinking", "thinking": "never closed"}]})],
        );
        finish_capture(&mut state, &mut events);
        assert!(artefacts(&events).is_empty());
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Warning { code, .. } if code == INCOMPLETE_ARTEFACT_CODE
        )));
        assert!(state.is_empty());
    }

    #[test]
    fn interleaved_blocks_assemble_by_group_key() {
        let rules: Vec<ReasoningCaptureRule> = serde_json::from_value(json!([
            {"id": "b", "kind": "blocks", "field": "blocks", "merge_keys": ["t"],
             "close_key": "sig", "group_key": "index"}
        ]))
        .unwrap();
        let (events, _) = run(
            &rules,
            &[
                json!({"blocks": [{"index": 0, "t": "a"}, {"index": 1, "t": "x"}]}),
                json!({"blocks": [{"index": 1, "t": "y", "sig": "S1"}]}),
                json!({"blocks": [{"index": 0, "t": "b", "sig": "S0"}]}),
            ],
        );
        let found = artefacts(&events);
        assert_eq!(found[0].1["t"], "xy");
        assert_eq!(found[1].1["t"], "ab");
    }

    #[test]
    fn text_key_also_emits_the_block_text_as_reasoning() {
        let rules: Vec<ReasoningCaptureRule> = serde_json::from_value(json!([
            {"id": "b", "kind": "blocks", "field": "blocks", "merge_keys": ["thinking"],
             "close_key": "signature", "text_key": "thinking"}
        ]))
        .unwrap();
        let (events, _) = run(
            &rules,
            &[json!({"blocks": [{"type": "thinking", "thinking": "hm", "signature": "S"}]})],
        );
        assert!(matches!(&events[0], StreamEvent::ReasoningDelta(text) if text == "hm"));
        assert_eq!(artefacts(&events).len(), 1);
    }

    #[test]
    fn a_tool_call_field_attaches_to_its_own_slot_only() {
        let rules: Vec<ReasoningCaptureRule> = serde_json::from_value(json!([
            {"id": "sig", "kind": "tool_call_field", "field": "extra_content",
             "merge": "object_merge"}
        ]))
        .unwrap();
        let mut state = HashMap::new();
        capture_tool_call_entry(&json!({"extra_content": {"a": 1}}), "0", &rules, &mut state);
        capture_tool_call_entry(&json!({"extra_content": {"b": 2}}), "0", &rules, &mut state);
        capture_tool_call_entry(&json!({"function": {}}), "1", &rules, &mut state);
        let first = take_tool_call_artefact("0", &rules, &mut state).expect("slot 0 value");
        assert_eq!(first.capture_id, "sig");
        assert_eq!(first.value, json!({"a": 1, "b": 2}));
        assert!(take_tool_call_artefact("1", &rules, &mut state).is_none());
        assert!(state.is_empty());
    }

    #[test]
    fn last_merge_replaces_the_value() {
        let rules: Vec<ReasoningCaptureRule> = serde_json::from_value(json!([
            {"id": "sig", "kind": "tool_call_field", "field": "sig", "merge": "last"}
        ]))
        .unwrap();
        let mut state = HashMap::new();
        capture_tool_call_entry(&json!({"sig": "one"}), "0", &rules, &mut state);
        capture_tool_call_entry(&json!({"sig": "two"}), "0", &rules, &mut state);
        assert_eq!(
            take_tool_call_artefact("0", &rules, &mut state)
                .unwrap()
                .value,
            json!("two")
        );
    }

    #[test]
    fn no_rules_read_nothing() {
        let (events, state) = run(
            &[],
            &[json!({"reasoning_content": "x", "thinking_blocks": [{"type": "thinking"}]})],
        );
        assert!(events.is_empty() && state.is_empty());
    }
}
