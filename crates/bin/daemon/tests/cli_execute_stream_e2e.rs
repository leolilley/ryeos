//! Live CLI e2e for `ryeos execute` streaming.
//!
//! Boots a real daemon with a mock provider + a project directive, then runs
//! the actual `ryeos` binary against it (RYEOS_APP_ROOT gives the CLI both the
//! daemon address and the operator signing key). `--stream` forces the live
//! path even though the subprocess stdout is a pipe, so we can assert:
//!   - a successful run streams a terminal and exits 0 and prints the final
//!     result (with the `/execute` shape, incl. outcome_code);
//!   - a failing run exits non-zero;
//!   - `--json` prints only the buffered JSON result (no live markers).

mod common;

use std::path::Path;

use common::fast_fixture::{
    register_config_fixture_bundle, register_standard_bundle, write_authorized_key_signed_by,
    FastFixture,
};
use common::mock_provider::{MockProvider, MockResponse};
use common::{ryeos_binary, ryeosd_binary, DaemonHarness};

fn plant_mock_provider(
    project: &Path,
    mock_base_url: &str,
    signer: &lillux::crypto::SigningKey,
) -> anyhow::Result<()> {
    let dir = project.join(".ai/config/ryeos-runtime/model-providers");
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
pricing:
  input_per_million: 0.0
  output_per_million: 0.0
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

fn plant_model_routing(project: &Path, signer: &lillux::crypto::SigningKey) -> anyhow::Result<()> {
    let dir = project.join(".ai/config/ryeos-runtime");
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

fn plant_directive(project: &Path, signer: &lillux::crypto::SigningKey) -> anyhow::Result<()> {
    let path = project.join(".ai/directives/test/cli_stream.md");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let body = r#"---
name: cli_stream
category: "test"
description: "CLI streaming e2e directive"
model:
  tier: general
---
Say hello.
"#;
    let signed = lillux::signature::sign_content(body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

fn plant_execute_stream_route(
    state_path: &Path,
    signer: &lillux::crypto::SigningKey,
) -> anyhow::Result<()> {
    let dir = state_path.join(".ai/node/routes");
    std::fs::create_dir_all(&dir)?;
    let body = r#"id: execute/stream
path: /execute/stream
methods:
  - POST
auth: ryeos_signed
limits:
  body_bytes_max: 10485760
  timeout_ms: 0
  concurrent_max: 32
request:
  body: json
response:
  mode: event_stream
  source: dispatch_launch
  source_config:
    keep_alive_secs: 15
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("execute-stream.yaml"), signed)?;
    Ok(())
}

/// Boot a daemon with the mock provider (given canned responses) + the project
/// directive. Returns the harness and the project path to pass via `-p`.
async fn boot(responses: Vec<MockResponse>) -> (DaemonHarness, String) {
    let mock = MockProvider::start(responses).await;
    let mock_url = mock.base_url.clone();

    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_standard_bundle(state_path, fixture)?;
            register_config_fixture_bundle(
                state_path,
                "fixture-cli-model-config",
                fixture,
                |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
            )?;
            plant_execute_stream_route(state_path, &fixture.publisher)?;
            write_authorized_key_signed_by(state_path, &fixture.user, &fixture.node)?;
            Ok(())
        };

    let (h, fixture) = DaemonHarness::start_fast_with(plant, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,ryeosd=warn".into()),
        );
    })
    .await
    .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(project.path(), &fixture.publisher).expect("plant directive");
    let project_path = project.path().to_str().unwrap().to_string();

    std::mem::forget(project);
    std::mem::forget(mock);
    (h, project_path)
}

fn run_cli(h: &DaemonHarness, args: &[&str]) -> std::process::Output {
    std::process::Command::new(ryeos_binary())
        .args(args)
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("RYEOSD_BIN", ryeosd_binary())
        .env("HOME", h.user_space.path())
        .output()
        .expect("spawn ryeos")
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_execute_stream_success_exits_zero_and_prints_result() {
    let (h, project) = boot(vec![
        MockResponse::Text("Hello ".into()),
        MockResponse::Text("world".into()),
    ])
    .await;

    let out = run_cli(
        &h,
        &[
            "-p",
            &project,
            "execute",
            "directive:test/cli_stream",
            "--ref-binding",
            "model=directive:test/cli_stream",
            "--stream",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected exit 0; code={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status.code()
    );
    // Live terminal marker on stdout.
    assert!(
        stdout.contains("thread_completed"),
        "no terminal marker in stdout: {stdout}"
    );
    // Final result printed in the /execute shape.
    assert!(
        stdout.contains("outcome_code"),
        "no final result in stdout: {stdout}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_execute_stream_failure_exits_nonzero() {
    // Empty mock queue → provider returns 500 on the first call → the run fails
    // → thread_failed → the CLI must exit non-zero.
    let (h, project) = boot(vec![]).await;

    let out = run_cli(
        &h,
        &[
            "-p",
            &project,
            "execute",
            "directive:test/cli_stream",
            "--ref-binding",
            "model=directive:test/cli_stream",
            "--stream",
        ],
    );
    assert!(
        !out.status.success(),
        "expected non-zero exit on a failing run\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_execute_json_flag_prints_buffered_json() {
    let (h, project) = boot(vec![
        MockResponse::Text("Hello ".into()),
        MockResponse::Text("world".into()),
    ])
    .await;

    let out = run_cli(
        &h,
        &[
            "-p",
            &project,
            "execute",
            "directive:test/cli_stream",
            "--ref-binding",
            "model=directive:test/cli_stream",
            "--json",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected exit 0; code={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status.code()
    );
    // Buffered: no live stream markers, and stdout is a single JSON value.
    assert!(
        !stdout.contains('▶'),
        "should not stream with --json: {stdout}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout.trim()).is_ok(),
        "expected buffered JSON output, got: {stdout}"
    );
}
