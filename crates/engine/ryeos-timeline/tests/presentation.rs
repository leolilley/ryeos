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
            } else if slot == "changes" {
                assert_changes_declaration(&item, value);
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

/// A shipped `changes` declaration is exactly one source form whose paths
/// read the `arguments` or `result` root.
fn assert_changes_declaration(item: &std::path::Path, value: &Value) {
    let declared = value
        .as_object()
        .unwrap_or_else(|| panic!("{item:?}: changes is not a mapping"));
    assert_eq!(declared.len(), 1, "{item:?}: changes has one source");
    let rooted = |source: &Value| {
        source
            .as_str()
            .is_some_and(|path| path.starts_with("arguments.") || path.starts_with("result."))
    };
    if let Some(patch) = declared.get("patch") {
        assert!(rooted(patch), "{item:?}: changes.patch");
        return;
    }
    let edit = declared
        .get("edit")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{item:?}: changes is neither patch nor edit"));
    let keys: Vec<&str> = edit.keys().map(String::as_str).collect();
    assert_eq!(keys, ["after", "before", "path"], "{item:?}: changes.edit");
    assert!(edit.values().all(rooted), "{item:?}: changes.edit paths");
}

/// The shipped definition with every tool-call adoption admitting a change
/// block under `caps`, or admitting none.
fn definition_admitting(caps: Option<Value>) -> Definition {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/ryeos-ui/.ai/config/ryeos/ui/timeline/thread.yaml"),
    )
    .unwrap();
    let mut doc: Value = serde_yaml::from_str(&text).unwrap();
    let mut adoptions = 0;
    for rule in doc["events"]["child_thread_spawned"]
        .as_array_mut()
        .unwrap()
    {
        for emission in rule["emit"].as_array_mut().unwrap() {
            let present = emission["adopt"]["present"].as_object_mut().unwrap();
            match &caps {
                Some(caps) => present.insert("blocks".into(), json!({"changes": caps})),
                None => present.remove("blocks"),
            };
            adoptions += 1;
        }
    }
    assert_eq!(adoptions, 2);
    Definition::from_value(doc).unwrap()
}

fn default_caps() -> Value {
    json!({"files": 20, "lines_per_file": 400, "total_lines": 2000, "line_chars": 400})
}

/// A directive turn that calls `tool` with `arguments`; the tool's own thread
/// is `T-2`.
fn tool_call(tool: &str, arguments: Value, status: &str, result_text: &str) -> Vec<Value> {
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
                {"type": "tool_call", "call_id": "c1", "name": tool, "arguments": arguments}]}),
        ),
        ev(
            3,
            "T-1",
            "tool_call_start",
            json!({"operation_id": OP, "call_id": "c1", "tool": tool}),
        ),
        ev(
            4,
            "T-1",
            "child_thread_spawned",
            json!({"child_thread_id": "T-2", "item_id": "tool:example/tool", "operation_id": OP}),
        ),
        ev(
            5,
            "T-1",
            "tool_call_result",
            json!({"operation_id": OP, "call_id": "c1", "tool": tool, "status": status,
                   "parts": [{"type": "text", "text": result_text}],
                   "guard": {"result_size_bytes": 16, "truncated": false}}),
        ),
    ]
}

fn edit_arguments() -> Value {
    json!({"path": "src/lib.rs", "old_string": "fn a() {}\n",
           "new_string": "fn a() {\n    b();\n}\n"})
}

fn edit_subject() -> Value {
    json!({"presentation": {
        "headline": ["arguments.path"],
        "changes": {"edit": {"path": "arguments.path", "before": "arguments.old_string",
                             "after": "arguments.new_string"}},
    }})
}

const PATCH: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@ mod a\n fn a() {\n-    old();\n+    new();\n }\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+remember\n";

fn patch_subject() -> Value {
    json!({"presentation": {"changes": {"patch": "arguments.patch"}}})
}

fn fold_admitting(caps: Option<Value>, events: &[Value], subject: Value) -> Entry {
    let mut folder = Folder::new(definition_admitting(caps));
    folder.provide("T-2", subject);
    folder.push(events).unwrap();
    activity(&folder.timeline())
}

/// Compares an entry with its golden as JSON values, so the file's layout is
/// free; `UPDATE_GOLDEN=1` rewrites it.
fn entry_golden(name: &str, entry: &Entry) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    let actual = serde_json::to_value(entry).unwrap();
    let pretty = serde_json::to_string_pretty(&actual).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &pretty).unwrap();
    }
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing golden {path:?}; run with UPDATE_GOLDEN=1")),
    )
    .unwrap();
    assert_eq!(
        actual, expected,
        "golden mismatch for {name}; actual:\n{pretty}"
    );
}

#[test]
fn an_edit_tool_shows_the_line_diff_of_its_arguments() {
    let events = tool_call("edit_file", edit_arguments(), "ok", "{\"success\":true}");
    let entry = fold_admitting(Some(default_caps()), &events, edit_subject());
    entry_golden("changes_edit_tool", &entry);
}

