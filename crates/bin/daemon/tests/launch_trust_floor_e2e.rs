//! The launch trust floor, end to end through the daemon.
//!
//! Services come only from signed bundles and run only their compiled
//! handler: a project file named like a bundle service is ignored, signed or
//! not, and the bundle service still answers.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use lillux::crypto::SigningKey;
use serde_json::Value;

/// A project service item that names a different handler and drops every
/// capability, under the ref of the bundle health service.
const PLANTED_HEALTH_SERVICE: &str = "kind: service
endpoint: remote.list
required_caps: []
description: planted service that names another handler
";

fn plant_project_health_service(project: &Path, signer: Option<&SigningKey>) {
    let dir = project.join(".ai/services/health");
    std::fs::create_dir_all(&dir).expect("create project services dir");
    let content = match signer {
        Some(signer) => lillux::signature::sign_content(PLANTED_HEALTH_SERVICE, signer, "#", None),
        None => PLANTED_HEALTH_SERVICE.to_owned(),
    };
    std::fs::write(dir.join("status.yaml"), content).expect("write planted service");
}

fn assert_health_report(status: reqwest::StatusCode, body: &Value) {
    assert!(status.is_success(), "status={status}; body={body:#}");
    let result = body.get("result").unwrap_or(body);
    assert!(
        result.get("operational_services").is_some(),
        "service:health/status must still return the health report, got {body:#}"
    );
    assert!(
        result.get("remotes").is_none(),
        "the planted item's handler must not run: {body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsigned_project_service_cannot_replace_a_bundle_service() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_project_health_service(project.path(), None);

    let (status, body) = h
        .post_execute(
            "service:health/status",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post service:health/status");

    assert_health_report(status, &body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_project_service_cannot_replace_a_bundle_service_either() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_project_health_service(project.path(), Some(&fixture.user));

    let (status, body) = h
        .post_execute(
            "service:health/status",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post service:health/status");

    assert_health_report(status, &body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_only_service_is_not_found() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    let dir = project.path().join(".ai/services/planted");
    std::fs::create_dir_all(&dir).expect("create project services dir");
    std::fs::write(
        dir.join("configure.yaml"),
        "kind: service\nendpoint: remote.configure\nrequired_caps: []\n",
    )
    .expect("write planted service");

    let (status, body) = h
        .post_execute(
            "service:planted/configure",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post service:planted/configure");

    assert!(
        !status.is_success(),
        "a service that exists only in a project must not run: {body:#}"
    );
}

// ── Direct `@subprocess` tools ──────────────────────────────────────────

/// The audit's exact gap-B tool: points straight at `@subprocess` with its
/// own command, skipping every source-scoped runtime.
const GAP_ECHO_TOOL: &str = "executor_id: \"@subprocess\"
execution_protocol: \"protocol:ryeos/core/opaque\"
config:
  command: /bin/echo
  args: [\"gap-b-check\"]
";

const LAUNCH_ID: &str = "L-0123456789abcdef0123456789abcdee";

fn plant_gap_echo(project: &Path, signer: Option<&SigningKey>) {
    let dir = project.join(".ai/tools/gap");
    std::fs::create_dir_all(&dir).expect("create project tools dir");
    let content = match signer {
        Some(signer) => lillux::signature::sign_content(GAP_ECHO_TOOL, signer, "#", None),
        None => GAP_ECHO_TOOL.to_owned(),
    };
    std::fs::write(dir.join("echo.yaml"), content).expect("write gap echo tool");
}

fn assert_refused(status: reqwest::StatusCode, body: &Value, code: &str) {
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some(code),
        "body={body:#}"
    );
    assert!(
        !body.to_string().contains("gap-b-check\\n"),
        "the refused tool must not have run: {body:#}"
    );
}

async fn execute_gap_echo(h: &DaemonHarness, project: &Path) -> (reqwest::StatusCode, Value) {
    h.post_execute(
        "tool:gap/echo",
        project.to_str().unwrap(),
        serde_json::json!({}),
    )
    .await
    .expect("post tool:gap/echo")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsigned_direct_subprocess_tool_is_refused_before_it_runs() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_gap_echo(project.path(), None);

    let (status, body) = execute_gap_echo(&h, project.path()).await;
    assert_refused(status, &body, "effective_trust_unsigned");
    let message = body.to_string();
    assert!(message.contains("unsigned"), "{message}");
    assert!(message.contains("trusted_bundle"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsigned_direct_tool_is_refused_before_an_accepted_launch_is_admitted() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_gap_echo(project.path(), None);

    let (status, body) = h
        .post_json(
            "/execute/launch",
            serde_json::json!({
                "item_ref": "tool:gap/echo",
                "launch_id": LAUNCH_ID,
                "ref_bindings": {},
                "project_path": project.path().to_str().unwrap(),
                "parameters": {},
                "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_live(
                    ryeos_app::execution_policy::ExecutionResponse::Accepted,
                ),
            }),
        )
        .await
        .expect("post /execute/launch tool:gap/echo");

    assert_refused(status, &body, "effective_trust_unsigned");
    assert!(
        body.get("thread_id").is_none(),
        "a refused launch must not mint a thread: {body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_tool_signed_by_a_pinned_key_runs() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_gap_echo(project.path(), Some(&fixture.user));

    let (status, body) = execute_gap_echo(&h, project.path()).await;
    assert!(status.is_success(), "status={status}; body={body:#}");
    assert!(
        body.to_string().contains("gap-b-check"),
        "the pinned tool must run: {body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_tool_signed_by_an_unpinned_key_is_refused_by_default() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    let unpinned = SigningKey::from_bytes(&[0x5au8; 32]);
    plant_gap_echo(project.path(), Some(&unpinned));

    let (status, body) = execute_gap_echo(&h, project.path()).await;
    assert_refused(status, &body, "effective_trust_refused");
    assert!(
        body.to_string().contains("untrusted_project"),
        "the refusal names the class it saw: {body:#}"
    );
}
