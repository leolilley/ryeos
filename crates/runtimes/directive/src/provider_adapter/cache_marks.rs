//! Cache marks written onto the final wire messages of a request.
//!
//! A pure post-pass over the Chat Completions message list the request
//! builder produced: it sees the history after unsendable turns were dropped,
//! images were lowered and reasoning was replayed, so a mark can only land on
//! bytes the request really sends, and the same history always yields the
//! same bytes. Nothing here knows a provider or a model: the entry's
//! `cache_marks` data says what a mark is, how a string becomes a part that
//! can hold one, and which messages carry them.
//!
//! Every system, user and tool message with text content renders as a part
//! array whether it is marked or not, and the mark is one extra key on its
//! last part. When a mark moves to a newer message the older message keeps
//! its bytes minus that key, so the request prefix stays byte-stable between
//! turns. Assistant messages are never rewritten or marked.

use std::collections::HashMap;

use serde_json::{Value, json};

use ryeos_directive_definition::{CacheMarkPosition, CacheMarksConfig};
use ryeos_runtime::template::apply_template;

/// What the pass did, for request telemetry.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheMarkOutcome {
    /// Marks written.
    pub placed: u32,
    /// Selected messages that could not take a mark: no content, empty
    /// content, or content not ending in an object part.
    pub skipped: u32,
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn carries_marks(message: &Value) -> bool {
    matches!(role(message), Some("system" | "user" | "tool"))
}

/// The wire index each position selects, in the entry's priority order.
fn select(messages: &[Value], positions: &[CacheMarkPosition]) -> Vec<usize> {
    let mut selected = Vec::new();
    for position in positions {
        let index = match position {
            CacheMarkPosition::System => messages
                .iter()
                .position(|message| role(message) == Some("system")),
            CacheMarkPosition::Newest => messages.len().checked_sub(1),
            CacheMarkPosition::PreviousTail => messages
                .iter()
                .rposition(|message| role(message) == Some("assistant"))
                .and_then(|assistant| assistant.checked_sub(1)),
        };
        if let Some(index) = index
            && !selected.contains(&index)
        {
            selected.push(index);
        }
    }
    selected
}

/// Renders every system, user and tool message that holds non-empty string
/// content as a one-part array.
fn render_text_parts(messages: &mut [Value], config: &CacheMarksConfig) {
    for message in messages.iter_mut().filter(|message| carries_marks(message)) {
        let Some(Value::String(text)) = message.get("content") else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let context = HashMap::from([("text".to_string(), json!(text))]);
        let part = apply_template(&config.text_part_template, &context);
        message["content"] = Value::Array(vec![part]);
    }
}

/// Merges the mark's keys into the last content part of `message`.
fn mark(message: &mut Value, config: &CacheMarksConfig) -> bool {
    let (Some(Value::Array(parts)), Value::Object(keys)) =
        (message.get_mut("content"), &config.mark)
    else {
        return false;
    };
    let Some(Value::Object(last)) = parts.last_mut() else {
        return false;
    };
    for (key, value) in keys {
        last.insert(key.clone(), value.clone());
    }
    true
}

