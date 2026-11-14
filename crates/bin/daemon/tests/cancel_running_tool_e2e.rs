//! Cancel-probe e2e: cancelling a thread while a long-running subprocess
//! TOOL is mid-run must kill the tool's whole process group within seconds
//! and settle the thread `cancelled`, with the tool's streamed
//! `tool_streaming` frames already visible as `subprocess_output_observed`
//! events. No model spend: the directive's model is the FIFO mock provider.

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

const LAUNCH_ID: &str = "L-0123456789abcdef0123456789abcdef";

/// Plant the `model-providers/mock` config under
/// `<root>/.ai/config/ryeos-runtime/model-providers/mock.yaml`.
/// `auth: {}` keeps the adapter's `Authorization` header skipped
/// (see `crates/runtimes/directive/src/adapter.rs:38-43`).
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

fn register_mock_provider_bundle(
    state_path: &Path,
    mock_base_url: &str,
    fixture: &FastFixture,
) -> anyhow::Result<()> {
    register_config_fixture_bundle(
        state_path,
        "fixture-directive-model-config",
        fixture,
        |bundle_root| plant_mock_provider(bundle_root, mock_base_url, &fixture.publisher),
    )
}

/// Plant `model_routing` mapping `tier: general` to provider `mock`.
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

/// Plant a directive at `<root>/.ai/directives/<rel>.md`. The body
/// is whatever the LLM should be asked; the mock returns canned
/// responses irrespective of body content, but a non-empty body is
/// required by the directive kind's `composer_config.body` rule
/// (`required: true, expect_value_type: string`).
///
/// `execute_caps`, if non-empty, is rendered into the directive's
/// `requires.capabilities.declared:` list. The directive kind's
/// `composer_config.policy_facts[name=effective_caps]` reads
/// `[requires, capabilities, declared]` and surfaces the values as
/// `EnvelopePolicy.effective_caps` for the runtime's
/// `Harness::check_permission` and `Dispatcher::check_permission` to
/// gate tool calls.
fn plant_directive(
    root: &Path,
    rel_path: &str,
    body_text: &str,
    execute_caps: &[&str],
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = root.join(format!(".ai/directives/{rel_path}.md"));
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let permissions_block = if execute_caps.is_empty() {
        String::new()
    } else {
        let lines = execute_caps
            .iter()
            .map(|c| format!("      - \"{c}\"\n"))
            .collect::<String>();
        format!("requires:\n  capabilities:\n    declared:\n{lines}")
    };
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    let body = format!(
        r#"---
name: {stem}
category: "{dir_relative}"
description: "P3b directive-runtime e2e fixture"
inputs:
  - name: name
    type: string
    required: true
model:
  tier: general
{permissions_block}---
{body_text}
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Streaming tool: spawns a same-process-group child, records both pids,
/// optionally emits three tool_streaming frames, then sleeps ~10 minutes.
fn plant_hanging_streaming_tool(
    root: &Path,
    pid_file: &Path,
    emit_frames: bool,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/tools/cancelprobe");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "cancelprobe"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/tool_streaming"
#   description: "cancel probe: streams frames then hangs"

import json, os, struct, subprocess, sys, time

child = subprocess.Popen(["sleep", "600"])
with open({pid_file:?}, "w") as f:
    f.write(str(os.getpid()) + " " + str(child.pid) + " " + str(os.getpgid(0)) + " " + str(os.getpgid(child.pid)))

def emit(seq, data):
    body = json.dumps({{"seq": seq, "kind": "stdout", "data": data, "terminal": False}}).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(body)) + body)
    sys.stdout.buffer.flush()

if {emit_frames}:
    for i in range(3):
        emit(i, "progress %d\n" % i)
time.sleep(600)
"#,
        pid_file = pid_file.to_str().expect("utf-8 pid file"),
        emit_frames = if emit_frames { "True" } else { "False" },
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("cancelprobe.py"), signed)?;
    Ok(())
}

fn pid_alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        // State is the field after the ")" closing the comm; Z = zombie = dead.
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.trim().chars().next())
            .is_some_and(|state| state != 'Z'),
    }
}

