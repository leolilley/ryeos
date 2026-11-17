//! Cancel through a follow: one cancel of a graph run stops the children it
//! followed, whether they are queued, mid-turn or mid-tool.
//!
//! Each case launches a graph whose `follow: true` node runs directive
//! children against the FIFO mock provider. The graph suspends: its first
//! segment is `continued`, its follow-resume successor is `created`, and the
//! children are linked to the suspended segment. The caller then cancels the
//! graph run by the id it was given (the first segment). The run must stop its
//! children with the stop proven before the cancel returns:
//!
//! - no process carrying a child's thread id (runtime or tool) is alive,
//! - tool pids and their same-group helpers are dead,
//! - every thread of the run, children and successor included, is terminal.
//!
//! Cases:
//! (a) a cohort child still queued behind a launch window of one (never
//!     started) while its sibling is mid-turn;
//! (b) a single child mid-turn, with the provider request in flight;
//! (c) a single child mid-tool, with the tool's helper in the tool's group.

#![cfg(unix)]

use ryeosd::test_support as common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::DaemonHarness;
use common::build_signed_headers_for_bytes;
use common::fast_fixture::{
    FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use common::mock_provider::{MockProvider, MockResponse};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

fn plant_vault_with_zen_key(state_path: &Path) -> anyhow::Result<()> {
    use std::collections::HashMap;
    let pub_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem");
    let pub_key = lillux::vault::read_public_key(&pub_path)?;
    let store_path = ryeos_app::vault::default_sealed_store_path(state_path);
    let secrets = HashMap::from([(
        "ZEN_API_KEY".to_string(),
        "test-zen-api-key-value".to_string(),
    )]);
    ryeos_app::vault::write_sealed_secrets(&store_path, &pub_key, &secrets)?;
    Ok(())
}

fn plant_mock_provider(
    root: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"family: chat_completions
transport:
  kind: remote_http
  base_url: "{mock_base_url}"
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {{}}
headers: {{}}
schemas:
  streaming:
    mode: delta_merge
  output_limit: {{path: max_tokens, semantics: provider_native_output_tokens}}
pricing:
  input_per_million: "0.0"
  output_per_million: "0.0"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

fn plant_model_routing(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = r#"tiers:
  general:
    provider: mock
    model: mock-model
    context_window: 200000
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

/// A top-level directive that asks the mock model one question. With
/// `declared_caps`, the directive may call tools.
fn plant_directive(
    project_dir: &Path,
    name: &str,
    declared_caps: &[&str],
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = project_dir.join(".ai/directives");
    std::fs::create_dir_all(&dir)?;
    let requires = if declared_caps.is_empty() {
        String::new()
    } else {
        let lines = declared_caps
            .iter()
            .map(|cap| format!("      - \"{cap}\"\n"))
            .collect::<String>();
        format!("requires:\n  capabilities:\n    declared:\n{lines}")
    };
    let body = format!(
        r#"---
name: {name}
category: ""
description: "cancel-through-a-follow e2e child"
model:
  tier: general
{requires}---
Do the work.
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(dir.join(format!("{name}.md")), signed)?;
    Ok(())
}

/// A tool that records its own pid and a same-group helper's pid, then hangs.
fn plant_hanging_tool(
    project_dir: &Path,
    pid_file: &Path,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = project_dir.join(".ai/tools/cancelprobe");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "cancelprobe"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/tool_streaming"
#   description: "cancel probe: hangs with a same-group helper"

import os, subprocess, time

child = subprocess.Popen(["sleep", "600"])
with open({pid_file:?}, "w") as f:
    f.write(str(os.getpid()) + " " + str(child.pid) + " " + str(os.getpgid(0)) + " " + str(os.getpgid(child.pid)))
time.sleep(600)
"#,
        pid_file = pid_file.to_str().expect("utf-8 pid file"),
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("cancelprobe.py"), signed)?;
    Ok(())
}

/// `fetch (follow -> directive:<child>)`, then `done`. The graph declares the
/// child's execute authority plus tool execution, so the child is bounded by
/// what the parent holds.
fn plant_single_follow_graph(
    project_dir: &Path,
    graph: &str,
    child: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.{child}
      - ryeos.execute.tool.*
config:
  start: fetch
  nodes:
    fetch:
      node_type: action
      follow: true
      action:
        item_id: "directive:{child}"
        ref_bindings:
          model: "directive:{child}"
        params: {{}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join(format!("{graph}.yaml")), signed)?;
    Ok(())
}

/// A cohort follow over two items with a launch window of one: the first child
/// goes live, the second stays queued until the first reaches a hard terminal.
fn plant_cohort_follow_graph(
    project_dir: &Path,
    graph: &str,
    child: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.{child}
config:
  start: fan
  nodes:
    fan:
      node_type: action
      over: "${{inputs.items}}"
      as: item
      parallel: true
      max_concurrency: 1
      follow: true
      action:
        item_id: "directive:{child}"
        ref_bindings:
          model: "directive:{child}"
        params: {{}}
      collect: results
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join(format!("{graph}.yaml")), signed)?;
    Ok(())
}

/// Live, non-zombie pids whose environment names `thread_id` as their
/// `RYEOS_THREAD_ID`: the thread's runtime and anything it started that kept
/// the environment.
fn processes_of_thread(thread_id: &str) -> Vec<i32> {
    let needle = format!("RYEOS_THREAD_ID={thread_id}");
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        let carries = environ
            .split(|byte| *byte == 0)
            .any(|var| var == needle.as_bytes());
        if carries && pid_alive(pid) {
            pids.push(pid);
        }
    }
    pids
}

fn pid_alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        // The state is the field after the ")" closing the comm; Z is dead.
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.trim().chars().next())
            .is_some_and(|state| state != 'Z'),
    }
}

