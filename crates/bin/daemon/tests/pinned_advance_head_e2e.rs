//! End-to-end **pinned CoW execution with `AdvanceHead` publication**.
//!
//! A pinned run realised copy-on-write from the principal's current project
//! HEAD may ask the node to advance that HEAD to its result generation. The
//! node binds the frozen generation before the HEAD compare-and-swap, so:
//!
//!   1. When HEAD has not moved, the run completes and HEAD names a new
//!      snapshot holding the run's writes; the live project is untouched.
//!   2. When HEAD moved during the run, the CAS is refused and the thread
//!      fails with outcome `head_advance_not_applied`, its error payload names
//!      the expected and observed HEAD and the retained generation, and HEAD
//!      stays where the other writer put it. This holds on the direct lane (a
//!      tool) and on a managed runtime lane (a graph running the same tool).
//!
//! The fixture establishes HEAD through the ordinary `tool:core/snapshot-create`
//! Tool and moves it the same way, so nothing here writes node state behind the
//! daemon's back.

use ryeosd::test_support as common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{
    ChildProjectPolicy, ExecutionPolicy, ExecutionResponse, PinnedRealization, PinnedSource,
    ProjectExecutionPolicy, TerminalPublication,
};
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_app::test_support::TestStateStore;
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::{Value, json};

const WRITTEN: &str = "advance-head-result.txt";
const MOVED: &str = "moved-by-another-writer.txt";

/// Plant ZEN_API_KEY in the sealed vault so a graph launch preflight passes
/// (mirrors the other graph e2e fixtures).
fn plant_vault_with_zen_key(state_path: &Path) -> Result<()> {
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

/// `tool:advance/write`: optionally waits for a gate file, then writes
/// [`WRITTEN`] into its (copy-on-write) project root. The wait gives the test a
/// window to move HEAD while the run holds its pinned base. If isolation hides
/// the gate path the wait simply times out and the run continues.
fn plant_writing_tool(project_dir: &Path, signer: &SigningKey) -> Result<()> {
    let tool_dir = project_dir.join(".ai/tools/advance");
    std::fs::create_dir_all(&tool_dir)?;
    let body = format!(
        r#"# ryeos-tool:
#   category: advance
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"

import json
import pathlib
import sys
import time

params = json.loads(sys.stdin.read() or "{{}}")
project_path = sys.argv[sys.argv.index("--project-path") + 1]
gate = params.get("gate")
if gate:
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline and not pathlib.Path(gate).exists():
        time.sleep(0.1)
pathlib.Path(project_path).joinpath("{WRITTEN}").write_text(
    "written under a pinned advance-head run\n", encoding="utf-8"
)
print(json.dumps({{"written": True}}))
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(tool_dir.join("write.py"), signed)?;
    Ok(())
}

/// `graph:advance_graph`: one action node running `tool:advance/write`, so the
/// same publication goes through the managed graph runtime lane.
fn plant_writing_graph(project_dir: &Path, signer: &SigningKey) -> Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: test
version: "1.0.0"
config:
  start: write
  config_schema:
    type: object
    properties:
      gate: {type: string}
  nodes:
    write:
      action:
        item_id: "tool:advance/write"
        params:
          gate: "${inputs.gate}"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
      output:
        written: true
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("advance_graph.yaml"), signed)?;
    Ok(())
}

async fn start(project: &Path) -> Result<(DaemonHarness, FastFixture)> {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {}).await?;
    plant_writing_tool(project, &fixture.publisher)?;
    plant_writing_graph(project, &fixture.publisher)?;
    Ok((h, fixture))
}

/// Capture the live project into a new snapshot and publish it as HEAD,
/// through the ordinary snapshot Tool. Returns the new HEAD.
async fn snapshot_create(h: &DaemonHarness, project: &Path, message: &str) -> Result<String> {
    let (status, body) = tokio::time::timeout(
        Duration::from_secs(150),
        h.post_json(
            "/execute",
            json!({
                "item_ref": "tool:core/snapshot-create",
                "ref_bindings": {},
                "project_path": project,
                "parameters": {"project_path": project, "message": message, "allow_empty": true},
                "execution_policy": ExecutionPolicy::local_live(ExecutionResponse::Wait)
                    .exclude_operator_vault(),
            }),
        ),
    )
    .await
    .context("snapshot-create timed out")??;
    anyhow::ensure!(
        status.is_success() && body.pointer("/thread/status") == Some(&json!("completed")),
        "snapshot-create did not complete: {status}: {body:#}"
    );
    let snapshot = body
        .pointer("/result/result")
        .context("snapshot-create terminal payload")?;
    anyhow::ensure!(
        snapshot["created"] == true && snapshot["snapshot_hash"] == snapshot["head_snapshot_hash"],
        "snapshot-create did not publish a new HEAD: {snapshot:#}"
    );
    Ok(snapshot["snapshot_hash"]
        .as_str()
        .context("snapshot hash")?
        .to_owned())
}

