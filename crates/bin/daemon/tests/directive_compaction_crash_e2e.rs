//! Compaction across process loss, end to end against the scripted chat stub.
//!
//! The directive runtime reports a named stage and blocks there; the test kills
//! the daemon at that exact cut, respawns it, and checks what recovery does. A
//! compaction record that was written is not written twice, a summary that was
//! recorded is not paid for twice, and a successor that lost its process before
//! its first act still makes it. They need a daemon built with
//! `crash-qualification-test-support` and populated bundles.
#![cfg(all(unix, feature = "crash-qualification-test-support"))]

#[path = "common/stub_rig.rs"]
mod stub_rig;
use stub_rig::*;

use std::time::{Duration, Instant};

use ryeosd::test_support::build_signed_headers_for_bytes;
use serde_json::{Value, json};

const TURN: &str = "text: \"SUMMARY-TEXT\"\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n";
const REJECTION_ROWS: &str = "request_rejections:\n  - code: provider_context_length_exceeded\n    statuses: [400, 413]\n    body_contains_any: [\"context length\", \"too long\"]\n";
const CONTINUING: &str = "continuation:\n  carry_turns: 2\n";

fn continuation_yaml(compaction: &str) -> String {
    format!("category: \"ryeos-runtime\"\ncompaction:\n  enabled: true\n{compaction}")
}

fn recoverable_execute_body(project: &str) -> Value {
    json!({
        "item_ref": DIRECTIVE_REF,
        "ref_bindings": {"model": DIRECTIVE_REF},
        "project_path": project,
        "parameters": {"name": "probe"},
        "execution_policy": {
            "schema_version": 2,
            "ownership": "daemon_owned",
            "recovery": "restart_recoverable",
            "response": "wait",
            "target": {"kind": "here"},
            "environment": {
                "kind": "project_overlay",
                "include_operator_vault": true,
                "name_policy": {"kind": "declared_required"}
            },
            "project": {
                "kind": "pinned",
                "source": {"kind": "capture_live", "scope": "full_project"},
                "realization": {"kind": "read_only"},
                "child_policy": {"kind": "inherit"}
            }
        }
    })
}

