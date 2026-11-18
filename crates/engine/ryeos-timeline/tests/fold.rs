use std::path::PathBuf;

use ryeos_timeline::{Definition, Folder, Item, Timeline, fold};
use serde_json::{Value, json};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn definition() -> Definition {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/ryeos-ui/.ai/config/ryeos/ui/timeline/thread.yaml"),
    )
    .unwrap();
    Definition::from_yaml(&text).unwrap()
}

fn events(name: &str) -> Vec<Value> {
    let text =
        std::fs::read_to_string(dir().join("fixtures").join(format!("{name}.json"))).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    doc["events"].as_array().unwrap().clone()
}

const FIXTURES: [&str; 7] = [
    "tool_run_completed",
    "tool_run_failed",
    "directive_provider_failure",
    "directive_tool_then_handoff_failure",
    "graph_foreach_failure_recovery",
    "directive_continuation_with_successor",
    "directive_turn_record",
];

fn golden(name: &str, timeline: &Timeline) {
    let path = dir().join("golden").join(format!("{name}.json"));
    let actual = serde_json::to_string_pretty(timeline).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing golden {path:?}; run with UPDATE_GOLDEN=1"));
    assert_eq!(actual, expected, "golden mismatch for {name}");
}

#[test]
fn every_fixture_folds_to_its_golden() {
    let def = definition();
    for name in FIXTURES {
        let timeline = fold(&events(name), &def).unwrap();
        golden(name, &timeline);
    }
}

#[test]
fn every_event_type_in_the_fixtures_is_declared() {
    let def = definition();
    for name in FIXTURES {
        let timeline = fold(&events(name), &def).unwrap();
        assert!(
            timeline.undeclared.is_empty(),
            "{name} has undeclared event types: {:?}",
            timeline.undeclared
        );
    }
}

#[test]
fn folding_is_deterministic() {
    let def = definition();
    for name in FIXTURES {
        let a = fold(&events(name), &def).unwrap();
        let b = fold(&events(name), &def).unwrap();
        assert_eq!(a, b, "{name}");
    }
}

fn entry_ids(timeline: &Timeline) -> Vec<(String, String)> {
    let mut ids = Vec::new();
    for item in &timeline.items {
        match item {
            Item::Entry(e) => ids.push((e.id.clone(), format!("{:?}", e.kind))),
            Item::Cluster(c) => ids.extend(
                c.members
                    .iter()
                    .map(|e| (e.id.clone(), format!("{:?}", e.kind))),
            ),
            Item::Recap(r) => ids.extend(entry_ids(&Timeline {
                items: r.members.clone(),
                undeclared: Default::default(),
                notice_noun: None,
                lane_matches: Vec::new(),
            })),
        }
    }
    ids
}

/// Appending events never renames or removes an entry already delivered.
#[test]
fn ids_are_stable_across_every_prefix() {
    let def = definition();
    for name in FIXTURES {
        let all = events(name);
        let mut previous: Vec<(String, String)> = Vec::new();
        for n in 1..=all.len() {
            let current = entry_ids(&fold(&all[..n], &def).unwrap());
            for id in &previous {
                assert!(
                    current.contains(id),
                    "{name}: entry {id:?} vanished or changed kind at prefix {n}"
                );
            }
            previous = current;
        }
    }
}

fn synthetic(seq: i64, ts: &str, event_type: &str, payload: Value) -> Value {
    json!({"chain_seq": seq, "thread_id": "T-1", "ts": ts, "event_type": event_type, "payload": payload})
}

/// One finished operation as a start and a result event; `failed` makes the
/// result an error.
fn op(seq: &mut i64, n: usize, start: &str, end: &str, failed: bool) -> Vec<Value> {
    let id = format!("{n:064x}");
    let result = json!({
        "operation_id": id, "call_id": "c", "tool": "t",
        "status": if failed { "error" } else { "ok" },
        "parts": [{"type": "text", "text": "r"}],
        "guard": {"result_size_bytes": 10, "truncated": false}
    });
    let out = vec![
        synthetic(
            *seq,
            start,
            "tool_call_start",
            json!({"operation_id": id, "call_id": "c", "tool": "t", "turn": 1, "call_index": 0}),
        ),
        synthetic(*seq + 1, end, "tool_call_result", result),
    ];
    *seq += 2;
    out
}

fn say(seq: &mut i64, ts: &str, text: &str) -> Value {
    *seq += 1;
    synthetic(
        *seq - 1,
        ts,
        "cognition_out",
        json!({"turn": 1, "parts": [{"type": "text", "text": text}], "finish": "stop"}),
    )
}

