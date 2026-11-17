//! Timeline items drawn as plain terminal text.
//!
//! The daemon folds a chain into timeline items; the shared client model
//! turns them into one line per entry whose text is the entry's accessible
//! label, so this terminal, a screen reader and a copy all read the same
//! string. This module adds only what a terminal needs: indentation for a
//! group's members, colour for tone when the terminal has it, and the
//! incremental view that `ryeos execute` uses to print what is new.

use std::collections::HashMap;

use ryeos_client_base::ui::timeline::{
    TimelineText, TimelineTextLine, merge_timeline_window, timeline_text,
};
use ryeos_client_base::ui::view_model::RyeOsTone;
use serde_json::Value;

use crate::tty::{Tone, style};

const INDENT: &str = "  ";

fn tone(tone: RyeOsTone) -> Tone {
    match tone {
        RyeOsTone::Good => Tone::Success,
        RyeOsTone::Warn => Tone::Warning,
        RyeOsTone::Danger => Tone::Failure,
        RyeOsTone::Accent => Tone::Active,
        RyeOsTone::Neutral => Tone::Neutral,
    }
}

/// One line as the terminal shows it: indented by depth, coloured by tone
/// when `color` is set, with control characters replaced.
pub(crate) fn render_line(line: &TimelineTextLine, color: bool) -> String {
    let text = crate::tty::sanitize_terminal_inline(&line.text);
    format!(
        "{}{}",
        INDENT.repeat(usize::from(line.indent)),
        style(&text, tone(line.tone), color)
    )
}

/// A whole response as lines: the chain header and the "N older" line, then
/// one line per entry with a group's members beneath it.
pub(crate) fn render_text(text: &TimelineText, color: bool) -> Vec<String> {
    text.lead
        .iter()
        .chain(&text.lines)
        .map(|line| render_line(line, color))
        .collect()
}

/// A response drawn in full, for `ryeos threads timeline`.
pub(crate) fn render_response(response: &Value, color: bool) -> Vec<String> {
    render_text(&timeline_text(response), color)
}

/// A line to print for the live view.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiveLine {
    pub line: TimelineTextLine,
    /// The entry was already printed and its text has changed since.
    pub update: bool,
}

/// What applying a response to the live view needs next.
pub(crate) enum Applied {
    Lines(Vec<LiveLine>),
    /// The response was an upsert the held window cannot absorb; refetch the
    /// whole window (the view has been reset).
    Refetch,
}

/// The timeline a running command has shown so far: the held window, the text
/// each entry was last printed with, and the `chain_seq` it is current to.
///
/// Conversation text (the user's message and the assistant's answer) is
/// printed inline from the event stream, so those entries are not repeated
/// here. A group's header changes as members arrive; its members are printed
/// as they appear, so only entries report an update.
#[derive(Default)]
pub(crate) struct LiveView {
    window: Option<Value>,
    shown: HashMap<String, String>,
    highest: Option<i64>,
}

fn printed_inline(role: &str) -> bool {
    matches!(role, "prose" | "message")
}

fn is_group(role: &str) -> bool {
    matches!(role, "cluster" | "recap")
}

impl LiveView {
    /// The `since_chain_seq` for the next fetch; `None` before the first.
    pub(crate) fn since(&self) -> Option<i64> {
        self.highest
    }

    pub(crate) fn apply(&mut self, response: Value) -> Applied {
        let highest = response.get("highest_chain_seq").and_then(Value::as_i64);
        let Some(window) = merge_timeline_window(self.window.as_ref(), response) else {
            self.window = None;
            self.highest = None;
            return Applied::Refetch;
        };
        let text = timeline_text(&window);
        let mut out = Vec::new();
        for line in text.lines {
            if printed_inline(&line.role) {
                continue;
            }
            match self.shown.get(&line.id) {
                None => {
                    self.shown.insert(line.id.clone(), line.text.clone());
                    out.push(LiveLine {
                        line,
                        update: false,
                    });
                }
                Some(before) if *before != line.text => {
                    self.shown.insert(line.id.clone(), line.text.clone());
                    if !is_group(&line.role) {
                        out.push(LiveLine { line, update: true });
                    }
                }
                Some(_) => {}
            }
        }
        self.window = Some(window);
        self.highest = highest;
        Applied::Lines(out)
    }
}

