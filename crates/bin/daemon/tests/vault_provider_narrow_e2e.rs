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
//!    Exercises the managed launch provider preflight via the `/execute`
//!    endpoint. The error surface (stable code, env_var, remediation) is
//!    asserted.
//!
//! 4. `generic_tool_resume_does_not_require_provider_secret` — real
//!    daemon-restart e2e: spawns a native_resume tool, kills the daemon
//!    mid-flight, restarts, and asserts generic tool resume is not coupled
//!    to provider auth.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_standard_bundle};
use common::mock_provider::{MockProvider, MockResponse};
use lillux::crypto::SigningKey;

// ── Helpers (mirror directive_provider_secret_injection_e2e.rs) ──

fn plant_provider_config(
    root: &Path,
    provider_id: &str,
    mock_base_url: &str,
    env_var: Option<&str>,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root
        .join(ryeos_engine::AI_DIR)
        .join("config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let auth_block = match env_var {
        Some(ev) => format!("  env_var: \"{ev}\"\n  header_name: \"Authorization\"\n"),
        None => "  env_var: null\n".to_string(),
    };
    let body = format!(
        r#"base_url: "{mock_base_url}"
family: chat_completions
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
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
    root: &Path,
    provider_id: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(ryeos_engine::AI_DIR).join("config/ryeos-runtime");
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

fn plant_directive(root: &Path, rel_path: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let path = root.join(format!("{}/directives/{rel_path}.md", ryeos_engine::AI_DIR));
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
  - name: name
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
    let store_path = ryeos_app::vault::default_sealed_store_path(state_path);
    ryeos_app::vault::write_sealed_secrets(&store_path, &sk.public_key(), secrets)?;
    Ok(())
}

// ── Test 1: missing selected secret → fail-loud ────────────────

#[tokio::test(flavor = "multi_thread")]
async fn missing_selected_secret_fails_before_provider_request() {
    // Provider `zen` declares `auth.env_var: ZEN_API_KEY`. Vault is
    // empty (no ZEN_API_KEY). Runtime envelope preflight must fail BEFORE
    // the runtime is spawned, so the mock provider receives zero HTTP
    // requests.
    //
    // The body carries the structured RequiredSecretMissing surface:
    //   - "ZEN_API_KEY"
    //   - "ryeos vault set --name ZEN_API_KEY --value <value>"

    // Start mock but expect it to receive ZERO requests.
    let mock = MockProvider::start(vec![MockResponse::Text("should not be called".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        // Empty vault — no ZEN_API_KEY sealed.
        plant_empty_vault(state_path)?;
        Ok(())
    };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
        cmd.env("RYEOS_ALLOW_PROJECT_PROVIDER_CONFIG", "1");
    })
    .await
    .expect("daemon starts (vault is read at request time)");

    // Project-tier plants override the standard bundle's shipped zen
    // routing/provider, pointing `zen` at the mock so the zero-request
    // assertion below stays sharp.
    let project = tempfile::tempdir().expect("project tempdir");
    plant_provider_config(
        project.path(),
        "zen",
        &mock_url,
        Some("ZEN_API_KEY"),
        &fixture.publisher,
    )
    .expect("plant provider");
    plant_model_routing_to(project.path(), "zen", &fixture.publisher).expect("plant routing");
    plant_directive(project.path(), "test/narrow_missing", &fixture.publisher)
        .expect("plant directive");
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
    let code = body
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(
        code, "required_secret_missing",
        "error body must have code=required_secret_missing; got code={code} body={body:#}"
    );

    // The body MUST contain the missing env var name.
    let env_var = body
        .get("env_var")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(
        env_var, "ZEN_API_KEY",
        "error body must have env_var=ZEN_API_KEY; got env_var={env_var} body={body:#}"
    );

    // The body MUST contain the remediation hint.
    let remediation = body
        .get("remediation")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        remediation.contains("ryeos vault set --name ZEN_API_KEY --value <value>"),
        "remediation must include the vault set command; got: {remediation}"
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

// ── Test 3: managed launch missing-secret typed surface ──────────
//
// Proves managed launch provider preflight propagates a structured
// RequiredSecretMissing surface with stable code, env_var, and remediation,
// not a generic anyhow message.

#[tokio::test(flavor = "multi_thread")]
async fn resume_missing_selected_secret_fails_with_typed_error() {
    // Set up a mock provider to construct a realistic composed view.
    let mock = MockProvider::start(vec![MockResponse::Text("unused".into())]).await;
    let mock_url = mock.base_url.clone();

    let (h, fixture) = DaemonHarness::start_fast_with(
        |state_path: &Path, _user: &Path, fixture: &FastFixture| {
            register_standard_bundle(state_path, fixture)?;
            // Empty vault — no ZEN_API_KEY.
            plant_empty_vault(state_path)?;
            Ok(())
        },
        |cmd| {
            cmd.env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
            );
            cmd.env("RYEOS_ALLOW_PROJECT_PROVIDER_CONFIG", "1");
        },
    )
    .await
    .expect("daemon starts");

    // Call /execute to trigger the preflight (which shares the same
    // helper as resume). The error is routed through
    // DispatchError::RequiredSecretMissing with stable code.
    let project = tempfile::tempdir().expect("project tempdir");
    plant_provider_config(
        project.path(),
        "zen",
        &mock_url,
        Some("ZEN_API_KEY"),
        &fixture.publisher,
    )
    .expect("plant provider");
    plant_model_routing_to(project.path(), "zen", &fixture.publisher).expect("plant routing");
    plant_directive(project.path(), "test/resume_typed", &fixture.publisher)
        .expect("plant directive");
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

    let code = body
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(
        code, "required_secret_missing",
        "must have stable code; got code={code} body={body:#}"
    );

    let env_var = body
        .get("env_var")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(
        env_var, "ZEN_API_KEY",
        "must have env_var=ZEN_API_KEY; got={env_var}"
    );

    let remediation = body
        .get("remediation")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        remediation.contains("ryeos vault set --name ZEN_API_KEY"),
        "remediation must contain the vault set command; got: {remediation}"
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

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        // Empty vault — fine because provider declares no env var.
        plant_empty_vault(state_path)?;
        Ok(())
    };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
        cmd.env("RYEOS_ALLOW_PROJECT_PROVIDER_CONFIG", "1");
    })
    .await
    .expect("daemon starts with noauth provider + empty vault");

    // Project routing overrides the bundle's shipped zen routing so
    // tier `general` resolves to the no-auth mock provider.
    let project = tempfile::tempdir().expect("project tempdir");
    plant_provider_config(
        project.path(),
        "noauth",
        &mock_url,
        None,
        &fixture.publisher,
    )
    .expect("plant provider");
    plant_model_routing_to(project.path(), "noauth", &fixture.publisher).expect("plant routing");
    plant_directive(project.path(), "test/narrow_noauth", &fixture.publisher)
        .expect("plant directive");
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

