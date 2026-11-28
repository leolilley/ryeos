//! V5.4 Phase 3b — directive-runtime end-to-end with mock LLM provider.
//!
//! These tests spawn the real `ryeosd` binary, register the
//! `bundles/directive` bundle (which ships
//! `runtime:directive-runtime` + the materializable
//! `bin/<host_triple>/ryeos-directive-runtime` binary in its CAS),
//! plant a directive + mock provider config, then exercise the full
//! HTTP `/execute` → daemon → directive-runtime → mock LLM round trip.
//!
//! P3b.1 — `common::mock_provider::MockProvider` (separate file).
//! P3b.2 — `e2e_directive_runtime_hello_world_succeeds` (this file).
//! P3b.3 — root semantics pin re-asserted vs real spawn (this file).
//! P3b.4 / P3b.5 — tool-call round-trip + cap-denial (follow-on).

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{
    FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use common::mock_provider::{MockProvider, MockResponse, MockToolCallSpec};
use lillux::crypto::SigningKey;

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

/// Plant a synth Python tool at `<root>/.ai/tools/<rel>.py`. The body
/// chains to the bundled `tool:ryeos/core/runtimes/python/script` runtime
/// so the daemon's subprocess terminator can actually execute it (we
/// reuse the dispatch_pin.rs::synth_tool_request pattern). The
/// directive-runtime's `bootstrap::scan_tools` walks
/// `<root>/.ai/tools/`, picks the file up via the loader's `tool` kind,
/// and registers it as `tool:<rel>.py` with the bare filename as the
/// LLM-visible tool name. Project-owned executable source is signed by the
/// fixture publisher so source-closure admission can retain its exact owner.
fn plant_python_echo_tool(root: &Path, rel: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let dir_relative = Path::new(rel)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(rel);
    let dir = root.join(format!(".ai/tools/{dir_relative}"));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{rel}.py"));
    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "{dir_relative}"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "P3b echo tool — prints its single arg back"

import json
import sys

# The daemon's python script runtime forwards the tool's `params` JSON
# on argv (or stdin, depending on the wrapper). We don't actually need
# the args for the round-trip pin — printing a known token is enough
# to confirm the runtime got us here and the tool result flowed back
# into the LLM context.
print(json.dumps({"echoed": "ok"}))
sys.exit(0)
"#
    .replace("{dir_relative}", dir_relative);
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(&path, signed)?;
    Ok(())
}

// ── P3b.2: Hello World e2e ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_runtime_hello_world_succeeds() {
    let mock = MockProvider::start(vec![MockResponse::Text("hello World".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_mock_provider_bundle(state_path, &mock_url, fixture)
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        // Bubble runtime tracing through to the daemon's stderr so a
        // hung directive-runtime child can be debugged from the test
        // panic message.
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock provider + workflow bundles");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(
        project.path(),
        "test/hello",
        "Say hello to ${inputs.name}.",
        &[],
        &fixture.publisher,
    )
    .expect("plant directive");
    let post_fut = h.post_execute(
        "directive:test/hello",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) = match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut)
        .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => panic!("post /execute failed: {e}"),
        Err(_) => {
            let stderr = h.drain_stderr_nonblocking().await;
            // Probe state dir for runtime exit + thread events
            let state = h.state_path.clone();
            // Ask the daemon, bounded, rather than opening its projection.
            let projection_dump =
                match tokio::time::timeout(std::time::Duration::from_secs(5), h.list_threads(10))
                    .await
                {
                    Ok(Ok(threads)) => format!("threads = {threads:#?}"),
                    Ok(Err(error)) => format!("threads/list error: {error:#}"),
                    Err(_) => "threads/list timed out".into(),
                };
            panic!(
                "POST /execute timed out after 30s — directive-runtime hung.\n\
                 --- daemon stderr ---\n{stderr}\n\
                 --- projection ---\n{projection_dump}\n\
                 state_path={}",
                state.display()
            );
        }
    };

    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "expected 200 OK from directive-runtime hello world; got {status}\nbody={body:#}\n--- daemon stderr ---\n{stderr}"
        );
    }

    let result = match body.get("result").cloned() {
        Some(r) => r,
        None => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "response missing `result` envelope\nbody={body:#}\n--- daemon stderr ---\n{stderr}"
            );
        }
    };
    if result.get("success").and_then(|v| v.as_bool()) != Some(true) {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!("result.success must be true\nbody={body:#}\n--- daemon stderr ---\n{stderr}");
    }

    let result_text = result
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        result_text.contains("hello World"),
        "terminal text must contain mock provider's `hello World`; got result_text={result_text:?}, full body={body:#}"
    );

    // Defense in depth: any callback drift surfaced via
    // `record_callback_warning` would land in `result.warnings`; if
    // the runtime ever starts dropping events the assertion can be
    // tightened to `warnings.is_empty()`. Today we just require the
    // field exists (post-launch.rs P3b extension).
    assert!(
        result.get("warnings").is_some(),
        "result envelope must surface `warnings` (extended in launch.rs for P3b); got: {body:#}"
    );

    // The body template substitutes the input: the provider sees the
    // rendered prompt, not the `${inputs.name}` expression or an appended
    // inputs block.
    let bodies = mock.captured_bodies().await;
    assert_eq!(bodies.len(), 1, "exactly one provider request: {bodies:#?}");
    let user_text = bodies[0]["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["role"] == "user")
                .and_then(|message| message["content"].as_str())
        })
        .unwrap_or_default();
    assert_eq!(
        user_text.trim_end(),
        "Say hello to World.",
        "the directive body must render its input; got request {:#}",
        bodies[0]
    );

    drop(project);
    drop(mock);
}

// ── P3b.3: root semantics pin against the REAL directive-runtime spawn ─
//
// P1.6 already pinned the root/runtime split using a fixture runtime
// whose binary doesn't exist (the dispatcher falls through to
// `build_and_launch` which creates the thread row before failing at
// materialization). This re-pin uses the REAL spawn + REAL
// directive-runtime binary so a regression in the RootSubject
// plumbing — only visible after the runtime actually finalizes the
// thread — will surface here.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_runtime_thread_records_subject_not_runtime() {
    let mock = MockProvider::start(vec![MockResponse::Text("hi P3b.3".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_mock_provider_bundle(state_path, &mock_url, fixture)
    };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(
        project.path(),
        "p3b3/subject",
        "irrelevant — mock returns canned text",
        &[],
        &fixture.publisher,
    )
    .expect("plant directive");
    let (status, body) = h
        .post_execute(
            "directive:p3b3/subject",
            project.path().to_str().unwrap(),
            serde_json::json!({"name": "x"}),
        )
        .await
        .expect("post /execute");

    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "expected 200 from successful spawn; got {status}: {body:#}"
    );

    // Confirm, through `service:threads/list`, that the thread row carries
    // the SUBJECT identity (`directive_run` / `directive:p3b3/subject`), not
    // the executor runtime's identity. The test principal requested it.
    let mut threads = Vec::new();
    for _ in 0..40 {
        threads = h.list_threads(100).await.expect("threads/list");
        if threads
            .iter()
            .any(|t| t.item_ref == "directive:p3b3/subject")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let subject = threads
        .iter()
        .find(|t| t.item_ref == "directive:p3b3/subject")
        .unwrap_or_else(|| {
            panic!(
                "no thread row for directive:p3b3/subject — root/runtime split regressed. \
                 All rows: {threads:#?}"
            )
        });

    assert_eq!(
        subject.kind, "directive_run",
        "thread.kind must be the SUBJECT's thread_profile (`directive_run`), not the runtime's \
         (`runtime_run`); got: {subject:#?}"
    );
    assert_eq!(
        subject.item_ref, "directive:p3b3/subject",
        "thread.item_ref must echo the user-typed directive ref; got: {subject:#?}"
    );
    // The executor ref is not part of the list row; read the thread itself.
    let subject_detail = h
        .get_thread(&subject.thread_id)
        .await
        .expect("threads/get")
        .expect("subject thread is visible to its requester");
    let executor_ref = subject_detail["thread"]["executor_ref"]
        .as_str()
        .unwrap_or_default();
    assert!(
        executor_ref.starts_with("native:"),
        "thread.executor_ref records the native runtime executor; got: {:?}",
        executor_ref
    );

    let runtime_rows: Vec<_> = threads
        .iter()
        .filter(|t| t.item_ref.starts_with("runtime:"))
        .collect();
    assert!(
        runtime_rows.is_empty(),
        "no thread row should be recorded against the runtime ref (subject must win the audit); \
         got: {runtime_rows:#?}"
    );

    drop(project);
    drop(mock);
}

// ── P3b.4: Tool-call round-trip ────────────────────────────────────────
//
// Pin the full agent loop with tool dispatch:
//   turn 1: provider returns tool_calls[echo(...)] → runner dispatches
//           via callback.dispatch_action → daemon → (subprocess attempt)
//           → tool_result message pushed back into the conversation
//   turn 2: provider returns plain text "got pong" → finalize
//
// The test does NOT assert the tool's *output* — only that the second
// LLM turn happened and produced the canned text. That is the surface
// the runner contract guarantees; whether the daemon-side subprocess
// actually executed (and what it produced) is a daemon-dispatch
// concern covered by `dispatch_pin.rs`. What we ARE pinning here is
// that the directive-runtime can complete a multi-turn dialogue
// involving a tool_calls turn without hanging or short-circuiting
// finalization on the first turn (which was the V5.4 P2.x bug class).

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_runtime_tool_call_round_trip() {
    let mock = MockProvider::start(vec![
        MockResponse::ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: r#"{"msg":"pong"}"#.into(),
        },
        MockResponse::Text("got pong".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_mock_provider_bundle(state_path, &mock_url, fixture)
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock + workflow bundles + echo tool");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_python_echo_tool(project.path(), "echo", &fixture.publisher).expect("plant echo tool");
    plant_directive(
        project.path(),
        "test/round_trip",
        "Call the echo tool, then summarise.",
        &["ryeos.execute.tool.*"],
        &fixture.publisher,
    )
    .expect("plant directive");
    let post_fut = h.post_execute(
        "directive:test/round_trip",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!(
                    "POST /execute timed out after 30s — directive-runtime hung mid-loop.\n\
                 --- daemon stderr ---\n{stderr}"
                );
            }
        };

    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "expected 200 OK from tool-round-trip directive; got {status}\nbody={body:#}\n\
             --- daemon stderr ---\n{stderr}"
        );
    }

    let result = body
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("response missing `result` envelope; body={body:#}"));
    if result.get("success").and_then(|v| v.as_bool()) != Some(true) {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "result.success must be true after tool round-trip\nbody={body:#}\n\
             --- daemon stderr ---\n{stderr}"
        );
    }
    let result_text = result
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        result_text.contains("got pong"),
        "second-turn assistant text must surface in result.result \
         (proves both LLM turns went through the loop); got={result_text:?}, body={body:#}"
    );

    drop(project);
    drop(mock);
}

