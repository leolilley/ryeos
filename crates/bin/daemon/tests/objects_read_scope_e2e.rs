//! Root-scoped object reads over a daemon's public signed routes.
//!
//! Principal A is the configured operator and pushes a project. Principal B
//! is a second authorized key holding the object-read capabilities, including
//! the `objects/get-any` capability, but it is not the node operator. The
//! proofs:
//!
//! - a scoped read under a root the caller owns is served;
//! - a rootless read, or a hash outside the named root, is refused with the
//!   typed `object_not_in_scope`, identically for a hash that exists on the
//!   node and one that does not;
//! - `objects/has` answers present only for the caller's own roots, so it
//!   never reveals another principal's objects;
//! - `objects/get-any` is operator-only, whatever capability a caller holds;
//! - the project push sequence (open an upload session, ask `objects/has`
//!   under the project head, upload, publish the head) works under scoping,
//!   and the objects of a head that does not exist yet are answered absent
//!   until the head is published.

use ryeosd::test_support as common;
#[path = "common/project_push.rs"]
mod project_push;

use std::path::Path;

use anyhow::{Context, Result, ensure};
use common::DaemonHarness;
use common::build_signed_headers_for_bytes;
use common::fast_fixture::{FastFixture, write_authorized_key_with_scopes};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

const OBJECT_NOT_IN_SCOPE: &str = "object_not_in_scope";

/// Everything a remote reader needs, `objects/get-any` included: the proof
/// that the capability alone does not make a caller the operator.
const READER_SCOPES: &[&str] = &[
    "ryeos.execute.service.objects/get",
    "ryeos.execute.service.objects/has",
    "ryeos.execute.service.objects/put",
    "ryeos.execute.service.objects/closure/get",
    "ryeos.execute.service.objects/closure/describe",
    "ryeos.execute.service.objects/get-any",
];

fn reader_key() -> SigningKey {
    SigningKey::from_bytes(&[91_u8; 32])
}

/// POST as `key`, an authorized principal of the daemon.
async fn post_as(
    harness: &DaemonHarness,
    key: &SigningKey,
    path: &str,
    body: Value,
) -> Result<(reqwest::StatusCode, Value)> {
    let bytes = serde_json::to_vec(&body)?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{}{path}", harness.bind))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in build_signed_headers_for_bytes(
        key,
        &common::fast_fixture::node_signing_key(),
        "POST",
        path,
        &bytes,
    ) {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let value = response.json().await.unwrap_or_else(|_| json!({}));
    Ok((status, value))
}

fn assert_not_in_scope(status: reqwest::StatusCode, body: &Value, what: &str) {
    assert!(
        status == reqwest::StatusCode::FORBIDDEN && body.to_string().contains(OBJECT_NOT_IN_SCOPE),
        "{what}: expected a typed {OBJECT_NOT_IN_SCOPE} refusal, got {status}: {body}"
    );
}

fn plant_reader(state_path: &Path, _user: &Path, fixture: &FastFixture) -> Result<()> {
    write_authorized_key_with_scopes(state_path, &reader_key(), &fixture.node, READER_SCOPES)
}

/// A project directory holding one AI file.
fn source_project(content: &str) -> Result<tempfile::TempDir> {
    let source = tempfile::tempdir()?;
    let directory = source.path().join(".ai/knowledge/scope");
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join("notes.md"), content)?;
    Ok(source)
}

fn live_project() -> Result<(tempfile::TempDir, String)> {
    let live = tempfile::tempdir()?;
    let path = live
        .path()
        .canonicalize()?
        .to_str()
        .context("live project path is not UTF-8")?
        .to_owned();
    Ok((live, path))
}

fn blob_hash(content: &str) -> String {
    lillux::sha256_hex(content.as_bytes())
}

