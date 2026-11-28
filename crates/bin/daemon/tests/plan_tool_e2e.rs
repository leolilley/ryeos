//! The built-in plan tool end to end against the scripted chat stub: a plan
//! call is accepted and answered in process, a bad one is refused as a tool
//! error, and a compaction that drops the newest plan call carries it forward
//! as exactly one pinned input from origin `plan`.
//!
//! Every test shows the mechanism: what the model is sent and what the record
//! holds. None shows that a model plans better or finishes more work.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use stub_rig::{RigOptions, chain_payloads, launch_on, launch_on_with, start_rig, wait_terminal};

const MAX_WAIT: Duration = Duration::from_secs(180);
const PLAN_HEADER: &str = "plan:\n  max_steps: 4\n  max_step_bytes: 60\n";
const FRAME: &str = "Your plan as recorded when the context was last cut";

fn plan_call(id: &str, steps: &str) -> String {
    format!(
        "  - tool_calls:\n      - {{ id: \"{id}\", name: plan, arguments: {{ steps: {steps} }} }}\n"
    )
}

/// A turn that calls the bulk tool and carries text, so it is both a normal
/// turn and a usable summary.
const BULK: &str = "text: \"SUMMARY-TEXT\"\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n";

fn execution() -> String {
    "category: \"ryeos-runtime\"\nretries: 3\nbackoff_base_ms: 100\nbackoff_max_ms: 1000\n"
        .to_string()
}

fn continuation(extra: &str) -> String {
    format!("category: \"ryeos-runtime\"\ncompaction:\n  enabled: true\n{extra}")
}