fn ask(seq: &mut i64, ts: &str, text: &str) -> Value {
    *seq += 1;
    synthetic(
        *seq - 1,
        ts,
        "cognition_in",
        json!({"origin": "operator", "parts": [{"type": "text", "text": text}]}),
    )
}

fn shape_of(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            Item::Entry(e) => format!("{:?}", e.kind),
            Item::Cluster(c) => format!("cluster[{}]", c.headline),
            Item::Recap(r) => format!(
                "recap[{} work, {:?}s: {}]",
                r.work,
                r.duration_s,
                shape_of(&r.members).join(", ")
            ),
        })
        .collect()
}

fn shape(evs: &[Value]) -> Vec<String> {
    shape_of(&fold(evs, &definition()).unwrap().items)
}

const T0: &str = "2026-11-16T00:00:00Z";
const T1: &str = "2026-11-16T00:00:01Z";
const T5: &str = "2026-11-16T00:00:05Z";
const T9: &str = "2026-11-16T00:00:09Z";

/// A run of finished operations merges under counted nouns; a failed one
/// escapes and breaks the run; a lone operation stays a row. The exchange has
/// no outcome yet, so it is live and the turn fold leaves it open.
#[test]
fn operations_cluster_and_a_failure_escapes_the_group() {
    let mut seq = 2;
    let mut evs = vec![ask(&mut 1, T0, "go")];
    for (i, failed) in [false, false, true, false, false].into_iter().enumerate() {
        evs.extend(op(&mut seq, i + 1, T1, T5, failed));
    }
    evs.push(say(&mut seq, T9, "done"));
    assert_eq!(
        shape(&evs),
        [
            "Message",
            "cluster[2 operations]",
            "Activity",
            "cluster[2 operations]",
            "Prose"
        ]
    );
}

/// "Failed" is data: a tone the definition lists escapes even when the
/// entry's state is a finished one.
#[test]
fn escape_policy_is_read_from_the_definition() {
    let yaml = |escape: &str| {
        Definition::from_yaml(&format!(
            r#"
folds:
  escape: {escape}
  group_states: [done]
events:
  tool_call_start:
    - emit: [{{type: activity, key: [payload.id], state: done, counts_as: other, tone: [{{default: warn}}]}}]
"#
        ))
        .unwrap()
    };
    let evs: Vec<Value> = (1..=3)
        .map(|n| synthetic(n, T0, "tool_call_start", json!({"id": n})))
        .collect();
    let with = |escape| shape_of(&fold(&evs, &yaml(escape)).unwrap().items);
    assert_eq!(
        with("{tones: [warn]}"),
        ["Activity", "Activity", "Activity"]
    );
    assert_eq!(with("{states: [failed]}"), ["cluster[3 other]"]);
}

/// A link breaks the run of operations around it.
#[test]
fn a_link_breaks_a_run() {
    let mut seq = 2;
    let mut evs = vec![ask(&mut 1, T0, "go")];
    evs.extend(op(&mut seq, 1, T1, T5, false));
    evs.extend(op(&mut seq, 2, T1, T5, false));
    let b = format!("{:064x}", 2);
    evs.push(synthetic(
        seq,
        T5,
        "child_thread_spawned",
        json!({"child_thread_id": "T-9", "item_id": "x", "operation_id": b}),
    ));
    seq += 1;
    evs.extend(op(&mut seq, 3, T1, T5, false));
    evs.extend(op(&mut seq, 4, T1, T5, false));
    assert_eq!(
        shape(&evs),
        [
            "Message",
            "cluster[2 operations]",
            "Link",
            "cluster[2 operations]"
        ]
    );
}

/// Two or more consecutive notices fold under the notice noun; a lone one and
/// notices split by prose stay rows.
#[test]
fn consecutive_notices_fold_and_a_lone_one_stays() {
    let mut seq = 2;
    let mut evs = vec![ask(&mut 1, T0, "go")];
    for (kind, payload) in [
        ("milestone", json!({"kind": "a"})),
        ("cost_untracked", json!({"model": "m"})),
        ("milestone", json!({"kind": "b"})),
    ] {
        evs.push(synthetic(seq, T1, kind, payload));
        seq += 1;
    }
    evs.push(say(&mut seq, T5, "ok"));
    evs.push(synthetic(seq, T9, "milestone", json!({"kind": "c"})));
    assert_eq!(
        shape(&evs),
        ["Message", "cluster[3 system events]", "Prose", "Marker"]
    );
}

