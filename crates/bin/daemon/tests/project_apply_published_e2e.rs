//! End to end: a published project result is applied to the caller's
//! registered checkout through `service:project/apply-published`.
//!
//! The test pushes two generations of a project as the caller's HEAD, registers
//! a live checkout of it while the node is stopped, and applies the second
//! generation: only the paths the result changes are checked, every conflict is
//! returned as one answer with nothing written, a dirty file the result does not
//! touch stays, a repeat is a no-op, and a project the caller never registered
//! or a snapshot that is not the published history is refused.

#![cfg(unix)]

use ryeosd::test_support as common;
#[path = "common/project_push.rs"]
mod project_push;

use std::path::Path;

use common::DaemonHarness;
use lillux::crypto::SigningKey;
use ryeos_node::offline_operation::BareHost;
use ryeos_node::offline_operation::apply::{LocalDraft, apply_local};
use serde_json::{Value, json};

const NAME: &str = "apply-project";
const A: &str = ".ai/knowledge/demo/a.md";
const B: &str = ".ai/knowledge/demo/b.md";
const C: &str = ".ai/knowledge/demo/c.md";

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative)).unwrap_or_default()
}

fn declare(root: &Path) {
    write(root, ".ai/manifest.yaml", &format!("name: {NAME}\n"));
}

/// Register `project` as the operator's checkout while the node is stopped.
async fn register(h: &mut DaemonHarness, operator: &SigningKey, project: &Path) {
    h.kill_daemon().await.expect("stop the daemon");
    apply_local(
        &h.state_path,
        &BareHost,
        operator,
        LocalDraft {
            operation: "node.project.register".to_owned(),
            arguments: json!({ "path": project.to_str().unwrap() }),
            inputs: std::collections::BTreeMap::new(),
            expected_result: std::collections::BTreeMap::new(),
        },
    )
    .expect("register the project");
    h.respawn_with(|_| {}).await.expect("restart the daemon");
}

async fn push(h: &DaemonHarness, source: &Path, live: &str, parent: Option<&str>) -> String {
    let captured =
        project_push::capture_ai_only_snapshot(source, parent).expect("capture the generation");
    project_push::push_snapshot(h, live, &captured)
        .await
        .expect("push the generation")
        .snapshot_hash
}

async fn apply(h: &DaemonHarness, project: &str, base: &str, result: &str) -> (u16, Value) {
    let (status, body) = h
        .post_execute(
            "service:project/apply-published",
            ".",
            json!({
                "project_name": project,
                "base_snapshot_hash": base,
                "result_snapshot_hash": result,
            }),
        )
        .await
        .expect("post apply-published");
    (status.as_u16(), body)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_published_result_is_applied_to_the_registered_checkout_without_merging() {
    let (mut h, fixture) = DaemonHarness::start_fast_core_only_prepared()
        .await
        .expect("start daemon");
    let live_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let live = live_dir.path().canonicalize().unwrap();
    let source = source_dir.path().canonicalize().unwrap();
    let live_text = live.to_str().unwrap().to_owned();
    declare(&live);
    declare(&source);
    register(&mut h, &fixture.user, &live).await;

    // Generation one, then the checkout holds exactly it.
    write(&source, A, "one");
    write(&source, B, "keep");
    let base = push(&h, &source, &live_text, None).await;
    write(&live, A, "one");
    write(&live, B, "keep");

    // Generation two changes a, adds c, leaves b alone.
    write(&source, A, "two");
    write(&source, C, "new");
    let result = push(&h, &source, &live_text, Some(&base)).await;

    // Not the published history: refused, nothing written.
    let (status, body) = apply(&h, NAME, &base, &"0".repeat(64)).await;
    assert_eq!(status, 409, "{body:#}");
    assert!(
        body.to_string().contains("project_apply_not_published"),
        "{body:#}"
    );
    // A project the caller never registered is the same absence.
    let (status, body) = apply(&h, "someone-elses", &base, &result).await;
    assert_eq!(status, 403, "{body:#}");
    assert!(
        body.to_string().contains("project_not_registered"),
        "{body:#}"
    );
    assert_eq!(read(&live, A), "one");

    // A file the person changed that the result also changes: one answer,
    // every conflict named, nothing written (not even the clean new file).
    write(&live, A, "my own edit");
    let (status, body) = apply(&h, NAME, &base, &result).await;
    assert_eq!(status, 200, "{body:#}");
    let answer = &body["result"];
    assert_eq!(answer["applied"], json!(false), "{body:#}");
    assert_eq!(answer["code"], json!("project_apply_conflicts"), "{body:#}");
    assert_eq!(
        answer["conflicts"],
        json!([{ "path": A, "reason": "drifted" }]),
        "{body:#}"
    );
    assert_eq!(read(&live, A), "my own edit");
    assert!(!live.join(C).exists());

    // Restore a; a dirty file the result does not touch never blocks.
    write(&live, A, "one");
    write(&live, B, "my change to b");
    let (status, body) = apply(&h, NAME, &base, &result).await;
    assert_eq!(status, 200, "{body:#}");
    let answer = &body["result"];
    assert_eq!(answer["applied"], json!(true), "{body:#}");
    assert_eq!(answer["files_updated"], json!(2), "{body:#}");
    assert_eq!(read(&live, A), "two");
    assert_eq!(read(&live, C), "new");
    assert_eq!(read(&live, B), "my change to b");

    // Applying the same result again is a no-op.
    let (status, body) = apply(&h, NAME, &base, &result).await;
    assert_eq!(status, 200, "{body:#}");
    assert_eq!(body["result"]["applied"], json!(true), "{body:#}");
    assert_eq!(body["result"]["files_updated"], json!(0), "{body:#}");
    assert_eq!(read(&live, B), "my change to b");
}
