use std::path::PathBuf;

use ryeos_timeline::{Definition, Item, Timeline, fold};
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
    let text = std::fs::read_to_string(dir().join("fixtures").join(format!("{name}.json"))).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    doc["events"].as_array().unwrap().clone()
}

const FIXTURES: [&str; 6] = [
    "tool_run_completed",
    "tool_run_failed",
    "directive_provider_failure",
    "directive_tool_then_handoff_failure",
    "graph_foreach_failure_recovery",
    "directive_continuation_with_successor",
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
            Item::Cluster(c) => ids.extend(c.members.iter().map(|e| (e.id.clone(), format!("{:?}", e.kind)))),
            Item::Recap(_) => {}
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

/// Mechanism test on constructed events: clusters, a recap, and a failure that
/// breaks a cluster. Real multi-call chains replace this once captured.
#[test]
fn consecutive_finished_activities_cluster_and_a_failure_breaks_the_run() {
    let def = definition();
    let mut evs = vec![synthetic(1, "2026-11-16T00:00:00Z", "cognition_in", json!({"content": "go"}))];
    let mut seq = 2;
    for (i, truncated) in [None, None, Some("error_envelope"), None, None].into_iter().enumerate() {
        let op = format!("{:064x}", i + 1);
        evs.push(synthetic(seq, "2026-11-16T00:00:01Z", "tool_call_start", json!({"operation_id": op, "tool": "t"})));
        seq += 1;
        let mut result = json!({"operation_id": op, "result_size_bytes": 10, "tool": "t"});
        if let Some(r) = truncated {
            result["truncated_reason"] = json!(r);
        }
        evs.push(synthetic(seq, "2026-11-16T00:00:05Z", "tool_call_result", result));
        seq += 1;
    }
    evs.push(synthetic(seq, "2026-11-16T00:00:09Z", "cognition_out", json!({"content": "done"})));
    let timeline = fold(&evs, &def).unwrap();
    let kinds: Vec<String> = timeline
        .items
        .iter()
        .map(|i| match i {
            Item::Entry(e) => format!("{:?}", e.kind),
            Item::Cluster(c) => format!("cluster[{}]", c.headline),
            Item::Recap(r) => format!("recap[{} work, {:?}s]", r.work, r.duration_s),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "Message",
            "cluster[2 operations]",
            "Activity",
            "cluster[2 operations]",
            "recap[5 work, Some(9)s]",
            "Prose"
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
        ev(1, "2026-11-16T00:00:00Z", "T-1", "cognition_in", json!({"content": "go"})),
        ev(2, "2026-11-16T00:00:01Z", "T-1", "tool_call_start", json!({"operation_id": op, "tool": "t"})),
        ev(3, "2026-11-16T00:00:01Z", "T-1", "child_thread_spawned",
           json!({"child_thread_id": "T-2", "item_id": "service:x", "operation_id": op})),
        ev(4, "2026-11-16T00:00:02Z", "T-2", "thread_completed", json!({"outcome_code": "success"})),
        ev(5, "2026-11-16T00:00:03Z", "T-1", "tool_call_result", json!({"operation_id": op, "result_size_bytes": 3})),
        ev(6, "2026-11-16T00:00:04Z", "T-1", "thread_completed", json!({"outcome_code": "success"})),
    ];
    let timeline = fold(&evs, &def).unwrap();
    let link = find(&timeline, "lnk:T-2").expect("link");
    assert_eq!(link.parent.as_deref(), Some(format!("act:{op}").as_str()));
    assert!(find(&timeline, "out:T-2").is_none(), "child terminal must not draw an outcome");
    assert!(find(&timeline, "out:T-1").is_some(), "spine terminal draws one");
}

/// A thread joined to the spine by a continuation edge draws its own outcome.
#[test]
fn a_continuation_successor_is_on_the_spine() {
    let def = definition();
    let evs = vec![
        ev(1, "2026-11-16T00:00:00Z", "T-1", "cognition_in", json!({"content": "go"})),
        ev(2, "2026-11-16T00:00:01Z", "T-1", "thread_continued", json!({"successor_thread_id": "T-2", "reason": "context_window"})),
        ev(3, "2026-11-16T00:00:02Z", "T-2", "thread_created", json!({"continuation_from": "T-1"})),
        ev(4, "2026-11-16T00:00:03Z", "T-2", "thread_completed", json!({"outcome_code": "success"})),
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
    let mut evs = vec![ev(1, "2026-11-16T00:00:00Z", "T-1", "graph_step_started", step.clone())];
    evs.push(ev(2, "2026-11-16T00:00:01Z", "T-1", "graph_node_retry", with(json!({"attempt": 1, "attempts": 2}))));
    let mid = fold(&evs, &def).unwrap();
    assert_eq!(find(&mid, "act:flaky/2").unwrap().state.as_deref(), Some("retrying"));
    evs.push(ev(3, "2026-11-16T00:00:02Z", "T-1", "graph_step_completed", with(json!({"status": "ok"}))));
    let end = fold(&evs, &def).unwrap();
    assert_eq!(find(&end, "act:flaky/2").unwrap().state.as_deref(), Some("done"));
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
            assert_eq!(kept, before, "{name}: order of existing entries changed at prefix {n}");
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
    assert_eq!(find(&timeline, "act:T-1/3/2").unwrap().slots["headline"], "grep");
    assert_eq!(find(&timeline, "pro:4/3").unwrap().slots["body"], "done");
}