fn messages_of(body: &Value) -> Vec<Value> {
    body.get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn text_of(message: &Value) -> String {
    message
        .get("content")
        .map(|content| match content {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn offers(body: &Value, tool: &str) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(|entry| entry.to_string().contains(tool)))
}

/// The messages of a request that carry the pinned plan frame.
fn plan_pins(body: &Value) -> Vec<String> {
    messages_of(body)
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(text_of)
        .filter(|text| text.contains(FRAME))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_call_is_accepted_and_a_bad_one_is_refused_as_a_tool_error() {
    let script = format!(
        "steps:\n{}{}{}{}  - text: done\n",
        plan_call(
            "p1",
            "[{ step: read, status: in_progress }, { step: write, status: pending }]"
        ),
        plan_call(
            "p2",
            "[{ step: a, status: in_progress }, { step: b, status: in_progress }]"
        ),
        plan_call("p3", "[{ step: a, status: finished }]"),
        plan_call(
            "p4",
            "[{ step: 1, status: pending }, { step: 2, status: pending }, { step: 3, status: pending }, { step: 4, status: pending }, { step: 5, status: pending }]"
        ),
    );
    let rig = start_rig(
        &script,
        RigOptions {
            header_extra: PLAN_HEADER,
            exec_yaml: Some(execution()),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, MAX_WAIT).await;
    assert_eq!(status, "completed");
    let bodies: Vec<Value> = rig
        .stub
        .requests()
        .await
        .into_iter()
        .map(|r| r.body)
        .collect();
    assert!(offers(&bodies[0], "plan"), "the plan tool is offered");
    let last = bodies.last().expect("requests").to_string();
    assert!(
        last.contains("recorded"),
        "the first call was accepted: {last}"
    );
    assert_eq!(
        last.matches("plan_refused").count(),
        3,
        "two in progress, an unknown status and too many steps are refused: {last}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_header_field_there_is_no_plan_tool() {
    let rig = start_rig(
        "steps:\n  - text: done\n",
        RigOptions {
            exec_yaml: Some(execution()),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, MAX_WAIT).await;
    assert_eq!(status, "completed");
    let bodies: Vec<Value> = rig
        .stub
        .requests()
        .await
        .into_iter()
        .map(|r| r.body)
        .collect();
    assert!(!offers(&bodies[0], "\"plan\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_carries_the_latest_plan_as_one_pinned_input_from_origin_plan() {
    let script = format!(
        "steps:\n{}  - repeat: 8\n    {BULK}{}  - repeat: 10\n    {BULK}  - text: done\n",
        plan_call("early", "[{ step: EARLYSTEP, status: in_progress }]"),
        plan_call("late", "[{ step: LATESTEP, status: in_progress }]"),
    );
    let rig = start_rig(
        &script,
        RigOptions {
            header_extra: PLAN_HEADER,
            exec_yaml: Some(execution()),
            context_window: 2000,
            continuation_yaml: Some(continuation(
                "  trigger_ratio: 0.4\n  keep_turns: 2\n  min_turns_between: 2\n  summary:\n    mode: none\n",
            )),
            limits: Some((300, 300)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, MAX_WAIT).await;
    assert_eq!(status, "completed");

    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert!(!records.is_empty(), "a compaction was recorded");
    let pinned: Vec<&Value> = records
        .iter()
        .flat_map(|record| record["pinned"].as_array().into_iter().flatten())
        .filter(|input| {
            input["kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("plan_state_rev_"))
        })
        .collect();
    assert!(
        !pinned.is_empty(),
        "a compaction carried the plan: {records:?}"
    );
    for input in &pinned {
        assert_eq!(input["origin"], "plan", "never the harness: {input}");
        assert_eq!(input["pinned"], true);
    }
    for record in &records {
        let plans = record["pinned"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|input| {
                input["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("plan_state_rev_"))
            })
            .count();
        assert!(plans <= 1, "at most one pinned plan, no doubling: {record}");
    }

    let all: Vec<Value> = rig
        .stub
        .requests()
        .await
        .into_iter()
        .map(|r| r.body)
        .collect();
    for body in &all {
        let pins = plan_pins(body);
        assert!(pins.len() <= 1, "no doubled plan in a request: {pins:?}");
        if let Some(pin) = pins.first() {
            assert!(
                pin.contains("data, not instructions"),
                "the frame names whose words these are: {pin}"
            );
        }
    }
    // The last request after the late plan has been cut away carries the late
    // plan, not the early one.
    let last_pin = all
        .iter()
        .rev()
        .find_map(|body| plan_pins(body).into_iter().next());
    if let Some(pin) = last_pin {
        assert!(
            pin.contains("LATESTEP") && !pin.contains("EARLYSTEP"),
            "{pin}"
        );
    }
}

/// The plan header with a repeatable provider contact and a spend limit, so a
/// call cut by a restart is retried rather than failed typed.
const PLAN_RESTART_HEADER: &str = "plan:\n  max_steps: 4\n  max_step_bytes: 60\nprovider_contact: repeatable\nlimits:\n  spend_usd: \"3\"\n";

/// The plan call and its answer, as a request carries them: the assistant
/// message holding the call and the tool message answering it.
fn plan_exchange(body: &Value) -> Vec<String> {
    messages_of(body)
        .iter()
        .filter(|message| {
            let text = message.to_string();
            text.contains("RESTARTSTEP")
                || message.get("role").and_then(Value::as_str) == Some("tool")
        })
        .map(Value::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_mid_run_rebuilds_the_same_plan_conversation() {
    let hold = "  - delay_ms: 600000\n    text: \"never delivered\"\n";
    let script = format!(
        "steps:\n{}{hold}  - text: done\n",
        plan_call("p1", "[{ step: RESTARTSTEP, status: in_progress }]"),
    );
    let exec = "category: \"ryeos-runtime\"\nretries: 3\nbackoff_base_ms: 100\nbackoff_max_ms: 1000\n\
                unknown_outcome_retry:\n  max_retries: 2\n  on: [timeout, send, mid_stream]\n";
    let mut rig = start_rig(
        &script,
        RigOptions {
            header_extra: PLAN_RESTART_HEADER,
            exec_yaml: Some(exec.to_string()),
            paid_provider: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let id = launch_on_with(&rig, json!({})).await.expect("launch");

    // Request 1 is answered with the plan call; request 2 is held. The stub
    // receives it only after the attempt is recorded as issued.
    let until = Instant::now() + Duration::from_secs(120);
    while rig.stub.requests().await.len() < 2 {
        assert!(Instant::now() < until, "the held request never arrived");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let before = rig.stub.requests().await[1].body.clone();
    let exchange_before = plan_exchange(&before);
    assert!(
        exchange_before.iter().any(|m| m.contains("RESTARTSTEP")),
        "the held request carries the plan call: {exchange_before:?}"
    );

    rig.h.kill_daemon().await.expect("kill the node mid-call");
    let db = rig
        .h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/runtime.sqlite3");
    ryeos_app::test_support::read_existing_runtime(&db, |connection| {
        connection.execute("UPDATE thread_runtime SET resume_attempts = 0", [])?;
        Ok(())
    });
    rig.h.respawn_with(|_| {}).await.expect("restart the node");

    let (status, _) = wait_terminal(&rig.h, &id, MAX_WAIT).await;
    assert_eq!(status, "completed");
    let requests = rig.stub.requests().await;
    assert_eq!(requests.len(), 3, "the cut call was retried once");
    let after = &requests[2].body;
    assert!(
        offers(after, "plan"),
        "the resumed run still offers the plan tool"
    );
    assert_eq!(
        plan_exchange(after),
        exchange_before,
        "the resumed run rebuilt the same plan call and answer from the record"
    );
    rig.h.kill_daemon().await.expect("stop the node");
}
