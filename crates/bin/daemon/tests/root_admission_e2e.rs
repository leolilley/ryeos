//! Strict root admission, end to end through the daemon.
//!
//! A thread never holds capabilities its caller did not lend: a caller whose
//! grant does not cover every capability an item declares is refused with the
//! typed `caller_capability_not_covered` launch-policy refusal, and the same
//! caller launches once the grant covers the declaration.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{
    FastFixture, register_workflow_bundles, write_authorized_key_with_scopes,
};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

const DECLARED_CAP: &str = "ryeos.execute.tool.echo/echo";
const SECOND_DECLARED_CAP: &str = "ryeos.execute.service.health/status";

/// A return-only graph that declares two capabilities it never exercises.
fn plant_declaring_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = format!(
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - {DECLARED_CAP}
      - {SECOND_DECLARED_CAP}
config:
  start: done
  nodes:
    done:
      node_type: return
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(graphs_dir.join("declares.yaml"), signed)?;
    Ok(())
}

/// The caller's scopes: the graph's execute cap and live-project access, plus
/// whatever declared caps the test chooses to lend.
fn caller_scopes(lent: &[&'static str]) -> Vec<&'static str> {
    let mut scopes = vec![
        "ryeos.execute.graph.declares",
        ryeos_app::execution_policy::LIVE_PROJECT_READ_CAPABILITY,
        ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY,
    ];
    scopes.extend_from_slice(lent);
    scopes
}

async fn start_with_caller(
    lent: &'static [&'static str],
) -> (DaemonHarness, FastFixture, tempfile::TempDir) {
    let plant =
        move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            write_authorized_key_with_scopes(
                state_path,
                &fixture.user,
                &fixture.node,
                &caller_scopes(lent),
            )
        };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with a narrow caller");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_declaring_graph(project.path(), &fixture.publisher).expect("plant declaring graph");
    (h, fixture, project)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_that_lacks_a_declared_cap_is_refused_with_every_uncovered_cap() {
    let (h, _fixture, project) = start_with_caller(&[]).await;

    let (status, body) = h
        .post_execute(
            "graph:declares",
            project.path().to_str().unwrap(),
            json!({}),
        )
        .await
        .expect("post graph:declares as a narrow caller");

    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("caller_capability_not_covered"),
        "body={body:#}"
    );
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        error.contains(DECLARED_CAP) && error.contains(SECOND_DECLARED_CAP),
        "the refusal must name every uncovered cap: {error}"
    );
    assert!(
        error.contains("narrow the item's `requires.capabilities.declared`"),
        "the refusal must name its cure: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_that_covers_the_declared_caps_launches() {
    let (h, _fixture, project) = start_with_caller(&[DECLARED_CAP, SECOND_DECLARED_CAP]).await;

    let (status, body) = h
        .post_execute(
            "graph:declares",
            project.path().to_str().unwrap(),
            json!({}),
        )
        .await
        .expect("post graph:declares as a covering caller");

    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    assert_eq!(
        body.pointer("/result/status").and_then(Value::as_str),
        Some("completed"),
        "body={body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wildcard_in_the_callers_grant_covers_the_declared_caps() {
    let (h, _fixture, project) =
        start_with_caller(&["ryeos.execute.tool.*", "ryeos.execute.service.*"]).await;

    let (status, body) = h
        .post_execute(
            "graph:declares",
            project.path().to_str().unwrap(),
            json!({}),
        )
        .await
        .expect("post graph:declares as a wildcard caller");

    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
}