/// Kills leftover probe pids if an assertion fails, so a regression cannot
/// leak ten-minute sleepers.
struct PidReaper(Vec<i32>);
impl Drop for PidReaper {
    fn drop(&mut self) {
        for pid in &self.0 {
            if pid_alive(*pid) {
                unsafe { libc::kill(*pid, libc::SIGKILL) };
            }
        }
    }
}

async fn cancel_thread(h: &DaemonHarness, thread_id: &str) -> (reqwest::StatusCode, Value) {
    let path = format!("/threads/{thread_id}/cancel");
    let node_key = h.node_key.as_ref().expect("fast fixture node key");
    let user_key = h.user_key.as_ref().expect("fast fixture user key");
    let mut request = reqwest::Client::new().post(format!("http://{}{}", h.bind, path));
    for (name, value) in build_signed_headers_for_bytes(user_key, node_key, "POST", &path, &[]) {
        request = request.header(name, value);
    }
    let response = request.send().await.expect("POST cancel");
    let status = response.status();
    (status, response.json().await.unwrap_or_else(|_| json!({})))
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

struct Run {
    h: DaemonHarness,
    project: tempfile::TempDir,
    mock: MockProvider,
}

async fn start_run(mock: MockProvider) -> (Run, FastFixture) {
    let mock_url = mock.base_url.clone();
    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            register_config_fixture_bundle(
                state_path,
                "fixture-cancel-follow-model-config",
                fixture,
                |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
            )?;
            plant_vault_with_zen_key(state_path)?;
            Ok(())
        };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock provider + workflow bundles");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    (Run { h, project, mock }, fixture)
}

/// Execute `graph:<name>`; it suspends on its follow. Returns the run id (the
/// first segment).
async fn launch_suspended_graph(run: &mut Run, graph: &str, params: Value) -> String {
    let fut = run.h.post_execute(
        &format!("graph:{graph}"),
        run.project.path().to_str().unwrap(),
        params,
    );
    let (status, body) = match tokio::time::timeout(Duration::from_secs(45), fut).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            let stderr = run.h.drain_stderr_nonblocking().await;
            panic!("post /execute failed: {e}\n--- daemon stderr ---\n{stderr}");
        }
        Err(_) => {
            let stderr = run.h.drain_stderr_nonblocking().await;
            panic!("POST /execute timed out.\n--- daemon stderr ---\n{stderr}");
        }
    };
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    assert_eq!(
        body.pointer("/result/status").and_then(Value::as_str),
        Some("continued"),
        "the graph must suspend on its follow; body={body:#}"
    );
    body.pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("execute envelope missing thread id; body={body:#}"))
        .to_string()
}