/// A finished exchange folds its work under one recap with the duration of
/// the stretch; the next exchange is live and stays open.
#[test]
fn a_finished_turn_folds_and_a_live_one_stays_open() {
    let mut seq = 2;
    let mut evs = vec![ask(&mut 1, T0, "first")];
    evs.extend(op(&mut seq, 1, T1, T5, false));
    evs.extend(op(&mut seq, 2, T5, T9, false));
    evs.push(say(&mut seq, T9, "answer"));
    evs.push(ask(&mut seq, T9, "second"));
    evs.extend(op(&mut seq, 3, T1, T5, false));
    evs.extend(op(&mut seq, 4, T1, T5, false));
    evs.push(say(&mut seq, T9, "answer two"));
    assert_eq!(
        shape(&evs),
        [
            "Message",
            "recap[2 work, Some(8)s: cluster[2 operations]]",
            "Prose",
            "Message",
            "cluster[2 operations]",
            "Prose"
        ]
    );
}

/// The last exchange finishes when its own outcome follows the answer, and
/// an exchange without an answer never folds.
#[test]
fn an_outcome_finishes_the_last_turn_and_no_answer_keeps_it_open() {
    let mut seq = 2;
    let mut evs = vec![ask(&mut 1, T0, "go")];
    evs.extend(op(&mut seq, 1, T1, T5, false));
    let unanswered = {
        let mut e = evs.clone();
        e.push(synthetic(
            seq,
            T9,
            "thread_failed",
            json!({"outcome_code": "boom", "message": "x"}),
        ));
        e
    };
    assert_eq!(shape(&unanswered), ["Message", "Activity", "Outcome"]);
    evs.push(say(&mut seq, T5, "answer"));
    evs.push(synthetic(
        seq,
        T9,
        "thread_completed",
        json!({"outcome_code": "success"}),
    ));
    assert_eq!(
        shape(&evs),
        [
            "Message",
            "recap[1 work, Some(4)s: Activity]",
            "Prose",
            "Outcome"
        ]
    );
}

/// A failed operation inside a finished turn stays a visible row and splits
/// the work into two recaps, so entry order never changes.
#[test]
fn a_failure_stays_visible_inside_a_folded_turn() {
    let mut seq = 2;
    let mut evs = vec![ask(&mut 1, T0, "go")];
    evs.extend(op(&mut seq, 1, T1, T5, false));
    evs.extend(op(&mut seq, 2, T5, T5, true));
    evs.extend(op(&mut seq, 3, T5, T9, false));
    evs.push(say(&mut seq, T9, "answer"));
    evs.push(ask(&mut seq, T9, "next"));
    assert_eq!(
        shape(&evs),
        [
            "Message",
            "recap[1 work, Some(4)s: Activity]",
            "Activity",
            "recap[1 work, Some(4)s: Activity]",
            "Prose",
            "Message"
        ]
    );
}

fn ev(seq: i64, ts: &str, thread: &str, event_type: &str, payload: Value) -> Value {
    json!({"chain_seq": seq, "chain_root_id": "T-1", "thread_id": thread, "ts": ts,
           "event_type": event_type, "payload": payload})
}

fn find<'a>(timeline: &'a Timeline, id: &str) -> Option<&'a ryeos_timeline::Entry> {
    timeline.items.iter().find_map(|item| match item {
        Item::Entry(e) if e.id == id => Some(e),
        Item::Cluster(c) => c.members.iter().find(|e| e.id == id),
        _ => None,
    })
}

/// Constructed events: no real chain has a recorded in-process child yet.
/// A child thread in the same chain is adopted by the activity that dispatched it
/// and its terminal event does not draw an outcome; the viewed thread's does.
#[test]
fn an_adopted_in_chain_child_nests_and_draws_no_outcome() {
    let def = definition();
    let op = format!("{:064x}", 7);
    let evs = vec![
        ev(
            1,
            "2026-11-16T00:00:00Z",
            "T-1",
            "cognition_in",
            json!({"origin": "launch", "parts": [{"type": "text", "text": "go"}]}),
        ),
        ev(
            2,
            "2026-11-16T00:00:01Z",
            "T-1",
            "tool_call_start",
            json!({"operation_id": op, "tool": "t"}),
        ),
        ev(
            3,
            "2026-11-16T00:00:01Z",
            "T-1",
            "child_thread_spawned",
            json!({"child_thread_id": "T-2", "item_id": "service:x", "operation_id": op}),
        ),
        ev(
            4,
            "2026-11-16T00:00:02Z",
            "T-2",
            "thread_completed",
            json!({"outcome_code": "success"}),
        ),
        ev(
            5,
            "2026-11-16T00:00:03Z",
            "T-1",
            "tool_call_result",
            json!({"operation_id": op, "call_id": "c", "tool": "t", "status": "ok",
                   "parts": [], "guard": {"result_size_bytes": 3, "truncated": false}}),
        ),
        ev(
            6,
            "2026-11-16T00:00:04Z",
            "T-1",
            "thread_completed",
            json!({"outcome_code": "success"}),
        ),
    ];
    let timeline = fold(&evs, &def).unwrap();
    let link = find(&timeline, "lnk:T-2").expect("link");
    assert_eq!(link.parent.as_deref(), Some(format!("act:{op}").as_str()));
    assert!(
        find(&timeline, "out:T-2").is_none(),
        "child terminal must not draw an outcome"
    );
    assert!(
        find(&timeline, "out:T-1").is_some(),
        "spine terminal draws one"
    );
}