/// Start a restart-recoverable execute in the background and return the
/// request task and the root thread id once the daemon lists it.
async fn start_recoverable(rig: &Rig) -> Option<(tokio::task::JoinHandle<()>, String)> {
    let bytes = serde_json::to_vec(&recoverable_execute_body(&rig.project_str())).ok()?;
    let headers = build_signed_headers_for_bytes(
        rig.h.user_key.as_ref()?,
        rig.h.node_key.as_ref()?,
        "POST",
        "/execute",
        &bytes,
    );
    let url = format!("http://{}/execute", rig.h.bind);
    let task = tokio::spawn(async move {
        let mut request = reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(bytes);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let _ = request.send().await;
    });
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(listed) = rig.h.list_threads(500).await
            && let Some(thread) = listed.into_iter().find(|t| t.item_ref == DIRECTIVE_REF)
        {
            return Some((task, thread.thread_id));
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn chain_threads(rig: &Rig, root: &str) -> Vec<(String, String)> {
    rig.h
        .list_threads(200)
        .await
        .map(|listed| {
            listed
                .into_iter()
                .filter(|t| t.chain_root_id == root || t.thread_id == root)
                .map(|t| (t.thread_id, t.status))
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until every thread of the chain is terminal and the count is stable,
/// respawning the daemon whenever it has exited (a native service window ends
/// an incarnation after a bounded amount of work).
async fn wait_chain_settled(
    rig: &mut Rig,
    root: &str,
    seconds: u64,
) -> (Vec<(String, String)>, u32) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut stable = 0u32;
    let mut last_count = 0usize;
    let mut respawns = 0u32;
    loop {
        if rig.h.child.try_wait().map(|s| s.is_some()).unwrap_or(false) {
            rig.h
                .respawn_with(|_| {})
                .await
                .expect("respawn the daemon after its service window ended");
            respawns += 1;
            stable = 0;
        }
        let chain = chain_threads(rig, root).await;
        let terminal =
            !chain.is_empty() && chain.iter().all(|(_, s)| TERMINAL.contains(&s.as_str()));
        if terminal && chain.len() == last_count {
            stable += 1;
        } else {
            stable = 0;
        }
        last_count = chain.len();
        if stable >= 8 || Instant::now() >= deadline {
            return (chain, respawns);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn request_bodies(rig: &Rig) -> Vec<Value> {
    rig.stub
        .requests()
        .await
        .into_iter()
        .map(|request| request.body)
        .collect()
}

fn messages_of(body: &Value) -> Vec<Value> {
    body.get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn offers_no_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

/// Run the rig to its blocked stage, kill the daemon there and bring it back.
/// Returns the root thread id and how many provider requests the dead process
/// had made.
async fn crash_at_stage(rig: &mut Rig) -> (String, usize) {
    let (task, id) = start_recoverable(rig).await.expect("root thread appeared");
    rig.phase_gate
        .as_mut()
        .expect("the rig has a phase gate")
        .wait_reached()
        .await
        .expect("the runtime reached the stage");
    let before = rig.stub.requests().await.len();
    rig.h
        .kill_daemon()
        .await
        .expect("kill the daemon at the cut");
    task.abort();
    rig.h
        .respawn_with(|_| {})
        .await
        .expect("respawn the daemon");
    (id, before)
}

/// E5. A crash right after the compaction record was written: recovery makes
/// the request the dead process would have made, and the compaction is not
/// recorded a second time.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_after_the_compaction_record_is_recovered_without_a_second_record() {
    let script = format!("steps:\n  - repeat: 14\n    {TURN}  - text: done\n");
    let mut rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.4\n  keep_turns: 2\n  min_turns_between: 100\n  summary:\n    mode: none\n",
            )),
            limits: Some((300, 300)),
            phase_cut: Some("compaction_recorded"),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (id, before) = crash_at_stage(&mut rig).await;
    let (chain, _) = wait_chain_settled(&mut rig, &id, 600).await;
    assert!(
        chain.iter().any(|(_, s)| s == "completed"),
        "recovered to completion: {chain:?}"
    );
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert_eq!(records.len(), 1, "one compaction, once: {records:?}");
    // The first request after recovery is the compacted conversation.
    let bodies = request_bodies(&rig).await;
    let after = messages_of(&bodies[before]);
    assert!(
        after.iter().any(|m| m
            .to_string()
            .contains("were removed to keep it within the context window")),
        "{after:?}"
    );
    assert!(after.len() < messages_of(&bodies[before - 1]).len());
}

/// E6. A crash after the summary call settled but before its record: recovery
/// writes the record from the retained summary and makes no second summary
/// request.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_after_the_summary_call_is_finished_from_the_retained_summary() {
    let script = format!("steps:\n  - repeat: 14\n    {TURN}  - text: done\n");
    let mut rig = start_rig(
        &script,
        RigOptions {
            context_window: 2000,
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.4\n  keep_turns: 2\n  min_turns_between: 100\n",
            )),
            limits: Some((300, 300)),
            phase_cut: Some("compaction_summary_recorded"),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (id, before) = crash_at_stage(&mut rig).await;
    let (chain, _) = wait_chain_settled(&mut rig, &id, 600).await;
    assert!(
        chain.iter().any(|(_, s)| s == "completed"),
        "recovered to completion: {chain:?}"
    );
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["summary_model_call"], true);
    let bodies = request_bodies(&rig).await;
    let summaries = bodies.iter().filter(|b| offers_no_tools(b)).count();
    assert_eq!(summaries, 1, "the summary was paid for once");
    assert!(offers_no_tools(&bodies[before - 1]));
    assert!(!offers_no_tools(&bodies[before]));
}

/// D. A successor that lost its process before its first act (the overflow
/// compaction) makes that act after the recovery, not an uncompacted request.
#[tokio::test(flavor = "multi_thread")]
async fn a_successor_lost_before_its_first_act_still_compacts() {
    let script = format!(
        "steps:\n  - repeat: 6\n    {TURN}  - fail: {{ status: 400, body: \"context length exceeded: prompt is too long\" }}\n  - text: \"RECOVERY-SUMMARY\"\n  - text: done\n"
    );
    let mut rig = start_rig(
        &script,
        RigOptions {
            continuation_yaml: Some(continuation_yaml(
                "  trigger_ratio: 0.99\n  keep_turns: 2\n  summary:\n    mode: model\n",
            )),
            header_extra: CONTINUING,
            provider_top_extra: REJECTION_ROWS.to_string(),
            limits: Some((300, 300)),
            phase_cut: Some("compaction_started"),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (id, before) = crash_at_stage(&mut rig).await;
    // Seven requests were made: six turns and the refused one.
    assert_eq!(before, 7);
    let (chain, _) = wait_chain_settled(&mut rig, &id, 600).await;
    assert!(chain.iter().any(|(_, s)| s == "continued"), "{chain:?}");
    assert!(chain.iter().any(|(_, s)| s == "completed"), "{chain:?}");
    let records = chain_payloads(&rig.h, &id, "cognition_compaction").await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["trigger"], "overflow");
    let bodies = request_bodies(&rig).await;
    // The request after the refusal is the summary call, not a second
    // uncompacted attempt.
    assert!(offers_no_tools(&bodies[7]), "request 7 is the summary call");
    assert_eq!(bodies.len(), 9);
}

/// The node's native service window ends a daemon incarnation after a bounded
/// amount of work. A chain of fifteen continuations crosses such boundaries; the
/// chain limit counts through them, and the sixteenth thread fails with the
/// typed limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_limit_holds_across_daemon_incarnations() {
    let script = "steps:\n  - repeat: 260\n    tool_calls:\n      - { id: \"call_{n}\", name: bulk_bulk, arguments: {} }\n  - repeat: 5\n    text: done\n";
    let mut rig = start_rig(
        script,
        RigOptions {
            context_window: 2000,
            header_extra: CONTINUING,
            continuation_yaml: Some(
                "category: \"ryeos-runtime\"\nchain_depth_limit: 15\n".to_string(),
            ),
            limits: Some((3000, 3000)),
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let (_task, id) = start_recoverable(&rig).await.expect("root thread appeared");
    let (chain, respawns) = wait_chain_settled(&mut rig, &id, 3000).await;
    assert_eq!(
        chain.len(),
        16,
        "fifteen continuations then the limit: {chain:?}"
    );
    assert_eq!(
        chain.iter().filter(|(_, s)| s == "continued").count(),
        15,
        "{chain:?}"
    );
    let failed = chain
        .iter()
        .find(|(_, s)| s == "failed")
        .unwrap_or_else(|| panic!("the sixteenth thread failed: {chain:?}"));
    let record = rig
        .h
        .get_thread(&failed.0)
        .await
        .ok()
        .flatten()
        .map(|r| r.to_string())
        .unwrap_or_default();
    assert!(record.contains("continuation_chain_limit"), "{record}");
    assert!(
        respawns >= 1,
        "the chain crossed at least one daemon incarnation boundary"
    );
}