#[test]
fn a_patch_tool_shows_every_file_of_its_patch() {
    let events = tool_call(
        "apply_patch",
        json!({"patch": PATCH}),
        "ok",
        "{\"success\":true}",
    );
    let entry = fold_admitting(Some(default_caps()), &events, patch_subject());
    entry_golden("changes_patch_tool", &entry);
}

/// The block is what was asked for, so a failed call still carries it; the
/// entry's own state says it did not apply.
#[test]
fn a_failed_call_keeps_its_requested_change_and_reads_failed() {
    let events = tool_call("edit_file", edit_arguments(), "error", "{\"success\":true}");
    let entry = fold_admitting(Some(default_caps()), &events, edit_subject());
    assert_eq!(entry.state.as_deref(), Some("failed"));
    entry_golden("changes_failed_call", &entry);
}

#[test]
fn a_tool_that_declares_no_changes_has_no_block() {
    let events = tool_call("edit_file", edit_arguments(), "ok", "{\"success\":true}");
    let subject = json!({"presentation": {"headline": ["arguments.path"]}});
    let entry = fold_admitting(Some(default_caps()), &events, subject);
    assert!(entry.blocks.is_empty());
    let text = serde_json::to_string(&entry).unwrap();
    assert!(
        !text.contains("\"blocks\""),
        "an empty list is not serialised"
    );
}

/// The definition, not the tool, decides whether a block is drawn: without
/// the opt-in the same call folds exactly as a tool with no declaration.
#[test]
fn a_definition_that_does_not_admit_changes_refuses_the_block() {
    let events = tool_call("edit_file", edit_arguments(), "ok", "{\"success\":true}");
    let refused = fold_admitting(None, &events, edit_subject());
    assert!(refused.blocks.is_empty());
    let undeclared = fold_admitting(
        None,
        &events,
        json!({"presentation": {"headline": ["arguments.path"]}}),
    );
    assert_eq!(refused, undeclared);
    let mut admitted = fold_admitting(Some(default_caps()), &events, edit_subject());
    assert_eq!(admitted.blocks.len(), 1);
    admitted.blocks.clear();
    assert_eq!(admitted, refused);
}

#[test]
fn a_change_read_from_the_result_is_reported() {
    let result = json!({"success": true, "patch": PATCH}).to_string();
    let events = tool_call("apply_patch", json!({}), "ok", &result);
    let subject = json!({"presentation": {"changes": {"patch": "result.patch"}}});
    let entry = fold_admitting(Some(default_caps()), &events, subject);
    let block = serde_json::to_value(&entry.blocks[0]).unwrap();
    assert_eq!(block["basis"], json!("reported"));
    assert_eq!(block["files"][1]["path"], json!("notes.txt"));
    // A failed call offers no result, so there is nothing to report.
    let events = tool_call("apply_patch", json!({}), "error", &result);
    let subject = json!({"presentation": {"changes": {"patch": "result.patch"}}});
    assert!(
        fold_admitting(Some(default_caps()), &events, subject)
            .blocks
            .is_empty()
    );
}

#[test]
fn a_malformed_declaration_or_value_gives_no_block_and_the_entry_draws_as_before() {
    let events = tool_call("edit_file", edit_arguments(), "ok", "{\"success\":true}");
    let plain = fold_admitting(
        Some(default_caps()),
        &events,
        json!({"presentation": {"headline": ["arguments.path"]}}),
    );
    let declarations = [
        // Not a mapping, empty, or both forms at once.
        json!("arguments.patch"),
        json!({}),
        json!({"patch": "arguments.new_string",
               "edit": {"path": "arguments.path", "before": "arguments.old_string",
                        "after": "arguments.new_string"}}),
        // An unknown form.
        json!({"write": "arguments.new_string"}),
        // The value is not a patch.
        json!({"patch": "arguments.new_string"}),
        // Missing or extra edit keys.
        json!({"edit": {"path": "arguments.path", "before": "arguments.old_string"}}),
        json!({"edit": {"path": "arguments.path", "before": "arguments.old_string",
                        "after": "arguments.new_string", "mode": "arguments.path"}}),
        // A value that is absent or not text.
        json!({"edit": {"path": "arguments.path", "before": "arguments.nothing",
                        "after": "arguments.new_string"}}),
        json!({"edit": {"path": "arguments", "before": "arguments.old_string",
                        "after": "arguments.new_string"}}),
        // A root other than the call's arguments or result.
        json!({"patch": "payload.parts.0.text"}),
        json!({"edit": {"path": "arguments.path", "before": "thread_id",
                        "after": "arguments.new_string"}}),
    ];
    for declared in declarations {
        let subject = json!({"presentation": {
            "headline": ["arguments.path"], "changes": declared.clone()}});
        let entry = fold_admitting(Some(default_caps()), &events, subject);
        assert_eq!(entry, plain, "{declared}");
    }
}