/// The object hashes of a pushed snapshot, read back as the operator through
/// the unscoped read: the snapshot, its tree and its policy.
async fn snapshot_object_hashes(
    harness: &DaemonHarness,
    snapshot_hash: &str,
) -> Result<Vec<String>> {
    let (status, body) = harness
        .post_json(
            "/objects/get-any",
            json!({"object_hashes": [snapshot_hash], "blob_hashes": []}),
        )
        .await?;
    ensure!(
        status.is_success(),
        "get-any of the snapshot: {status} {body}"
    );
    let value = &body["entries"][0]["value"];
    Ok(vec![
        snapshot_hash.to_owned(),
        value["project_tree_hash"]
            .as_str()
            .context("snapshot has no tree")?
            .to_owned(),
        value["effective_policy_hash"]
            .as_str()
            .context("snapshot has no policy")?
            .to_owned(),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_get_serves_the_owner_and_refuses_everything_else() -> Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(plant_reader, |_| {}).await?;
    let (_live, live_path) = live_project()?;
    let source = source_project("alpha")?;
    let captured = project_push::capture_ai_only_snapshot(source.path(), None)?;
    let pushed = project_push::push_snapshot(&harness, &live_path, &captured).await?;
    let hashes = snapshot_object_hashes(&harness, &pushed.snapshot_hash).await?;
    let root = json!({"kind": "project_head", "project_path": live_path});

    // The owner reads its own head's objects and blob under its root.
    let (status, body) = harness
        .post_json(
            "/objects/get",
            json!({
                "root": root,
                "object_hashes": hashes,
                "blob_hashes": [blob_hash("alpha")],
            }),
        )
        .await?;
    ensure!(
        status.is_success(),
        "scoped get by the owner: {status} {body}"
    );
    let entries = body["entries"].as_array().context("no entries")?;
    assert_eq!(entries.len(), 4);
    assert!(
        entries
            .iter()
            .all(|entry| entry["kind"] == "object" || entry["kind"] == "blob"),
        "every entry under the owner's root is served: {body}"
    );

    // A rootless read is refused, naming the cure.
    let (status, body) = harness
        .post_json(
            "/objects/get",
            json!({"object_hashes": hashes, "blob_hashes": []}),
        )
        .await?;
    assert_not_in_scope(status, &body, "rootless get");
    assert!(
        body.to_string().contains("Name a root"),
        "the refusal names the cure: {body}"
    );

    // A hash outside the root is refused, and a hash that exists on the node
    // but outside the root is indistinguishable from one that does not exist.
    let (_other, other_path) = live_project()?;
    let absent = "ab".repeat(32);
    let (status_absent, body_absent) = harness
        .post_json(
            "/objects/get",
            json!({"root": root, "object_hashes": [absent], "blob_hashes": []}),
        )
        .await?;
    assert_not_in_scope(status_absent, &body_absent, "absent hash under the root");
    let (status_foreign, body_foreign) = harness
        .post_json(
            "/objects/get",
            json!({
                "root": json!({"kind": "project_head", "project_path": other_path}),
                "object_hashes": [hashes[0]],
                "blob_hashes": [],
            }),
        )
        .await?;
    assert_not_in_scope(
        status_foreign,
        &body_foreign,
        "existing hash under a root that does not reach it",
    );

    // Principal B names the same project path but its own head namespace:
    // A's objects are out of scope for it, present or not.
    let (status, body) = post_as(
        &harness,
        &reader_key(),
        "/objects/get",
        json!({"root": root, "object_hashes": hashes, "blob_hashes": []}),
    )
    .await?;
    assert_not_in_scope(status, &body, "another principal reading the owner's head");
    let (status, body) = post_as(
        &harness,
        &reader_key(),
        "/objects/closure/get",
        json!({"root": root, "roots": [pushed.snapshot_hash]}),
    )
    .await?;
    assert_not_in_scope(status, &body, "another principal's closure read");

    // The closure read serves the owner under the same root.
    let (status, body) = harness
        .post_json(
            "/objects/closure/get",
            json!({"root": root, "roots": [pushed.snapshot_hash]}),
        )
        .await?;
    ensure!(
        status.is_success(),
        "closure get by the owner: {status} {body}"
    );
    assert_eq!(body["closure"]["complete"], true);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn get_any_is_operator_only() -> Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(plant_reader, |_| {}).await?;
    let (_live, live_path) = live_project()?;
    let source = source_project("beta")?;
    let captured = project_push::capture_ai_only_snapshot(source.path(), None)?;
    let pushed = project_push::push_snapshot(&harness, &live_path, &captured).await?;
    let request = json!({"object_hashes": [pushed.snapshot_hash], "blob_hashes": []});

    // The operator reads unscoped.
    let (status, body) = harness
        .post_json("/objects/get-any", request.clone())
        .await?;
    ensure!(status.is_success(), "operator get-any: {status} {body}");
    assert_eq!(body["entries"][0]["kind"], "object");

    // The operator may not name a root there: unscoped means unscoped.
    let (status, body) = harness
        .post_json(
            "/objects/get-any",
            json!({
                "root": {"kind": "project_head", "project_path": live_path},
                "object_hashes": [pushed.snapshot_hash],
                "blob_hashes": [],
            }),
        )
        .await?;
    ensure!(
        status == reqwest::StatusCode::BAD_REQUEST,
        "get-any with a root: {status} {body}"
    );

    // A principal holding the capability, but not the operator, is refused.
    let (status, body) = post_as(&harness, &reader_key(), "/objects/get-any", request).await?;
    assert!(
        status == reqwest::StatusCode::FORBIDDEN && body.to_string().contains("operator"),
        "get-any by a non-operator holding the capability: {status} {body}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn has_answers_only_for_the_callers_roots_and_leaks_nothing() -> Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(plant_reader, |_| {}).await?;
    let (_live, live_path) = live_project()?;
    let source = source_project("gamma")?;
    let captured = project_push::capture_ai_only_snapshot(source.path(), None)?;
    let pushed = project_push::push_snapshot(&harness, &live_path, &captured).await?;
    let hashes = snapshot_object_hashes(&harness, &pushed.snapshot_hash).await?;
    let root = json!({"kind": "project_head", "project_path": live_path});
    let absent = "cd".repeat(32);
    let request = json!({
        "root": root,
        "object_hashes": [hashes[0], absent],
        "blob_hashes": [blob_hash("gamma")],
    });

    // The owner sees its head's objects and blob as present.
    let (status, body) = harness.post_json("/objects/has", request.clone()).await?;
    ensure!(status.is_success(), "owner has: {status} {body}");
    assert_eq!(body["found_object_hashes"], json!([hashes[0]]));
    assert_eq!(body["missing_object_hashes"], json!([absent]));
    assert_eq!(body["found_blob_hashes"], json!([blob_hash("gamma")]));

    // Another principal naming the same path reaches its own, empty head:
    // everything is absent, the owner's objects included. The answer for an
    // object that exists elsewhere equals the answer for one that does not.
    let (status, body) = post_as(&harness, &reader_key(), "/objects/has", request.clone()).await?;
    ensure!(status.is_success(), "reader has: {status} {body}");
    assert_eq!(body["found_object_hashes"], json!([]));
    assert_eq!(body["found_blob_hashes"], json!([]));
    assert_eq!(
        body["missing_object_hashes"]
            .as_array()
            .context("missing list")?
            .len(),
        2
    );

    // A rootless presence check is refused.
    let (status, body) = harness
        .post_json(
            "/objects/has",
            json!({"object_hashes": [hashes[0]], "blob_hashes": []}),
        )
        .await?;
    assert_not_in_scope(status, &body, "rootless has");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn project_push_works_under_the_project_head_root() -> Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(plant_reader, |_| {}).await?;
    let (_live, live_path) = live_project()?;
    let root = json!({"kind": "project_head", "project_path": live_path});

    // First push to a project that has no head yet: nothing is reachable, so
    // the receiver reports everything absent and the sender uploads it.
    let first = source_project("one")?;
    let first_captured = project_push::capture_ai_only_snapshot(first.path(), None)?;
    let (status, body) = harness
        .post_json(
            "/objects/has",
            json!({
                "root": root,
                "object_hashes": [first_captured.snapshot_hash],
                "blob_hashes": [blob_hash("one")],
            }),
        )
        .await?;
    ensure!(
        status.is_success(),
        "has before the first push: {status} {body}"
    );
    assert_eq!(body["found_object_hashes"], json!([]));
    assert_eq!(body["found_blob_hashes"], json!([]));
    let first_pushed = project_push::push_snapshot(&harness, &live_path, &first_captured).await?;

    // Second push, a descendant: the new head's objects are not yet reachable
    // from the current head and are answered absent; the first head's are
    // present. The push then completes and the new head becomes readable.
    let second = source_project("two")?;
    let second_captured =
        project_push::capture_ai_only_snapshot(second.path(), Some(&first_pushed.snapshot_hash))?;
    let (status, body) = harness
        .post_json(
            "/objects/has",
            json!({
                "root": root,
                "object_hashes": [first_pushed.snapshot_hash, second_captured.snapshot_hash],
                "blob_hashes": [blob_hash("one"), blob_hash("two")],
            }),
        )
        .await?;
    ensure!(
        status.is_success(),
        "has before the second push: {status} {body}"
    );
    assert_eq!(
        body["found_object_hashes"],
        json!([first_pushed.snapshot_hash])
    );
    assert_eq!(
        body["missing_object_hashes"],
        json!([second_captured.snapshot_hash])
    );
    assert_eq!(body["found_blob_hashes"], json!([blob_hash("one")]));
    assert_eq!(body["missing_blob_hashes"], json!([blob_hash("two")]));
    let second_pushed = project_push::push_snapshot(&harness, &live_path, &second_captured).await?;

    let (status, body) = harness
        .post_json(
            "/objects/has",
            json!({
                "root": root,
                "object_hashes": [second_pushed.snapshot_hash],
                "blob_hashes": [blob_hash("two")],
            }),
        )
        .await?;
    ensure!(
        status.is_success(),
        "has after the second push: {status} {body}"
    );
    assert_eq!(
        body["found_object_hashes"],
        json!([second_pushed.snapshot_hash])
    );
    assert_eq!(body["found_blob_hashes"], json!([blob_hash("two")]));
    Ok(())
}
