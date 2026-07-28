//! V5.4 Phase 3b — directive-runtime end-to-end with mock LLM provider.
//!
//! These tests spawn the real `ryeosd` binary, register the
//! `bundles/standard` bundle (which ships
//! `runtime:directive-runtime` + the materializable
//! `bin/<host_triple>/ryeos-directive-runtime` binary in its CAS),
//! plant a directive + mock provider config, then exercise the full
//! HTTP `/execute` → daemon → directive-runtime → mock LLM round trip.
//!
//! P3b.1 — `common::mock_provider::MockProvider` (separate file).
//! P3b.2 — `e2e_directive_runtime_hello_world_succeeds` (this file).
//! P3b.3 — root semantics pin re-asserted vs real spawn (this file).
//! P3b.4 / P3b.5 — tool-call round-trip + cap-denial (follow-on).

mod common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_config_fixture_bundle, register_standard_bundle};
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
        r#"base_url: "{mock_base_url}"
family: chat_completions
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
/// LLM-visible tool name. Unsigned is fine — `verified_loader` accepts
/// missing signatures and returns the content as-is.
fn plant_python_echo_tool(root: &Path, rel: &str) -> anyhow::Result<()> {
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
    std::fs::write(&path, body)?;
    Ok(())
}

// ── P3b.2: Hello World e2e ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_runtime_hello_world_succeeds() {
    let mock = MockProvider::start(vec![MockResponse::Text("hello World".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
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
    .expect("start daemon with mock provider + standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(
        project.path(),
        "test/hello",
        "Say hello to {{ name }}.",
        &[],
        &fixture.publisher,
    )
    .expect("plant directive");
    let post_fut = h.post_execute(
        "directive:test/hello",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) =
        match tokio::time::timeout(std::time::Duration::from_secs(30), post_fut).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => panic!("post /execute failed: {e}"),
            Err(_) => {
                let stderr = h.drain_stderr_nonblocking().await;
                // Probe state dir for runtime exit + thread events
                let state = h.state_path.clone();
                let projection = common::selected_projection_path(&state).ok();
                let projection_dump = if projection.as_ref().is_some_and(|path| path.exists()) {
                    let projection = projection.as_ref().expect("checked selected projection");
                    match ryeos_state::projection::ProjectionDb::open(projection) {
                        Ok(db) => format!(
                            "threads = {:#?}",
                            ryeos_state::queries::list_threads(&db, 10).ok()
                        ),
                        Err(e) => format!("projection open error: {e}"),
                    }
                } else {
                    "no selected projection generation".into()
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
        register_standard_bundle(state_path, fixture)?;
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

    // Open the projection DB and confirm the thread row carries the
    // SUBJECT identity (`directive_run` / `directive:p3b3/subject`),
    // not the executor runtime's identity.
    let projection_path =
        common::selected_projection_path(&h.state_path).expect("resolve selected projection");
    for _ in 0..40 {
        if projection_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        projection_path.exists(),
        "selected projection must exist at {}",
        projection_path.display()
    );

    let db =
        ryeos_state::projection::ProjectionDb::open(&projection_path).expect("open projection db");
    let threads = ryeos_state::queries::list_threads(&db, 100).expect("list_threads");

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
    assert!(
        subject.executor_ref.starts_with("native:"),
        "thread.executor_ref records the native runtime executor; got: {:?}",
        subject.executor_ref
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
        register_standard_bundle(state_path, fixture)?;
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
    .expect("start daemon with mock + standard bundle + echo tool");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_python_echo_tool(project.path(), "echo").expect("plant echo tool");
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
        register_standard_bundle(state_path, fixture)?;
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
    plant_python_echo_tool(project.path(), "echo").expect("plant echo tool");
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

/// Poll the projection for the first thread matching `pred`, optionally waiting
/// until it reaches a terminal status. Returns `None` if it never appears.
async fn poll_thread(
    projection_path: &Path,
    pred: impl Fn(&ryeos_state::queries::ThreadRow) -> bool,
    require_terminal: bool,
) -> Option<ryeos_state::queries::ThreadRow> {
    for _ in 0..120 {
        if projection_path.exists() {
            let database = ryeos_state::projection::ProjectionDb::open(projection_path);
            if let Ok(db) = database {
                let listed = ryeos_state::queries::list_threads(&db, 200);
                if let Ok(threads) = listed {
                    if let Some(t) = threads.into_iter().find(|t| pred(t)) {
                        let terminal = matches!(
                            t.status.as_str(),
                            "completed"
                                | "failed"
                                | "cancelled"
                                | "killed"
                                | "timed_out"
                                | "continued"
                        );
                        if !require_terminal || terminal {
                            return Some(t);
                        }
                    }
                }
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
        register_standard_bundle(state_path, fixture)?;
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
    .expect("start daemon with mock provider + standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project.path().to_str().unwrap().to_string();
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(
        project.path(),
        "cont/dir",
        "Answer {{ name }}.",
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
    let projection_path =
        common::selected_projection_path(&h.state_path).expect("resolve selected projection");
    let first = poll_thread(
        &projection_path,
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
    let successor = poll_thread(&projection_path, |t| t.thread_id == successor_id, true)
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
        r#"base_url: "{mock_base_url}"
family: chat_completions
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
        register_standard_bundle(state_path, fixture)?;
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
        "Say hello to {{ name }}.",
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
        "Say hello to {{ name }}.",
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
) -> anyhow::Result<()> {
    let dir = root.join(".ai/tools").join(name);
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "{name}"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
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
    std::fs::write(dir.join(format!("{name}.py")), body)?;
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
            register_standard_bundle(state_path, fixture)?;
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
    .expect("start daemon with mock + standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    for (i, sleep) in sleeps.iter().enumerate() {
        plant_sleepy_marker_tool(
            project.path(),
            &format!("sleep{}", i + 1),
            (i + 1) as u32,
            *sleep,
            marker_dir,
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
            register_standard_bundle(state_path, fixture)?;
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