// ── Test 4: generic tool resume stays provider-independent ──────
//
// Exercises `run_existing_detached` through a real spawn-kill-
// respawn cycle. Uses a tool with `native_resume: true` so the
// reconciler picks up the orphaned thread and calls
// `run_existing_detached`. A generic tool does not have a runtime descriptor
// requiring `provider_snapshot`, so resume must not resolve model routing or
// require provider auth.

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
    let db_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/runtime.sqlite3");
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let mut stmt = conn
        .prepare("SELECT pid FROM thread_runtime WHERE thread_id = ?1")
        .ok()?;
    stmt.query_row(rusqlite::params![thread_id], |row| row.get(0))
        .ok()
        .flatten()
}

/// Read thread status + outcome_code from the projection DB.
/// Returns `(status, outcome_code, error_detail)` read directly from the
/// persisted `thread_results` columns.
fn read_thread_outcome_full(
    state_path: &Path,
    thread_id: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    let db_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/projection.sqlite3");
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;

    let mut stmt = conn
        .prepare("SELECT status, outcome_code, error FROM thread_results WHERE thread_id = ?1")
        .ok()?;
    let (status, outcome_code, error_json) = stmt
        .query_row(rusqlite::params![thread_id], |row| {
            let status: String = row.get(0)?;
            let outcome_code: Option<String> = row.get(1)?;
            let error: Option<String> = row.get(2)?;
            Ok((status, outcome_code, error))
        })
        .ok()?;

    Some((status, outcome_code, error_json))
}

/// Read thread status + outcome_code from the projection DB.
/// Returns `(status, outcome_code)` read from the persisted columns.
fn read_thread_outcome(state_path: &Path, thread_id: &str) -> Option<(String, Option<String>)> {
    read_thread_outcome_full(state_path, thread_id).map(|(s, oc, _)| (s, oc))
}