fn user_principal(fixture: &FastFixture) -> String {
    format!(
        "fp:{}",
        lillux::signature::compute_fingerprint(&fixture.user.verifying_key())
    )
}

fn project_hash(project: &Path) -> Result<String> {
    let project_ref = ryeos_executor::execution::project_source::canonical_project_ref(
        project.to_str().context("UTF-8 project path")?,
    )?;
    Ok(lillux::sha256_hex(project_ref.as_bytes()))
}

/// Pinned CoW from the current HEAD, advancing that HEAD from `expected`.
fn advance_head_policy(
    fixture: &FastFixture,
    project: &Path,
    expected: &str,
    response: ExecutionResponse,
) -> Result<ExecutionPolicy> {
    let principal = user_principal(fixture);
    let principal_key = ryeos_state::refs::principal_storage_key(&principal)?;
    let policy = ExecutionPolicy {
        project: ProjectExecutionPolicy::Pinned {
            source: PinnedSource::CurrentHead,
            realization: PinnedRealization::Cow {
                terminal_publication: TerminalPublication::AdvanceHead {
                    head_ref: format!("projects/{principal_key}/{}/head", project_hash(project)?),
                    expected_hash: expected.to_owned(),
                },
            },
            child_policy: ChildProjectPolicy::Inherit,
        },
        ..ExecutionPolicy::local_live(response)
    }
    .exclude_operator_vault();
    policy.validate()?;
    Ok(policy)
}

fn open_stopped_state(state_path: &Path) -> Result<TestStateStore> {
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    let identity = NodeIdentity::load(&identity_path)?;
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    TestStateStore::open(state_path, signer, Arc::new(head_trust))
}

/// HEAD and the file names of a snapshot's tree, read from the stopped node.
fn read_head_and_tree(
    state_path: &Path,
    fixture: &FastFixture,
    project: &Path,
    snapshot: Option<&str>,
) -> Result<(Option<String>, Vec<String>)> {
    let store = open_stopped_state(state_path)?;
    let principal = user_principal(fixture);
    let principal_key = ryeos_state::refs::principal_storage_key(&principal)?.to_owned();
    let project_hash = project_hash(project)?;
    let head = store.with_state_db(|db| db.read_project_head(&principal_key, &project_hash))?;
    let mut files = Vec::new();
    if let Some(snapshot) = snapshot.or(head.as_deref()) {
        let authority = store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        let cas = authority.cas_store()?;
        let snapshot_value = cas
            .get_object(snapshot)?
            .with_context(|| format!("snapshot {snapshot} is absent"))?;
        let tree_hash = snapshot_value["project_tree_hash"]
            .as_str()
            .context("snapshot tree hash")?;
        let tree = cas
            .get_object(tree_hash)?
            .with_context(|| format!("tree {tree_hash} is absent"))?;
        files = tree["files"]
            .as_object()
            .context("tree files")?
            .keys()
            .cloned()
            .collect();
    }
    Ok((head, files))
}

/// The first object anywhere in `value` that has `key`.
fn find_with_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) if map.contains_key(key) => Some(value),
        Value::Object(map) => map.values().find_map(|child| find_with_key(child, key)),
        Value::Array(items) => items.iter().find_map(|child| find_with_key(child, key)),
        _ => None,
    }
}