/// Thread ids and statuses of every listed thread whose item ref contains
/// `needle`.
async fn threads_of(h: &DaemonHarness, needle: &str) -> Vec<(String, String)> {
    h.list_threads(200)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|thread| thread.item_ref.contains(needle))
        .map(|thread| (thread.thread_id, thread.status))
        .collect()
}

async fn wait_for<F, Fut>(what: &str, secs: u64, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !probe().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Cancel `graph_id` once and prove the stop: the cancel is accepted and names
/// the live tip, the children's processes are gone within seconds, and every
/// thread of the run is terminal.
async fn cancel_and_prove(run: &Run, graph_id: &str, child_ids: &[String], extra_pids: &[i32]) {
    let (status, body) = cancel_thread(&run.h, graph_id).await;
    assert!(status.is_success(), "cancel returned {status}: {body}");
    assert_eq!(
        body["requested_thread_id"], graph_id,
        "the response names the id the caller cancelled: {body}"
    );
    assert_ne!(
        body["thread_id"], graph_id,
        "the cancel must land on the run's live tip, not the continued segment: {body}"
    );
    assert_eq!(body["status"], "cancelled", "body={body}");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let alive: Vec<i32> = child_ids
            .iter()
            .flat_map(|id| processes_of_thread(id))
            .chain(extra_pids.iter().copied().filter(|pid| pid_alive(*pid)))
            .collect();
        if alive.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "processes still alive 10s after the cancel: {alive:?}; cancel body={body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let all = run.h.list_threads(200).await.expect("list threads");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let all = run.h.list_threads(200).await.expect("list threads");
        let unsettled: Vec<(String, String)> = all
            .iter()
            .filter(|thread| !is_terminal(&thread.status))
            .map(|thread| (thread.thread_id.clone(), thread.status.clone()))
            .collect();
        if unsettled.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "threads not settled 20s after the cancel: {unsettled:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for child in child_ids {
        let thread = all
            .iter()
            .find(|thread| &thread.thread_id == child)
            .unwrap_or_else(|| panic!("child {child} missing from the thread list"));
        assert!(
            matches!(thread.status.as_str(), "cancelled" | "killed"),
            "child {child} must settle as stopped, not {}",
            thread.status
        );
    }
}

/// (b) A child mid-turn: the provider request is in flight when the graph run
/// is cancelled.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_graph_run_stops_a_followed_child_mid_turn() {
    let mock = MockProvider::start_blocked(vec![MockResponse::Text("never served".into())]).await;
    let (mut run, fixture) = start_run(mock).await;
    plant_directive(run.project.path(), "hangchild", &[], &fixture.publisher).unwrap();
    plant_single_follow_graph(
        run.project.path(),
        "parent_turn",
        "hangchild",
        &fixture.publisher,
    )
    .unwrap();

    let graph_id = launch_suspended_graph(&mut run, "parent_turn", json!({})).await;

    wait_for(
        "the child's provider request to be in flight",
        60,
        || async { !run.mock.captured_bodies().await.is_empty() },
    )
    .await;
    let children = threads_of(&run.h, "hangchild").await;
    assert_eq!(children.len(), 1, "one child expected: {children:?}");
    let child_id = children[0].0.clone();
    assert_eq!(children[0].1, "running", "child must be mid-turn");
    let live = processes_of_thread(&child_id);
    assert!(
        !live.is_empty(),
        "the child's runtime must be alive pre-cancel"
    );
    let _reaper = PidReaper(live);

    cancel_and_prove(&run, &graph_id, &[child_id], &[]).await;
}

