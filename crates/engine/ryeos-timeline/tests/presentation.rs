//! A tool's presentation block, supplied as a thread subject, shapes the
//! activity its call runs under and nothing else.

use std::path::PathBuf;

use ryeos_timeline::{Definition, Entry, Folder, Item, Timeline, fold};
use serde_json::{Value, json};

fn definition() -> Definition {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/ryeos-ui/.ai/config/ryeos/ui/timeline/thread.yaml"),
    )
    .unwrap();
    Definition::from_yaml(&text).unwrap()
}

fn ev(seq: i64, thread: &str, event_type: &str, payload: Value) -> Value {
    json!({"chain_seq": seq, "chain_root_id": "T-1", "thread_id": thread,
           "ts": format!("2026-11-16T00:00:{seq:02}Z"),
           "event_type": event_type, "payload": payload})
}

const OP: &str = "0000000000000000000000000000000000000000000000000000000000000007";

/// A directive turn that calls one tool, whose own thread is `T-2`.
fn fetch_call(result_text: &str, guard: Value) -> Vec<Value> {
    vec![
        ev(
            1,
            "T-1",
            "cognition_in",
            json!({"origin": "launch", "parts": [{"type": "text", "text": "go"}]}),
        ),
        ev(
            2,
            "T-1",
            "cognition_out",
            json!({"finish": "tool_calls", "turn": 1, "parts": [
                {"type": "tool_call", "call_id": "c1", "name": "web_fetch",
                 "arguments": {"url": "https://example.com/a", "format": "text"}}]}),
        ),
        ev(
            3,
            "T-1",
            "tool_call_start",
            json!({"operation_id": OP, "call_id": "c1", "tool": "web_fetch"}),
        ),
        ev(
            4,
            "T-1",
            "child_thread_spawned",
            json!({"child_thread_id": "T-2", "item_id": "tool:rye/web/fetch", "operation_id": OP}),
        ),
        ev(
            5,
            "T-1",
            "tool_call_result",
            json!({"operation_id": OP, "call_id": "c1", "tool": "web_fetch", "status": "ok",
                   "parts": [{"type": "text", "text": result_text}], "guard": guard}),
        ),
    ]
}

fn page_result() -> String {
    json!({"success": true, "output": "Example Domain\nThis domain is for examples."}).to_string()
}

fn subject() -> Value {
    json!({"presentation": {
        "counts_as": "fetch",
        "headline": ["arguments.url"],
        "detail": ["arguments.format"],
        "body": ["result.output"],
    }})
}

fn activity(timeline: &Timeline) -> Entry {
    let mut found = Vec::new();
    for item in &timeline.items {
        match item {
            Item::Entry(e) if e.id.starts_with("act:") => found.push(e.clone()),
            Item::Cluster(c) => found.extend(
                c.members
                    .iter()
                    .filter(|e| e.id.starts_with("act:"))
                    .cloned(),
            ),
            Item::Recap(r) => {
                for m in &r.members {
                    match m {
                        Item::Entry(e) if e.id.starts_with("act:") => found.push(e.clone()),
                        Item::Cluster(c) => found.extend(
                            c.members
                                .iter()
                                .filter(|e| e.id.starts_with("act:"))
                                .cloned(),
                        ),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    assert_eq!(found.len(), 1, "one activity: {found:?}");
    found.remove(0)
}

fn fold_with(events: &[Value], subjects: &[(&str, Value)]) -> Timeline {
    let mut folder = Folder::new(definition());
    for (thread, subject) in subjects {
        folder.provide(*thread, subject.clone());
    }
    folder.push(events).unwrap();
    folder.timeline()
}

#[test]
fn a_tool_with_a_block_shows_its_url_excerpt_and_noun() {
    let events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": false}),
    );
    let entry = activity(&fold_with(&events, &[("T-2", subject())]));
    assert_eq!(entry.slots["headline"], "https://example.com/a");
    assert_eq!(entry.slots["detail"], "text");
    assert_eq!(
        entry.slots["body"],
        "Example Domain\nThis domain is for examples."
    );
    // The model-visible tool name stays drawn beside the presented headline.
    assert_eq!(entry.slots["tool"], "web_fetch");
    assert_eq!(entry.counts_as.as_deref(), Some("fetch"));
    // State and tone are the engine's, from the events.
    assert_eq!(entry.state.as_deref(), Some("done"));
    assert_eq!(entry.tone.as_deref(), Some("good"));
}

#[test]
fn a_tool_without_a_subject_or_a_block_folds_exactly_as_before() {
    let events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": false}),
    );
    let plain = fold(&events, &definition()).unwrap();
    assert_eq!(fold_with(&events, &[]), plain);
    assert_eq!(fold_with(&events, &[("T-2", json!({}))]), plain);
    assert_eq!(
        fold_with(&events, &[("T-2", json!({"presentation": "no"}))]),
        plain
    );
    let entry = activity(&plain);
    assert_eq!(entry.slots["headline"], "web_fetch");
    assert_eq!(entry.counts_as.as_deref(), Some("other"));
    assert!(!entry.slots.contains_key("body"));
}

#[test]
fn a_cut_result_gives_no_body_but_keeps_the_headline() {
    let events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": true, "truncated_reason": "size"}),
    );
    let entry = activity(&fold_with(&events, &[("T-2", subject())]));
    assert_eq!(entry.slots["headline"], "https://example.com/a");
    assert!(!entry.slots.contains_key("body"));
    assert_eq!(entry.tone.as_deref(), Some("warn"));
}

#[test]
fn a_result_that_is_not_json_gives_no_body() {
    let events = fetch_call(
        "plain words",
        json!({"result_size_bytes": 11, "truncated": false}),
    );
    let entry = activity(&fold_with(&events, &[("T-2", subject())]));
    assert!(!entry.slots.contains_key("body"));
}

#[test]
fn a_failed_call_keeps_the_engine_tone_whatever_the_block_says() {
    let mut events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": false}),
    );
    events[4]["payload"]["status"] = json!("error");
    let entry = activity(&fold_with(&events, &[("T-2", subject())]));
    assert_eq!(entry.state.as_deref(), Some("failed"));
    assert_eq!(entry.tone.as_deref(), Some("danger"));
}

#[test]
fn a_count_key_outside_the_vocabulary_counts_as_the_definition_says() {
    let events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": false}),
    );
    let subject = json!({"presentation": {"counts_as": "invented", "headline": ["arguments.url"]}});
    let entry = activity(&fold_with(&events, &[("T-2", subject)]));
    assert_eq!(entry.counts_as.as_deref(), Some("other"));
    assert_eq!(entry.slots["headline"], "https://example.com/a");
}

