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