// ── P3b.5: Cap denial fails cleanly ────────────────────────────────────
//
// The directive declares a `declared` cap that does NOT
// match the tool the LLM tries to invoke. The runner's
// `DispatchingTools` state catches this BEFORE any
// `callback.dispatch_action` call: it pushes a synthetic
// `{"error": "permission denied: <tool>"}` tool_result message and
// continues the loop. The mock's second response then closes the
// conversation with a graceful acknowledgement.
//
// "Fails cleanly" here means: HTTP stays 200, the runtime completes
// (no panic, no daemon 500, no provider exhaustion), the LLM-visible
// permission denial appears as a final-turn assistant text. This pins
// today's self-policing behaviour: cap denial is a CONVERSATION
// signal, not a runtime crash. If a future change wants to make cap
// denials hard-fail the directive, this test will catch the silent
// drift.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_with_unauthorized_tool_call_fails_cleanly() {
    let mock = MockProvider::start(vec![
        MockResponse::ToolCall {
            id: "denied-1".into(),
            name: "echo".into(),
            arguments: r#"{"msg":"nope"}"#.into(),
        },
        MockResponse::Text("acknowledged: permission denied for echo".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_mock_provider_bundle(state_path, &mock_url, fixture)
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock + non-matching cap");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_python_echo_tool(project.path(), "echo", &fixture.publisher).expect("plant echo tool");
    plant_directive(
        project.path(),
        "test/denied",
        "Try to call echo; you should be denied.",
        &["ryeos.execute.tool.allowed_only"],
        &fixture.publisher,
    )
    .expect("plant directive");
    let post_fut = h.post_execute(
        "directive:test/denied",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "X"}),
    );
    let (status, body) = match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut)
        .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => panic!("post /execute failed: {e}"),
        Err(_) => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "POST /execute timed out after 30s — denial path hung instead of failing cleanly.\n\
                 --- daemon stderr ---\n{stderr}"
            );
        }
    };

    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "cap denial must produce 200 (in-protocol) — NOT a daemon-side 500. \
         body={body:#}"
    );

    let result = body
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("response missing `result` envelope; body={body:#}"));

    // Runner self-corrects: the LLM saw the synthetic permission-denied
    // tool_result and the second mock turn closes the conversation
    // gracefully. Status MUST be `completed` — anything else (errored,
    // cancelled) means the runner short-circuited instead of letting
    // the model handle the denial.
    assert_eq!(
        result.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "cap denial must NOT crash the directive — the runner is supposed to surface the \
         denial to the LLM as a tool_result and continue. body={body:#}"
    );
    let result_text = result
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        result_text.contains("permission denied"),
        "final assistant text must reflect the denial that the LLM saw mid-conversation; \
         got result_text={result_text:?}, body={body:#}"
    );

    drop(project);
    drop(mock);
}

// ── operator follow-up continuation launches + completes the successor ──
//
// A directive carries no per-item `executor_id`; its launch identity is the
// serving runtime's `native:<binary>`, captured into the resume context. This
// pins that an operator follow-up reconstructs that identity, spawns the
// successor, and runs it to completion: the successor reaches `completed`,
// braids onto the predecessor, and runs a second LLM turn. State-store tests
// only prove the successor ROW is created; this e2e exercises the actual
// successor spawn + run.

