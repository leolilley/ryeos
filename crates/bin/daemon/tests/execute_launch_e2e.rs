//! `/execute/launch` accepted-mode durability tests.
//!
//! Accepted/background launch admits any kind whose schema declares it
//! root-executable (`execution.thread_profile.root_executable`) — tool,
//! directive, graph, and knowledge alike — gated by the kind registry rather
//! than a hardcoded kind list.
//!
//! A returned `thread_id` always reaches a terminal, inspectable state. Cheap
//! pre-handoff failures (terminal `executor_id`, invalid tool `requires`,
//! missing method arg, direct-runtime caps, in-process, non-root-executable,
//! projection, and trust policy) are rejected synchronously without exposing
//! the pre-minted id. Once an id is returned, a scheduled subprocess task owns
//! the durable row and exact execution request; later failures finalize that
//! row, so accepted launch never returns a phantom or leaves `created` stuck.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::fast_fixture::{
    register_config_fixture_bundle, register_standard_bundle, write_authorized_key_with_scopes,
    FastFixture,
};
use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::SigningKey;
use serde_json::{json, Value};

fn accepted_policy(has_project: bool) -> ryeos_app::execution_policy::ExecutionPolicy {
    use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};

    if has_project {
        ExecutionPolicy::local_live(ExecutionResponse::Accepted)
    } else {
        ExecutionPolicy::projectless(ExecutionResponse::Accepted)
    }
}

fn unwrap_result(status: reqwest::StatusCode, body: &Value, ctx: &str) -> Value {
    assert!(
        status.is_success(),
        "{ctx}: expected success, got {status}; body={body}"
    );
    body.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{ctx}: response had no result field; body={body}"))
}

async fn thread_get(h: &DaemonHarness, thread_id: &str) -> Value {
    let (status, body) = h
        .post_execute(
            "service:threads/get",
            ".",
            json!({ "thread_id": thread_id }),
        )
        .await
        .expect("post threads/get");
    unwrap_result(status, &body, "threads.get")
}

/// Poll until the thread reaches a terminal status, returning the
/// `threads.get` result.
async fn wait_for_terminal_thread(h: &DaemonHarness, thread_id: &str) -> Value {
    // This suite runs many full fixture daemons concurrently. Accepted launch
    // must return its durable id promptly, but the background runtime is allowed
    // to finish under host contention; keep those two contracts distinct.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let result = thread_get(h, thread_id).await;
        if let Some(status) = result
            .get("thread")
            .and_then(|t| t.get("status"))
            .and_then(Value::as_str)
        {
            if ryeos_state::objects::ThreadStatus::from_str_lossy(status)
                .is_some_and(|s| s.is_terminal())
            {
                return result;
            }
        }
        assert!(
            Instant::now() < deadline,
            "thread {thread_id} never reached a terminal state; last={result}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Assert an accepted-launch response is 202 with a `thread_id`, returning it.
fn unwrap_accepted(status: reqwest::StatusCode, body: &Value) -> String {
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "body={body}");
    assert_eq!(body.get("status").and_then(Value::as_str), Some("accepted"));
    body.get("thread_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("accepted response missing thread_id: {body}"))
        .to_string()
}

/// Assert that the inspectable thread's id matches the accepted id and that
/// it completed. Catches both phantom ids (wrong/uncreated thread) and silent
/// post-creation runtime failures.
fn assert_completed(thread: &Value, expected_id: &str) {
    assert_eq!(
        thread
            .get("thread")
            .and_then(|t| t.get("thread_id"))
            .and_then(Value::as_str),
        Some(expected_id),
        "accepted-launch thread id mismatch: {thread}"
    );
    assert_eq!(
        thread
            .get("thread")
            .and_then(|t| t.get("status"))
            .and_then(Value::as_str),
        Some("completed"),
        "accepted-launch thread did not complete: {thread}"
    );
}

/// Plant an UNSIGNED knowledge item (no signature line) so resolution yields
/// an Unsigned trust class that fails the projection trust gate.
fn plant_unsigned_knowledge_item(project: &Path, rel_path: &str) -> anyhow::Result<()> {
    let path = project.join(format!(".ai/knowledge/{rel_path}.md"));
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    std::fs::create_dir_all(path.parent().expect("knowledge parent dir"))?;
    let content = format!(
        "```yaml\nname: {stem}\ncategory: \"{dir_relative}\"\n```\n\nUnsigned knowledge fixture.\n"
    );
    std::fs::write(&path, content)?;
    Ok(())
}

/// Plant an UNSIGNED directive (no signature line).
fn plant_unsigned_directive(project: &Path, rel_path: &str) -> anyhow::Result<()> {
    let path = project.join(format!(".ai/directives/{rel_path}.md"));
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let content = format!(
        "---\nname: {stem}\ncategory: \"{dir_relative}\"\ndescription: \"unsigned fixture\"\ninputs: []\nmodel:\n  tier: general\n---\nSay hello.\n"
    );
    std::fs::write(&path, content)?;
    Ok(())
}

/// Plant a trivial return-only graph in the project's `.ai/graphs/`.
fn plant_smoke_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: ""
version: "1.0.0"
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("smoke.yaml"), signed)?;
    Ok(())
}