async fn wait_for_pid_file(path: &Path) -> Vec<i32> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let pids: Vec<i32> = text
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            if pids.len() == 4 {
                return pids;
            }
        }
        assert!(Instant::now() < deadline, "tool never wrote its pid file");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn thread_status(h: &DaemonHarness, thread_id: &str) -> String {
    h.get_thread(thread_id)
        .await
        .expect("threads/get")
        .and_then(|v| {
            v.get("thread")
                .and_then(|t| t.get("status"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
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

async fn frame_event_count(h: &DaemonHarness, thread_id: &str) -> usize {
    h.chain_events(thread_id)
        .await
        .ok()
        .flatten()
        .map(|events| {
            events
                .iter()
                .filter(|(_, t, _)| t == "subprocess_output_observed")
                .count()
        })
        .unwrap_or(0)
}

/// Kills the probe's pids if an assertion fails, so a regression cannot leak
/// ten-minute sleepers.
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

async fn run_probe(emit_frames: bool) {
    let mock = MockProvider::start(vec![
        MockResponse::ToolCall {
            id: "c1".into(),
            name: "cancelprobe_cancelprobe".into(),
            arguments: "{}".into(),
        },
        MockResponse::Text("done".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();
    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            register_mock_provider_bundle(state_path, &mock_url, fixture)
        };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock + workflow bundles");

    let project = tempfile::tempdir().expect("project tempdir");
    let pid_dir = tempfile::tempdir().expect("pid tempdir");
    let pid_file = pid_dir.path().join("pids");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_hanging_streaming_tool(project.path(), &pid_file, emit_frames, &fixture.publisher)
        .expect("plant tool");
    plant_directive(
        project.path(),
        "test/cancel_probe",
        "Call the cancelprobe tool.",
        &["ryeos.execute.tool.*"],
        &fixture.publisher,
    )
    .expect("plant directive");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "directive:test/cancel_probe",
                "launch_id": LAUNCH_ID,
                "ref_bindings": { "model": "directive:test/cancel_probe" },
                "project_path": project.path().to_str().unwrap(),
                "parameters": {"name": "World"},
                "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_live(
                    ryeos_app::execution_policy::ExecutionResponse::Accepted
                ),
            }),
        )
        .await
        .expect("post /execute/launch");
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "body={body}");
    let thread_id = body
        .get("thread_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("accepted response missing thread_id: {body}"))
        .to_string();

    // The tool is mid-run once it has written its pids.
    let pids = wait_for_pid_file(&pid_file).await;
    let _reaper = PidReaper(vec![pids[0], pids[1]]);
    assert_eq!(
        pids[2], pids[3],
        "tool and child must share a process group"
    );

    let frames_before = if emit_frames {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let n = frame_event_count(&h, &thread_id).await;
            if n >= 3 {
                break n;
            }
            if Instant::now() >= deadline {
                let events = h.chain_events(&thread_id).await;
                let summary = match &events {
                    Ok(Some(events)) => events
                        .iter()
                        .map(|(t, ty, _)| format!("{t}:{ty}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    other => format!("{other:?}"),
                };
                let stderr = h.drain_stderr_nonblocking().await;
                panic!(
                    "streamed frames never became events (saw {n}); thread status={}; \
                     chain events=[{summary}]\n--- daemon stderr ---\n{stderr}",
                    thread_status(&h, &thread_id).await
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else {
        0
    };
    assert!(
        pid_alive(pids[0]) && pid_alive(pids[1]),
        "tool must be running pre-cancel"
    );

    let (cancel_status, cancel_body) = cancel_thread(&h, &thread_id).await;
    assert!(
        cancel_status.is_success(),
        "cancel returned {cancel_status}: {cancel_body}"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && (pid_alive(pids[0]) || pid_alive(pids[1])) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !pid_alive(pids[0]) && !pid_alive(pids[1]),
        "tool pid {} alive={} / child pid {} alive={} 15s after cancel",
        pids[0],
        pid_alive(pids[0]),
        pids[1],
        pid_alive(pids[1])
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut status = thread_status(&h, &thread_id).await;
    while status != "cancelled" && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        status = thread_status(&h, &thread_id).await;
    }
    assert_eq!(status, "cancelled", "thread must settle cancelled");

    if emit_frames {
        let after = frame_event_count(&h, &thread_id).await;
        assert!(
            after >= frames_before && frames_before >= 3,
            "pre-cancel frames must remain visible as events (before={frames_before}, after={after})"
        );
    }
    drop(mock);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_kills_running_streaming_tool_process_group() {
    run_probe(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_before_first_frame_kills_tool_process_group() {
    run_probe(false).await;
}