/// Applies the entry's cache marks to the final wire messages. At most
/// `max_marks` distinct messages are marked, taken from the entry's positions
/// in priority order; a position that selects nothing (no earlier assistant
/// turn yet) costs no mark.
pub fn apply_cache_marks(messages: &mut [Value], config: &CacheMarksConfig) -> CacheMarkOutcome {
    render_text_parts(messages, config);
    let mut outcome = CacheMarkOutcome::default();
    let limit = config.max_marks as usize;
    for index in select(messages, &config.positions).into_iter().take(limit) {
        if carries_marks(&messages[index]) && mark(&mut messages[index], config) {
            outcome.placed += 1;
        } else {
            outcome.skipped += 1;
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(max_marks: u32, positions: Value) -> CacheMarksConfig {
        serde_json::from_value(json!({
            "mark": {"cache_control": {"type": "ephemeral"}},
            "text_part_template": {"type": "text", "text": "{text}"},
            "max_marks": max_marks,
            "positions": positions
        }))
        .unwrap()
    }

    fn all() -> CacheMarksConfig {
        config(4, json!(["system", "newest", "previous_tail"]))
    }

    fn sys(text: &str) -> Value {
        json!({"role": "system", "content": text})
    }
    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }
    fn assistant(text: &str) -> Value {
        json!({"role": "assistant", "content": text,
               "tool_calls": [{"id": "c", "type": "function",
                               "function": {"name": "f", "arguments": "{}"}}]})
    }
    fn tool(text: &str) -> Value {
        json!({"role": "tool", "tool_call_id": "c", "content": text})
    }

    fn marked(messages: &[Value]) -> Vec<usize> {
        messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.to_string().contains("cache_control"))
            .map(|(index, _)| index)
            .collect()
    }

    fn without_marks(messages: &[Value]) -> Vec<Value> {
        fn strip(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    map.remove("cache_control");
                    map.values_mut().for_each(strip);
                }
                Value::Array(items) => items.iter_mut().for_each(strip),
                _ => {}
            }
        }
        let mut copy = messages.to_vec();
        copy.iter_mut().for_each(strip);
        copy
    }

    #[test]
    fn the_first_request_marks_system_and_newest() {
        let mut messages = vec![sys("rules"), user("go")];
        let outcome = apply_cache_marks(&mut messages, &all());
        assert_eq!(
            outcome,
            CacheMarkOutcome {
                placed: 2,
                skipped: 0
            }
        );
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": [
                {"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}]})
        );
        assert_eq!(marked(&messages), vec![0, 1]);
    }

    #[test]
    fn a_later_request_marks_system_the_previous_tail_and_the_newest() {
        let mut messages = vec![
            sys("rules"),
            user("go"),
            assistant("a"),
            tool("r1"),
            assistant("b"),
            tool("r2"),
        ];
        let outcome = apply_cache_marks(&mut messages, &all());
        assert_eq!(outcome.placed, 3);
        // previous_tail is the message before the last assistant message.
        assert_eq!(marked(&messages), vec![0, 3, 5]);
    }

    #[test]
    fn no_assistant_message_is_marked_or_rewritten() {
        let original = assistant("a");
        let mut messages = vec![sys("s"), user("u"), original.clone(), tool("t")];
        apply_cache_marks(&mut messages, &all());
        assert_eq!(messages[2], original);
        assert!(messages[2]["content"].is_string());
    }

    #[test]
    fn max_marks_keeps_the_highest_priority_positions() {
        let mut messages = vec![sys("s"), user("u"), assistant("a"), tool("t")];
        apply_cache_marks(
            &mut messages,
            &config(2, json!(["system", "newest", "previous_tail"])),
        );
        assert_eq!(marked(&messages), vec![0, 3]);
        let mut messages = vec![sys("s"), user("u"), assistant("a"), tool("t")];
        apply_cache_marks(&mut messages, &config(1, json!(["newest", "system"])));
        assert_eq!(marked(&messages), vec![3]);
    }

    #[test]
    fn positions_that_select_one_message_collapse_and_absent_ones_cost_nothing() {
        let mut messages = vec![sys("s")];
        let outcome = apply_cache_marks(
            &mut messages,
            &config(2, json!(["system", "newest", "previous_tail"])),
        );
        assert_eq!(outcome.placed, 1);
        // The previous tail is absent on a first request, so max_marks 2
        // still reaches the position after it.
        let mut messages = vec![sys("s"), user("u")];
        apply_cache_marks(
            &mut messages,
            &config(2, json!(["previous_tail", "newest", "system"])),
        );
        assert_eq!(marked(&messages), vec![0, 1]);
    }

    #[test]
    fn array_content_takes_the_mark_on_its_last_part() {
        let image = json!({"type": "image_url", "image_url": {"url": "data:x"}});
        let mut messages = vec![json!({"role": "user", "content": [
                {"type": "text", "text": "label"}, image.clone()]})];
        apply_cache_marks(&mut messages, &config(1, json!(["newest"])));
        let parts = messages[0]["content"].as_array().unwrap();
        assert!(parts[0].get("cache_control").is_none());
        assert_eq!(parts[1]["image_url"], image["image_url"]);
        assert_eq!(parts[1]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn content_that_cannot_hold_a_mark_is_skipped_and_counted() {
        let unmarkable = [
            json!({"role": "user", "content": ""}),
            json!({"role": "tool", "tool_call_id": "c", "content": null}),
            json!({"role": "user", "content": []}),
            json!({"role": "user", "content": ["bare"]}),
        ];
        for (case, message) in unmarkable.into_iter().enumerate() {
            let mut one = vec![message];
            let before = one.clone();
            let outcome = apply_cache_marks(&mut one, &config(1, json!(["newest"])));
            assert_eq!(
                outcome,
                CacheMarkOutcome {
                    placed: 0,
                    skipped: 1
                },
                "{case}"
            );
            assert_eq!(one, before, "{case}");
        }
    }

    #[test]
    fn a_selected_assistant_message_is_skipped_not_marked() {
        let mut messages = vec![user("u"), assistant("a")];
        let outcome = apply_cache_marks(&mut messages, &config(1, json!(["newest"])));
        assert_eq!(
            outcome,
            CacheMarkOutcome {
                placed: 0,
                skipped: 1
            }
        );
        assert_eq!(messages[1], assistant("a"));
    }

    #[test]
    fn a_moved_mark_leaves_every_older_message_equal_modulo_the_mark_key() {
        let turn1 = vec![sys("s"), user("go")];
        let turn2 = [turn1.clone(), vec![assistant("a"), tool("r1")]].concat();
        let turn3 = [turn2.clone(), vec![assistant("b"), tool("r2")]].concat();
        let render = |history: &[Value]| {
            let mut wire = history.to_vec();
            apply_cache_marks(&mut wire, &all());
            wire
        };
        let (a, b, c) = (render(&turn1), render(&turn2), render(&turn3));
        assert_eq!(without_marks(&b)[..a.len()], without_marks(&a)[..]);
        assert_eq!(without_marks(&c)[..b.len()], without_marks(&b)[..]);
        // The system message keeps its mark on every turn.
        assert_eq!(a[0], b[0]);
        assert_eq!(b[0], c[0]);
        // The mark written on turn 2's newest is read back on turn 3 at the
        // previous tail with identical bytes.
        assert_eq!(b[3], c[3]);
    }

    #[test]
    fn without_a_mark_in_play_text_still_renders_as_parts_uniformly() {
        let mut messages = vec![sys("s"), user("a"), assistant("x"), tool("t"), user("b")];
        apply_cache_marks(&mut messages, &config(1, json!(["newest"])));
        for index in [0, 1, 3, 4] {
            assert!(messages[index]["content"].is_array(), "{index}");
        }
        assert_eq!(marked(&messages), vec![4]);
    }

    #[test]
    fn a_custom_mark_and_part_shape_are_data() {
        let custom: CacheMarksConfig = serde_json::from_value(json!({
            "mark": {"cache_control": {"type": "ephemeral", "ttl": "1h"}},
            "text_part_template": {"kind": "t", "body": "{text}"},
            "max_marks": 1,
            "positions": ["newest"]
        }))
        .unwrap();
        let mut messages = vec![user("hi")];
        apply_cache_marks(&mut messages, &custom);
        assert_eq!(
            messages[0]["content"],
            json!([{"kind": "t", "body": "hi",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}}])
        );
    }
}