/// Plant ZEN_API_KEY in the sealed vault so runtime preflight passes.
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

/// Plant a minimal directive that resolves, trust-verifies, and runs against
/// the mock provider.
fn plant_directive(project: &Path, rel_path: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let path = project.join(format!(".ai/directives/{rel_path}.md"));
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let body = format!(
        r#"---
name: {stem}
category: "{dir_relative}"
description: "accepted-launch e2e fixture"
inputs: []
model:
  tier: general
---
Say hello.
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Plant a minimal knowledge item (method-dispatched via `compose`/`query`).
fn plant_knowledge_item(project: &Path, rel_path: &str, signer: &SigningKey) -> anyhow::Result<()> {
    let path = project.join(format!(".ai/knowledge/{rel_path}.md"));
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    std::fs::create_dir_all(path.parent().expect("knowledge parent dir"))?;
    let content = format!(
        r#"```yaml
name: {stem}
category: "{dir_relative}"
```

An accepted-launch knowledge fixture.
"#
    );
    let signed = lillux::signature::sign_content(&content, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Plant a signed wrapper tool that declares `requires.capabilities.manifest`
/// with no signed bundle manifest backing it — so manifest-cap derivation
/// fails. Resolves + trust-verifies (signed) and has an executor_id, so it
/// reaches the manifest check in the terminal preflight.
fn plant_wrapper_tool_bad_manifest(
    project: &Path,
    rel_path: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = project.join(format!(".ai/tools/{rel_path}.yaml"));
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    std::fs::create_dir_all(path.parent().expect("tool parent dir"))?;
    let body = format!(
        r#"category: "{dir_relative}"
version: "1.0.0"
tool_type: "subprocess"
executor_id: "@subprocess"
description: "wrapper tool declaring manifest runtime caps with no manifest backing"
config:
  command: "/bin/true"
  timeout_secs: 30
requires:
  capabilities:
    manifest:
      runtime_authority:
        runtime_vault:
          - namespace: "testns"
            operations: ["get"]
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Plant a chat_completions provider pointed at the mock server.
fn plant_mock_provider(
    project: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
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

/// Map the `general` tier to the mock provider.
fn plant_model_routing(project: &Path, signer: &SigningKey) -> anyhow::Result<()> {
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

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_returns_inspectable_thread_id() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "tool:ryeos/core/identity/public_key",
                "ref_bindings": {},
                "project_path": null,
                "parameters": {},
                "execution_policy": accepted_policy(false)
            }),
        )
        .await
        .expect("post /execute/launch");

    let thread_id = unwrap_accepted(status, &body);
    let thread = wait_for_terminal_thread(&h, &thread_id).await;
    assert_completed(&thread, &thread_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_admits_graph_ref() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_smoke_graph(project.path(), &fixture.publisher).expect("plant smoke graph");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "graph:smoke",
                "ref_bindings": {},
                "project_path": project.path().to_str().unwrap(),
                "parameters": {},
                "execution_policy": accepted_policy(true)
            }),
        )
        .await
        .expect("post /execute/launch graph ref");

    let thread_id = unwrap_accepted(status, &body);
    let thread = wait_for_terminal_thread(&h, &thread_id).await;
    assert_completed(&thread, &thread_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_admits_directive_ref() {
    let mock = MockProvider::start(vec![MockResponse::Text("hello".into())]).await;
    let mock_url = mock.base_url.clone();

    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-execute-launch-model-config",
            fixture,
            |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
        )
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_directive(project.path(), "test/launch", &fixture.publisher).expect("plant directive");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "directive:test/launch",
                "ref_bindings": { "model": "directive:test/launch" },
                "project_path": project.path().to_str().unwrap(),
                "parameters": {},
                "execution_policy": accepted_policy(true)
            }),
        )
        .await
        .expect("post /execute/launch directive ref");

    let thread_id = unwrap_accepted(status, &body);
    let thread = wait_for_terminal_thread(&h, &thread_id).await;
    assert_completed(&thread, &thread_id);
}

/// Knowledge is method-dispatched. This proves the method-dispatch path
/// honors the pre-minted thread id — the returned id is the id created, not a
/// phantom.
#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_admits_knowledge_query() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_knowledge_item(project.path(), "test/fact", &fixture.publisher).expect("plant knowledge");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "knowledge:test/fact",
                "ref_bindings": {},
                "project_path": project.path().to_str().unwrap(),
                "call": { "method": "query", "args": { "query": "accepted" } },
                "execution_policy": accepted_policy(true)
            }),
        )
        .await
        .expect("post /execute/launch knowledge query");

    let thread_id = unwrap_accepted(status, &body);
    let thread = wait_for_terminal_thread(&h, &thread_id).await;
    assert_completed(&thread, &thread_id);
}

/// A method launch missing a required arg must fail in preflight — before a
/// thread_id is minted (no phantom).
#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_method_missing_required_arg_is_rejected() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_knowledge_item(project.path(), "test/fact", &fixture.publisher).expect("plant knowledge");

    // `query` requires the `query` arg; omit it.
    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "knowledge:test/fact",
                "ref_bindings": {},
                "project_path": project.path().to_str().unwrap(),
                "call": { "method": "query", "args": {} },
                "execution_policy": accepted_policy(true)
            }),
        )
        .await
        .expect("post /execute/launch knowledge query missing arg");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    assert!(
        body.get("thread_id").is_none(),
        "method missing-arg rejection must not include thread_id: {body}"
    );
}

/// Corpus method scope changes the projected data, not launch authority: an
/// unsigned invoked root receives the same typed trust-policy rejection as an
/// envelope root and never exposes its pre-minted id.
#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_unsigned_knowledge_is_rejected_by_trust_policy() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };
    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_unsigned_knowledge_item(project.path(), "test/unsigned").expect("plant knowledge");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "knowledge:test/unsigned",
                "ref_bindings": {},
                "project_path": project.path().to_str().unwrap(),
                "call": { "method": "query", "args": { "query": "x" } },
                "execution_policy": accepted_policy(true)
            }),
        )
        .await
        .expect("post /execute/launch unsigned knowledge");

    assert_trust_policy_rejection(status, &body);
}

/// An unsigned managed root is a typed trust-policy rejection, never an
/// opaque internal failure. The route does not expose the pre-minted id when
/// the launcher refuses the execution before subprocess handoff.
#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_unsigned_directive_is_rejected_by_trust_policy() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)
    };
    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with standard bundle");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_unsigned_directive(project.path(), "test/unsigned").expect("plant directive");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "directive:test/unsigned",
                "ref_bindings": { "model": "directive:test/unsigned" },
                "project_path": project.path().to_str().unwrap(),
                "parameters": {},
                "execution_policy": accepted_policy(true)
            }),
        )
        .await
        .expect("post /execute/launch unsigned directive");

    assert_trust_policy_rejection(status, &body);
}

fn assert_trust_policy_rejection(status: reqwest::StatusCode, body: &Value) {
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("effective_trust_unsigned"),
        "body={body}"
    );
    assert_eq!(body.get("retryable").and_then(Value::as_bool), Some(false));
    assert!(
        body.get("thread_id").is_none(),
        "trust rejection must not expose the pre-minted thread id: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_terminal_tool_without_executor_id_is_rejected() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "tool:ryeos/core/subprocess/execute",
                "ref_bindings": {},
                "project_path": null,
                "parameters": {},
                "execution_policy": accepted_policy(false)
            }),
        )
        .await
        .expect("post /execute/launch terminal tool");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("root_executor_missing"),
        "expected root_executor_missing, got: {body}"
    );
    assert!(
        body.get("thread_id").is_none(),
        "terminal-tool rejection must not include thread_id: {body}"
    );
}

/// A terminal tool whose `requires.capabilities.manifest` has no signed
/// manifest backing must be rejected synchronously (the full manifest-cap
/// derivation runs in preflight), before a thread_id is minted.
#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_terminal_tool_bad_manifest_requires_is_rejected() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_wrapper_tool_bad_manifest(project.path(), "test/wrapper", &fixture.publisher)
        .expect("plant wrapper tool");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "tool:test/wrapper",
                "ref_bindings": {},
                "project_path": project.path().to_str().unwrap(),
                "parameters": {},
                "execution_policy": accepted_policy(true)
            }),
        )
        .await
        .expect("post /execute/launch wrapper tool bad manifest");

    assert!(
        status.is_client_error(),
        "expected client error, got {status}; body={body}"
    );
    assert!(
        body.get("thread_id").is_none(),
        "bad-manifest rejection must not include thread_id: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_in_process_service_is_rejected() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "service:node/status",
                "ref_bindings": {},
                "project_path": null,
                "parameters": {},
                "execution_policy": accepted_policy(false)
            }),
        )
        .await
        .expect("post /execute/launch in-process service");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    let error = body.get("error").and_then(Value::as_str).unwrap_or("");
    assert!(
        error.contains("pre-minted thread root") && error.contains("without --async"),
        "expected accepted-launch persistence rejection, got: {body}"
    );
    assert!(
        body.get("thread_id").is_none(),
        "in-process service rejection must not include thread_id: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_non_root_executable_ref_does_not_return_phantom_thread_id() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "config:some/thing",
                "ref_bindings": {},
                "project_path": null,
                "parameters": {},
                "execution_policy": accepted_policy(false)
            }),
        )
        .await
        .expect("post /execute/launch non-root-executable ref");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    let error = body.get("error").and_then(Value::as_str).unwrap_or("");
    assert!(
        error.contains("root-executable"),
        "expected root-executable rejection, got: {body}"
    );
    assert!(
        body.get("thread_id").is_none(),
        "non-root-executable ref response must not include thread_id: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_invalid_item_does_not_return_phantom_thread_id() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "tool:no/such-tool",
                "ref_bindings": {},
                "project_path": null,
                "parameters": {},
                "execution_policy": accepted_policy(false)
            }),
        )
        .await
        .expect("post /execute/launch invalid item");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    assert!(
        body.get("thread_id").is_none(),
        "invalid item response must not include thread_id: {body}"
    );
}

/// A direct `runtime:` launch whose caller lacks the runtime's registry caps
/// is rejected before a thread_id is minted (no phantom on cap failure).
#[tokio::test(flavor = "multi_thread")]
async fn execute_launch_direct_runtime_missing_cap_is_rejected() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_standard_bundle(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        // Grant only the per-ref execute cap, NOT the runtime's required
        // `runtime.execute` cap, so the direct-runtime cap gate rejects.
        write_authorized_key_with_scopes(
            state_path,
            &fixture.user,
            &fixture.node,
            &["ryeos.execute.runtime.directive-runtime"],
        )?;
        Ok(())
    };
    let (h, _fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with standard bundle");

    let (status, body) = h
        .post_json(
            "/execute/launch",
            json!({
                "item_ref": "runtime:directive-runtime",
                "ref_bindings": {},
                "project_path": null,
                "parameters": {},
                "execution_policy": accepted_policy(false)
            }),
        )
        .await
        .expect("post /execute/launch runtime ref");

    assert!(
        status.is_client_error(),
        "expected client error, got {status}; body={body}"
    );
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("insufficient_caps"),
        "expected insufficient_caps rejection: {body}"
    );
    assert!(
        body.get("thread_id").is_none(),
        "runtime cap rejection must not include thread_id: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_subprocess_terminal_execution_is_bad_request() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    let (status, body) = h
        .post_execute(
            "tool:ryeos/core/subprocess/execute",
            ".",
            json!({ "command": "/bin/true" }),
        )
        .await
        .expect("post direct subprocess terminal");

    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("root_executor_missing"),
        "unexpected direct terminal rejection body: {body}"
    );
    let error = body.get("error").and_then(Value::as_str).unwrap_or("");
    assert!(error.contains("@subprocess"), "missing remediation: {body}");
    assert!(error.contains("config"), "missing config guidance: {body}");
}
