//! The launch trust floor on a scheduled fire, end to end through the daemon.
//!
//! A schedule is signed node state, but the work it names is not: when the
//! fire reaches `dispatch_scheduled_item`, the target goes through the same
//! root preflight as `/execute`, and an unsigned or unpinned project tool is
//! refused there. The fire is recorded as failed and the tool never runs.

#![cfg(all(unix, feature = "scheduling"))]

use ryeosd::test_support as common;

use std::path::Path;
use std::time::Duration;

use common::DaemonHarness;
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

/// The audit's gap-B tool from `launch_trust_floor_e2e.rs`: points straight
/// at `@subprocess` with its own command.
const GAP_ECHO_TOOL: &str = "category: gap
executor_id: \"@subprocess\"
execution_protocol: \"protocol:ryeos/core/opaque\"
config:
  command: /bin/echo
  args: [\"gap-b-check\"]
";

fn plant_gap_echo(project: &Path, signer: Option<&SigningKey>) {
    let dir = project.join(".ai/tools/gap");
    std::fs::create_dir_all(&dir).expect("create project tools dir");
    let content = match signer {
        Some(signer) => lillux::signature::sign_content(GAP_ECHO_TOOL, signer, "#", None),
        None => GAP_ECHO_TOOL.to_owned(),
    };
    std::fs::write(dir.join("echo.yaml"), content).expect("write gap echo tool");
}

/// A live read-only project under the harness user space, as the Core doctor
/// schedule in `scheduler_e2e.rs` uses, holding the planted tool.
fn scheduled_project(h: &DaemonHarness, name: &str, signer: Option<&SigningKey>) -> String {
    let project = h.user_space.path().join(name);
    plant_gap_echo(&project, signer);
    project
        .canonicalize()
        .expect("resolve scheduled project root")
        .display()
        .to_string()
}

/// Register a one-shot schedule for `tool:gap/echo` a few seconds out. The
/// capabilities and policy are the ones the Core doctor schedule fires with,
/// narrowed to this tool, so the scheduler's pre-authorization admits the
/// fire and only the trust gate is left to refuse it.
async fn schedule_gap_echo(h: &DaemonHarness, schedule_id: &str, project_root: &str) {
    let fire_at = chrono::Utc::now() + chrono::Duration::seconds(6);
    let (status, body) = h
        .post_execute(
            "service:scheduler/register",
            ".",
            json!({
                "schedule_id": schedule_id,
                "item_ref": "tool:gap/echo",
                "ref_bindings": {},
                "schedule_type": "at",
                "expression": fire_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "params": {},
                "timezone": "UTC",
                "misfire_policy": "skip",
                "overlap_policy": "skip",
                "lateness_grace_secs": 60,
                "enabled": true,
                "project_root": project_root,
                "capabilities": [
                    "ryeos.execute.tool.gap/echo",
                    "ryeos.read.project.live"
                ],
                "execution_policy": {
                    "schema_version": 2,
                    "ownership": "daemon_owned",
                    "recovery": "restart_recoverable",
                    "response": "accepted",
                    "target": {"kind": "here"},
                    "environment": {"kind": "none"},
                    "project": {
                        "kind": "live_direct",
                        "access": "read_only",
                        "child_policy": {"kind": "inherit"}
                    }
                },
            }),
        )
        .await
        .expect("post service:scheduler/register");
    assert!(
        status.is_success(),
        "registration must succeed; the target is judged at fire time, not here: \
         status={status}; body={body:#}"
    );
}

/// Poll `show_fires` until the schedule's first fire has left `reserved`.
async fn settled_fire(h: &DaemonHarness, schedule_id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = h
            .post_execute(
                "service:scheduler/show_fires",
                ".",
                json!({"schedule_id": schedule_id}),
            )
            .await
            .expect("post service:scheduler/show_fires");
        assert!(
            status.is_success(),
            "show_fires must answer for the schedule's owner: status={status}; body={body:#}"
        );
        if let Some(fire) = body["result"]["fires"].as_array().and_then(|f| f.first())
            && fire["status"] != "reserved"
        {
            return fire.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "schedule {schedule_id} did not settle a fire in time: {body:#}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The scheduler records a refused dispatch only as `dispatch_failed`; the
/// cause is the `schedule dispatch failed` error line it logs right after.
/// Wait for that line for `schedule_id` in the daemon's stderr.
async fn dispatch_failure_line(h: &mut DaemonHarness, schedule_id: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let stderr = h.drain_stderr_nonblocking().await;
        if let Some(line) = stderr
            .lines()
            .find(|line| line.contains("schedule dispatch failed") && line.contains(schedule_id))
        {
            return line.to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the daemon never logged why schedule {schedule_id} failed to dispatch"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A refused fire: failed at dispatch, refused by the trust gate with `code`,
/// and no thread behind it.
async fn assert_fire_refused(h: &mut DaemonHarness, schedule_id: &str, code: &str) {
    let fire = settled_fire(h, schedule_id).await;
    assert_eq!(
        fire["status"], "failed",
        "a fire whose target the trust gate refuses must fail: {fire:#}"
    );
    assert_eq!(
        fire["outcome"], "dispatch_failed",
        "the refusal must happen at dispatch, not after a thread ran: {fire:#}"
    );
    assert!(
        fire["admitted_capsule_hash"].is_null(),
        "a refused fire must not record an admitted launch: {fire:#}"
    );

    let line = dispatch_failure_line(h, schedule_id).await;
    assert!(
        line.contains(code),
        "the dispatch failure must be the trust gate's {code} refusal, not a capability \
         or policy refusal: {line}"
    );
    assert!(
        line.contains("tool:gap/echo"),
        "the refusal must name the scheduled tool: {line}"
    );

    let thread_id = fire["thread_id"]
        .as_str()
        .expect("a reserved fire carries its deterministic thread id");
    let thread = h
        .get_thread(thread_id)
        .await
        .expect("read the fire's thread through threads/get");
    // No thread means no subprocess and no result: the tool never ran.
    assert!(
        thread.is_none(),
        "a refused fire must not mint its thread, so the tool cannot have run: {thread:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schedule_naming_an_unsigned_project_tool_is_refused_when_it_fires() {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");
    let project_root = scheduled_project(&h, "unsigned-schedule-project", None);

    schedule_gap_echo(&h, "trust-floor-unsigned", &project_root).await;

    assert_fire_refused(&mut h, "trust-floor-unsigned", "effective_trust_unsigned").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schedule_naming_an_unpinned_signed_project_tool_is_refused_when_it_fires() {
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");
    let unpinned = SigningKey::from_bytes(&[0x5au8; 32]);
    let project_root = scheduled_project(&h, "unpinned-schedule-project", Some(&unpinned));

    schedule_gap_echo(&h, "trust-floor-unpinned", &project_root).await;

    assert_fire_refused(&mut h, "trust-floor-unpinned", "effective_trust_refused").await;
}
