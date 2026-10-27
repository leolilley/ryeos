//! Apply a pushed snapshot to a live project through the daemon and read the
//! deployed snapshot back; only the schedule-deploy flow needs these.

use anyhow::Result;
use serde_json::{Value, json};

use crate::common::DaemonHarness;
use crate::project_push::post_ok;

/// The `/project/apply-snapshot` request body for one pushed HEAD.
pub fn apply_request(
    live_project: &str,
    snapshot_hash: &str,
    expected_deployed: Option<&str>,
) -> Value {
    json!({
        "project_path": live_project,
        "snapshot_hash": snapshot_hash,
        "expected_deployed_hash": expected_deployed,
    })
}

/// Deploy a pushed HEAD, returning the node's status and body unchanged.
pub async fn apply_snapshot(
    h: &DaemonHarness,
    live_project: &str,
    snapshot_hash: &str,
    expected_deployed: Option<&str>,
) -> Result<(reqwest::StatusCode, Value)> {
    h.post_json(
        "/project/apply-snapshot",
        apply_request(live_project, snapshot_hash, expected_deployed),
    )
    .await
}

/// The node's deployed snapshot for `live_project`, or `None` when the
/// project has never been deployed.
pub async fn deployed_snapshot(h: &DaemonHarness, live_project: &str) -> Result<Option<String>> {
    let status = post_ok(h, "/project/status", json!({"project_path": live_project})).await?;
    match status.get("deployed_snapshot_hash") {
        Some(Value::String(hash)) => Ok(Some(hash.clone())),
        Some(Value::Null) => Ok(None),
        other => anyhow::bail!("project/status returned no deployed snapshot field: {other:?}"),
    }
}
