//! A release service called as a graph step is judged against the key that
//! requested the graph run.
//!
//! Every `bundle-release/*` operation served through the generic handler
//! requires the admitted node operator. When one is the action of a graph
//! node, the key the check sees must be the requesting key, not the node's
//! key or a runtime's:
//!
//! - the operator's key runs the graph and its step is never refused as a
//!   non-operator (it may still fail for another reason, such as the node
//!   having no release authority configured);
//! - a second key that is authorized for the graph, the service and the live
//!   project, but is not the operator, runs the same graph and its step is
//!   refused with an operator refusal.

use ryeosd::test_support as common;

use std::path::Path;
use std::time::Duration;

use common::DaemonHarness;
use common::fast_fixture::{
    FastFixture, register_presigned_fixture_bundle, register_workflow_bundles,
    write_authorized_key_with_scopes,
};
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{
    ExecutionPolicy, ExecutionResponse, LIVE_PROJECT_READ_CAPABILITY, LIVE_PROJECT_WRITE_CAPABILITY,
};
use serde_json::{Value, json};

const SERVICE_CAP: &str = "ryeos.execute.service.bundle-release/status";
const GRAPH_CAP: &str = "ryeos.execute.graph.release_status";

/// Every refusal an operator-only handler can answer a non-operator with.
const OPERATOR_REFUSAL_CODES: &[&str] = &[
    "local_operator_required",
    "operator_owner_not_spoken_for",
    "operator_class_required",
    "operator_rejects_remote_node",
];

/// The sentences those refusals carry as context, for a step error that keeps
/// the message of the failure but not its code.
const OPERATOR_REFUSAL_SENTENCES: &[&str] = &[
    "action requires a local_client configured operator",
    "local action requires the configured local operator",
    "a remote_principal is an operator only if its key speaks for the node operator's owner",
    "action requires an authenticated admitted-operator class",
    "operator actions reject remote_node grants",
];

fn second_key() -> SigningKey {
    SigningKey::from_bytes(&[0x37_u8; 32])
}

/// A graph whose one step calls `service:bundle-release/status`.
fn plant_release_status_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = format!(
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - {SERVICE_CAP}
config:
  start: release_status
  nodes:
    release_status:
      action:
        item_id: "service:bundle-release/status"
        ref_bindings: {{}}
        params:
          operation_id: "caller-identity-probe"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(graphs_dir.join("release_status.yaml"), signed)?;
    Ok(())
}

/// Start a node with the workflow bundles and the bundle-release bundle, the
/// second key authorized for the graph, the service and the live project, and
/// a project holding the graph.
async fn start_node() -> (DaemonHarness, FastFixture, tempfile::TempDir) {
    let second = second_key();
    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            register_presigned_fixture_bundle(
                state_path,
                "bundle-release",
                &common::workspace_root().join("bundles/bundle-release"),
                fixture,
            )?;
            // The graph, the service and the live project, and nothing that
            // makes the key an operator.
            write_authorized_key_with_scopes(
                state_path,
                &second,
                &fixture.node,
                &[
                    GRAPH_CAP,
                    SERVICE_CAP,
                    "ryeos.execute.service.threads/get",
                    LIVE_PROJECT_READ_CAPABILITY,
                    LIVE_PROJECT_WRITE_CAPABILITY,
                ],
            )
        };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with the workflow and bundle-release bundles");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_release_status_graph(project.path(), &fixture.publisher)
        .expect("plant the release status graph");
    // A key that is not the operator runs a live project only at a checkout
    // registered to its owner.
    h.register_checkout_for_key(&second_key(), project.path(), "release-caller-project")
        .expect("register the second key's checkout");
    (h, fixture, project)
}

/// A signed POST to `path` as `key`.
async fn post_as(
    h: &DaemonHarness,
    key: &SigningKey,
    path: &str,
    body: Value,
) -> (reqwest::StatusCode, Value) {
    let bytes = serde_json::to_vec(&body).expect("encode request");
    let node_key = h.node_key.as_ref().expect("fast fixture node key");
    let mut request = reqwest::Client::new()
        .post(format!("http://{}{path}", h.bind))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in common::build_signed_headers_for_bytes(key, node_key, "POST", path, &bytes)
    {
        request = request.header(name, value);
    }
    let response = tokio::time::timeout(Duration::from_secs(60), request.send())
        .await
        .expect("request timed out")
        .expect("request failed to send");
    let status = response.status();
    let value = response.json().await.unwrap_or(Value::Null);
    (status, value)
}

