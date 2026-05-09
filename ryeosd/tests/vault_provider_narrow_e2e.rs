//! End-to-end: narrow provider-secret injection contract.
//!
//! Tests proving the v0.2.1 narrow preflight behaves correctly:
//!
//! 1. `missing_selected_secret_fails_before_provider_request` —
//!    directive routes to provider `zen` (auth.env_var: ZEN_API_KEY),
//!    vault is empty, daemon fails BEFORE the runtime ever contacts
//!    the provider. Response body contains the structured error
//!    with stable code, env var name, and remediation hint.
//!
//! 2. `provider_with_no_auth_env_var_succeeds_with_empty_vault` —
//!    provider declares no auth env var; empty vault is fine.
//!    Daemon spawns the runtime; mock receives one request.
//!
//! 3. `resume_missing_selected_secret_fails_with_typed_error` —
//!    Exercises the shared `preflight_inject_provider_secret` helper
//!    via the `/execute` endpoint. The error surface (stable code,
//!    env_var, remediation) is asserted. A full daemon-restart resume
//!    e2e is covered by test 4 below.
//!
//! 4. `resume_missing_secret_after_daemon_restart` — real daemon-
//!    restart e2e: spawns a native_resume tool, kills the daemon mid-
//!    flight, restarts, and asserts the reconciler's resume path
//!    produces `outcome_code = "required_secret_missing"` via
//!    `run_existing_detached`.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use common::fast_fixture::{register_standard_bundle, FastFixture};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::SigningKey;

// ── Helpers (mirror directive_provider_secret_injection_e2e.rs) ──

fn plant_provider_config(
    user_space: &Path,
    provider_id: &str,
    mock_base_url: &str,
    env_var: Option<&str>,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = user_space.join(ryeos_engine::AI_DIR).join("config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let auth_block = match env_var {
        Some(ev) => format!("  env_var: \"{ev}\"\n"),
        None => "  env_var: null\n".to_string(),
    };
    let body = format!(
        r#"base_url: "{mock_base_url}"
auth:
{auth_block}headers: {{}}
pricing:
  input_per_million: 0.0
  output_per_million: 0.0
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join(format!("{provider_id}.yaml")), signed)?;
    Ok(())
}