/// (c) A child mid-tool: the tool and its same-group helper are running when
/// the graph run is cancelled.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_graph_run_stops_a_followed_child_mid_tool() {
    let mock = MockProvider::start(vec![
        MockResponse::ToolCall {
            id: "c1".into(),
            name: "cancelprobe_cancelprobe".into(),
            arguments: "{}".into(),
        },
        MockResponse::Text("done".into()),
    ])
    .await;
    let (mut run, fixture) = start_run(mock).await;
    let pid_dir = tempfile::tempdir().expect("pid tempdir");
    let pid_file = pid_dir.path().join("pids");
    plant_hanging_tool(run.project.path(), &pid_file, &fixture.publisher).unwrap();
    plant_directive(
        run.project.path(),
        "toolchild",
        &["ryeos.execute.tool.*"],
        &fixture.publisher,
    )
    .unwrap();
    plant_single_follow_graph(
        run.project.path(),
        "parent_tool",
        "toolchild",
        &fixture.publisher,
    )
    .unwrap();

    let graph_id = launch_suspended_graph(&mut run, "parent_tool", json!({})).await;

    wait_for("the tool to record its pids", 60, || async {
        std::fs::read_to_string(&pid_file)
            .map(|text| text.split_whitespace().count() == 4)
            .unwrap_or(false)
    })
    .await;
    let pids: Vec<i32> = std::fs::read_to_string(&pid_file)
        .unwrap()
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect();
    let tool_pids = vec![pids[0], pids[1]];
    let _reaper = PidReaper(tool_pids.clone());
    assert_eq!(
        pids[2], pids[3],
        "tool and helper must share a process group"
    );
    assert!(
        tool_pids.iter().all(|pid| pid_alive(*pid)),
        "tool must be running pre-cancel"
    );
    let children = threads_of(&run.h, "toolchild").await;
    assert_eq!(children.len(), 1, "one child expected: {children:?}");
    let child_id = children[0].0.clone();

    cancel_and_prove(&run, &graph_id, &[child_id], &tool_pids).await;
}

/// (a) A cohort of two with a launch window of one: the first child is
/// mid-turn, the second was never started. One cancel stops the first and
/// cancels the second before launch.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_graph_run_stops_a_live_child_and_a_queued_child() {
    let mock = MockProvider::start_blocked(vec![MockResponse::Text("never served".into())]).await;
    let (mut run, fixture) = start_run(mock).await;
    plant_directive(run.project.path(), "hangchild", &[], &fixture.publisher).unwrap();
    plant_cohort_follow_graph(
        run.project.path(),
        "parent_cohort",
        "hangchild",
        &fixture.publisher,
    )
    .unwrap();

    let graph_id =
        launch_suspended_graph(&mut run, "parent_cohort", json!({"items": ["one", "two"]})).await;

    wait_for("the first child's provider request", 60, || async {
        !run.mock.captured_bodies().await.is_empty()
    })
    .await;
    wait_for("both cohort children to be admitted", 30, || async {
        threads_of(&run.h, "hangchild").await.len() == 2
    })
    .await;
    let children = threads_of(&run.h, "hangchild").await;
    let live: Vec<&(String, String)> = children
        .iter()
        .filter(|(_, status)| status == "running")
        .collect();
    let queued: Vec<&(String, String)> = children
        .iter()
        .filter(|(_, status)| status == "created")
        .collect();
    assert_eq!(live.len(), 1, "one child live: {children:?}");
    assert_eq!(queued.len(), 1, "one child queued: {children:?}");
    assert!(
        processes_of_thread(&queued[0].0).is_empty(),
        "the queued child must not have started"
    );
    let live_pids = processes_of_thread(&live[0].0);
    assert!(!live_pids.is_empty(), "the live child must be running");
    let _reaper = PidReaper(live_pids);
    let child_ids: Vec<String> = children.iter().map(|(id, _)| id.clone()).collect();

    cancel_and_prove(&run, &graph_id, &child_ids, &[]).await;
}