#[tokio::test(flavor = "multi_thread")]
async fn generic_tool_resume_does_not_require_provider_secret() {
    // ── Setup ────────────────────────────────────────────────────
    //
    // Vault is empty (no ZEN_API_KEY). A tool with native_resume: true is
    // planted in the project.
    //
    // The initial execute succeeds. On daemon restart, the reconciler resumes
    // the orphaned thread via `run_existing_detached`. Generic tool resume
    // must not derive provider auth from model routing or fail with
    // required_secret_missing.

    // Mock provider — not needed for the test assertions; kept only
    // to back the zero-request assertion at the end.
    let mock = MockProvider::start(vec![MockResponse::Text("unused".into())]).await;

    let (mut h, fixture) = DaemonHarness::start_fast_with(
        |state_path: &Path, _user: &Path, f: &FastFixture| {
            register_standard_bundle(state_path, f)?;
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

    // Create a project with a native_resume tool.
    let project = tempfile::tempdir().expect("project tempdir");
    plant_native_resume_tool(project.path(), &fixture.publisher).expect("plant native_resume tool");

    // ── Phase 1: Execute the tool (detached) ────────────────────
    let body = serde_json::json!({
        "item_ref": "tool:resume/resume_test",
        "project_path": project.path().to_str().unwrap(),
        "parameters": {},
        "launch_mode": "detached",
    });
    let body_bytes = serde_json::to_vec(&body).expect("serialize body");
    let user_key = h.user_key.as_ref().expect("fast fixture user key");
    let node_key = h.node_key.as_ref().expect("fast fixture node key");
    let signed_headers =
        common::build_signed_headers_for_bytes(user_key, node_key, "POST", "/execute", &body_bytes);
    let mut req = reqwest::Client::new()
        .post(format!("http://{}/execute", h.bind))
        .header("content-type", "application/json")
        .body(body_bytes)
        .timeout(Duration::from_secs(30));
    for (k, v) in signed_headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.expect("/execute send failed");
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
        if let Some((status, outcome, full_error)) =
            read_thread_outcome_full(&h.state_path, &thread_id)
        {
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
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
        );
    })
    .await
    .expect("respawn daemon");

    // ── Phase 3: Poll for resumed running status ─────────────
    //
    // Generic tool resume no longer runs provider preflight by hidden
    // coupling. The reconciler should restart the native subprocess and
    // leave the thread running, not fail with required_secret_missing.
    let outcome_deadline = std::time::Instant::now() + Duration::from_secs(15);
    let resumed_pid = loop {
        if let Some(new_pid) = read_pid_from_runtime_db(&h.state_path, &thread_id) {
            if new_pid != pid {
                break new_pid;
            }
        }
        if let Some((status, outcome, full_error)) =
            read_thread_outcome_full(&h.state_path, &thread_id)
        {
            assert_ne!(
                outcome.as_deref(),
                Some("required_secret_missing"),
                "generic tool resume must not require provider auth; error={full_error:?}"
            );
            if status == "failed" || status == "completed" || status == "killed" {
                let stderr = h.drain_stderr_nonblocking().await;
                panic!(
                    "thread {thread_id} reached terminal status after resume; \
                     status={status} outcome_code={outcome:?} error={full_error:?}\n\
                     Daemon stderr (tail):\n{}",
                    &stderr[stderr.len().saturating_sub(3000)..]
                );
            }
        }
        if std::time::Instant::now() > outcome_deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "thread {thread_id} did not get a replacement PID within 15s. \
                 Last known state: {:?}. Daemon stderr (tail):\n{}",
                read_thread_outcome(&h.state_path, &thread_id),
                &stderr[stderr.len().saturating_sub(2000)..]
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    if let Some((status, outcome_code)) = read_thread_outcome(&h.state_path, &thread_id) {
        assert_ne!(
            outcome_code.as_deref(),
            Some("required_secret_missing"),
            "generic tool resume must not fail with provider missing"
        );
        assert!(
            status == "running" || status == "created",
            "thread should remain non-terminal after generic tool resume; got status={status} outcome={outcome_code:?}"
        );
    }

    // Clean up the resumed subprocess so test teardown does not leave a
    // sleeping child behind.
    unsafe {
        libc::kill(resumed_pid as i32, libc::SIGKILL);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The mock provider must have received zero requests: generic tool resume
    // neither preflights provider auth nor calls a provider.
    let captured = mock.captured_headers().await;
    assert!(
        captured.is_empty(),
        "mock provider received {} request(s) during generic tool resume",
        captured.len(),
    );

    drop(project);
    drop(mock);
}

// ── Test 5: SSE stream emits structured required_secret_missing ──
//
// F3: proves the `/execute/stream` endpoint surfaces a missing provider secret
// as a `thread_failed` terminal whose payload carries the structured error
// fields (env_var, source_kind, source_name, remediation). The secret preflight
// runs inside the spawned launch (after the thread is created), so the failure
// is a persisted thread lifecycle terminal — not a pre-spawn `stream_error`,
// which is reserved for synchronous gateway rejections before any thread exists.

#[tokio::test(flavor = "multi_thread")]
async fn execute_stream_emits_structured_required_secret_missing_event() {
    use base64::Engine;
    use lillux::crypto::Signer;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Set up a mock provider (won't be called — preflight blocks).
    let mock = MockProvider::start(vec![MockResponse::Text("unused".into())]).await;
    let mock_url = mock.base_url.clone();

    let (h, fixture) = DaemonHarness::start_fast_with(
        |state_path: &Path, _user: &Path, f: &FastFixture| {
            register_standard_bundle(state_path, f)?;
            plant_empty_vault(state_path)?;
            // Authorize the user key so /execute/stream accepts signed requests.
            common::fast_fixture::write_authorized_key_signed_by(state_path, &f.user, &f.node)?;
            Ok(())
        },
        |cmd| {
            cmd.env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=debug".into()),
            );
            cmd.env("RYEOS_ALLOW_PROJECT_PROVIDER_CONFIG", "1");
        },
    )
    .await
    .expect("daemon starts");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_provider_config(
        project.path(),
        "zen",
        &mock_url,
        Some("ZEN_API_KEY"),
        &fixture.publisher,
    )
    .expect("plant provider");
    plant_model_routing_to(project.path(), "zen", &fixture.publisher).expect("plant routing");
    plant_directive(project.path(), "test/sse_secret", &fixture.publisher)
        .expect("plant directive");

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
    let user_fp = lillux::signature::compute_fingerprint(&fixture.user.verifying_key());
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

    // Parse SSE lines looking for the `thread_failed` terminal whose payload
    // carries a structured `required_secret_missing` error:
    //   event: thread_failed
    //   data: {"payload":{"outcome_code":"required_secret_missing",
    //          "error":{"code":"required_secret_missing","env_var":...}},...}
    let mut found_error = false;
    let mut current_event_type = String::new();
    for line in text.lines() {
        if let Some(ev) = line.strip_prefix("event: ") {
            current_event_type = ev.trim().to_string();
        }
        if let Some(data) = line.strip_prefix("data: ") {
            if current_event_type == "thread_failed" {
                let parsed: serde_json::Value =
                    serde_json::from_str(data.trim()).unwrap_or(serde_json::json!({}));

                let outcome_code = parsed
                    .pointer("/payload/outcome_code")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let error = parsed
                    .pointer("/payload/error")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let code = error.get("code").and_then(|v| v.as_str()).unwrap_or_default();
                if outcome_code == "required_secret_missing" && code == "required_secret_missing" {
                    found_error = true;

                    // Assert structured fields.
                    let env_var = error
                        .get("env_var")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(
                        env_var, "ZEN_API_KEY",
                        "thread_failed error must have env_var=ZEN_API_KEY; got: {env_var}"
                    );

                    let source_kind = error
                        .get("source_kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(
                        source_kind, "provider",
                        "thread_failed error must have source_kind=provider; got: {source_kind}"
                    );

                    let source_name = error
                        .get("source_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert_eq!(
                        source_name, "zen",
                        "thread_failed error must have source_name=zen; got: {source_name}"
                    );

                    let remediation = error
                        .get("remediation")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert!(
                        remediation.contains("ryeos vault set --name ZEN_API_KEY"),
                        "remediation must contain vault set command; got: {remediation}"
                    );

                    // The error message must be a non-empty string.
                    let error_msg = error
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert!(
                        !error_msg.is_empty(),
                        "thread_failed error must have non-empty error message"
                    );
                    break;
                }
            }
        }
    }

    assert!(
        found_error,
        "SSE stream must contain a thread_failed terminal whose payload carries a \
         structured required_secret_missing error; raw SSE output:\n{text}"
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