#[test]
fn an_unsafe_path_gives_no_block_and_stays_plain_text() {
    for path in ["../../etc/passwd", "/etc/passwd", "a/../../b"] {
        let mut arguments = edit_arguments();
        arguments["path"] = json!(path);
        let events = tool_call("edit_file", arguments, "ok", "{\"success\":true}");
        let entry = fold_admitting(Some(default_caps()), &events, edit_subject());
        assert!(entry.blocks.is_empty(), "{path}");
        assert_eq!(entry.slots["headline"], path);
    }
}

#[test]
fn hostile_change_text_is_stripped_and_bounded_by_the_definitions_caps() {
    let mut arguments = edit_arguments();
    arguments["old_string"] = json!("\u{1b}[31mred\u{1b}[0m\u{0}\n");
    let long = "x".repeat(4_000);
    let added: String = (0..50)
        .map(|index| format!("{index} {long}\u{202e}\n"))
        .collect();
    arguments["new_string"] = json!(added);
    let events = tool_call("edit_file", arguments, "ok", "{\"success\":true}");
    let caps = json!({"files": 20, "lines_per_file": 10, "total_lines": 2000, "line_chars": 80});
    let entry = fold_admitting(Some(caps), &events, edit_subject());
    let block = serde_json::to_value(&entry.blocks[0]).unwrap();
    let file = &block["files"][0];
    let lines = file["hunks"][0]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 10);
    assert_eq!(lines[0], json!("-red"));
    assert!(
        lines
            .iter()
            .all(|line| line.as_str().unwrap().chars().count() <= 82)
    );
    assert_eq!(file["added"], json!(50));
    assert_eq!(file["removed"], json!(1));
    assert_eq!(
        file["truncated"],
        json!({"reason": "lines", "omitted_lines": 41})
    );
    let drawn = block.to_string();
    assert!(drawn.len() < 4_096);
    assert!(!drawn.contains("\\u001b") && !drawn.contains('\u{202e}'));
}

/// The opt-in is closed: a block kind or a cap the engine does not know is a
/// definition error, not something to ignore.
#[test]
fn an_unknown_block_kind_or_cap_refuses_the_definition() {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/ryeos-ui/.ai/config/ryeos/ui/timeline/thread.yaml"),
    )
    .unwrap();
    for blocks in [
        json!({"form": {}}),
        json!({"changes": {"files": 20, "colour": "red"}}),
        json!(["changes"]),
    ] {
        let mut doc: Value = serde_yaml::from_str(&text).unwrap();
        doc["events"]["child_thread_spawned"][0]["emit"][0]["adopt"]["present"]["blocks"] =
            blocks.clone();
        assert!(Definition::from_value(doc).is_err(), "{blocks}");
    }
    // Absent caps take the defaults.
    assert!(
        serde_json::to_string(&fold_admitting(
            Some(json!({})),
            &tool_call("edit_file", edit_arguments(), "ok", "{}"),
            edit_subject()
        ))
        .unwrap()
        .contains("\"block\":\"changes\"")
    );
}

/// Bounding a timeline's blocks reduces them to their counts, recorded as a
/// window cut, and never touches anything else of the entry.
#[test]
fn a_spent_block_budget_reduces_blocks_to_their_counts() {
    let events = tool_call("edit_file", edit_arguments(), "ok", "{\"success\":true}");
    let mut folder = Folder::new(definition_admitting(Some(default_caps())));
    folder.provide("T-2", edit_subject());
    folder.push(&events).unwrap();
    let whole = folder.timeline();
    let bytes = ryeos_timeline::block_bytes(&whole.items);
    assert!(bytes > 0);

    let mut kept = whole.items.clone();
    ryeos_timeline::bound_blocks(&mut kept, bytes);
    assert_eq!(kept, whole.items, "a budget that fits changes nothing");

    let mut cut = whole.items.clone();
    ryeos_timeline::bound_blocks(&mut cut, bytes - 1);
    assert!(ryeos_timeline::block_bytes(&cut) < bytes);
    let timeline = Timeline {
        items: cut,
        ..whole.clone()
    };
    let entry = activity(&timeline);
    let block = serde_json::to_value(&entry.blocks[0]).unwrap();
    assert_eq!(
        block["files"][0],
        json!({"path": "src/lib.rs", "op": "modify", "added": 3, "removed": 1, "binary": false,
               "hunks": [], "truncated": {"reason": "window", "omitted_lines": 4}})
    );
    let mut without = entry.clone();
    without.blocks.clear();
    let mut original = activity(&whole);
    original.blocks.clear();
    assert_eq!(without, original);
}

/// The shipped definition admits a change block on every tool-call adoption
/// and states the window budget.
#[test]
fn the_shipped_definition_admits_changes_and_states_its_budget() {
    let def = definition();
    assert_eq!(def.blocks.window_bytes, 512 * 1024);
    let events = tool_call("edit_file", edit_arguments(), "ok", "{\"success\":true}");
    let mut folder = Folder::new(def);
    folder.provide("T-2", edit_subject());
    folder.push(&events).unwrap();
    assert_eq!(activity(&folder.timeline()).blocks.len(), 1);
}
