//! Project push and apply-snapshot over a daemon's public signed routes.
//!
//! The test process plays the pushing client: it captures an AI-only snapshot
//! of a source directory, uploads every blob and typed object through
//! `/objects/put`, publishes the caller-scoped HEAD through `/push-head`, and
//! asks the node to deploy it into a live project through
//! `/project/apply-snapshot`. Object addresses are the canonical-JSON SHA-256
//! the node recomputes, so nothing here is trusted by the target.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::{Value, json};

use ryeos_state::ignore::{IgnoreConfig, IgnoreMatcher};
use ryeos_state::objects::project_file::ProjectFile;
use ryeos_state::objects::project_snapshot::ProjectSnapshot;
use ryeos_state::objects::project_tree::ProjectTree;
use ryeos_state::project_sync::ProjectSyncScope;

use crate::common::DaemonHarness;

/// One captured AI-only generation ready to upload.
pub struct CapturedSnapshot {
    pub snapshot_hash: String,
    blobs: Vec<Vec<u8>>,
    objects: Vec<Value>,
    snapshot: ProjectSnapshot,
}

/// A published caller HEAD, as returned by the node.
pub struct PushedHead {
    pub snapshot_hash: String,
}

fn object_hash(value: &Value) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(value)?.as_bytes(),
    ))
}

/// Capture every regular file below `source_root` as one AI-only snapshot
/// whose parent is `parent`. Only paths inside managed AI surfaces may be
/// present; the node's own path validation is the authority on that.
pub fn capture_ai_only_snapshot(
    source_root: &Path,
    parent: Option<&str>,
) -> Result<CapturedSnapshot> {
    let pinned = lillux::PinnedDirectory::open(source_root)?
        .with_context(|| format!("source project {} is absent", source_root.display()))?;
    let no_node_patterns = IgnoreMatcher::from_config(&IgnoreConfig {
        patterns: Vec::new(),
    })?;
    let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
        &pinned,
        &no_node_patterns,
        ProjectSyncScope::AiOnly,
    )?;

    let mut files = BTreeMap::new();
    let mut blobs = Vec::new();
    let mut objects = Vec::new();
    let mut pending = vec![source_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            anyhow::ensure!(
                file_type.is_file(),
                "source project entry {} is not a regular file",
                path.display()
            );
            let relative = path
                .strip_prefix(source_root)?
                .to_str()
                .context("source project path is not UTF-8")?
                .to_owned();
            let bytes = std::fs::read(&path)?;
            let file = ProjectFile {
                blob_hash: lillux::sha256_hex(&bytes),
                size: u64::try_from(bytes.len())?,
                normalized_mode: ProjectFile::REGULAR_MODE,
            };
            let file_value = file.to_value();
            files.insert(relative, object_hash(&file_value)?);
            objects.push(file_value);
            blobs.push(bytes);
        }
    }
    let tree = ProjectTree { files };
    let tree_value = tree.to_value();
    let policy_value = policy.to_value();
    let snapshot = ProjectSnapshot {
        project_tree_hash: object_hash(&tree_value)?,
        effective_policy_hash: object_hash(&policy_value)?,
        message: None,
        parent_hashes: parent.map(str::to_owned).into_iter().collect(),
        created_at: lillux::time::iso8601_now(),
        source: "push".to_owned(),
    };
    let snapshot_value = snapshot.to_value();
    let snapshot_hash = object_hash(&snapshot_value)?;
    objects.push(tree_value);
    objects.push(policy_value);
    objects.push(snapshot_value);
    Ok(CapturedSnapshot {
        snapshot_hash,
        blobs,
        objects,
        snapshot,
    })
}

pub(crate) async fn post_ok(h: &DaemonHarness, route: &str, body: Value) -> Result<Value> {
    let (status, response) = h.post_json(route, body).await?;
    anyhow::ensure!(
        status.is_success(),
        "{route} refused with {status}: {response}"
    );
    Ok(response)
}

/// Upload `captured` into a fresh staging capability for `live_project` and
/// publish it as the caller's HEAD. The capability's observed previous HEAD
/// must equal the snapshot's declared parent.
pub async fn push_snapshot(
    h: &DaemonHarness,
    live_project: &str,
    captured: &CapturedSnapshot,
) -> Result<PushedHead> {
    let session = post_ok(
        h,
        "/objects/put",
        json!({
            "staging_id": null,
            "project_path": live_project,
            "blobs": [],
            "blob_chunks": [],
            "objects": [],
        }),
    )
    .await?;
    let staging_id = session["staging_id"]
        .as_str()
        .context("objects/put returned no staging_id")?
        .to_owned();
    let expected_previous = match session.get("expected_previous_hash") {
        Some(Value::String(hash)) => Some(hash.clone()),
        Some(Value::Null) => None,
        other => anyhow::bail!("objects/put returned no nullable previous HEAD: {other:?}"),
    };
    anyhow::ensure!(
        captured.snapshot.parent_hashes == expected_previous.iter().cloned().collect::<Vec<_>>(),
        "captured parent {:?} differs from the node's previous HEAD {:?}",
        captured.snapshot.parent_hashes,
        expected_previous
    );
    let blobs: Vec<Value> = captured
        .blobs
        .iter()
        .map(|bytes| json!({"data": base64::engine::general_purpose::STANDARD.encode(bytes)}))
        .collect();
    let objects: Vec<Value> = captured
        .objects
        .iter()
        .map(|value| json!({"value": value}))
        .collect();
    post_ok(
        h,
        "/objects/put",
        json!({
            "staging_id": staging_id,
            "project_path": live_project,
            "blobs": blobs,
            "blob_chunks": [],
            "objects": objects,
        }),
    )
    .await?;
    post_ok(
        h,
        "/push-head",
        json!({
            "staging_id": staging_id,
            "project_path": live_project,
            "snapshot_hash": captured.snapshot_hash,
            "expected_previous_hash": expected_previous,
        }),
    )
    .await?;
    Ok(PushedHead {
        snapshot_hash: captured.snapshot_hash.clone(),
    })
}
