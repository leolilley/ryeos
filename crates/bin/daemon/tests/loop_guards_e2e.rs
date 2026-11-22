//! The loop guards and the turn-limit wrap-up end to end against the scripted
//! chat stub: a model that repeats one call is warned, then stopped with the
//! typed failure `loop_detected`; the last turn a turn limit allows carries a
//! wrap-up input. The stub records every request body, so the assertions read
//! the wire and the chain.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::Duration;

use serde_json::Value;
use stub_rig::{RigOptions, chain_payloads, launch_on, start_rig, thread_detail, wait_terminal};

const MAX_WAIT: Duration = Duration::from_secs(150);

fn repeat_calls(count: usize) -> String {
    let mut script = String::from("steps:\n");
    for index in 0..count {
        script.push_str(&format!(
            "  - tool_calls:\n      - {{ id: call_{index}, name: small_text_small_text, arguments: {{}} }}\n"
        ));
    }
    script.push_str("  - text: \"done\"\n");
    script
}

fn failure_code(detail: &str) -> Option<String> {
    let value: Value = serde_json::from_str(detail).ok()?;
    fn find(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => map
                .get("code")
                .and_then(Value::as_str)
                .filter(|code| *code == "loop_detected" || *code == "directive_failed")
                .map(str::to_owned)
                .or_else(|| map.values().find_map(find)),
            Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    find(&value)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_repeats_one_call_is_warned_then_stopped_with_a_typed_failure() {
    let rig = start_rig(
        &repeat_calls(10),
        RigOptions {
            spill_tools: true,
            loop_guards: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let thread_id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    let detail = thread_detail(&rig.h, &thread_id, 100_000).await;
    assert_eq!(status, "failed", "{detail}");
    assert_eq!(
        failure_code(&detail).as_deref(),
        Some("loop_detected"),
        "{detail}"
    );

    // Warned once, with the finding named in the input's kind.
    let inputs = chain_payloads(&rig.h, &thread_id, "cognition_in").await;
    let warnings: Vec<&Value> = inputs
        .iter()
        .filter(|input| input["kind"] == "loop_warning_repetition")
        .collect();
    assert_eq!(warnings.len(), 1, "{inputs:?}");
    assert_eq!(warnings[0]["origin"], "harness");

    // The stop came grace_turns (2) turns after the warning at the fifth
    // identical turn: seven provider requests, not ten.
    assert_eq!(rig.stub.request_bodies().await.len(), 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_permitted_turn_carries_the_wrap_up_input() {
    let rig = start_rig(
        &repeat_calls(1),
        RigOptions {
            spill_tools: true,
            limits: Some((2, 0)),
            loop_guards: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let thread_id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 800).await
    );
    let inputs = chain_payloads(&rig.h, &thread_id, "cognition_in").await;
    let notices: Vec<&Value> = inputs
        .iter()
        .filter(|input| input["kind"] == "limit_wrap_up")
        .collect();
    assert_eq!(notices.len(), 1, "{inputs:?}");
    // The notice came before the second and last turn: the second request
    // ends with it.
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    let last = bodies[1]["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .expect("last message");
    assert_eq!(last["role"], "user");
    assert!(
        last["content"]
            .as_str()
            .is_some_and(|text| text.starts_with("Turn limit notice")),
        "{last}"
    );
}