/// A live line for the terminal. An update carries a marker so it reads as a
/// change to something above rather than a new entry.
pub(crate) fn render_live_line(live: &LiveLine, color: bool, unicode: bool) -> String {
    let body = render_line(&live.line, color);
    if live.update {
        let marker = if unicode { "↻" } else { "~" };
        format!("{marker} {body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, state: &str, slots: Value) -> Value {
        json!({
            "item": "entry", "id": id, "type": "activity", "state": state, "tone": null,
            "thread": "T-1", "parent": null, "exchange": "x1", "counts_as": null,
            "slots": slots, "seq": [1, 1], "ts": ["t", "t"]
        })
    }

    fn response(items: Vec<Value>, highest: i64) -> Value {
        json!({
            "chain_root_id": "T-1", "older": 0, "highest_chain_seq": highest,
            "summary": {"status": "running", "input_tokens": 0, "output_tokens": 0,
                        "spend_usd": 0.0, "turns": 1},
            "items": items,
        })
    }

    #[test]
    fn an_entry_line_is_its_accessible_label() {
        let response = response(
            vec![entry(
                "a",
                "failed",
                json!({"headline": "fetch", "detail": "404"}),
            )],
            3,
        );
        let lines = render_response(&response, false);
        assert_eq!(lines.last().map(String::as_str), Some("fetch, failed: 404"));
    }

    #[test]
    fn a_cluster_and_a_recap_show_their_summary_with_members_indented() {
        let items = vec![
            json!({"item": "cluster", "id": "c1", "headline": "2 operations", "members": [
                entry("a", "done", json!({"headline": "one"})),
                entry("b", "done", json!({"headline": "two"})),
            ]}),
            json!({"item": "recap", "id": "r1", "exchange": "x1", "work": 5, "duration_s": 12,
                   "members": [entry("c", "done", json!({"headline": "three"}))]}),
        ];
        let lines = render_response(&response(items, 9), false);
        assert_eq!(
            lines[1..],
            [
                "2 operations",
                "  one, done",
                "  two, done",
                "Worked for 12s, 5 steps",
                "  three, done",
            ]
        );
    }

    #[test]
    fn tone_becomes_colour_only_when_the_terminal_has_it() {
        let mut item = entry("a", "failed", json!({"headline": "fetch"}));
        item["tone"] = json!("danger");
        let text = timeline_text(&response(vec![item], 3));
        let line = &text.lines[0];
        assert!(!render_line(line, false).contains('\x1b'));
        assert!(render_line(line, true).contains("\x1b[1;38;2;251;73;52m"));
    }

    #[test]
    fn the_live_view_prints_new_entries_once_and_one_update_on_a_state_change() {
        let mut view = LiveView::default();
        let running = entry("a", "running", json!({"headline": "fetch"}));
        let Applied::Lines(first) = view.apply(response(vec![running], 3)) else {
            panic!("a whole window applies");
        };
        let texts: Vec<_> = first
            .iter()
            .map(|l| (l.line.text.as_str(), l.update))
            .collect();
        assert_eq!(texts, [("fetch, running", false)]);
        assert_eq!(view.since(), Some(3));

        let mut upsert = response(
            vec![entry(
                "a",
                "done",
                json!({"headline": "fetch", "detail": "3.2 KiB"}),
            )],
            5,
        );
        upsert["order"] = json!(["a"]);
        let Applied::Lines(second) = view.apply(upsert) else {
            panic!("an upsert applies");
        };
        let texts: Vec<_> = second
            .iter()
            .map(|l| (l.line.text.as_str(), l.update))
            .collect();
        assert_eq!(texts, [("fetch, done: 3.2 KiB", true)]);
        assert_eq!(
            render_live_line(&second[0], false, true),
            "↻ fetch, done: 3.2 KiB"
        );
        assert_eq!(view.since(), Some(5));

        let mut same = response(Vec::new(), 6);
        same["order"] = json!(["a"]);
        let Applied::Lines(third) = view.apply(same) else {
            panic!("an empty upsert applies");
        };
        assert!(third.is_empty(), "nothing changed, nothing printed");
    }

    #[test]
    fn conversation_entries_are_left_to_the_inline_stream() {
        let mut view = LiveView::default();
        let mut prose = entry("p", "", json!({"body": "done"}));
        prose["type"] = json!("prose");
        let Applied::Lines(lines) = view.apply(response(vec![prose], 2)) else {
            panic!("a whole window applies");
        };
        assert!(lines.is_empty());
    }

    #[test]
    fn an_upsert_the_window_cannot_absorb_asks_for_a_refetch() {
        let mut view = LiveView::default();
        let mut upsert = response(Vec::new(), 4);
        upsert["order"] = json!(["missing"]);
        assert!(matches!(view.apply(upsert), Applied::Refetch));
        assert_eq!(view.since(), None);
    }
}