/// Poll `service:threads/list` for the first thread matching `pred`,
/// optionally waiting until it reaches a terminal status. Returns `None` if it
/// never appears. The test principal requests every thread these tests poll
/// for, so the principal filter hides none of them.
async fn poll_thread(
    h: &DaemonHarness,
    pred: impl Fn(&common::ListedThread) -> bool,
    require_terminal: bool,
) -> Option<common::ListedThread> {
    for _ in 0..120 {
        if let Ok(threads) = h.list_threads(200).await
            && let Some(t) = threads.into_iter().find(|t| pred(t))
        {
            let terminal = matches!(
                t.status.as_str(),
                "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
            );
            if !require_terminal || terminal {
                return Some(t);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_operator_follow_up_successor_completes() {
    let mock = MockProvider::start(vec![
        MockResponse::Text("turn one".into()),
        MockResponse::Text("turn two".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_mock_provider_bundle(state_path, &mock_url, fixture)
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock provider + workflow bundles");

    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project.path().to_str().unwrap().to_string();
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(
        project.path(),
        "cont/dir",
        "Answer ${inputs.name}.",
        &[],
        &fixture.publisher,
    )
    .expect("plant directive");

    // Turn one: launch the directive synchronously to completion.
    let (status, body) = h
        .post_execute(
            "directive:cont/dir",
            &project_path,
            serde_json::json!({"name": "World"}),
        )
        .await
        .expect("post /execute (turn one)");
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "turn one must succeed: {body:#}"
    );

    // The settled directive thread (no upstream).
    let first = poll_thread(
        &h,
        |t| t.item_ref == "directive:cont/dir" && t.upstream_thread_id.is_none(),
        true,
    )
    .await
    .expect("first directive thread reaches a terminal status");
    let first_id = first.thread_id.clone();

    // Operator follow-up via the threads.input service → creates AND launches a
    // continuation successor. The service result rides inside the /execute
    // envelope under `result`.
    let (status, body) = h
        .post_execute(
            "service:threads/input",
            &project_path,
            serde_json::json!({
                "input": "continue",
                "target": {
                    "kind": "thread",
                    "thread_id": first_id.clone(),
                },
            }),
        )
        .await
        .expect("post /execute (service:threads/input)");
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "threads.input follow-up must be accepted: {body:#}"
    );
    let successor_id = body
        .pointer("/result/thread_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("threads.input result missing successor thread_id: {body:#}"))
        .to_string();
    assert_ne!(successor_id, first_id, "successor must be a new thread");

    // The pin: the successor must actually launch and reach `completed` — its
    // runtime launch identity reconstructed from the captured resume context.
    let successor = poll_thread(&h, |t| t.thread_id == successor_id, true)
        .await
        .unwrap_or_else(|| panic!("successor {successor_id} never reached a terminal status"));
    assert_eq!(
        successor.upstream_thread_id.as_deref(),
        Some(first_id.as_str()),
        "successor must braid onto the first thread: {successor:#?}"
    );
    if successor.status != "completed" {
        let stderr = h.drain_stderr_nonblocking().await;
        let detail = h
            .post_execute(
                "service:threads/get",
                &project_path,
                serde_json::json!({"thread_id": successor_id}),
            )
            .await
            .ok();
        panic!(
            "successor must reach `completed`; a non-completed status means launch \
             reconstruction failed to resolve the runtime executor identity.\n\
             row={successor:#?}\ndetail={detail:#?}\n--- daemon stderr ---\n{stderr}"
        );
    }

    // Corroborate the second LLM turn actually ran (mock's `turn two`).
    let (_s, detail) = h
        .post_execute(
            "service:threads/get",
            &project_path,
            serde_json::json!({"thread_id": successor_id}),
        )
        .await
        .expect("threads.get successor");
    assert!(
        detail.to_string().contains("turn two"),
        "successor result must surface the mock's second-turn text `turn two`: {detail:#}"
    );

    drop(project);
    drop(mock);
}

// ── Thread title: stamped with the thread by `threads/input` ───────────

/// A directive whose single required input is the operator text a
/// `threads/input` launch binds as `input`.
fn plant_input_directive(root: &Path, rel_path: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let path = root.join(format!(".ai/directives/{rel_path}.md"));
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let body = format!(
        r#"---
name: {rel_path}
category: ""
description: "thread title e2e fixture"
inputs:
  - name: input
    type: string
    required: true
model:
  tier: general
---
Respond to: ${{inputs.input}}
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

async fn input_post(
    h: &DaemonHarness,
    project_path: &str,
    body: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    h.post_execute("service:threads/input", project_path, body)
        .await
        .expect("post /execute (service:threads/input)")
}

async fn title_of(h: &DaemonHarness, thread_id: &str) -> Option<String> {
    h.get_thread(thread_id)
        .await
        .expect("threads/get")
        .expect("thread is known")
        .pointer("/facets/title")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_threads_input_stamps_a_title_with_the_thread() {
    let mock = MockProvider::start(vec![
        MockResponse::Text("one".into()),
        MockResponse::Text("two".into()),
        MockResponse::Text("three".into()),
        MockResponse::Text("four".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_mock_provider_bundle(state_path, &mock_url, fixture)
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with mock provider + workflow bundles");
    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project.path().to_str().unwrap().to_string();
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_input_directive(project.path(), "title/dir", &fixture.publisher)
        .expect("plant directive");
    let fresh = |input: &str, title: Option<&str>| {
        let mut body = serde_json::json!({
            "input": input,
            "target": {
                "kind": "fresh",
                "item_ref": "directive:title/dir",
                "project_path": project_path.clone(),
                "ref_bindings": {},
            },
        });
        if let Some(title) = title {
            body["title"] = serde_json::json!(title);
        }
        body
    };

    // Refusals leave no thread behind: over-length, blank and control-character
    // titles are bad requests.
    for bad in ["x".repeat(161), "   ".to_string(), "a\nb".to_string()] {
        let (status, body) = input_post(&h, &project_path, fresh("hello", Some(&bad))).await;
        assert_eq!(
            status,
            reqwest::StatusCode::BAD_REQUEST,
            "title {bad:?} must be refused: {body:#}"
        );
    }
    assert!(
        h.list_threads(50)
            .await
            .expect("threads/list")
            .iter()
            .all(|t| t.item_ref != "directive:title/dir"),
        "a refused title must not launch a thread"
    );

    // Fresh launch without a title: derived from the first non-empty line.
    let (status, body) = input_post(
        &h,
        &project_path,
        fresh("\n  Plan   the trip  \nsecond line", None),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:#}");
    let first = body
        .pointer("/result/thread_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("no thread_id: {body:#}"))
        .to_string();
    assert_eq!(title_of(&h, &first).await.as_deref(), Some("Plan the trip"));
    // The stamp is an event of the thread itself, so it survives a rebuild.
    let events = h
        .thread_events(&first)
        .await
        .expect("events/replay")
        .expect("thread is known");
    assert!(
        events.iter().any(|(kind, payload)| {
            kind == "thread_facet_set"
                && payload["key"] == "title"
                && payload["value"] == "Plan the trip"
        }),
        "the title must be a thread_facet_set event: {events:#?}"
    );
    // The list row carries it as `facets.title`.
    let (_, listed) = h
        .post_execute(
            "service:threads/list",
            ".",
            serde_json::json!({ "limit": 50 }),
        )
        .await
        .expect("threads/list");
    let row = listed
        .pointer("/result/threads")
        .and_then(|v| v.as_array())
        .and_then(|rows| rows.iter().find(|row| row["thread_id"] == first.as_str()))
        .unwrap_or_else(|| panic!("first thread not listed: {listed:#}"));
    assert_eq!(row["facets"]["title"], "Plan the trip");
    poll_thread(&h, |t| t.thread_id == first, true)
        .await
        .expect("first thread settles");

    // A continuation inherits its predecessor's title.
    let (status, body) = input_post(
        &h,
        &project_path,
        serde_json::json!({
            "input": "and then what",
            "target": {"kind": "thread", "thread_id": first.clone()},
        }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:#}");
    let second = completed_successor(&mut h, &body, &first).await;
    assert_eq!(
        title_of(&h, &second).await.as_deref(),
        Some("Plan the trip")
    );

    // A supplied title wins over the inherited one, and is checked on this
    // path too.
    let (status, _) = input_post(
        &h,
        &project_path,
        serde_json::json!({
            "input": "rename",
            "title": "y".repeat(161),
            "target": {"kind": "thread", "thread_id": second.clone()},
        }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    let (status, body) = input_post(
        &h,
        &project_path,
        serde_json::json!({
            "input": "rename",
            "title": "  Packing list ",
            "target": {"kind": "thread", "thread_id": second.clone()},
        }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:#}");
    let third = completed_successor(&mut h, &body, &second).await;
    assert_eq!(title_of(&h, &third).await.as_deref(), Some("Packing list"));
    // The predecessors keep theirs.
    assert_eq!(title_of(&h, &first).await.as_deref(), Some("Plan the trip"));

    // An explicit title on a fresh launch overrides the derived one.
    let (status, body) = input_post(&h, &project_path, fresh("ignored", Some("Chosen"))).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:#}");
    let fourth = body
        .pointer("/result/thread_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("no thread_id: {body:#}"))
        .to_string();
    assert_eq!(title_of(&h, &fourth).await.as_deref(), Some("Chosen"));

    drop(project);
    drop(mock);
}

// ── Operator rebind: continue a thread on a different model binding ────
//
// The directive carries no model of its own; the launch binds one at the
// `model` ref binding. An operator continuation may bind a different model
// directive there. Two mock providers stand behind two bindings, so which
// provider a turn reaches shows which binding its thread runs with.

/// A priced mock provider under `model-providers/<id>.yaml`. The signed
/// tariff makes every attempt a ledger row, which is how the test reads the
/// budget scope each thread charges.
fn plant_rebind_provider(
    root: &Path,
    id: &str,
    base_url: &str,
    auth: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"family: chat_completions
transport:
  kind: remote_http
  base_url: "{base_url}"
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {auth}
headers: {{}}
schemas:
  streaming:
    mode: delta_merge
  output_limit: {{path: max_tokens, semantics: provider_native_output_tokens}}
pricing:
  input_per_million: "2"
  output_per_million: "10"
spend_authority:
  billing_principal: e2e-{id}
  credential_authority_generation: gen-1
  pricing_contract_subject: e2e-{id}-2026-07
  tariff:
    schema_version: 1
    currency: usd
    pricing_generation: e2e-2026-07
    input_per_million: "2"
    output_per_million: "10"
    covered_dimensions: [input_tokens, output_tokens]
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join(format!("{id}.yaml")), signed)?;
    Ok(())
}

/// A model binding directive: nothing but an explicit model target.
fn plant_model_binding(
    root: &Path,
    name: &str,
    provider: &str,
    model: &str,
    context_window: u64,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = root.join(format!(".ai/directives/rebind/models/{name}.md"));
    std::fs::create_dir_all(path.parent().expect("binding parent dir"))?;
    let body = format!(
        r#"---
description: "rebind e2e model binding"
version: "1.0.0"
model:
  provider: {provider}
  name: {model}
  context_window: {context_window}
---
Model binding fixture `{name}`.
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// The directive the threads run: no `model` block, so its model comes only
/// from the `model` ref binding, and a hard spend limit so the ledger
/// accounts every provider attempt against one execution budget.
fn plant_unmodelled_directive(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let path = root.join(".ai/directives/rebind/turn.md");
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let body = r#"---
name: turn
category: "rebind"
description: "rebind e2e directive"
inputs:
  - name: name
    type: string
    required: true
limits:
  spend_usd: "5"
---
Answer ${inputs.name}.
"#;
    let signed = lillux::signature::sign_content(body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Every string a provider request body carries in its `messages`.
fn request_message_text(body: &serde_json::Value) -> String {
    body["messages"].to_string()
}

async fn rebind_input(
    h: &DaemonHarness,
    project_path: &str,
    thread_id: &str,
    input: &str,
    rebind: Option<serde_json::Value>,
) -> (reqwest::StatusCode, serde_json::Value) {
    let mut target = serde_json::json!({"kind": "thread", "thread_id": thread_id});
    if let Some(rebind) = rebind {
        target["rebind"] = rebind;
    }
    h.post_execute(
        "service:threads/input",
        project_path,
        serde_json::json!({"input": input, "target": target}),
    )
    .await
    .expect("post /execute (service:threads/input)")
}

/// Threads continued from `source`, as the daemon lists them.
async fn successors_of(h: &DaemonHarness, source: &str) -> Vec<String> {
    h.list_threads(200)
        .await
        .expect("threads/list")
        .into_iter()
        .filter(|thread| thread.upstream_thread_id.as_deref() == Some(source))
        .map(|thread| thread.thread_id)
        .collect()
}

async fn completed_successor(
    h: &mut DaemonHarness,
    body: &serde_json::Value,
    source: &str,
) -> String {
    assert_eq!(
        body.pointer("/result/delivery").and_then(|v| v.as_str()),
        Some("launched"),
        "the continuation must launch a successor: {body:#}"
    );
    let successor_id = body
        .pointer("/result/thread_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("threads.input result has no successor thread_id: {body:#}"))
        .to_string();
    let successor = poll_thread(h, |t| t.thread_id == successor_id, true)
        .await
        .unwrap_or_else(|| panic!("successor {successor_id} never reached a terminal status"));
    if successor.status != "completed" {
        let stderr = h.drain_stderr_nonblocking().await;
        let detail = h.get_thread(&successor_id).await.ok();
        panic!(
            "successor must reach `completed`.\nrow={successor:#?}\ndetail={detail:#?}\n--- daemon stderr ---\n{stderr}"
        );
    }
    assert_eq!(successor.upstream_thread_id.as_deref(), Some(source));
    successor_id
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_operator_rebind_continues_on_another_model() {
    const FIRST: &str = "directive:rebind/models/first";
    const SECOND: &str = "directive:rebind/models/second";

    let mock_one =
        MockProvider::start(vec![MockResponse::Text("answer from model one".into())]).await;
    let mock_two = MockProvider::start(vec![
        MockResponse::Text("answer from model two".into()),
        MockResponse::Text("second answer from model two".into()),
        MockResponse::Text("third answer from model two".into()),
        MockResponse::Text("fourth answer from model two".into()),
    ])
    .await;
    let url_one = mock_one.base_url.clone();
    let url_two = mock_two.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-rebind-model-config",
            fixture,
            |bundle_root| {
                plant_rebind_provider(bundle_root, "mockone", &url_one, "{}", &fixture.publisher)?;
                plant_rebind_provider(bundle_root, "mocktwo", &url_two, "{}", &fixture.publisher)?;
                // A provider whose credential this node does not hold.
                plant_rebind_provider(
                    bundle_root,
                    "mockkeyed",
                    &url_two,
                    "{env_var: ZEN_API_KEY, header_name: Authorization}",
                    &fixture.publisher,
                )
            },
        )
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env_remove("ZEN_API_KEY");
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with two mock providers");

    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project.path().to_str().unwrap().to_string();
    let signer = &fixture.publisher;
    plant_unmodelled_directive(project.path(), signer).expect("plant directive");
    plant_model_binding(
        project.path(),
        "first",
        "mockone",
        "model-one",
        200_000,
        signer,
    )
    .expect("plant first binding");
    plant_model_binding(
        project.path(),
        "second",
        "mocktwo",
        "model-two",
        200_000,
        signer,
    )
    .expect("plant second binding");
    plant_model_binding(
        project.path(),
        "third",
        "mocktwo",
        "model-three",
        200_000,
        signer,
    )
    .expect("plant third binding");
    // A project-space binding signed by a key this node does not trust.
    plant_model_binding(
        project.path(),
        "rogue",
        "mocktwo",
        "model-rogue",
        200_000,
        &SigningKey::from_bytes(&[77; 32]),
    )
    .expect("plant rogue binding");
    // Below the token budget a successor's conversation starts from.
    plant_model_binding(
        project.path(),
        "tiny",
        "mocktwo",
        "model-tiny",
        2_000,
        signer,
    )
    .expect("plant tiny binding");
    plant_model_binding(
        project.path(),
        "keyed",
        "mockkeyed",
        "model-keyed",
        200_000,
        signer,
    )
    .expect("plant keyed binding");

    // Turn one: launch on the first model binding and run to completion.
    let (status, body) = h
        .post_json(
            "/execute",
            serde_json::json!({
                "item_ref": "directive:rebind/turn",
                "ref_bindings": {"model": FIRST},
                "project_path": project_path,
                "parameters": {"name": "World"},
                "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_live(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                ),
            }),
        )
        .await
        .expect("post /execute (turn one)");
    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!("turn one must succeed: {status} {body:#}\n--- daemon stderr ---\n{stderr}");
    }
    let first = poll_thread(
        &h,
        |t| t.item_ref == "directive:rebind/turn" && t.upstream_thread_id.is_none(),
        true,
    )
    .await
    .expect("first thread reaches a terminal status");
    assert_eq!(first.status, "completed", "turn one: {first:#?}");
    let first_id = first.thread_id.clone();
    assert_eq!(mock_one.captured_bodies().await.len(), 1);
    assert!(mock_two.captured_bodies().await.is_empty());

    // Refusals. Each is decided before any successor exists, so the thread
    // stays continuable and no provider is contacted.
    for (rebind, expected) in [
        // The bound model's context window is below the runtime contract's
        // signed minimum for a rebind.
        (
            serde_json::json!({"model": "directive:rebind/models/tiny"}),
            "ref_binding_rebind_fact_below_minimum",
        ),
        // The admitted runtime contract declares no such rebindable binding.
        (
            serde_json::json!({"extra": SECOND}),
            "ref_binding_rebind_not_permitted",
        ),
        // The new provider's credential is missing on this node.
        (
            serde_json::json!({"model": "directive:rebind/models/keyed"}),
            "ZEN_API_KEY",
        ),
    ] {
        let (status, body) = rebind_input(
            &h,
            &project_path,
            &first_id,
            "continue",
            Some(rebind.clone()),
        )
        .await;
        let text = format!("{status} {body}");
        assert_ne!(
            status,
            reqwest::StatusCode::OK,
            "rebind {rebind} must be refused: {text}"
        );
        assert!(
            text.contains(expected),
            "rebind {rebind} must be refused with `{expected}`: {text}"
        );
        assert!(
            successors_of(&h, &first_id).await.is_empty(),
            "a refused rebind {rebind} must not create a successor"
        );
    }
    // A project-space binding whose signer is not trusted is refused.
    let (status, body) = rebind_input(
        &h,
        &project_path,
        &first_id,
        "continue",
        Some(serde_json::json!({"model": "directive:rebind/models/rogue"})),
    )
    .await;
    assert_ne!(
        status,
        reqwest::StatusCode::OK,
        "a binding signed by an untrusted key must be refused: {body:#}"
    );
    assert!(successors_of(&h, &first_id).await.is_empty());
    // A binding that does not resolve is refused as well.
    let (status, body) = rebind_input(
        &h,
        &project_path,
        &first_id,
        "continue",
        Some(serde_json::json!({"model": "directive:rebind/models/absent"})),
    )
    .await;
    assert_ne!(
        status,
        reqwest::StatusCode::OK,
        "an unresolvable binding must be refused: {body:#}"
    );
    assert!(successors_of(&h, &first_id).await.is_empty());
    assert_eq!(mock_one.captured_bodies().await.len(), 1);
    assert!(mock_two.captured_bodies().await.is_empty());

    // The operator continues the thread on the second model binding.
    let (status, body) = rebind_input(
        &h,
        &project_path,
        &first_id,
        "continue on the second model",
        Some(serde_json::json!({"model": SECOND})),
    )
    .await;
    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!("the rebind must be accepted: {status} {body:#}\n--- daemon stderr ---\n{stderr}");
    }
    assert_eq!(
        body.pointer("/result/rebind"),
        Some(&serde_json::json!({"applied": true, "names": ["model"]})),
        "{body:#}"
    );
    let second_id = completed_successor(&mut h, &body, &first_id).await;

    // The successor called the second provider, and only it.
    assert_eq!(mock_one.captured_bodies().await.len(), 1);
    let two = mock_two.captured_bodies().await;
    assert_eq!(
        two.len(),
        1,
        "the rebound successor calls model two: {two:#?}"
    );
    assert_eq!(two[0]["model"], "model-two", "{:#}", two[0]);
    // The conversation is carried: model two sees turn one's input and
    // answer as well as the new input.
    let carried = request_message_text(&two[0]);
    for expected in [
        "Answer World.",
        "answer from model one",
        "continue on the second model",
    ] {
        assert!(
            carried.contains(expected),
            "model two's request must carry `{expected}`: {carried}"
        );
    }

    // The daemon recorded the rebind in the successor's own events.
    let events = h
        .thread_events(&second_id)
        .await
        .expect("replay successor events")
        .expect("successor events");
    let rebound: Vec<&serde_json::Value> = events
        .iter()
        .filter(|(event_type, _)| event_type == "launch_binding_rebound")
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(rebound.len(), 1, "one rebind record: {events:#?}");
    let payload = rebound[0];
    assert_eq!(
        payload["source_thread_id"],
        first_id.as_str(),
        "{payload:#}"
    );
    assert_eq!(
        payload["changes"],
        serde_json::json!([{"input": "ref_binding", "name": "model", "from": FIRST, "to": SECOND}]),
        "{payload:#}"
    );
    assert_eq!(
        payload["from_facts"]["model_name"], "model-one",
        "{payload:#}"
    );
    assert_eq!(
        payload["to_facts"]["model_name"], "model-two",
        "{payload:#}"
    );
    assert_eq!(payload["to_facts"]["provider_id"], "mocktwo", "{payload:#}");
    // Only the facts the runtime contract marks for the record are written:
    // no request URL and no configuration source reaches the thread's events.
    for side in ["from_facts", "to_facts"] {
        let recorded: Vec<&str> = payload[side]
            .as_object()
            .unwrap_or_else(|| panic!("{side} is an object: {payload:#}"))
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            recorded,
            ["context_window", "model_name", "provider_id"],
            "{side}: {payload:#}"
        );
    }

    // The bound item is edited after the rebind: it now names model one.
    // Nothing that follows may resolve it again.
    plant_model_binding(
        project.path(),
        "second",
        "mockone",
        "model-edited",
        200_000,
        signer,
    )
    .expect("edit second binding");

    // The same rebind submitted again returns the successor it already
    // produced, without preparing anything.
    let (status, body) = rebind_input(
        &h,
        &project_path,
        &first_id,
        "continue on the second model",
        Some(serde_json::json!({"model": SECOND})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "retried rebind: {body:#}");
    assert_eq!(
        body.pointer("/result/delivery").and_then(|v| v.as_str()),
        Some("launched"),
        "{body:#}"
    );
    assert_eq!(
        body.pointer("/result/thread_id").and_then(|v| v.as_str()),
        Some(second_id.as_str()),
        "a retried rebind names the same successor: {body:#}"
    );
    assert_eq!(
        body.pointer("/result/rebind"),
        Some(&serde_json::json!({"applied": true, "names": ["model"]})),
        "{body:#}"
    );
    assert_eq!(successors_of(&h, &first_id).await, vec![second_id.clone()]);
    assert_eq!(mock_one.captured_bodies().await.len(), 1);
    assert_eq!(mock_two.captured_bodies().await.len(), 1);

    // A follow-up with no rebind stays on the new model.
    let (status, body) = rebind_input(&h, &project_path, &second_id, "and once more", None).await;
    assert_eq!(status, reqwest::StatusCode::OK, "plain follow-up: {body:#}");
    assert!(
        body.pointer("/result/rebind").is_none(),
        "a request with no rebind reports none: {body:#}"
    );
    let third_id = completed_successor(&mut h, &body, &second_id).await;
    assert_eq!(mock_one.captured_bodies().await.len(), 1);
    let two = mock_two.captured_bodies().await;
    assert_eq!(two.len(), 2, "the plain follow-up stays on model two");
    assert_eq!(two[1]["model"], "model-two");
    assert!(request_message_text(&two[1]).contains("answer from model two"));
    let third_events = h
        .thread_events(&third_id)
        .await
        .expect("replay third events")
        .expect("third events");
    assert!(
        third_events
            .iter()
            .all(|(event_type, _)| event_type != "launch_binding_rebound"),
        "an inherited binding is not a rebind"
    );

    // Naming the binding the thread already runs with changes nothing: the
    // ordinary continuation runs and reports the rebind as not applied.
    let (status, body) = rebind_input(
        &h,
        &project_path,
        &third_id,
        "same model again",
        Some(serde_json::json!({"model": SECOND})),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "unchanged rebind: {body:#}"
    );
    assert_eq!(
        body.pointer("/result/rebind"),
        Some(&serde_json::json!({"applied": false, "reason": "unchanged"})),
        "{body:#}"
    );
    let fourth_id = completed_successor(&mut h, &body, &third_id).await;
    assert_eq!(mock_two.captured_bodies().await.len(), 3);
    assert_eq!(mock_one.captured_bodies().await.len(), 1);

    // The first thread already continued on model two. Continuing it again
    // with another choice is refused and names the existing successor.
    let (status, body) = rebind_input(
        &h,
        &project_path,
        &first_id,
        "continue on the second model",
        Some(serde_json::json!({"model": "directive:rebind/models/third"})),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "a conflicting continuation is answered, not failed: {body:#}"
    );
    assert_eq!(
        body.pointer("/result/delivery").and_then(|v| v.as_str()),
        Some("refused"),
        "a second continuation of the same thread with another choice is refused: {body:#}"
    );
    assert_eq!(
        body.pointer("/result/thread_id").and_then(|v| v.as_str()),
        Some(second_id.as_str()),
        "{body:#}"
    );
    assert_eq!(
        body.pointer("/result/rebind"),
        Some(&serde_json::json!({"applied": false, "reason": "conflict"})),
        "{body:#}"
    );
    assert_eq!(
        successors_of(&h, &first_id).await,
        vec![second_id.clone()],
        "the first thread keeps its one successor"
    );
    assert_eq!(mock_two.captured_bodies().await.len(), 3);

    // After a daemon restart the chain still runs from its rooted capsules:
    // a plain follow-up stays on model two although the bound item on disk
    // now names model one.
    h.restart().await.expect("restart daemon");
    let (status, body) =
        rebind_input(&h, &project_path, &fourth_id, "after the restart", None).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "follow-up after restart: {body:#}"
    );
    let fifth_id = completed_successor(&mut h, &body, &fourth_id).await;
    let two = mock_two.captured_bodies().await;
    assert_eq!(two.len(), 4, "the restarted chain stays on model two");
    assert_eq!(two[3]["model"], "model-two");
    assert_eq!(mock_one.captured_bodies().await.len(), 1);

    // Every segment charges the execution budget the first launch minted:
    // a rebind does not reset the allowance.
    let chain = [&first_id, &second_id, &third_id, &fourth_id, &fifth_id];
    let mut rows = Vec::new();
    for _ in 0..60 {
        let (s, b) = h
            .post_execute(
                "service:threads/accounting/summary",
                &project_path,
                serde_json::json!({"detail": true, "limit": 200}),
            )
            .await
            .expect("summary service reachable");
        assert_eq!(s, reqwest::StatusCode::OK, "summary status: {b:#}");
        let payload = b
            .get("result")
            .and_then(|r| {
                if r.get("totals").is_some() {
                    Some(r.clone())
                } else {
                    r.get("result").cloned()
                }
            })
            .unwrap_or_default();
        rows = payload
            .get("rows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let seen = |thread: &str| {
            rows.iter()
                .any(|row| row.get("thread_id").and_then(|v| v.as_str()) == Some(thread))
        };
        if chain.iter().all(|thread| seen(thread.as_str())) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let field = |row: &serde_json::Value, name: &str| {
        row.get(name)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let chain_rows: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|row| {
            chain
                .iter()
                .any(|thread| field(row, "thread_id") == **thread)
        })
        .collect();
    for thread in chain {
        assert!(
            chain_rows
                .iter()
                .any(|row| field(row, "thread_id") == *thread),
            "thread {thread} has no ledger row: {rows:#?}"
        );
    }
    let budgets: std::collections::BTreeSet<(String, String)> = chain_rows
        .iter()
        .map(|row| {
            (
                field(row, "execution_budget_id"),
                field(row, "directive_budget_id"),
            )
        })
        .collect();
    assert_eq!(
        budgets.len(),
        1,
        "every segment must charge one unchanged budget scope: {chain_rows:#?}"
    );
    assert!(
        !budgets.iter().next().unwrap().0.is_empty(),
        "the budget scope is named: {chain_rows:#?}"
    );
    for row in &chain_rows {
        let expected = if field(row, "thread_id") == first_id {
            "mockone"
        } else {
            "mocktwo"
        };
        assert_eq!(field(row, "provider_id"), expected, "{row:#}");
    }

    drop(project);
    drop(mock_one);
    drop(mock_two);
}

/// Input sent to a running thread folds into it live. A rebind carried with
/// that input is validated, reported as not applied, and not kept.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_rebind_on_a_running_thread_is_not_applied() {
    const FIRST: &str = "directive:rebind/models/first";
    const SECOND: &str = "directive:rebind/models/second";

    let mock = MockProvider::start_blocked(vec![
        MockResponse::Text("held answer".into()),
        MockResponse::Text("answer after the folded input".into()),
    ])
    .await;
    let url = mock.base_url.clone();
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-rebind-running-model-config",
            fixture,
            |bundle_root| {
                plant_rebind_provider(bundle_root, "mockone", &url, "{}", &fixture.publisher)
            },
        )
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with a held mock provider");

    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project.path().to_str().unwrap().to_string();
    let signer = &fixture.publisher;
    plant_unmodelled_directive(project.path(), signer).expect("plant directive");
    plant_model_binding(
        project.path(),
        "first",
        "mockone",
        "model-one",
        200_000,
        signer,
    )
    .expect("plant first binding");
    plant_model_binding(
        project.path(),
        "second",
        "mockone",
        "model-two",
        200_000,
        signer,
    )
    .expect("plant second binding");

    // The launch waits for the thread to finish; the provider holds its
    // answer, so the thread stays running while input is sent to it.
    let launch = h.post_json(
        "/execute",
        serde_json::json!({
            "item_ref": "directive:rebind/turn",
            "ref_bindings": {"model": FIRST},
            "project_path": project_path,
            "parameters": {"name": "World"},
            "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_live(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
        }),
    );
    let steer = async {
        let mut held = false;
        for _ in 0..300 {
            if mock.captured_bodies().await.len() == 1 {
                held = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(held, "the provider must receive the first request");
        let running = poll_thread(&h, |t| t.item_ref == "directive:rebind/turn", false)
            .await
            .expect("the launched thread is listed");
        assert_eq!(running.status, "running", "{running:#?}");

        // A malformed rebind is a bad request even though a running thread
        // would not use it.
        let (status, body) = rebind_input(
            &h,
            &project_path,
            &running.thread_id,
            "steer",
            Some(serde_json::json!({"model": "not a ref"})),
        )
        .await;
        assert_ne!(
            status,
            reqwest::StatusCode::OK,
            "a malformed rebind must be refused: {body:#}"
        );

        let (status, body) = rebind_input(
            &h,
            &project_path,
            &running.thread_id,
            "steer the running thread",
            Some(serde_json::json!({"model": SECOND})),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK, "live input: {body:#}");
        assert_eq!(
            body.pointer("/result/delivery").and_then(|v| v.as_str()),
            Some("submitted"),
            "{body:#}"
        );
        assert_eq!(
            body.pointer("/result/thread_id").and_then(|v| v.as_str()),
            Some(running.thread_id.as_str()),
            "{body:#}"
        );
        assert_eq!(
            body.pointer("/result/rebind"),
            Some(&serde_json::json!({"applied": false, "reason": "thread_running"})),
            "{body:#}"
        );
        mock.release_responses();
        running.thread_id
    };
    let (launched, thread_id) = tokio::join!(launch, steer);
    let (status, body) = launched.expect("post /execute");
    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!("the launch must complete: {status} {body:#}\n--- daemon stderr ---\n{stderr}");
    }
    let settled = poll_thread(&h, |t| t.thread_id == thread_id, true)
        .await
        .expect("the thread settles");
    assert_eq!(settled.status, "completed", "{settled:#?}");

    // Nothing was rebound and nothing was kept for later: no successor, no
    // rebind record, and every request went out on the admitted model.
    assert!(successors_of(&h, &thread_id).await.is_empty());
    let events = h
        .thread_events(&thread_id)
        .await
        .expect("replay events")
        .expect("thread events");
    assert!(
        events
            .iter()
            .all(|(event_type, _)| event_type != "launch_binding_rebound"),
        "{events:#?}"
    );
    let bodies = mock.captured_bodies().await;
    assert!(!bodies.is_empty());
    for body in &bodies {
        assert_eq!(body["model"], "model-one", "{body:#}");
    }

    drop(project);
    drop(mock);
}

// ── Hard budget: reserve → issue → settle through the daemon ledger ────

/// Mock provider carrying a signed spend authority: a
/// `DerivedWorstCaseCharge` tariff at $2/$10 per million plus the
/// output-limit contract the derived certificate requires. Worst case at
/// context 200_000 and the default output ceiling 32_768:
/// `0.4 + 0.32768 = $0.72768` reserved per attempt.
fn plant_hard_budget_mock_provider(
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
  input_per_million: "2"
  output_per_million: "10"
spend_authority:
  billing_principal: e2e-mock
  credential_authority_generation: gen-1
  pricing_contract_subject: e2e-mock-2026-07
  tariff:
    schema_version: 1
    currency: usd
    pricing_generation: e2e-2026-07
    input_per_million: "2"
    output_per_million: "10"
    covered_dimensions: [input_tokens, output_tokens]
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

/// `plant_directive` plus an authored hard spend limit (canonical decimal
/// string, per the fixed-point cut).
fn plant_directive_with_spend_limit(
    root: &Path,
    rel_path: &str,
    body_text: &str,
    spend_usd: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = root.join(format!(".ai/directives/{rel_path}.md"));
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
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
description: "hard budget e2e fixture"
inputs:
  - name: name
    type: string
    required: true
model:
  tier: general
limits:
  spend_usd: "{spend_usd}"
---
{body_text}
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_hard_budget_reserves_settles_and_denies_via_daemon_ledger() {
    let mock = MockProvider::start(vec![MockResponse::Text("within budget".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-hard-budget-model-config",
            fixture,
            |bundle_root| {
                plant_hard_budget_mock_provider(bundle_root, &mock_url, &fixture.publisher)
            },
        )
    };

    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with hard-budget mock provider");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");

    // 1. A $1 hard limit fits the $0.72768 route maximum: the attempt is
    //    reserved, durably issued, sent, and settled from the mock's usage.
    plant_directive_with_spend_limit(
        project.path(),
        "test/hard_ok",
        "Say hello to ${inputs.name}.",
        "1",
        &fixture.publisher,
    )
    .expect("plant admitted directive");
    let post_fut = h.post_execute(
        "directive:test/hard_ok",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "Ledger"}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(60), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!("hard-budget directive timed out\n--- daemon stderr ---\n{stderr}");
            }
        };
    if status != reqwest::StatusCode::OK {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "expected 200 OK for the admitted hard-budget directive; got {status}\nbody={body:#}\n--- daemon stderr ---\n{stderr}"
        );
    }
    let result = body.get("result").cloned().expect("result envelope");
    assert_eq!(
        result.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "admitted hard-budget run must succeed: {body:#}"
    );

    // 2. The daemon-authored audit trail reaches the projection through the
    //    outbox publisher; poll the durable summary service until the
    //    settled attempt appears.
    let mut settled_seen = false;
    for _ in 0..40 {
        let (s, b) = h
            .post_execute(
                "service:threads/accounting/summary",
                project.path().to_str().unwrap(),
                serde_json::json!({"detail": true}),
            )
            .await
            .expect("summary service reachable");
        assert_eq!(s, reqwest::StatusCode::OK, "summary status: {b:#}");
        let payload = b
            .get("result")
            .and_then(|r| {
                if r.get("totals").is_some() {
                    Some(r.clone())
                } else {
                    r.get("result").cloned()
                }
            })
            .unwrap_or_default();
        let totals = payload.get("totals").cloned().unwrap_or_default();
        if totals
            .get("attempt_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            >= 1
        {
            assert!(
                payload
                    .get("health")
                    .and_then(|health| health.get("ledger_available"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                "ledger must be available: {payload:#}"
            );
            let rows = payload
                .get("rows")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            settled_seen = rows
                .iter()
                .any(|row| row.get("transition").and_then(|v| v.as_str()) == Some("reconciled"));
            if settled_seen {
                // Deterministic tariff: 10 in × $2/M + 5 out × $10/M = 70k nanos.
                let reconciled = rows
                    .iter()
                    .find(|row| {
                        row.get("transition").and_then(|v| v.as_str()) == Some("reconciled")
                    })
                    .expect("reconciled row");
                assert_eq!(
                    reconciled
                        .get("budget_charge_usd_nanos")
                        .and_then(|v| v.as_i64()),
                    Some(70_000),
                    "settled charge must be the deterministic tariff cost: {reconciled:#}"
                );
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        settled_seen,
        "a reconciled provider attempt must reach the audit projection"
    );

    // 3. A $0.5 hard limit is below the $0.72768 route maximum: the daemon
    //    denies the reservation durably and the provider is never contacted.
    let requests_before_denial = mock.captured_headers().await.len();
    plant_directive_with_spend_limit(
        project.path(),
        "test/hard_denied",
        "Say hello to ${inputs.name}.",
        "0.5",
        &fixture.publisher,
    )
    .expect("plant denied directive");
    let (denied_status, denied_body) = h
        .post_execute(
            "directive:test/hard_denied",
            project.path().to_str().unwrap(),
            serde_json::json!({"name": "Denied"}),
        )
        .await
        .expect("post denied directive");
    let denied_text = format!("{denied_status} {denied_body:#}");
    assert!(
        denied_text.contains("budget_exceeded") || denied_text.contains("denied"),
        "the under-budget run must fail with a durable reservation denial; got {denied_text}"
    );
    assert_eq!(
        mock.captured_headers().await.len(),
        requests_before_denial,
        "a denied reservation must never contact the provider"
    );

    drop(project);
    drop(mock);
}

// ── Hard budget: a fan-out shares the root's limit ─────────────────────

/// A directive with no limits of its own and no inputs: whatever allowance it
/// has comes from the run that launched it.
fn plant_fanout_child_directive(
    root: &Path,
    name: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/directives");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"---
name: {name}
category: ""
description: "fan-out budget e2e child"
model:
  tier: general
---
Do the work.
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(dir.join(format!("{name}.md")), signed)?;
    Ok(())
}

/// A graph whose one node follows a directive child per item, all launched
/// at once, under an authored hard spend limit. The graph runtime does not
/// pay providers itself; its paid descendants charge the graph run's budget.
fn plant_fanout_graph(
    root: &Path,
    graph: &str,
    child: &str,
    spend_usd: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/graphs");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.{child}
limits:
  spend_usd: "{spend_usd}"
config:
  start: fan
  max_steps: 100
  on_error: continue
  config_schema:
    type: object
    required: [items]
    properties:
      items: {{type: array}}
  nodes:
    fan:
      node_type: action
      over: "${{inputs.items}}"
      as: item
      parallel: true
      max_concurrency: 4
      follow: true
      action:
        item_id: "directive:{child}"
        ref_bindings:
          model: "directive:{child}"
        params: {{}}
      collect: results
      collect_threads: result_threads
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

/// One provider attempt in the accounting ledger.
/// The fan-out's child threads, as the daemon lists them.
async fn fanout_children(h: &DaemonHarness) -> Vec<common::ListedThread> {
    h.list_threads(200)
        .await
        .expect("list threads")
        .into_iter()
        .filter(|thread| thread.item_ref.contains("fanout_child"))
        .collect()
}

#[derive(Debug, Clone)]
struct LedgerAttempt {
    thread_id: String,
    execution_budget_id: String,
    state: String,
    reserved_nanos: i64,
    charge_nanos: Option<i64>,
}

/// One budget account row: `(account_kind, execution_budget_id, limit, committed, held)`.
type LedgerAccount = (String, String, Option<i64>, i64, i64);

fn read_fanout_ledger(h: &DaemonHarness) -> (Vec<LedgerAttempt>, Vec<LedgerAccount>) {
    let db = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join(ryeos_app::accounting_db::ACCOUNTING_DB_FILENAME);
    ryeos_app::test_support::read_existing_accounting(&db, |connection| {
        let mut attempts = connection.prepare(
            "SELECT thread_id, execution_budget_id, state, reserved_usd_nanos, \
             budget_charge_usd_nanos FROM provider_attempt_reservation \
             ORDER BY created_at_ms",
        )?;
        let attempts = attempts
            .query_map([], |row| {
                Ok(LedgerAttempt {
                    thread_id: row.get(0)?,
                    execution_budget_id: row.get(1)?,
                    state: row.get(2)?,
                    reserved_nanos: row.get(3)?,
                    charge_nanos: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut accounts = connection.prepare(
            "SELECT account_kind, execution_budget_id, limit_usd_nanos, \
             committed_usd_nanos, held_usd_nanos FROM budget_account",
        )?;
        let accounts = accounts
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((attempts, accounts))
    })
}

/// A run with a finite hard spend limit fans out four model-thread children.
/// The children share the root's allowance: together they never reserve or
/// charge past it, the ones that cannot reserve are refused before any
/// provider contact, and every attempt is booked to the root's one execution
/// budget rather than a fresh one per child.
///
/// Per attempt the route's worst case is the input tariff over the 200_000
/// token context window plus the output tariff over the 32_768 token default
/// output ceiling: 0.4 + 0.32768 = $0.72768 (727_680_000 nanos). A $1 limit
/// holds one such reservation and not two, so while the first child's
/// attempt is in flight the other three cannot reserve.
#[tokio::test(flavor = "multi_thread")]
async fn a_fan_out_shares_the_roots_spend_limit() {
    const RESERVATION_NANOS: i64 = 727_680_000;
    const LIMIT_NANOS: i64 = 1_000_000_000;
    const CHILDREN: usize = 4;

    // The provider holds every response until released, so the first child's
    // reservation stays open while its siblings try to reserve.
    let mock = MockProvider::start_blocked(vec![
        MockResponse::Text("child answer one".into()),
        MockResponse::Text("child answer two".into()),
        MockResponse::Text("child answer three".into()),
        MockResponse::Text("child answer four".into()),
        MockResponse::Text("spare answer".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-fanout-budget-model-config",
            fixture,
            |bundle_root| {
                plant_hard_budget_mock_provider(bundle_root, &mock_url, &fixture.publisher)
            },
        )
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with hard-budget mock provider");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_fanout_child_directive(project.path(), "fanout_child", &fixture.publisher)
        .expect("plant child directive");
    plant_fanout_graph(
        project.path(),
        "fanout_root",
        "fanout_child",
        "1",
        &fixture.publisher,
    )
    .expect("plant fan-out graph");

    let post_fut = h.post_execute(
        "graph:fanout_root",
        project.path().to_str().unwrap(),
        serde_json::json!({"items": ["a", "b", "c", "d"]}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(60), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!("fan-out graph launch timed out\n--- daemon stderr ---\n{stderr}");
            }
        };
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");

    let terminal = |status: &str| {
        matches!(
            status,
            "completed" | "failed" | "cancelled" | "killed" | "timed_out"
        )
    };

    // While the first attempt is held in flight, the siblings that cannot
    // reserve fail without contacting the provider.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let listed = fanout_children(&h).await;
        let settled = listed.iter().filter(|t| terminal(&t.status)).count();
        let contacts = mock.captured_bodies().await.len();
        if (listed.len() == CHILDREN && settled >= CHILDREN - 1) || contacts >= CHILDREN {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "children never reached the held state: {listed:#?}, {contacts} provider \
                 contacts\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(
        mock.captured_bodies().await.len(),
        1,
        "only the child holding the reservation may contact the provider"
    );
    let (in_flight, accounts) = read_fanout_ledger(&h);
    let held: i64 = in_flight
        .iter()
        .filter(|a| matches!(a.state.as_str(), "reserved" | "issued"))
        .map(|a| a.reserved_nanos)
        .sum();
    assert_eq!(
        held, RESERVATION_NANOS,
        "exactly one reservation is open while the first attempt is held: {in_flight:#?}"
    );
    assert!(
        held <= LIMIT_NANOS,
        "open reservations exceed the root's limit: {in_flight:#?}"
    );
    for (kind, _, limit, committed, account_held) in &accounts {
        if kind == "execution" {
            assert_eq!(*limit, Some(LIMIT_NANOS), "{accounts:#?}");
            assert!(committed + account_held <= LIMIT_NANOS, "{accounts:#?}");
        }
    }

    // Let the held attempt answer and wait for every child to settle.
    mock.release_responses();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let listed = loop {
        let listed = fanout_children(&h).await;
        if listed.len() == CHILDREN && listed.iter().all(|t| terminal(&t.status)) {
            break listed;
        }
        if std::time::Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("children never settled: {listed:#?}\n--- daemon stderr ---\n{stderr}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    let completed = listed.iter().filter(|t| t.status == "completed").count();
    assert!(
        completed < CHILDREN,
        "at least one child must be refused its reservation instead of running: {listed:#?}"
    );

    let (attempts, accounts) = read_fanout_ledger(&h);

    // (1) What the run charged plus what is still reserved never exceeds the
    // root's limit.
    let charged: i64 = attempts.iter().filter_map(|a| a.charge_nanos).sum();
    let still_reserved: i64 = attempts
        .iter()
        .filter(|a| matches!(a.state.as_str(), "reserved" | "issued"))
        .map(|a| a.reserved_nanos)
        .sum();
    assert!(
        charged + still_reserved <= LIMIT_NANOS,
        "the fan-out spent past the root's limit: charged {charged}, still reserved \
         {still_reserved}: {attempts:#?}"
    );
    assert!(
        charged > 0,
        "the admitted child settled a charge: {attempts:#?}"
    );

    // (2) A child that could not reserve is refused durably, not run.
    let denied = attempts
        .iter()
        .filter(|a| a.state == "reservation_denied")
        .count();
    assert!(
        denied >= 1,
        "at least one child must carry a denied reservation: {attempts:#?}"
    );
    assert_eq!(
        mock.captured_bodies().await.len(),
        CHILDREN - denied,
        "a denied reservation never contacts the provider: {attempts:#?}"
    );

    // (3) Every attempt of every child is booked to the root's one execution
    // budget; the ledger holds exactly one execution account, carrying the
    // root's limit, and no per-child fresh allowance.
    let execution_accounts: Vec<&LedgerAccount> = accounts
        .iter()
        .filter(|(kind, ..)| kind == "execution")
        .collect();
    assert_eq!(
        execution_accounts.len(),
        1,
        "the fan-out must charge one execution budget, not one per child: {accounts:#?}"
    );
    assert_eq!(execution_accounts[0].2, Some(LIMIT_NANOS), "{accounts:#?}");
    let budgets: std::collections::BTreeSet<&str> = attempts
        .iter()
        .map(|a| a.execution_budget_id.as_str())
        .collect();
    assert_eq!(
        budgets,
        std::collections::BTreeSet::from([execution_accounts[0].1.as_str()]),
        "every child's attempts are booked to the root's execution budget: {attempts:#?}"
    );
    let child_ids: std::collections::BTreeSet<&str> =
        listed.iter().map(|t| t.thread_id.as_str()).collect();
    assert!(
        attempts
            .iter()
            .all(|a| child_ids.contains(a.thread_id.as_str())),
        "every ledger attempt belongs to a fan-out child: {attempts:#?}"
    );

    drop(project);
    drop(mock);
}

// -- Concurrent intra-turn tool dispatch (execution.tool_concurrency) --
//
// One assistant message carries THREE tool calls to three sleepy tools.
// Each tool stamps `<idx>.start` / `<idx>.end` marker files (unix seconds,
// fractional) into a test-owned directory, sleeping in between. Overlap is
// proven from the markers, not wall-clock guesses: if the LATEST start
// precedes the EARLIEST end, all three executions coexisted. The captured
// second provider request proves the fold: tool-result messages appear in
// CALL order (c1, c2, c3) even though completion order differs by sleep.

fn plant_sleepy_marker_tool(
    root: &Path,
    name: &str,
    index: u32,
    sleep_secs: f64,
    marker_dir: &Path,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/tools").join(name);
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "{name}"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "concurrency e2e marker tool {index}"

import json
import time

MARKER_DIR = {marker_dir:?}
INDEX = {index}

with open(MARKER_DIR + "/" + str(INDEX) + ".start", "w") as f:
    f.write(repr(time.time()))
time.sleep({sleep_secs})
with open(MARKER_DIR + "/" + str(INDEX) + ".end", "w") as f:
    f.write(repr(time.time()))
print(json.dumps({{"idx": INDEX}}))
"#,
        index = index,
        name = name,
        sleep_secs = sleep_secs,
        marker_dir = marker_dir.to_str().expect("utf-8 marker dir"),
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join(format!("{name}.py")), signed)?;
    Ok(())
}

fn read_marker(marker_dir: &Path, index: u32, which: &str) -> f64 {
    let path = marker_dir.join(format!("{index}.{which}"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("marker {} missing: {e}", path.display()))
        .trim()
        .parse::<f64>()
        .expect("marker holds a float timestamp")
}

/// Plant a signed project execution config (`ryeos-runtime/execution`),
/// deep-merged over the bundle defaults at directive boot.
fn plant_execution_config(root: &Path, signer: &SigningKey, body: &str) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("execution.yaml"), signed)?;
    Ok(())
}

fn sleepy_batch_response() -> MockResponse {
    MockResponse::ToolCalls(vec![
        MockToolCallSpec {
            id: "c1".into(),
            // Canonical project ref `tool:sleep1/sleep1` is flattened for
            // the provider-facing inventory name.
            name: "sleep1_sleep1".into(),
            arguments: "{}".into(),
        },
        MockToolCallSpec {
            id: "c2".into(),
            name: "sleep2_sleep2".into(),
            arguments: "{}".into(),
        },
        MockToolCallSpec {
            id: "c3".into(),
            name: "sleep3_sleep3".into(),
            arguments: "{}".into(),
        },
    ])
}

/// The tool-role messages of the SECOND captured provider request, as
/// `(tool_call_id, content)` in transcript order.
async fn second_request_tool_messages(mock: &MockProvider) -> Vec<(String, String)> {
    let bodies = mock.captured_bodies().await;
    assert!(
        bodies.len() >= 2,
        "expected at least two provider requests (tool turn + final), got {}",
        bodies.len()
    );
    bodies[1]
        .get("messages")
        .and_then(|m| m.as_array())
        .expect("second request carries messages")
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .map(|m| {
            (
                m.get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                m.get("content")
                    .and_then(|content| content.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        m.get("content")
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    }),
            )
        })
        .collect()
}

fn tool_message_failed(content: &str) -> bool {
    let envelope: serde_json::Value = serde_json::from_str(content)
        .unwrap_or_else(|e| panic!("tool message content must be a JSON envelope: {e}; {content}"));
    envelope.get("error").is_some_and(|error| !error.is_null())
}

async fn run_sleepy_batch(
    mock_url: &str,
    marker_dir: &Path,
    sleeps: [f64; 3],
    execution_config: Option<&str>,
) -> (reqwest::StatusCode, serde_json::Value) {
    let mock_url = mock_url.to_string();
    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            register_mock_provider_bundle(state_path, &mock_url, fixture)
        };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon with mock + workflow bundles");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    for (i, sleep) in sleeps.iter().enumerate() {
        plant_sleepy_marker_tool(
            project.path(),
            &format!("sleep{}", i + 1),
            (i + 1) as u32,
            *sleep,
            marker_dir,
            &fixture.publisher,
        )
        .expect("plant sleepy tool");
    }
    if let Some(body) = execution_config {
        plant_execution_config(project.path(), &fixture.publisher, body)
            .expect("plant execution config");
    }
    plant_directive(
        project.path(),
        "test/sleepy_batch",
        "Call all three sleep tools, then summarise.",
        &["ryeos.execute.tool.*"],
        &fixture.publisher,
    )
    .expect("plant directive");

    let post_fut = h.post_execute(
        "directive:test/sleepy_batch",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let out = match tokio::time::timeout(std::time::Duration::from_secs(60), post_fut).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => panic!("post /execute failed: {e}"),
        Err(_) => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("sleepy-batch POST timed out\n--- daemon stderr ---\n{stderr}");
        }
    };
    drop(project);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_tool_batch_dispatches_concurrently_and_folds_in_call_order() {
    let mock = MockProvider::start(vec![
        sleepy_batch_response(),
        MockResponse::Text("batch done".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();
    let markers = tempfile::tempdir().expect("marker tempdir");

    // Sleeps chosen so COMPLETION order (2, 3, 1) differs from CALL order,
    // while leaving enough headroom for process-admission jitter on loaded CI
    // runners before the shortest-lived tool exits.
    let (status, body) = run_sleepy_batch(&mock_url, markers.path(), [6.0, 3.5, 4.8], None).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    let result = body.get("result").cloned().expect("result envelope");
    assert_eq!(
        result.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "body={body:#}"
    );
    let tool_messages = second_request_tool_messages(&mock).await;
    assert!(
        tool_messages
            .iter()
            .all(|(_, content)| !tool_message_failed(content)),
        "batch tools must execute successfully; messages={tool_messages:?}"
    );

    // Overlap: the latest start precedes the earliest end, so all three tool
    // executions coexisted. Serial dispatch cannot produce this shape (each
    // start would follow the previous end by construction).
    let starts: Vec<f64> = (1..=3)
        .map(|i| read_marker(markers.path(), i, "start"))
        .collect();
    let ends: Vec<f64> = (1..=3)
        .map(|i| read_marker(markers.path(), i, "end"))
        .collect();
    let latest_start = starts.iter().cloned().fold(f64::MIN, f64::max);
    let earliest_end = ends.iter().cloned().fold(f64::MAX, f64::min);
    assert!(
        latest_start < earliest_end,
        "expected three-way overlap; starts={starts:?} ends={ends:?}"
    );

    // Fold order: transcript tool messages are in CALL order despite the
    // completion order being (2, 3, 1).
    let ids: Vec<&str> = tool_messages.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["c1", "c2", "c3"], "messages={tool_messages:?}");

    drop(mock);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_tool_batch_at_width_one_is_strictly_serial() {
    let mock = MockProvider::start(vec![
        sleepy_batch_response(),
        MockResponse::Text("serial done".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();
    let markers = tempfile::tempdir().expect("marker tempdir");

    let (status, body) = run_sleepy_batch(
        &mock_url,
        markers.path(),
        [0.8, 0.8, 0.8],
        Some("tool_concurrency: 1\n"),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    assert_eq!(
        body.get("result")
            .and_then(|r| r.get("success"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "body={body:#}"
    );
    let tool_messages = second_request_tool_messages(&mock).await;
    assert!(
        tool_messages
            .iter()
            .all(|(_, content)| !tool_message_failed(content)),
        "serial tools must execute successfully; messages={tool_messages:?}"
    );

    // Strict serial: every next call starts only after the previous ended.
    for i in 1..3u32 {
        let prev_end = read_marker(markers.path(), i, "end");
        let next_start = read_marker(markers.path(), i + 1, "start");
        assert!(
            next_start >= prev_end,
            "width 1 must serialize: call {} started at {next_start} before call {i} ended at {prev_end}",
            i + 1
        );
    }
    let ids: Vec<&str> = tool_messages.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);

    drop(mock);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_tool_batch_refused_member_settles_error_envelope_in_place() {
    // Call 2 targets a tool that does not exist: resolution fails, the call
    // settles an error envelope IN SLOT 2, and the real calls on either side
    // still execute — every call id yields exactly one result.
    let mock = MockProvider::start(vec![
        MockResponse::ToolCalls(vec![
            MockToolCallSpec {
                id: "c1".into(),
                name: "sleep1_sleep1".into(),
                arguments: "{}".into(),
            },
            MockToolCallSpec {
                id: "c2".into(),
                name: "no_such_tool".into(),
                arguments: "{}".into(),
            },
            MockToolCallSpec {
                id: "c3".into(),
                name: "sleep3_sleep3".into(),
                arguments: "{}".into(),
            },
        ]),
        MockResponse::Text("mixed done".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();
    let markers = tempfile::tempdir().expect("marker tempdir");

    let (status, body) = run_sleepy_batch(&mock_url, markers.path(), [0.3, 0.0, 0.3], None).await;
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    assert_eq!(
        body.get("result")
            .and_then(|r| r.get("success"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "a refused batch member must not sink the run; body={body:#}"
    );
    let tool_messages = second_request_tool_messages(&mock).await;
    assert_eq!(
        tool_messages.len(),
        3,
        "every batch member must settle once; messages={tool_messages:?}"
    );
    assert!(
        !tool_message_failed(&tool_messages[0].1)
            && tool_message_failed(&tool_messages[1].1)
            && !tool_message_failed(&tool_messages[2].1),
        "only the refused middle member may fail; messages={tool_messages:?}"
    );

    // The real tools on both sides of the refusal executed.
    let _ = read_marker(markers.path(), 1, "end");
    let _ = read_marker(markers.path(), 3, "end");

    let ids: Vec<&str> = tool_messages.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["c1", "c2", "c3"], "messages={tool_messages:?}");
    assert!(
        tool_messages[1].1.contains("error"),
        "slot 2 must carry the resolve-failure envelope; messages={tool_messages:?}"
    );

    drop(mock);
}

// -- Admission resolution cache: transparent hits + recompute on edit --
//
// Exercises the wired cache through a real daemon: a second launch of the
// same directive is served from the resolution cache, and a launch after the
// directive's bytes change (re-signed) must recompute rather than serve the
// stale entry. Both are asserted only by end-to-end success — the cache's
// validation logic (changed dep, appearing shadow, generation identity) is
// proven by the unit suite in ryeos_app::resolution_cache.

#[tokio::test(flavor = "multi_thread")]
async fn e2e_resolution_cache_transparent_across_hit_and_edit() {
    let mock = MockProvider::start(vec![
        MockResponse::Text("launch one".into()),
        MockResponse::Text("launch two".into()),
        MockResponse::Text("launch three".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();

    let plant = {
        let mock_url = mock_url.clone();
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            register_mock_provider_bundle(state_path, &mock_url, fixture)
        }
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeos_directive_runtime=debug,ryeosd=debug".into()),
        );
    })
    .await
    .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("routing");
    let project_path = project.path().to_str().unwrap().to_string();

    async fn launch_ok(h: &common::DaemonHarness, project_path: &str) {
        let (status, body) = h
            .post_execute(
                "directive:test/cache_probe",
                project_path,
                serde_json::json!({"name": "World"}),
            )
            .await
            .expect("post /execute");
        assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
        assert_eq!(
            body.get("result")
                .and_then(|r| r.get("success"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "body={body:#}"
        );
    }

    // v1, launched twice: launch 2 is served from the resolution cache.
    plant_directive(
        project.path(),
        "test/cache_probe",
        "Summarise the ratings briefing. MARKER_ALPHA_ONE.",
        &["ryeos.execute.tool.*"],
        &fixture.publisher,
    )
    .expect("plant directive v1");
    launch_ok(&h, &project_path).await;
    launch_ok(&h, &project_path).await;

    // Edit the directive body → new signed bytes → new content digest. The
    // cached entry's project positive dependency no longer matches, so the
    // third launch must recompute rather than serve the stale resolution.
    plant_directive(
        project.path(),
        "test/cache_probe",
        "Summarise the ratings briefing. MARKER_BETA_TWO.",
        &["ryeos.execute.tool.*"],
        &fixture.publisher,
    )
    .expect("plant directive v2");
    launch_ok(&h, &project_path).await;

    // The directive body flows into each provider request. Assert what the
    // provider actually saw: a stale-serve on launch 3 would carry the v1
    // marker, and a never-populated cache is caught indirectly by the same
    // three-request shape. This is the assertion that makes the test catch a
    // wrong-serve, not merely a crash.
    let bodies = mock.captured_bodies().await;
    assert_eq!(bodies.len(), 3, "three launches → three provider requests");
    assert!(
        bodies[0].to_string().contains("MARKER_ALPHA_ONE"),
        "launch 1 saw v1"
    );
    assert!(
        bodies[1].to_string().contains("MARKER_ALPHA_ONE"),
        "launch 2 (cache hit) saw the same v1 body"
    );
    assert!(
        bodies[2].to_string().contains("MARKER_BETA_TWO"),
        "launch 3 must recompute after the edit and carry v2, not the stale v1"
    );
    assert!(
        !bodies[2].to_string().contains("MARKER_ALPHA_ONE"),
        "launch 3 must NOT serve the stale v1 resolution"
    );

    drop(project);
    drop(mock);
}
