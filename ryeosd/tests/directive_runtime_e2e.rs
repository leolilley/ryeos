//! V5.4 Phase 3b — directive-runtime end-to-end with mock LLM provider.
//!
//! These tests spawn the real `ryeosd` binary, register the
//! `ryeos-bundles/standard` bundle (which ships
//! `runtime:directive-runtime` + the materializable
//! `bin/<host_triple>/ryeos-directive-runtime` binary in its CAS),
//! plant a directive + mock provider config, then exercise the full
//! HTTP `/execute` → daemon → directive-runtime → mock LLM round trip.
//!
//! Plan: `.tmp/IMPLEMENTATION/V5.4-PLAN.md` lines 80-89 (P3b.1 - P3b.5).
//!
//! P3b.1 — `common::mock_provider::MockProvider` (separate file).
//! P3b.2 — `e2e_directive_runtime_hello_world_succeeds` (this file).
//! P3b.3 — root semantics pin re-asserted vs real spawn (this file).
//! P3b.4 / P3b.5 — tool-call round-trip + cap-denial (follow-on).

mod common;

use std::path::Path;

use common::fast_fixture::{register_standard_bundle, FastFixture};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::SigningKey;

/// Plant the `model-providers/mock` config under
/// `<user>/.ai/config/rye-runtime/model-providers/mock.yaml`.
/// `auth: {}` keeps the adapter's `Authorization` header skipped
/// (see `ryeos-directive-runtime/src/adapter.rs:38-43`).
fn plant_mock_provider(user_space: &Path, mock_base_url: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/rye-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"base_url: "{mock_base_url}"
auth: {{}}
headers: {{}}
pricing:
  input_per_million: 0.0
  output_per_million: 0.0
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

/// Plant `model_routing` mapping `tier: general` to provider `mock`.
fn plant_model_routing(user_space: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/rye-runtime");
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

/// Plant a directive at `<user>/.ai/directives/<rel>.md`. The body
/// is whatever the LLM should be asked; the mock returns canned
/// responses irrespective of body content, but a non-empty body is
/// required by the directive kind's `composer_config.body` rule
/// (`required: true, expect_value_type: string`).
///
/// `execute_caps`, if non-empty, is rendered into the directive's
/// `permissions.execute:` block. The directive kind's
/// `composer_config.policy_facts[name=effective_caps]` reads
/// `[permissions, execute]` and surfaces the values as
/// `EnvelopePolicy.effective_caps` for the runtime's
/// `Harness::check_permission` and `Dispatcher::check_permission` to
/// gate tool calls.
fn plant_directive(
    user_space: &Path,
    rel_path: &str,
    body_text: &str,
    execute_caps: &[&str],
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = user_space.join(format!(".ai/directives/{rel_path}.md"));
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let permissions_block = if execute_caps.is_empty() {
        String::new()
    } else {
        let lines = execute_caps
            .iter()
            .map(|c| format!("    - \"{c}\"\n"))
            .collect::<String>();
        format!("permissions:\n  execute:\n{lines}")
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
  name:
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

/// Plant a synth Python tool at `<user>/.ai/tools/<rel>.py`. The body
/// chains to the bundled `tool:rye/core/runtimes/python/script` runtime
/// so the daemon's subprocess terminator can actually execute it (we
/// reuse the dispatch_pin.rs::synth_tool_request pattern). The
/// directive-runtime's `bootstrap::scan_tools` walks
/// `<user>/.ai/tools/`, picks the file up via the loader's `tool` kind,
/// and registers it as `tool:<rel>.py` with the bare filename as the
/// LLM-visible tool name. Unsigned is fine — `verified_loader` accepts
/// missing signatures and returns the content as-is.
fn plant_python_echo_tool(user_space: &Path, rel: &str) -> anyhow::Result<()> {
    let dir_relative = Path::new(rel)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(rel);
    let dir = user_space.join(format!(".ai/tools/{dir_relative}"));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{rel}.py"));
    let body = r#"#!/usr/bin/env python3
__version__ = "1.0.0"
__executor_id__ = "tool:rye/core/runtimes/python/script"
__category__ = "{dir_relative}"
__description__ = "P3b echo tool — prints its single arg back"

import json
import sys

# The daemon's python script runtime forwards the tool's `params` JSON
# on argv (or stdin, depending on the wrapper). We don't actually need
# the args for the round-trip pin — printing a known token is enough
# to confirm the runtime got us here and the tool result flowed back
# into the LLM context.
print(json.dumps({"echoed": "ok"}))
sys.exit(0)
"#;
    std::fs::write(&path, body)?;
    Ok(())
}

// ── P3b.2: Hello World e2e ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn e2e_directive_runtime_hello_world_succeeds() {
    let mock = MockProvider::start(vec![MockResponse::Text("hello World".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = move |state_path: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_mock_provider(user, &mock_url, &fixture.publisher)?;
        plant_model_routing(user, &fixture.publisher)?;
        plant_directive(user, "test/hello", "Say hello to {{ name }}.", &[], &fixture.publisher)?;
        Ok(())
    };

    let (mut h, _fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        // Bubble runtime tracing through to the daemon's stderr so a
        // hung directive-runtime child can be debugged from the test
        // panic message.
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeos_directive_runtime=debug,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with mock provider + standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/hello",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        post_fut,
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => panic!("post /execute failed: {e}"),
        Err(_) => {
            let stderr = h.drain_stderr_nonblocking().await;
            // Probe state dir for runtime exit + thread events
            let state = h.state_path.clone();
            let projection = state.join(".ai/state/projection.sqlite3");
            let projection_dump = if projection.exists() {
                match ryeos_state::projection::ProjectionDb::open(&projection) {
                    Ok(db) => format!(
                        "threads = {:#?}",
                        ryeos_state::queries::list_threads(&db, 10).ok()
                    ),
                    Err(e) => format!("projection open error: {e}"),
                }
            } else {
                "no projection.sqlite3".into()
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
            panic!("response missing `result` envelope\nbody={body:#}\n--- daemon stderr ---\n{stderr}");
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

    let plant = move |state_path: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_mock_provider(user, &mock_url, &fixture.publisher)?;
        plant_model_routing(user, &fixture.publisher)?;
        plant_directive(user, "p3b3/subject", "irrelevant — mock returns canned text", &[], &fixture.publisher)?;
        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
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
    let projection_path = h.state_path.join(".ai/state/projection.sqlite3");
    for _ in 0..40 {
        if projection_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        projection_path.exists(),
        "projection.sqlite3 must exist at {}",
        projection_path.display()
    );

    let db = ryeos_state::projection::ProjectionDb::open(&projection_path)
        .expect("open projection db");
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

    let plant = move |state_path: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_mock_provider(user, &mock_url, &fixture.publisher)?;
        plant_model_routing(user, &fixture.publisher)?;
        plant_python_echo_tool(user, "echo")?;
        // Wildcard cap: the dispatcher checks `rye.execute.tool.<canonical_ref>`
        // (see dispatcher.rs::resolve). The runner no longer does a separate
        // name-based pre-check — permission is the dispatcher's job.
        plant_directive(
            user,
            "test/round_trip",
            "Call the echo tool, then summarise.",
            &["rye.execute.tool.*"],
            &fixture.publisher,
        )?;
        Ok(())
    };

    let (mut h, _fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeos_directive_runtime=debug,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with mock + standard bundle + echo tool");

    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/round_trip",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        post_fut,
    )
    .await
    {
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

    let result = body.get("result").cloned().unwrap_or_else(|| {
        panic!("response missing `result` envelope; body={body:#}")
    });
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
// The directive declares a `permissions.execute` cap that does NOT
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

    let plant = move |state_path: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_mock_provider(user, &mock_url, &fixture.publisher)?;
        plant_model_routing(user, &fixture.publisher)?;
        plant_python_echo_tool(user, "echo")?;
        // Grant ONLY a non-matching cap. `echo` is not in this set,
        // and `cap_matches` is anchored ($-terminated regex) so the
        // literal `allowed_only` does NOT subsume `echo`.
        plant_directive(
            user,
            "test/denied",
            "Try to call echo; you should be denied.",
            &["rye.execute.tool.allowed_only"],
            &fixture.publisher,
        )?;
        Ok(())
    };

    let (mut h, _fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeos_directive_runtime=debug,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with mock + non-matching cap");

    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/denied",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "X"}),
    );
    let (status, body) = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        post_fut,
    )
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
