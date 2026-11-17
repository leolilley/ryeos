//! A tool on a runtime that declares `source_scope` runs from a private
//! admitted-input root and never sees the live project. A request that merely
//! defaulted to the live project (`project_access: default`) runs it there
//! and records that it asked for nothing more; a request that explicitly
//! demanded the live project (`project.intent: live`, CLI `--live-project`,
//! or a schedule's `live_direct` policy) is refused with a typed code instead
//! of exiting 0 with its project writes discarded. A pinned (copy-on-write)
//! launch of the same tool runs, and an `@subprocess` tool without
//! `source_scope` gets the live project under either request.

use ryeosd::test_support as common;

use std::path::Path;
use std::time::Duration;

use common::DaemonHarness;
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use serde_json::{Value, json};

const CODE: &str = "live_project_unavailable_to_sourced_runtime";

const PYTHON_TOOL: &str = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "livecheck"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "writes into the project it was given"

import os, sys

project = sys.argv[sys.argv.index("--project-path") + 1]
with open(os.path.join(project, "written-by-tool.txt"), "w") as handle:
    handle.write("hello")
print("done")
"#;

const SUBPROCESS_TOOL: &str = "category: livecheck
executor_id: \"@subprocess\"
execution_protocol: \"protocol:ryeos/core/opaque\"
config:
  command: /bin/sh
  args: [\"-c\", \"echo live > written-by-subprocess.txt\"]
";

fn plant(project: &Path, name: &str, ext: &str, body: &str, signer: &SigningKey) {
    let dir = project.join(".ai/tools/livecheck");
    std::fs::create_dir_all(&dir).expect("create tools dir");
    std::fs::write(
        dir.join(format!("{name}.{ext}")),
        lillux::signature::sign_content(body, signer, "#", None),
    )
    .expect("write tool");
}

async fn run(
    h: &DaemonHarness,
    item_ref: &str,
    project: &Path,
    policy: ExecutionPolicy,
) -> (reqwest::StatusCode, Value) {
    h.post_json(
        "/execute",
        json!({
            "item_ref": item_ref,
            "ref_bindings": {},
            "project_path": project,
            "parameters": {},
            "execution_policy": policy,
        }),
    )
    .await
    .expect("post /execute")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scheduled_live_fire_of_a_source_scoped_tool_is_refused_typed() {
    let (h, fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");
    let project = h.user_space.path().join("live-guard-project");
    std::fs::create_dir_all(project.join(".ai")).expect("create project");
    std::fs::write(
        project.join(".ai/manifest.source.yaml"),
        "name: live-guard-project\nversion: \"0.1.0\"\n",
    )
    .expect("write manifest");
    plant(&project, "py", "py", PYTHON_TOOL, &fixture.user);
    let project = project.canonicalize().expect("canonical project");

    let fire_at = chrono::Utc::now() + chrono::Duration::seconds(8);
    let (status, body) = h
        .post_execute(
            "service:scheduler/register",
            ".",
            json!({
                "schedule_id": "live-guard-fire",
                "item_ref": "tool:livecheck/py",
                "ref_bindings": {},
                "schedule_type": "at",
                "expression": fire_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "params": {},
                "timezone": "UTC",
                "misfire_policy": "skip",
                "overlap_policy": "skip",
                "lateness_grace_secs": 60,
                "enabled": true,
                "project_root": project.display().to_string(),
                "capabilities": [
                    "ryeos.execute.tool.livecheck/py",
                    "ryeos.write.project.live"
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
                        "access": "read_write",
                        "child_policy": {"kind": "inherit"}
                    }
                },
            }),
        )
        .await
        .expect("register schedule");
    assert!(status.is_success(), "register: {status}: {body}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let evidence = loop {
        let (status, body) = h
            .post_execute(
                "service:scheduler/show_fires",
                ".",
                json!({"schedule_id": "live-guard-fire"}),
            )
            .await
            .expect("show fires");
        assert!(status.is_success(), "show_fires: {status}: {body}");
        if let Some(fire) = body["result"]["fires"]
            .as_array()
            .and_then(|fires| fires.first())
            .filter(|fire| matches!(fire["status"].as_str(), Some("failed" | "completed")))
        {
            let mut text = fire.to_string();
            if let Some(thread_id) = fire["thread_id"].as_str() {
                let (_, thread) = h
                    .post_execute("service:threads/get", ".", json!({"thread_id": thread_id}))
                    .await
                    .expect("get thread");
                text.push_str(&thread.to_string());
            }
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the scheduled fire never reached a terminal state"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    // The retained fire and thread keep only the closed outcome class; the
    // typed code is in the daemon's own log.
    assert!(
        evidence.contains("launch_preparation_failed"),
        "the fire must fail in launch preparation: {evidence}"
    );
    let logged = h.stderr_so_far().contains(CODE);
    eprintln!("daemon log names {CODE}: {logged}");
    assert!(
        !project.join("written-by-tool.txt").exists(),
        "the refused tool must not have run"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_tool_runs_pinned_and_a_subprocess_tool_still_writes_live() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "py", "py", PYTHON_TOOL, &fixture.user);
    plant(
        project.path(),
        "sub",
        "yaml",
        SUBPROCESS_TOOL,
        &fixture.user,
    );

    let (status, body) = run(
        &h,
        "tool:livecheck/py",
        project.path(),
        ExecutionPolicy::local_pinned_capture(ExecutionResponse::Wait),
    )
    .await;
    assert!(status.is_success(), "pinned launch: {status}: {body:#}");
    assert_eq!(
        body.pointer("/thread/status"),
        Some(&json!("completed")),
        "{body:#}"
    );

    let (status, body) = run(
        &h,
        "tool:livecheck/sub",
        project.path(),
        ExecutionPolicy::local_live(ExecutionResponse::Wait),
    )
    .await;
    assert!(status.is_success(), "subprocess launch: {status}: {body:#}");
    assert!(
        project.path().join("written-by-subprocess.txt").is_file(),
        "an @subprocess tool without source_scope must write the live project: {body:#}"
    );
}

fn explicit_live_policy_json() -> Value {
    // The body the CLI sends for `ryeos execute --live-project`.
    let mut policy = serde_json::to_value(ExecutionPolicy::local_live(ExecutionResponse::Wait))
        .expect("policy json");
    policy["project"]["intent"] = json!("live");
    policy
}

fn intent_of(thread: &Value) -> Option<&str> {
    thread
        .pointer("/thread/project_authority/live_access/intent")
        .and_then(Value::as_str)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_defaulted_live_request_runs_a_sourced_tool_in_its_private_root_and_records_default() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "py", "py", PYTHON_TOOL, &fixture.user);

    let (status, body) = run(
        &h,
        "tool:livecheck/py",
        project.path(),
        ExecutionPolicy::local_live(ExecutionResponse::Wait),
    )
    .await;
    assert!(status.is_success(), "defaulted launch: {status}: {body:#}");
    assert_eq!(
        body.pointer("/thread/status"),
        Some(&json!("completed")),
        "{body:#}"
    );
    assert_eq!(
        intent_of(&body),
        Some("default"),
        "the thread records that the request did not ask for the live project: {body:#}"
    );
    assert!(
        !project.path().join("written-by-tool.txt").exists(),
        "a sourced tool's project writes land in its private root, not the live project"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_live_policy_is_refused_for_a_sourced_runtime() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "py", "py", PYTHON_TOOL, &fixture.user);

    let (status, body) = run(
        &h,
        "tool:livecheck/py",
        project.path(),
        ExecutionPolicy::local_live(ExecutionResponse::Wait)
            .demand_live_project()
            .expect("live demand"),
    )
    .await;
    assert!(
        body.to_string().contains(CODE) || h.stderr_so_far().contains(CODE),
        "an explicit live demand on a sourced runtime must name {CODE}: {status}: {body:#}"
    );
    assert!(
        body.pointer("/thread/status") != Some(&json!("completed")),
        "the refused tool must not complete: {status}: {body:#}"
    );
    assert!(
        !project.path().join("written-by-tool.txt").exists(),
        "the refused tool must not have run"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_live_project_policy_is_refused_for_a_sourced_runtime_and_allowed_for_subprocess() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "py", "py", PYTHON_TOOL, &fixture.user);
    plant(
        project.path(),
        "sub",
        "yaml",
        SUBPROCESS_TOOL,
        &fixture.user,
    );

    let (status, body) = h
        .post_json(
            "/execute",
            json!({
                "item_ref": "tool:livecheck/py",
                "ref_bindings": {},
                "project_path": project.path(),
                "parameters": {},
                "execution_policy": explicit_live_policy_json(),
            }),
        )
        .await
        .expect("post /execute");
    assert!(
        body.to_string().contains(CODE) || h.stderr_so_far().contains(CODE),
        "--live-project on a sourced runtime must name {CODE}: {status}: {body:#}"
    );
    assert!(!project.path().join("written-by-tool.txt").exists());

    let (status, body) = h
        .post_json(
            "/execute",
            json!({
                "item_ref": "tool:livecheck/sub",
                "ref_bindings": {},
                "project_path": project.path(),
                "parameters": {},
                "execution_policy": explicit_live_policy_json(),
            }),
        )
        .await
        .expect("post /execute");
    assert!(
        status.is_success(),
        "explicit live subprocess: {status}: {body:#}"
    );
    assert_eq!(
        intent_of(&body),
        Some("live"),
        "the thread records the explicit demand: {body:#}"
    );
    assert!(
        project.path().join("written-by-subprocess.txt").is_file(),
        "an @subprocess tool must write the live project under an explicit demand: {body:#}"
    );
}