async fn wait_terminal(h: &mut DaemonHarness, thread_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok(Some(view)) = h.get_thread(thread_id).await {
            let status = view
                .pointer("/thread/status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(status, "completed" | "failed" | "cancelled" | "killed") {
                return view;
            }
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("thread {thread_id} never settled\n--- daemon stderr ---\n{stderr}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_running(h: &mut DaemonHarness, thread_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some(view)) = h.get_thread(thread_id).await
            && view.pointer("/thread/status").and_then(Value::as_str) == Some("running")
        {
            return;
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("thread {thread_id} never started running\n--- daemon stderr ---\n{stderr}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_advance_head_publishes_the_result_generation_as_head() -> Result<()> {
    let project = tempfile::tempdir()?;
    let (mut h, fixture) = start(project.path()).await?;
    let base = snapshot_create(&h, project.path(), "advance-head base").await?;

    let (status, body) = h
        .post_json(
            "/execute",
            json!({
                "item_ref": "tool:advance/write",
                "ref_bindings": {},
                "project_path": project.path(),
                "parameters": {},
                "execution_policy": advance_head_policy(
                    &fixture, project.path(), &base, ExecutionResponse::Wait)?,
            }),
        )
        .await?;
    if !(status.is_success() && body.pointer("/thread/status") == Some(&json!("completed"))) {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!("advance-head run did not complete: {status}: {body:#}\n--- stderr ---\n{stderr}");
    }
    // Copy-on-write: the live project never saw the run's write.
    assert!(!project.path().join(WRITTEN).exists());

    h.stop_gracefully().await?;
    let (head, files) = read_head_and_tree(&h.state_path, &fixture, project.path(), None)?;
    let head = head.expect("HEAD survives the run");
    assert_ne!(head, base, "the run advanced HEAD past its base");
    assert!(
        files.iter().any(|file| file == WRITTEN),
        "the new HEAD holds the run's write: {files:?}"
    );
    let (_, base_files) = read_head_and_tree(&h.state_path, &fixture, project.path(), Some(&base))?;
    assert!(!base_files.iter().any(|file| file == WRITTEN));
    Ok(())
}

/// Start `item_ref` under advance-head, move HEAD while it waits on its gate,
/// release it and check the typed refusal and the retained generation.
async fn head_moved_case(item_ref: &str) -> Result<()> {
    let project = tempfile::tempdir()?;
    let gate_dir = tempfile::tempdir()?;
    let gate = gate_dir.path().join("release");
    let (mut h, fixture) = start(project.path()).await?;
    let base = snapshot_create(&h, project.path(), "advance-head base").await?;

    let (status, body) = h
        .post_json(
            "/execute",
            json!({
                "item_ref": item_ref,
                "ref_bindings": {},
                "project_path": project.path(),
                "parameters": {"gate": gate},
                "execution_policy": advance_head_policy(
                    &fixture, project.path(), &base, ExecutionResponse::Accepted)?,
            }),
        )
        .await?;
    assert!(
        status.is_success(),
        "advance-head run refused: {status}: {body:#}"
    );
    let thread_id = body
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .with_context(|| format!("accepted response has no thread id: {body:#}"))?
        .to_owned();
    wait_running(&mut h, &thread_id).await;

    // Another writer publishes a new HEAD while the run holds its pinned base.
    std::fs::write(project.path().join(MOVED), "another writer\n")?;
    let moved = snapshot_create(&h, project.path(), "moved under the run").await?;
    assert_ne!(moved, base);
    std::fs::write(&gate, b"go")?;

    let view = wait_terminal(&mut h, &thread_id).await;
    assert_eq!(
        view.pointer("/thread/status").and_then(Value::as_str),
        Some("failed"),
        "a refused HEAD advance fails the run: {view:#}"
    );
    let rendered = view.to_string();
    assert!(
        rendered.contains("head_advance_not_applied"),
        "the outcome is head_advance_not_applied: {view:#}"
    );
    let payload = find_with_key(&view, "retained_snapshot_hash")
        .unwrap_or_else(|| panic!("no retained-generation payload: {view:#}"));
    assert_eq!(payload["expected_head"], json!(base), "{payload:#}");
    assert_eq!(payload["observed_head"], json!(moved), "{payload:#}");
    let retained = payload["retained_snapshot_hash"]
        .as_str()
        .unwrap_or_else(|| panic!("retained snapshot hash: {payload:#}"))
        .to_owned();
    assert!(lillux::valid_hash(&retained));
    assert_ne!(retained, base);
    assert_ne!(retained, moved);

    h.stop_gracefully().await?;
    let (head, head_files) = read_head_and_tree(&h.state_path, &fixture, project.path(), None)?;
    assert_eq!(
        head.as_deref(),
        Some(moved.as_str()),
        "HEAD keeps the other writer's snapshot"
    );
    assert!(!head_files.iter().any(|file| file == WRITTEN));
    // The retained generation holds the run's work and is still in the store.
    let (_, retained_files) =
        read_head_and_tree(&h.state_path, &fixture, project.path(), Some(&retained))?;
    assert!(
        retained_files.iter().any(|file| file == WRITTEN),
        "the retained generation holds the run's write: {retained_files:?}"
    );
    assert!(!retained_files.iter().any(|file| file == MOVED));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_tool_fails_head_advance_not_applied_when_head_moved() -> Result<()> {
    head_moved_case("tool:advance/write").await
}

#[tokio::test(flavor = "multi_thread")]
async fn managed_graph_fails_head_advance_not_applied_when_head_moved() -> Result<()> {
    head_moved_case("graph:advance_graph").await
}