/// A thread joined to the spine by a continuation edge draws its own outcome.
#[test]
fn a_continuation_successor_is_on_the_spine() {
    let def = definition();
    let evs = vec![
        ev(
            1,
            "2026-11-16T00:00:00Z",
            "T-1",
            "cognition_in",
            json!({"origin": "launch", "parts": [{"type": "text", "text": "go"}]}),
        ),
        ev(
            2,
            "2026-11-16T00:00:01Z",
            "T-1",
            "thread_continued",
            json!({"successor_thread_id": "T-2", "reason": "context_window"}),
        ),
        ev(
            3,
            "2026-11-16T00:00:02Z",
            "T-2",
            "thread_created",
            json!({"continuation_from": "T-1"}),
        ),
        ev(
            4,
            "2026-11-16T00:00:03Z",
            "T-2",
            "thread_completed",
            json!({"outcome_code": "success"}),
        ),
    ];
    let timeline = fold(&evs, &def).unwrap();
    assert!(find(&timeline, "out:T-2").is_some());
}

/// Retry is state on the step's activity; the step's completion overwrites it,
/// so it cannot stick.
#[test]
fn retry_is_state_and_the_next_completion_clears_it() {
    let def = definition();
    let step = json!({"graph_run_id": "gr-1", "node": "flaky", "step": 2});
    let with = |extra: Value| {
        let mut p = step.clone();
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    let mut evs = vec![ev(
        1,
        "2026-11-16T00:00:00Z",
        "T-1",
        "graph_step_started",
        step.clone(),
    )];
    evs.push(ev(
        2,
        "2026-11-16T00:00:01Z",
        "T-1",
        "graph_node_retry",
        with(json!({"attempt": 1, "attempts": 2})),
    ));
    let mid = fold(&evs, &def).unwrap();
    assert_eq!(
        find(&mid, "act:flaky/2").unwrap().state.as_deref(),
        Some("retrying")
    );
    evs.push(ev(
        3,
        "2026-11-16T00:00:02Z",
        "T-1",
        "graph_step_completed",
        with(json!({"status": "ok"})),
    ));
    let end = fold(&evs, &def).unwrap();
    assert_eq!(
        find(&end, "act:flaky/2").unwrap().state.as_deref(),
        Some("done")
    );
}

/// Order of the flattened entries is also stable, not only their presence.
#[test]
fn entry_order_is_stable_across_every_prefix() {
    let def = definition();
    for name in FIXTURES {
        let all = events(name);
        let mut previous: Vec<String> = Vec::new();
        for n in 1..=all.len() {
            let current: Vec<String> = entry_ids(&fold(&all[..n], &def).unwrap())
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            let kept: Vec<&String> = current.iter().filter(|id| previous.contains(id)).collect();
            let before: Vec<&String> = previous.iter().collect();
            assert_eq!(
                kept, before,
                "{name}: order of existing entries changed at prefix {n}"
            );
            previous = current;
        }
    }
}

/// `over` emits once per array element; elements are visible as `item` and
/// `item_index`, and keyless entries are numbered by position.
#[test]
fn over_emits_one_entry_per_element_with_positional_ids() {
    let def = Definition::from_yaml(
        r#"
events:
  cognition_out:
    - over: payload.parts
      emit:
        - type: activity
          when: {eq: [item.type, tool_call]}
          key: [thread_id, payload.turn, item_index]
          project: {headline: item.name}
        - type: prose
          when: {eq: [item.type, text]}
          project: {body: item.text}
"#,
    )
    .unwrap();
    let evs = vec![ev(
        4,
        "2026-11-16T00:00:00Z",
        "T-1",
        "cognition_out",
        json!({"turn": 3, "parts": [
            {"type": "text", "text": "looking"},
            {"type": "tool_call", "name": "read"},
            {"type": "tool_call", "name": "grep"},
            {"type": "text", "text": "done"}
        ]}),
    )];
    let timeline = fold(&evs, &def).unwrap();
    let ids: Vec<&str> = timeline
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Entry(e) => Some(e.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["pro:4/0", "act:T-1/3/1", "act:T-1/3/2", "pro:4/3"]);
    assert_eq!(
        find(&timeline, "act:T-1/3/2").unwrap().slots["headline"],
        "grep"
    );
    assert_eq!(find(&timeline, "pro:4/3").unwrap().slots["body"], "done");
}

/// A lookup without `array`/`find` takes the latest earlier value of the same
/// thread where `get` resolves, skipping events that lack it.
#[test]
fn a_bare_lookup_takes_the_latest_earlier_value_of_the_same_thread() {
    let def = Definition::from_yaml(
        r#"
events:
  thread_failed:
    - emit:
        - type: outcome
          key: [thread_id]
          project:
            retry_input: {lookup: {event: cognition_in, get: payload.content}}
"#,
    )
    .unwrap();
    let evs = vec![
        ev(
            1,
            "2026-11-16T00:00:00Z",
            "T-1",
            "cognition_in",
            json!({"content": "first"}),
        ),
        ev(
            2,
            "2026-11-16T00:00:01Z",
            "T-1",
            "cognition_in",
            json!({"content": "second"}),
        ),
        ev(
            3,
            "2026-11-16T00:00:02Z",
            "T-2",
            "cognition_in",
            json!({"content": "other thread"}),
        ),
        ev(
            4,
            "2026-11-16T00:00:03Z",
            "T-1",
            "cognition_in",
            json!({"attachments": 1}),
        ),
        ev(5, "2026-11-16T00:00:04Z", "T-1", "thread_failed", json!({})),
        ev(6, "2026-11-16T00:00:05Z", "T-3", "thread_failed", json!({})),
    ];
    let timeline = fold(&evs, &def).unwrap();
    assert_eq!(
        find(&timeline, "out:T-1")
            .unwrap()
            .slots
            .get("retry_input")
            .map(String::as_str),
        Some("second")
    );
    assert!(
        !find(&timeline, "out:T-3")
            .unwrap()
            .slots
            .contains_key("retry_input")
    );
}

/// Only a failed thread carries `retry_input`; killed and timed-out ones
/// cannot be continued.
#[test]
fn only_a_failed_thread_projects_retry_input() {
    let def = definition();
    let run = |terminal: &str| {
        let evs = vec![
            ev(
                1,
                "2026-11-16T00:00:00Z",
                "T-1",
                "cognition_in",
                json!({"origin": "launch", "parts": [{"type": "text", "text": "go"}]}),
            ),
            ev(
                2,
                "2026-11-16T00:00:01Z",
                "T-1",
                terminal,
                json!({"outcome_code": "x"}),
            ),
        ];
        fold(&evs, &def).unwrap()
    };
    let failed = run("thread_failed");
    assert_eq!(
        find(&failed, "out:T-1")
            .unwrap()
            .slots
            .get("retry_input")
            .map(String::as_str),
        Some("go")
    );
    for terminal in ["thread_killed", "thread_timed_out", "thread_completed"] {
        let timeline = run(terminal);
        if let Some(entry) = find(&timeline, "out:T-1") {
            assert!(!entry.slots.contains_key("retry_input"), "{terminal}");
        }
    }
}

fn pushed_in_chunks(all: &[Value], size: usize, def: &Definition) -> Timeline {
    let mut folder = Folder::new(def.clone());
    for chunk in all.chunks(size) {
        folder.push(chunk).unwrap();
    }
    folder.timeline()
}

/// Pushing events in any split equals folding them at once, including a
/// continuation whose late edge grows the spine.
#[test]
fn pushing_in_any_split_equals_one_fold() {
    let def = definition();
    for name in FIXTURES {
        let all = events(name);
        let whole = fold(&all, &def).unwrap();
        for size in [1, 2, 3, all.len().div_ceil(2).max(1), all.len().max(1)] {
            assert_eq!(
                pushed_in_chunks(&all, size, &def),
                whole,
                "{name} by {size}"
            );
        }
    }
}

#[test]
fn a_push_out_of_order_is_refused_and_appends_nothing() {
    let def = definition();
    let all = events("tool_run_completed");
    let mut folder = Folder::new(def.clone());
    folder.push(&all[..2]).unwrap();
    let before = folder.timeline();
    assert!(folder.push(&all[1..3]).is_err());
    assert_eq!(folder.timeline(), before);
    folder.push(&all[2..]).unwrap();
    assert_eq!(folder.timeline(), fold(&all, &def).unwrap());
}

/// A lookup's `when` filters the earlier events it considers, and `find` may
/// match an element against a literal: the text of the first text part of the
/// latest earlier operator or launch input.
#[test]
fn a_lookup_filters_by_when_and_finds_an_element_by_literal() {
    let def = Definition::from_yaml(
        r#"
events:
  thread_failed:
    - emit:
        - type: outcome
          key: [thread_id]
          project:
            retry_input:
              lookup:
                event: cognition_in
                when: {one_of: [payload.origin, [operator, launch]]}
                array: payload.parts
                find: {field: type, equals: text}
                get: text
"#,
    )
    .unwrap();
    let input = |seq, thread, origin: &str, parts: Value| {
        ev(
            seq,
            "2026-11-16T00:00:00Z",
            thread,
            "cognition_in",
            json!({"origin": origin, "parts": parts}),
        )
    };
    let image = json!({"type": "image", "media_type": "image/png"});
    let evs = vec![
        input(
            1,
            "T-1",
            "launch",
            json!([{"type": "text", "text": "first"}]),
        ),
        input(
            2,
            "T-1",
            "operator",
            json!([image, {"type": "text", "text": "second"}, {"type": "text", "text": "tail"}]),
        ),
        input(
            3,
            "T-1",
            "harness",
            json!([{"type": "text", "text": "nudge"}]),
        ),
        input(
            4,
            "T-1",
            "context",
            json!([{"type": "text", "text": "ctx"}]),
        ),
        input(
            5,
            "T-2",
            "operator",
            json!([{"type": "text", "text": "other thread"}]),
        ),
        ev(6, "2026-11-16T00:00:01Z", "T-1", "thread_failed", json!({})),
        ev(7, "2026-11-16T00:00:01Z", "T-3", "thread_failed", json!({})),
    ];
    let timeline = fold(&evs, &def).unwrap();
    assert_eq!(
        find(&timeline, "out:T-1").unwrap().slots["retry_input"],
        "second"
    );
    assert!(
        !find(&timeline, "out:T-3")
            .unwrap()
            .slots
            .contains_key("retry_input")
    );
}

/// A lookup with a literal `find` and no `array` is not a lookup.
#[test]
fn a_find_without_an_array_yields_nothing() {
    let def = Definition::from_yaml(
        r#"
events:
  thread_failed:
    - emit:
        - type: outcome
          key: [thread_id]
          project:
            x: {lookup: {event: cognition_in, find: {field: type, equals: text}, get: text}}
"#,
    )
    .unwrap();
    let evs = vec![
        ev(
            1,
            "2026-11-16T00:00:00Z",
            "T-1",
            "cognition_in",
            json!({"text": "a"}),
        ),
        ev(2, "2026-11-16T00:00:01Z", "T-1", "thread_failed", json!({})),
    ];
    let timeline = fold(&evs, &def).unwrap();
    assert!(!find(&timeline, "out:T-1").unwrap().slots.contains_key("x"));
}

/// `ne` holds when the path is absent or different; `one_of` when the value
/// is any listed literal; `item_last` marks the final element of an `over`.
#[test]
fn ne_one_of_and_item_last_conditions() {
    let def = Definition::from_yaml(
        r#"
events:
  cognition_out:
    - over: payload.parts
      emit:
        - type: prose
          when: {ne: [item_index, 0]}
          project: {body: item.text}
        - type: marker
          when: {eq: [item_last, true], one_of: [payload.finish, [interrupted, length]]}
          project: {headline: payload.finish}
"#,
    )
    .unwrap();
    let out = |seq, finish: &str| {
        ev(
            seq,
            "2026-11-16T00:00:00Z",
            "T-1",
            "cognition_out",
            json!({"finish": finish, "parts": [{"text": "a"}, {"text": "b"}, {"text": "c"}]}),
        )
    };
    let timeline = fold(&[out(1, "interrupted"), out(2, "stop")], &def).unwrap();
    assert_eq!(
        entry_ids(&timeline)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        ["pro:1/1", "pro:1/2", "mrk:1/2", "pro:2/1", "pro:2/2"]
    );
}

fn all_entries(items: &[Item]) -> Vec<&ryeos_timeline::Entry> {
    items
        .iter()
        .flat_map(|item| match item {
            Item::Entry(e) => vec![e],
            Item::Cluster(c) => c.members.iter().collect(),
            Item::Recap(r) => all_entries(&r.members),
        })
        .collect()
}

/// A chunked input draws one message from its first chunk; a context chunk
/// draws nothing; an input that opens with an image opens its own exchange;
/// and the retry input is read from the latest operator input only, so an
/// image-only one offers no retry rather than an older text.
#[test]
fn chunks_image_first_inputs_and_retry_read_only_the_latest_input() {
    let def = definition();
    let chunk = |seq, origin: &str, index: u32, text: &str| {
        ev(
            seq,
            "2026-11-17T00:00:00Z",
            "T-1",
            "cognition_in",
            json!({"schema_version": 1, "origin": origin, "content_hash": "h",
                   "chunk_index": index, "chunk_count": 2, "content_chunk": text}),
        )
    };
    let evs = vec![
        ev(
            1,
            "2026-11-17T00:00:00Z",
            "T-1",
            "cognition_in",
            json!({"origin": "operator", "parts": [{"type": "text", "text": "first"}]}),
        ),
        chunk(2, "operator", 0, "hello "),
        chunk(3, "operator", 1, "world"),
        chunk(4, "context", 0, "ctx"),
        ev(
            5,
            "2026-11-17T00:00:01Z",
            "T-1",
            "cognition_in",
            json!({"origin": "operator", "parts": [{"type": "image",
                   "media_type": "image/png", "attachment": "a", "byte_length": 10}]}),
        ),
        ev(6, "2026-11-17T00:00:02Z", "T-1", "thread_failed", json!({})),
    ];
    let timeline = fold(&evs, &def).unwrap();
    let entries = all_entries(&timeline.items);
    let at = |seq: i64| -> Vec<&ryeos_timeline::Entry> {
        entries.iter().copied().filter(|e| e.seq.0 == seq).collect()
    };
    assert_eq!(at(2).len(), 1, "the first chunk draws the message");
    assert_eq!(at(2)[0].slots["body"], "hello ");
    assert!(
        at(3).is_empty() && at(4).is_empty(),
        "later and context chunks draw nothing"
    );
    let image = at(5);
    assert_eq!(image.len(), 1);
    assert!(image[0].exchange.is_some());
    assert_ne!(
        image[0].exchange,
        at(2)[0].exchange,
        "the image opens its own exchange"
    );
    let outcome = find(&timeline, "out:T-1").expect("outcome");
    assert!(
        !outcome.slots.contains_key("retry_input"),
        "an image-only latest input offers no retry, not an older text"
    );
}

#[test]
fn a_provider_retry_marker_names_the_reason_and_any_superseded_output() {
    let def = definition();
    let retry = |seq: i64, output_events: u64| {
        ev(
            seq,
            "2026-11-18T00:00:00Z",
            "T-1",
            "provider_retry",
            json!({
                "turn": 1,
                "attempt": 1,
                "backoff_ms": 1000,
                "supersedes": {"turn": 1, "attempt": 1, "output_events": output_events},
                "retry_advance": {"decision": {"reason": {"kind": "mid_stream"}}},
            }),
        )
    };
    let headlines = |evs: &[Value]| -> Vec<String> {
        let timeline = fold(evs, &def).unwrap();
        let text = serde_json::to_string(&timeline).unwrap();
        let mut found = Vec::new();
        for needle in [
            "provider retry, attempt 1 · partial output superseded, live events: 4",
            "provider retry, attempt 1",
        ] {
            if text.contains(needle) {
                found.push(needle.to_string());
            }
        }
        assert!(text.contains("mid_stream"), "{text}");
        found
    };
    let cut = headlines(&[retry(1, 4)]);
    assert!(cut.contains(
        &"provider retry, attempt 1 · partial output superseded, live events: 4".to_string()
    ));
    let clean = headlines(&[retry(1, 0)]);
    assert_eq!(clean, vec!["provider retry, attempt 1".to_string()]);
}

#[test]
fn a_turn_cut_at_the_output_limit_draws_a_marker() {
    let def = definition();
    let cut = ev(
        1,
        "2026-11-18T00:00:00Z",
        "T-1",
        "cognition_out",
        json!({
            "turn": 4,
            "finish": "length",
            "parts": [{"type": "text", "text": "partial answer"}],
        }),
    );
    let text = serde_json::to_string(&fold(&[cut], &def).unwrap()).unwrap();
    assert!(text.contains("turn cut at the output limit · 4"), "{text}");
    assert!(text.contains("partial answer"), "{text}");
}

/// A followed child's settlement updates its link: the outcome kind sets the
/// state and tone, and the result summary becomes the body. A failed child
/// escapes the run it sits in.
#[test]
fn a_settled_child_gives_its_link_the_outcome_and_summary() {
    let def = definition();
    let settle = |kind: &str, status: &str| {
        let b = format!("{:064x}", 2);
        let evs = vec![
            ask(&mut 1, T0, "go"),
            synthetic(
                2,
                T1,
                "child_thread_spawned",
                json!({"child_thread_id": "T-9", "item_id": "worker", "operation_id": b}),
            ),
            synthetic(
                3,
                T5,
                "child_thread_settled",
                json!({"child_thread_id": "T-9", "follow_key": "f", "status": status,
                       "outcome_kind": kind, "result_summary": "all done",
                       "truncated": false, "cost": {"total_usd": 0.25}}),
            ),
        ];
        fold(&evs, &def).unwrap()
    };
    let done = settle("completed", "completed");
    let link = find(&done, "lnk:T-9").expect("link");
    assert_eq!(link.state.as_deref(), Some("done"));
    assert_eq!(link.tone.as_deref(), Some("good"));
    assert_eq!(link.slots["body"], "all done");
    assert_eq!(link.slots["headline"], "worker");
    for (kind, status, state, tone) in [
        ("cancelled", "cancelled", "stopped", "warn"),
        ("deadline", "timed_out", "timed_out", "danger"),
        ("worker_failure", "failed", "failed", "danger"),
    ] {
        let timeline = settle(kind, status);
        let link = find(&timeline, "lnk:T-9").expect("link");
        assert_eq!(link.state.as_deref(), Some(state), "{kind}");
        assert_eq!(link.tone.as_deref(), Some(tone), "{kind}");
    }
}

fn lane_label(events: &[Value]) -> Option<String> {
    lane_for(events, "running").map(|lane| lane.label)
}

fn lane_for(events: &[Value], status: &str) -> Option<ryeos_timeline::Lane> {
    fold(events, &definition()).unwrap().lane(status)
}
fn running_call(seq: i64) -> Value {
    ev(
        seq,
        "2026-11-16T00:00:00Z",
        "T-1",
        "tool_call_start",
        json!({"operation_id": "op-1", "call_id": "c", "tool": "t"}),
    )
}

fn approval_requested(seq: i64) -> Value {
    ev(
        seq,
        "2026-11-16T00:00:01Z",
        "T-1",
        "hosted_session.approval_requested",
        json!({"approval_id": "ap-1", "observation": {"operation_class": "write"}}),
    )
}

#[test]
fn a_running_chain_with_no_entries_is_working_by_status_with_no_entry() {
    let lane = lane_for(&[], "running").expect("the status row matches");
    assert_eq!(lane.label, "Working");
    assert_eq!(lane.entry, None);
    // Other statuses name nothing, and an entry row outranks the status row.
    assert_eq!(lane_for(&[], "completed"), None);
    let events = [approval_requested(1)];
    assert_eq!(
        lane_for(&events, "running")
            .map(|lane| lane.label)
            .as_deref(),
        Some("Waiting for approval")
    );
}

#[test]
fn an_idle_thread_has_no_lane() {
    let mut seq = 1;
    let mut events = vec![ask(&mut seq, "2026-11-16T00:00:00Z", "hello")];
    events.extend(op(
        &mut seq,
        1,
        "2026-11-16T00:00:01Z",
        "2026-11-16T00:00:02Z",
        false,
    ));
    events.push(say(&mut seq, "2026-11-16T00:00:03Z", "done"));
    // Nothing is running and the chain has finished.
    assert_eq!(lane_for(&events, "completed"), None);
}

#[test]
fn a_running_operation_is_working_and_names_its_entry() {
    let lane = lane_for(&[running_call(1)], "running").expect("a running call is working");
    assert_eq!(lane.label, "Working");
    assert_eq!(lane.entry.as_deref(), Some("act:op-1"));
}

#[test]
fn a_pending_ask_beats_working() {
    let events = [running_call(1), approval_requested(2)];
    let lane = lane_for(&events, "running").expect("the ask is pending");
    assert_eq!(lane.label, "Waiting for approval");
    assert_eq!(lane.entry.as_deref(), Some("ask:ap-1"));
}

#[test]
fn the_latest_state_of_an_entry_decides_the_lane() {
    // The ask is answered: its latest state is no longer pending, so the
    // running call shows again.
    let mut events = vec![running_call(1), approval_requested(2)];
    events.push(ev(
        3,
        "2026-11-16T00:00:02Z",
        "T-1",
        "hosted_approval.decision_reserved",
        json!({"approval_id": "ap-1", "decision": "approve"}),
    ));
    assert_eq!(lane_label(&events).as_deref(), Some("Working"));

    // A retry outranks plain work, and the step's completion clears both.
    let step = json!({"graph_run_id": "gr-1", "node": "flaky", "step": 2});
    let graph = |seq: i64, event_type: &str, extra: Value| {
        let mut payload = step.clone();
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        ev(seq, "2026-11-16T00:00:03Z", "T-1", event_type, payload)
    };
    let mut events = vec![running_call(1), graph(2, "graph_step_started", json!({}))];
    assert_eq!(lane_label(&events).as_deref(), Some("Working"));
    events.push(graph(
        3,
        "graph_node_retry",
        json!({"attempt": 1, "attempts": 2}),
    ));
    assert_eq!(lane_label(&events).as_deref(), Some("Retrying"));
    events.push(graph(4, "graph_step_completed", json!({"status": "ok"})));
    assert_eq!(lane_label(&events).as_deref(), Some("Working"));
}

#[test]
fn the_lane_comes_only_from_the_definition_table() {
    let mut def = definition();
    def.lane.clear();
    let timeline = fold(&[running_call(1), approval_requested(2)], &def).unwrap();
    assert_eq!(timeline.lane("running"), None);
}