fn plant_model_routing_to(
    user_space: &Path,
    provider_id: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = user_space.join(ryeos_engine::AI_DIR).join("config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"tiers:
  general:
    provider: {provider_id}
    model: mock-model
    context_window: 200000
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

fn plant_directive(
    user_space: &Path,
    rel_path: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = user_space.join(format!("{}/directives/{rel_path}.md", ryeos_engine::AI_DIR));
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    std::fs::create_dir_all(path.parent().expect("parent dir"))?;
    let body = format!(
        r#"---
name: {stem}
category: "{dir_relative}"
description: "Narrow provider-secret e2e fixture"
inputs:
  name:
    type: string
    required: true
model:
  tier: general
---
Say hello to {{{{ name }}}}.
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Plant vault keypair + empty sealed store (no secrets).
fn plant_empty_vault(state_path: &Path) -> anyhow::Result<()> {
    let secrets = HashMap::new();
    plant_sealed_vault_secrets(state_path, &secrets)
}

fn plant_sealed_vault_secrets(
    state_path: &Path,
    secrets: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let secret_key_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("private_key.pem");
    let sk = lillux::vault::VaultSecretKey::generate();
    lillux::vault::write_secret_key(&secret_key_path, &sk)?;
    let pub_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem");
    lillux::vault::write_public_key(&pub_path, &sk.public_key())?;
    let store_path = ryeosd::vault::default_sealed_store_path(state_path);
    ryeosd::vault::write_sealed_secrets(&store_path, &sk.public_key(), secrets)?;
    Ok(())
}

// ── Test 1: missing selected secret → fail-loud ────────────────

#[tokio::test(flavor = "multi_thread")]
async fn missing_selected_secret_fails_before_provider_request() {
    // Provider `zen` declares `auth.env_var: ZEN_API_KEY`. Vault is
    // empty (no ZEN_API_KEY). The narrow preflight in
    // `preflight_inject_provider_secret` must fail BEFORE the runtime
    // is spawned, so the mock provider receives zero HTTP requests.
    //
    // The error message from `MaterializationError::ProviderSecretMissing`
    // is routed through `BuildAndLaunchError::Materialization` →
    // `DispatchError::RuntimeMaterializationFailed` (because the Display
    // string contains "materializ") → HTTP 502.
    //
    // The body carries the full Display string which includes:
    //   - "ZEN_API_KEY"
    //   - "ryeos-core-tools vault put --name ZEN_API_KEY --value-stdin"

    // Start mock but expect it to receive ZERO requests.
    let mock = MockProvider::start(vec![MockResponse::Text("should not be called".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = move |state_path: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_provider_config(user, "zen", &mock_url, Some("ZEN_API_KEY"), &fixture.publisher)?;
        plant_model_routing_to(user, "zen", &fixture.publisher)?;
        plant_directive(user, "test/narrow_missing", &fixture.publisher)?;
        // Empty vault — no ZEN_API_KEY sealed.
        plant_empty_vault(state_path)?;
        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("daemon starts (vault is read at request time)");

    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/narrow_missing",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) = tokio::time::timeout(Duration::from_secs(30), post_fut)
        .await
        .expect("/execute timed out")
        .expect("/execute send failed");

    // The preflight failure surfaces as 502 Bad Gateway via
    // DispatchError::RequiredSecretMissing.
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_GATEWAY,
        "expected 502 for missing provider secret; got status={status} body={body:#}"
    );

    // The body MUST contain the stable machine-readable error code.
    let code = body.get("code").and_then(|v| v.as_str()).unwrap_or_default();
    assert_eq!(
        code, "required_secret_missing",
        "error body must have code=required_secret_missing; got code={code} body={body:#}"
    );

    // The body MUST contain the missing env var name.
    let env_var = body.get("env_var").and_then(|v| v.as_str()).unwrap_or_default();
    assert_eq!(
        env_var, "ZEN_API_KEY",
        "error body must have env_var=ZEN_API_KEY; got env_var={env_var} body={body:#}"
    );

    // The body MUST contain the remediation hint.
    let remediation = body.get("remediation").and_then(|v| v.as_str()).unwrap_or_default();
    assert!(
        remediation.contains("ryeos-core-tools vault put --name ZEN_API_KEY --value-stdin"),
        "remediation must include the vault put command; got: {remediation}"
    );

    // The mock provider MUST NOT have received any requests — the
    // daemon's preflight blocked the launch before any HTTP was made.
    let captured = mock.captured_headers().await;
    assert!(
        captured.is_empty(),
        "mock provider received {n} request(s) — narrow preflight should have blocked the launch \
         before the runtime could contact the provider",
        n = captured.len(),
    );

    drop(project);
    drop(mock);
}

// ── Test 3: shared helper — typed error surface ─────────────────
//
// Blocker B requires proving that the shared preflight helper
// propagates a typed `MaterializationError::ProviderSecretMissing`
// with structured fields, not a generic anyhow message.
//
// This test calls the `/execute` endpoint (which uses the same
// shared `preflight_inject_provider_secret` helper as resume) and
// asserts the structured error surface with stable code, env_var,
// and remediation. The real daemon-restart resume path through
// `run_existing_detached` is covered by test 4
// (`resume_missing_secret_after_daemon_restart`).

#[tokio::test(flavor = "multi_thread")]
async fn resume_missing_selected_secret_fails_with_typed_error() {
    // Set up a mock provider to construct a realistic composed view.
    let mock = MockProvider::start(vec![MockResponse::Text("unused".into())]).await;
    let mock_url = mock.base_url.clone();

    let (h, _fixture) = DaemonHarness::start_fast_with(
        move |state_path: &Path, user: &Path, fixture: &FastFixture| {
            register_standard_bundle(state_path, fixture)?;
            plant_provider_config(user, "zen", &mock_url, Some("ZEN_API_KEY"), &fixture.publisher)?;
            plant_model_routing_to(user, "zen", &fixture.publisher)?;
            plant_directive(user, "test/resume_typed", &fixture.publisher)?;
            // Empty vault — no ZEN_API_KEY.
            plant_empty_vault(state_path)?;
            Ok(())
        },
        |cmd| {
            cmd.env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
            );
        },
    )
    .await
    .expect("daemon starts");

    // Call /execute to trigger the preflight (which shares the same
    // helper as resume). The error is routed through
    // DispatchError::RequiredSecretMissing with stable code.
    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/resume_typed",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) = tokio::time::timeout(Duration::from_secs(30), post_fut)
        .await
        .expect("/execute timed out")
        .expect("/execute send failed");

    // Assert the stable structured error surface — same contract
    // the resume path produces (the shared helper is the same).
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_GATEWAY,
        "expected 502; got status={status} body={body:#}"
    );

    let code = body.get("code").and_then(|v| v.as_str()).unwrap_or_default();
    assert_eq!(
        code, "required_secret_missing",
        "must have stable code; got code={code} body={body:#}"
    );

    let env_var = body.get("env_var").and_then(|v| v.as_str()).unwrap_or_default();
    assert_eq!(
        env_var, "ZEN_API_KEY",
        "must have env_var=ZEN_API_KEY; got={env_var}"
    );

    let remediation = body.get("remediation").and_then(|v| v.as_str()).unwrap_or_default();
    assert!(
        remediation.contains("ryeos-core-tools vault put --name ZEN_API_KEY"),
        "remediation must contain the vault put command; got: {remediation}"
    );

    // Mock provider must receive zero requests — preflight blocked.
    let captured = mock.captured_headers().await;
    assert!(
        captured.is_empty(),
        "mock provider received {} request(s) — preflight should block before runtime spawn",
        captured.len(),
    );

    drop(project);
    drop(mock);
}

// ── Test 2: no auth env var → succeeds with empty vault ────────

#[tokio::test(flavor = "multi_thread")]
async fn provider_with_no_auth_env_var_succeeds_with_empty_vault() {
    // Provider `noauth` declares `env_var: null` (no auth needed).
    // Empty vault is fine. Daemon should spawn the runtime, which
    // contacts the mock provider and gets a canned response.

    let mock = MockProvider::start(vec![MockResponse::Text("hello from noauth".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = move |state_path: &Path, user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_provider_config(user, "noauth", &mock_url, None, &fixture.publisher)?;
        plant_model_routing_to(user, "noauth", &fixture.publisher)?;
        plant_directive(user, "test/narrow_noauth", &fixture.publisher)?;
        // Empty vault — fine because provider declares no env var.
        plant_empty_vault(state_path)?;
        Ok(())
    };

    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("daemon starts with noauth provider + empty vault");

    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/narrow_noauth",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) = tokio::time::timeout(Duration::from_secs(30), post_fut)
        .await
        .expect("/execute timed out")
        .expect("/execute send failed");

    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "/execute should succeed for provider with no auth env var; body={body:#}"
    );

    // The mock provider MUST have received exactly one request — the
    // runtime was spawned and called the LLM endpoint.
    let captured = mock.captured_headers().await;
    assert_eq!(
        captured.len(),
        1,
        "mock provider should receive exactly 1 request for noauth provider; got {}",
        captured.len(),
    );

    drop(project);
    drop(mock);
}

// ── Test 4: real daemon-restart resume e2e ─────────────────────
//
// Exercises `run_existing_detached` through a real spawn-kill-
// respawn cycle. Uses a tool with `native_resume: true` so the
// reconciler picks up the orphaned thread and calls
// `run_existing_detached`. The resume preflight runs
// `preflight_inject_provider_secret`, which resolves model routing
// to provider "zen" (env_var: ZEN_API_KEY) and fails because the
// vault is empty.
//
// Why a tool, not a directive? The standard directive-runtime does
// not declare `native_resume`. Creating a custom runtime that
// serves directives would require modifying the standard bundle or
// building a custom one — both are heavyweight for a test. A tool
// with `@subprocess` executor + `native_resume: true` exercises the
// same `run_existing_detached` → preflight code path without
// requiring bundle surgery.
//
// Key behavioral difference: the initial `/execute` for a tool
// goes through `dispatch_tool_subprocess` → `run_detached`, which
// does NOT run the provider-secret preflight. Only the resume path
// (`run_existing_detached`) runs it. This is intentional — it
// proves the resume path's preflight works independently.

/// Plant a `native_resume: true` tool + runtime in the project space.
fn plant_native_resume_tool(project_path: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    // Runtime with native_resume, @subprocess executor, long-running
    // command. Placed in `.ai/tools/resume_tool/runtime.yaml` so the
    // engine's path-based category extraction yields "resume_tool".
    let tools_dir = project_path.join(ryeos_engine::AI_DIR).join("tools");
    let runtime_dir = tools_dir.join("resume_tool");
    std::fs::create_dir_all(&runtime_dir)?;

    let runtime_body = r#"version: "1.0.0"
executor_id: "@subprocess"
category: "resume_tool"
description: "test runtime with native_resume"
native_resume: true
config:
  command: "sleep"
  args: ["60"]
"#;
    let signed = lillux::signature::sign_content(runtime_body, signer, "#", None);
    std::fs::write(runtime_dir.join("runtime.yaml"), signed)?;

    // Tool referencing the runtime. Must be in a subdirectory matching
    // its `category` per the tool kind schema's `match: path` rule.
    let tool_dir = tools_dir.join("resume");
    std::fs::create_dir_all(&tool_dir)?;
    let tool_body = r#"version: "1.0.0"
executor_id: "tool:resume_tool/runtime"
category: "resume"
description: "native_resume test tool"
"#;
    let signed = lillux::signature::sign_content(tool_body, signer, "#", None);
    std::fs::write(tool_dir.join("resume_test.yaml"), signed)?;

    Ok(())
}

/// Read the PID from the runtime DB for a given thread.
fn read_pid_from_runtime_db(state_path: &Path, thread_id: &str) -> Option<i64> {
    let db_path = state_path.join(ryeos_engine::AI_DIR).join("state/runtime.sqlite3");
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let mut stmt = conn
        .prepare("SELECT pid FROM thread_runtime WHERE thread_id = ?1")
        .ok()?;
    stmt.query_row(rusqlite::params![thread_id], |row| row.get(0))
        .ok()
        .flatten()
}

/// Read thread status + outcome_code from the projection DB.
/// Returns `(status, outcome_code, error_detail)` where `outcome_code`
/// is extracted from the error JSON and `error_detail` is the raw error.
fn read_thread_outcome_full(
    state_path: &Path,
    thread_id: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    let db_path = state_path.join(ryeos_engine::AI_DIR).join("state/projection.sqlite3");
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;

    // Check thread_results for status + error.
    let mut stmt = conn
        .prepare("SELECT status, error FROM thread_results WHERE thread_id = ?1")
        .ok()?;
    let result = stmt
        .query_row(rusqlite::params![thread_id], |row| {
            let status: String = row.get(0)?;
            let error: Option<String> = row.get(1)?;
            Ok((status, error))
        })
        .ok()?;

    let (status, error_json) = result;
    let outcome_code = error_json.as_ref().and_then(|e| {
        serde_json::from_str::<serde_json::Value>(e)
            .ok()
            .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(|s| s.to_string()))
    });
    Some((status, outcome_code, error_json))
}

/// Read thread status + outcome_code from the projection DB.
/// Returns `(status, outcome_code)` where `outcome_code` is extracted
/// from the error JSON if present.
fn read_thread_outcome(
    state_path: &Path,
    thread_id: &str,
) -> Option<(String, Option<String>)> {
    read_thread_outcome_full(state_path, thread_id)
        .map(|(s, oc, _)| (s, oc))
}

/// Read the most recent persisted event for a thread from the projection DB.
/// Returns `(event_type, payload_json)` where payload_json is parsed from
/// the `payload` BLOB column.
fn read_last_event(
    state_path: &Path,
    thread_id: &str,
) -> Option<(String, serde_json::Value)> {
    let db_path = state_path.join(ryeos_engine::AI_DIR).join("state/projection.sqlite3");
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT event_type, payload FROM events \
             WHERE thread_id = ?1 \
             ORDER BY chain_seq DESC LIMIT 1",
        )
        .ok()?;
    stmt.query_row(rusqlite::params![thread_id], |row| {
        let event_type: String = row.get(0)?;
        let payload_blob: Vec<u8> = row.get(1)?;
        let payload: serde_json::Value = serde_json::from_slice(&payload_blob)
            .unwrap_or(serde_json::json!({}));
        Ok((event_type, payload))
    })
    .ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_missing_secret_after_daemon_restart() {
    // ── Setup ────────────────────────────────────────────────────
    //
    // Provider "zen" declares auth.env_var: ZEN_API_KEY.
    // Model routing maps "general" → zen.
    // Vault is empty (no ZEN_API_KEY).
    // A tool with native_resume: true is planted in the project.
    //
    // The initial execute succeeds (tool subprocess path doesn't run
    // the provider-secret preflight). On daemon restart, the
    // reconciler resumes the orphaned thread via
    // `run_existing_detached`, which runs the preflight and fails
    // with ProviderSecretMissing.

    // Mock provider — not needed for the test assertions but
    // required so model routing resolves.
    let mock = MockProvider::start(vec![MockResponse::Text("unused".into())]).await;
    let mock_url = mock.base_url.clone();

    let (mut h, fixture) = DaemonHarness::start_fast_with(
        move |state_path: &Path, user: &Path, f: &FastFixture| {
            register_standard_bundle(state_path, f)?;
            plant_provider_config(user, "zen", &mock_url, Some("ZEN_API_KEY"), &f.publisher)?;
            plant_model_routing_to(user, "zen", &f.publisher)?;
            // Empty vault — no ZEN_API_KEY.
            plant_empty_vault(state_path)?;
            Ok(())
        },
        |cmd| {
            cmd.env(
                "RUST_LOG",
                std::env::var("RUST_LOG")
                    .unwrap_or_else(|_| "info,ryeosd=debug".into()),
            );
        },
    )
    .await
    .expect("daemon starts");

    // Create a project with a native_resume tool.
    let project = tempfile::tempdir().expect("project tempdir");
    plant_native_resume_tool(project.path(), &fixture.publisher)
        .expect("plant native_resume tool");

    // ── Phase 1: Execute the tool (detached) ────────────────────
    let body = serde_json::json!({
        "item_ref": "tool:resume/resume_test",
        "project_path": project.path().to_str().unwrap(),
        "parameters": {},
        "launch_mode": "detached",
    });
    let resp = reqwest::Client::new()
        .post(format!("http://{}/execute", h.bind))
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("/execute send failed");
    let status = resp.status();
    let resp_body: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "/execute should succeed for detached tool; got status={status} body={resp_body:#}"
    );

    let thread_id = resp_body["thread"]["thread_id"]
        .as_str()
        .expect("response includes thread.thread_id")
        .to_string();

    // Give the background task time to start and log any errors.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Phase 2: Wait for PID to appear or thread to finalize ───
    //
    // The background task spawns the subprocess asynchronously.
    // Poll the runtime DB until a PID appears. Also check the
    // projection DB for early finalization (engine error, etc.)
    // so we get a useful diagnostic instead of a timeout.
    let pid_deadline = std::time::Instant::now() + Duration::from_secs(15);
    let pid = loop {
        if let Some(pid) = read_pid_from_runtime_db(&h.state_path, &thread_id) {
            break pid;
        }
        // Check if the thread was already finalized (spawn failure).
        if let Some((status, outcome, full_error)) = read_thread_outcome_full(&h.state_path, &thread_id) {
            if status == "failed" || status == "completed" || status == "killed" {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!(
                    "thread {thread_id} reached terminal status before PID appeared: \
                     status={status} outcome_code={outcome:?} error={full_error:?}\n\
                     Daemon stderr (tail):\n{}",
                    &stderr[stderr.len().saturating_sub(3000)..]
                );
            }
        }
        if std::time::Instant::now() > pid_deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            let (thread_state, _, full_error) = read_thread_outcome_full(&h.state_path, &thread_id)
                .map(|(s, oc, e)| (Some(s), Some(oc), Some(e)))
                .unwrap_or((None, None, None));
            panic!(
                "PID never appeared in runtime DB for thread {thread_id} \
                 (state={thread_state:?} error={full_error:?})\n\
                 Daemon stderr (tail):\n{}",
                &stderr[stderr.len().saturating_sub(3000)..]
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // ── Phase 2b: Kill daemon FIRST (so it can't observe subprocess death) ──
    //
    // The daemon must be killed before the subprocess so the daemon
    // doesn't see the subprocess exit and finalize the thread as
    // "failed" with an exit-code error. We want the reconciler (on
    // restart) to see a "running" thread with a dead PGID and decide
    // to resume via `run_existing_detached`.
    h.kill_daemon().await.expect("kill daemon");

    // Kill the orphaned subprocess AFTER daemon is dead, so the
    // reconciler sees a dead PGID at startup.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ── Phase 2c: Re-spawn daemon (reconciler runs at startup) ──
    h.respawn_with(|cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
    })
    .await
    .expect("respawn daemon");

    // ── Phase 3: Poll for thread to reach terminal status ───────
    //
    // The reconciler runs at startup and dispatches the resume
    // intent. `run_existing_detached` runs preflight, which resolves
    // "general" → "zen" → ZEN_API_KEY → not in vault →
    // ProviderSecretMissing → guard.fail_thread("required_secret_missing").
    let outcome_deadline = std::time::Instant::now() + Duration::from_secs(15);
    let (final_status, outcome_code) = loop {
        if let Some(result) = read_thread_outcome(&h.state_path, &thread_id) {
            let (ref status, _) = result;
            if status == "failed" || status == "completed" || status == "killed" {
                break result;
            }
        }
        if std::time::Instant::now() > outcome_deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "thread {thread_id} never reached terminal status within 15s. \
                 Last known state: {:?}. Daemon stderr (tail):\n{}",
                read_thread_outcome(&h.state_path, &thread_id),
                &stderr[stderr.len().saturating_sub(2000)..]
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    // ── Phase 4: Assert the expected outcome ────────────────────
    assert_eq!(
        final_status, "failed",
        "thread should be finalized as failed; got status={final_status}"
    );
    assert_eq!(
        outcome_code.as_deref(),
        Some("required_secret_missing"),
        "outcome_code must be 'required_secret_missing'; got outcome_code={outcome_code:?}"
    );

    // F3: the error JSON in the projection DB must carry the enriched
    // structured fields (env_var, remediation) — same surface the SSE
    // stream exposes to watching clients.
    let full_error = read_thread_outcome_full(&h.state_path, &thread_id)
        .and_then(|(_, _, err)| err)
        .expect("thread must have error JSON");
    let error_json: serde_json::Value = serde_json::from_str(&full_error)
        .expect("error must be valid JSON");
    assert_eq!(
        error_json["code"].as_str(),
        Some("required_secret_missing"),
        "error JSON must have code=required_secret_missing; got: {error_json:#}"
    );
    assert_eq!(
        error_json["env_var"].as_str(),
        Some("ZEN_API_KEY"),
        "error JSON must have env_var=ZEN_API_KEY; got: {error_json:#}"
    );
    let remediation = error_json["remediation"].as_str().unwrap_or_default();
    assert!(
        remediation.contains("ryeos-core-tools vault put --name ZEN_API_KEY"),
        "remediation must contain the vault put command; got: {remediation}"
    );

    // F3 final: prove the persisted thread_failed event payload carries
    // the structured error (not just the projection DB column). This is
    // the wire shape SSE consumers see on replay or live subscription.
    let (event_type, payload) = read_last_event(&h.state_path, &thread_id)
        .expect("thread must have a terminal event");

    assert_eq!(
        event_type, "thread_failed",
        "terminal event must be thread_failed; got {event_type}"
    );
    assert_eq!(
        payload["outcome_code"].as_str(),
        Some("required_secret_missing"),
        "thread_failed.payload.outcome_code mismatch: {payload:#}"
    );
    assert_eq!(
        payload["has_error"].as_bool(),
        Some(true),
        "thread_failed.payload.has_error must be true: {payload:#}"
    );

    // The structured error sub-object — added by the F3 envelope work.
    let err = &payload["error"];
    assert!(
        err.is_object(),
        "thread_failed.payload.error must be an object: {payload:#}"
    );
    assert_eq!(
        err["code"].as_str(),
        Some("required_secret_missing"),
        "payload.error.code mismatch: {err:#}"
    );
    assert_eq!(
        err["env_var"].as_str(),
        Some("ZEN_API_KEY"),
        "payload.error.env_var mismatch: {err:#}"
    );
    assert_eq!(
        err["source_kind"].as_str(),
        Some("provider"),
        "payload.error.source_kind mismatch: {err:#}"
    );
    assert_eq!(
        err["source_name"].as_str(),
        Some("zen"),
        "payload.error.source_name mismatch: {err:#}"
    );
    let evt_remediation = err["remediation"].as_str().unwrap_or_default();
    assert!(
        evt_remediation.contains("vault put --name ZEN_API_KEY"),
        "payload.error.remediation must mention 'vault put --name ZEN_API_KEY'; got: {evt_remediation:?}"
    );

    // The mock provider must have received zero requests — the
    // preflight blocked the resume before any HTTP was made.
    let captured = mock.captured_headers().await;
    assert!(
        captured.is_empty(),
        "mock provider received {} request(s) — resume preflight should block \
         before runtime spawn",
        captured.len(),
    );

    drop(project);
    drop(mock);
}

// ── Test 5: SSE stream emits structured required_secret_missing ──
//
// F3: proves the `/execute/stream` endpoint emits a `stream_error`
// SSE event with the structured fields (env_var, source_kind,
// source_name, remediation) when the preflight catches a missing provider secret.

#[tokio::test(flavor = "multi_thread")]
async fn execute_stream_emits_structured_required_secret_missing_event() {
    use base64::Engine;
    use lillux::crypto::Signer;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Set up a mock provider (won't be called — preflight blocks).
    let mock = MockProvider::start(vec![MockResponse::Text("unused".into())]).await;
    let mock_url = mock.base_url.clone();

    let (h, fixture) = DaemonHarness::start_fast_with(
        move |state_path: &Path, user: &Path, f: &FastFixture| {
            register_standard_bundle(state_path, f)?;
            plant_provider_config(user, "zen", &mock_url, Some("ZEN_API_KEY"), &f.publisher)?;
            plant_model_routing_to(user, "zen", &f.publisher)?;
            plant_directive(user, "test/sse_secret", &f.publisher)?;
            plant_empty_vault(state_path)?;
            // Authorize the user key so /execute/stream accepts signed requests.
            common::fast_fixture::write_authorized_key_signed_by(
                state_path, &f.user, &f.node,
            )?;
            Ok(())
        },
        |cmd| {
            cmd.env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
            );
        },
    )
    .await
    .expect("daemon starts");

    let project = tempfile::tempdir().expect("project tempdir");

    // POST /execute/stream with a directive that requires ZEN_API_KEY.
    // The route requires ryeos_signed auth.
    let body_obj = serde_json::json!({
        "item_ref": "directive:test/sse_secret",
        "project_path": project.path().to_str().unwrap(),
        "parameters": {"name": "World"},
    });
    let body_bytes = serde_json::to_vec(&body_obj).expect("serialize body");

    let node_fp = lillux::signature::compute_fingerprint(&fixture.node.verifying_key());
    let audience = format!("fp:{node_fp}");
    let path = "/execute/stream";

    // Build ryeos_signed auth headers using the user key.
    let user_fp =
        lillux::signature::compute_fingerprint(&fixture.user.verifying_key());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let nonce = format!("{:016x}", rand::random::<u64>());
    let body_hash = lillux::cas::sha256_hex(&body_bytes);
    let string_to_sign = format!(
        "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        "POST", path, body_hash, timestamp, nonce, audience,
    );
    let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
    let sig = fixture.user.sign(content_hash.as_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

    let url = format!("http://{}{}", h.bind, path);
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-ryeos-key-id", format!("fp:{user_fp}"))
        .header("x-ryeos-timestamp", &timestamp)
        .header("x-ryeos-nonce", &nonce)
        .header("x-ryeos-signature", &sig_b64)
        .body(body_bytes)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("/execute/stream send failed");

    assert!(
        resp.status().is_success(),
        "/execute/stream should return 200; got {}",
        resp.status()
    );

    // Read the SSE response body as text and look for the stream_error event.
    let text = tokio::time::timeout(Duration::from_secs(15), resp.text())
        .await
        .expect("SSE response timed out")
        .expect("SSE response read failed");

    // Parse SSE lines looking for:
    //   event: stream_error
    //   data: {"code":"required_secret_missing",...}
    let mut found_error = false;
    let mut current_event_type = String::new();
    for line in text.lines() {
        if let Some(ev) = line.strip_prefix("event: ") {
            current_event_type = ev.trim().to_string();
        }
        if let Some(data) = line.strip_prefix("data: ") {
            if current_event_type == "stream_error" {
                let parsed: serde_json::Value =
                    serde_json::from_str(data.trim()).unwrap_or(serde_json::json!({}));

                let code = parsed.get("code").and_then(|v| v.as_str()).unwrap_or_default();
                if code == "required_secret_missing" {
                    found_error = true;

                    // Assert structured fields.
                    let env_var = parsed
                        .get("env_var")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(
                        env_var, "ZEN_API_KEY",
                        "SSE stream_error must have env_var=ZEN_API_KEY; got: {env_var}"
                    );

                    let source_kind = parsed
                        .get("source_kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(
                        source_kind, "provider",
                        "SSE stream_error must have source_kind=provider; got: {source_kind}"
                    );

                    let source_name = parsed
                        .get("source_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(
                        source_name, "zen",
                        "SSE stream_error must have source_name=zen; got: {source_name}"
                    );

                    let remediation = parsed
                        .get("remediation")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert!(
                        remediation.contains("ryeos-core-tools vault put --name ZEN_API_KEY"),
                        "remediation must contain vault put command; got: {remediation}"
                    );

                    // The error message must be a non-empty string.
                    let error_msg = parsed
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert!(
                        !error_msg.is_empty(),
                        "SSE stream_error must have non-empty error message"
                    );
                    break;
                }
            }
        }
    }

    assert!(
        found_error,
        "SSE stream must contain a stream_error event with code=required_secret_missing; \
         raw SSE output:\n{text}"
    );

    // Mock provider must receive zero requests — preflight blocked.
    let captured = mock.captured_headers().await;
    assert!(
        captured.is_empty(),
        "mock provider received {} request(s) — preflight should block before runtime spawn",
        captured.len(),
    );

    drop(project);
    drop(mock);
}