#[test]
fn presentation_text_is_stripped_and_capped() {
    let mut events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": false}),
    );
    let long = "x".repeat(500);
    events[1]["payload"]["parts"][0]["arguments"]["url"] =
        json!(format!("\u{1b}[31m{long}\u{202e}"));
    let entry = activity(&fold_with(&events, &[("T-2", subject())]));
    let headline = &entry.slots["headline"];
    assert_eq!(headline.chars().count(), 301);
    assert!(headline.starts_with("[31mxxx"));
    assert!(headline.ends_with('…'));
    assert!(!headline.contains('\u{202e}'));
}

#[test]
fn a_path_missing_or_the_wrong_shape_leaves_the_definitions_slot() {
    let events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": false}),
    );
    let subject = json!({"presentation": {
        "headline": ["arguments.nothing", "arguments"],
        "detail": ["arguments.url"],
    }});
    let entry = activity(&fold_with(&events, &[("T-2", subject)]));
    assert_eq!(entry.slots["headline"], "web_fetch");
    assert_eq!(entry.slots["detail"], "https://example.com/a");
}

#[test]
fn the_subject_is_wanted_until_provided() {
    let events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": false}),
    );
    let mut folder = Folder::new(definition());
    assert_eq!(folder.subjects_wanted(&events), ["T-2"]);
    folder.provide("T-2", subject());
    assert!(folder.subjects_wanted(&events).is_empty());
}

/// Pushing the events in pieces, with the subject provided before the piece
/// that adopts its thread, gives the whole fold; an entry whose subject
/// arrived reads as changed from that event on.
#[test]
fn pushing_in_pieces_equals_folding_whole_and_the_entry_reads_changed() {
    let events = fetch_call(
        &page_result(),
        json!({"result_size_bytes": 80, "truncated": false}),
    );
    let whole = fold_with(&events, &[("T-2", subject())]);
    for split in 1..events.len() {
        let mut folder = Folder::new(definition());
        for thread in folder.subjects_wanted(&events[..split]) {
            folder.provide(thread, subject());
        }
        folder.push(&events[..split]).unwrap();
        for thread in folder.subjects_wanted(&events[split..]) {
            folder.provide(thread, subject());
        }
        folder.push(&events[split..]).unwrap();
        assert_eq!(folder.timeline(), whole, "split at {split}");
    }
    // After the spawn event the entry's last seq is that event's, so a window
    // held from before it is resent the entry.
    let mut folder = Folder::new(definition());
    folder.push(&events[..3]).unwrap();
    let before = activity(&folder.timeline());
    folder.provide("T-2", subject());
    folder.push(&events[3..4]).unwrap();
    let after = activity(&folder.timeline());
    assert_eq!(before.seq.1, 3);
    assert_eq!(after.seq.1, 4);
    assert_eq!(after.slots["headline"], "https://example.com/a");
}

fn tool_items(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            tool_items(&path, out);
        } else if path.extension().is_some_and(|e| e == "yaml") {
            out.push(path);
        }
    }
}

/// A shipped tool's `counts_as` key that the definition's vocabulary lacks is
/// reported, not refused: such a call counts as the definition says. Every
/// shipped block must still be a mapping of paths.
#[test]
fn shipped_presentation_blocks_are_well_formed_and_unknown_keys_are_reported() {
    let def = definition();
    let bundles = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../bundles");
    let mut items = Vec::new();
    for bundle in std::fs::read_dir(&bundles).unwrap().flatten() {
        tool_items(&bundle.path().join(".ai/tools"), &mut items);
    }
    let mut seen = 0;
    for item in items {
        let text = std::fs::read_to_string(&item).unwrap();
        let Ok(doc) = serde_yaml::from_str::<Value>(&text) else {
            continue;
        };
        let Some(block) = doc.get("presentation") else {
            continue;
        };
        seen += 1;
        let block = block
            .as_object()
            .unwrap_or_else(|| panic!("{item:?}: not a mapping"));
        for (slot, value) in block {
            if slot == "counts_as" {
                let key = value
                    .as_str()
                    .unwrap_or_else(|| panic!("{item:?}: counts_as"));
                if !def.vocabulary.iter().any(|noun| noun.key == key) {
                    eprintln!("warning: {item:?} counts as {key:?}, not in the vocabulary");
                }
            } else {
                let paths = value
                    .as_array()
                    .unwrap_or_else(|| panic!("{item:?}: {slot} is not a list of paths"));
                assert!(paths.iter().all(Value::is_string), "{item:?}: {slot}");
            }
        }
    }
    assert!(seen >= 1, "web/fetch declares a block");
}