/// Run `graph:release_status` in `project` as `key`.
async fn run_graph_as(
    h: &DaemonHarness,
    key: &SigningKey,
    project: &Path,
) -> (reqwest::StatusCode, Value) {
    post_as(
        h,
        key,
        "/execute",
        json!({
            "item_ref": "graph:release_status",
            "ref_bindings": {},
            "project_path": project.to_str().unwrap(),
            "parameters": {},
            // The vault overlay needs a capability the second key does not
            // hold and the step does not use.
            "execution_policy": ExecutionPolicy::local_live(ExecutionResponse::Wait)
                .exclude_operator_vault(),
        }),
    )
    .await
}

/// Everything the run says about its step's failure: the whole response and
/// the error each persisted node receipt carries, read as `key`.
async fn step_failure_text(h: &DaemonHarness, key: &SigningKey, body: &Value) -> String {
    let mut text = body.to_string();
    let Some(thread_id) = body.pointer("/thread/thread_id").and_then(Value::as_str) else {
        return text;
    };
    let (status, thread) = post_as(
        h,
        key,
        "/execute",
        json!({
            "item_ref": "service:threads/get",
            "ref_bindings": {},
            "parameters": {"thread_id": thread_id},
            "execution_policy": ExecutionPolicy::projectless(ExecutionResponse::Wait),
        }),
    )
    .await;
    assert!(
        status.is_success(),
        "threads.get of the graph thread as the requesting key: {status} {thread:#}"
    );
    for artifact in thread
        .pointer("/result/artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if artifact.get("artifact_type").and_then(Value::as_str) == Some("graph_node_receipt")
            && let Some(error) = artifact.pointer("/metadata/error").and_then(Value::as_str)
        {
            text.push('\n');
            text.push_str(error);
        }
    }
    text
}

fn names_an_operator_refusal(text: &str) -> bool {
    OPERATOR_REFUSAL_CODES
        .iter()
        .chain(OPERATOR_REFUSAL_SENTENCES)
        .any(|marker| text.contains(marker))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_release_service_step_run_by_the_operator_is_not_refused_as_a_non_operator() {
    let (h, _fixture, project) = start_node().await;
    let operator_key = h.user_key.clone().expect("fast fixture operator key");

    let (status, body) = run_graph_as(&h, &operator_key, project.path()).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the operator's graph run is admitted: {body:#}"
    );
    // The step may fail for another reason, such as no release authority
    // configured on the node; it must not fail as a non-operator.
    let text = step_failure_text(&h, &operator_key, &body).await;
    assert!(
        !names_an_operator_refusal(&text),
        "the operator's key must reach the release service as the operator: {text}"
    );
    // The step reached the release service past its operator check: either
    // it succeeded, or it failed at dispatch because this node composes no
    // release authority. Anything else means the step never got that far.
    let succeeded = body.pointer("/result/success").and_then(Value::as_bool) == Some(true);
    assert!(
        succeeded || text.contains("is unavailable"),
        "the operator's step must reach the release service's dispatch: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_release_service_step_run_by_a_non_operator_is_refused_as_a_non_operator() {
    let (h, _fixture, project) = start_node().await;
    let second = second_key();

    let (status, body) = run_graph_as(&h, &second, project.path()).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the second key is authorized to run the graph, so the run is admitted and the step \
         is what is refused: {body:#}"
    );
    assert_eq!(
        body.pointer("/result/success").and_then(Value::as_bool),
        Some(false),
        "the non-operator's release service step must fail: {body:#}"
    );
    let text = step_failure_text(&h, &second, &body).await;
    assert!(
        names_an_operator_refusal(&text),
        "the step must be refused as a non-operator: {text}"
    );
}
