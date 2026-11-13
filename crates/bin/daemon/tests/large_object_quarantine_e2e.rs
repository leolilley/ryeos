//! Scrub quarantines a byte-damaged large object, a launch that needs it is
//! refused typed, a re-import publishes a clean copy, and the stopped-node
//! operation `node.large-objects.purge-quarantine` discards the evidence.
//!
//! Everything runs through a real daemon and the real CLI:
//!
//! 1. `service:external-content/import` stages a file from a declared import
//!    root into the node's large-object store, and
//!    `service:external-content/bind` binds it to a pinned project Tool that
//!    declares it as locator-free pinned external content. The Tool launches.
//! 2. One byte of the stored object is flipped on disk.
//! 3. `service:external-content/scrub` lists the object in
//!    `objects_quarantined`, moves it (and its sidecar) under the store's
//!    `quarantine/`, and reports the binding that needs it.
//! 4. Launching the Tool again is refused with `large_object_quarantined`,
//!    whose message names the cure.
//! 5. Importing the original bytes again publishes a clean copy; the Tool
//!    launches and reads the original bytes.
//! 6. With the daemon stopped, `ryeos node large-objects purge-quarantine`
//!    previews (`--dry-run`) and then removes (`--confirm`) the quarantined
//!    copy, leaving the served clean object in place.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use common::DaemonHarness;
use common::fast_fixture::FastFixture;
use ryeos_app::execution_policy::{
    ChildProjectPolicy, ExecutionEnvironmentPolicy, ExecutionPolicy, ExecutionResponse,
    PinnedRealization, PinnedSource, ProjectExecutionPolicy,
};
use serde_json::{Value, json};

const IMPORT_ROOT: &str = "quarantine-inputs";
const PAYLOAD_NAME: &str = "weights.bin";
const PAYLOAD_BYTES: usize = 256 * 1024 + 17;
const MAXIMUM_BYTES: u64 = 1024 * 1024;
const TOOL_REF: &str = "tool:test/weights-reader";
const QUARANTINED_CODE: &str = "large_object_quarantined";

/// Deterministic incompressible bytes.
fn payload() -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut bytes = Vec::with_capacity(PAYLOAD_BYTES);
    while bytes.len() < PAYLOAD_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(PAYLOAD_BYTES);
    bytes
}

/// Declare `input_root` as a signed import root of the node, before the
/// policy generation is sealed by initialization.
fn admit_import_root(state: &Path, fixture: &FastFixture, input_root: &Path) -> Result<()> {
    use ryeos_node_runtime::node_policy::sections::external_content::{
        ExternalContentImportPolicyRecord, ExternalContentImportRoot,
    };
    let root = lillux::PinnedDirectory::open(input_root)?.context("import root is absent")?;
    let path = state.join(".ai/node/policies/external_content.yaml");
    let mut policy: ExternalContentImportPolicyRecord = serde_yaml::from_str(
        &lillux::signature::strip_signature_lines(&std::fs::read_to_string(&path)?),
    )?;
    ensure!(
        policy.limits.max_file_bytes >= MAXIMUM_BYTES
            && policy.limits.max_total_bytes >= MAXIMUM_BYTES,
        "fixture import limits cannot admit the payload"
    );
    policy.roots.insert(
        IMPORT_ROOT.into(),
        ExternalContentImportRoot {
            path: input_root.to_path_buf(),
            root_identity: root.identity()?,
        },
    );
    policy.validate()?;
    std::fs::write(
        path,
        lillux::signature::sign_content_at(
            &serde_yaml::to_string(&policy)?,
            &fixture.node,
            "#",
            None,
            common::fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    Ok(())
}

/// A pinned project Tool that mounts the imported file and reports the
/// SHA-256 of the bytes it sees there.
fn write_consumer_tool(project: &Path, fixture: &FastFixture, manifest_hash: &str) -> Result<()> {
    let body = format!(
        r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "test"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Read the pinned large weights file"
#   external_content:
#     - id: weights
#       kind: file
#       mode: pinned
#       digest: "{manifest_hash}"
#       mount_root: project
#       mount: data/weights.bin

import hashlib
import json
import os
import sys

project = sys.argv[sys.argv.index("--project-path") + 1]
path = os.path.join(project, "data", "weights.bin")
digest = None
if os.path.isfile(path):
    with open(path, "rb") as handle:
        digest = hashlib.sha256(handle.read()).hexdigest()
print(json.dumps({{"sha256": digest}}))
"#
    );
    let path = project.join(".ai/tools/test/weights-reader.py");
    std::fs::create_dir_all(path.parent().context("tool parent")?)?;
    std::fs::write(
        path,
        lillux::signature::sign_content(&body, &fixture.publisher, "#", None),
    )?;
    Ok(())
}

async fn service(h: &DaemonHarness, item_ref: &str, parameters: Value) -> Result<Value> {
    let (status, response) = tokio::time::timeout(
        Duration::from_secs(120),
        h.post_execute(item_ref, ".", parameters),
    )
    .await
    .with_context(|| format!("{item_ref} timed out"))??;
    ensure!(
        status.is_success(),
        "{item_ref} failed: {status}: {response}"
    );
    response
        .get("result")
        .cloned()
        .with_context(|| format!("{item_ref} returned no result: {response}"))
}

async fn import_payload(h: &DaemonHarness) -> Result<Value> {
    let imported = service(
        h,
        "service:external-content/import",
        json!({
            "source": "filesystem", "root": IMPORT_ROOT, "path": PAYLOAD_NAME,
            "shape": "file", "storage": "large_content", "maximum_bytes": MAXIMUM_BYTES,
        }),
    )
    .await?;
    ensure!(
        imported["manifest_kind"] == ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
        "the import did not produce a large-content manifest: {imported}"
    );
    Ok(imported)
}

/// Capture the project as an exact snapshot through the ordinary Tool.
async fn snapshot_project(h: &DaemonHarness, project: &Path) -> Result<String> {
    let (status, response) = tokio::time::timeout(
        Duration::from_secs(150),
        h.post_json(
            "/execute",
            json!({
                "item_ref": "tool:core/snapshot-create", "ref_bindings": {},
                "project_path": project,
                "parameters": {"project_path": project,
                    "message": "large-object quarantine consumer", "allow_empty": true},
                "execution_policy": ExecutionPolicy::local_live(ExecutionResponse::Wait)
                    .exclude_operator_vault(),
            }),
        ),
    )
    .await
    .context("snapshot-create timed out")??;
    ensure!(
        status.is_success() && response.pointer("/thread/status") == Some(&json!("completed")),
        "snapshot-create did not complete: {status}: {response}"
    );
    let hash = response
        .pointer("/result/result/snapshot_hash")
        .and_then(Value::as_str)
        .context("snapshot-create returned no snapshot hash")?;
    ensure!(lillux::valid_hash(hash), "invalid snapshot hash {hash}");
    Ok(hash.to_owned())
}

/// Launch the consumer against its pinned snapshot.
async fn launch_consumer(
    h: &DaemonHarness,
    project: &Path,
    snapshot_hash: &str,
) -> Result<(reqwest::StatusCode, Value)> {
    let mut policy = ExecutionPolicy::local_pinned_capture(ExecutionResponse::Wait);
    policy.environment = ExecutionEnvironmentPolicy::None;
    policy.project = ProjectExecutionPolicy::Pinned {
        source: PinnedSource::Snapshot {
            hash: snapshot_hash.to_owned(),
        },
        realization: PinnedRealization::ReadOnly,
        child_policy: ChildProjectPolicy::Inherit,
    };
    policy.validate()?;
    tokio::time::timeout(
        Duration::from_secs(120),
        h.post_json(
            "/execute",
            json!({
                "item_ref": TOOL_REF, "project_path": project, "ref_bindings": {},
                "parameters": {}, "execution_policy": policy,
            }),
        ),
    )
    .await
    .context("consumer launch timed out")?
}

/// Require a completed consumer launch that read exactly `expected` bytes
/// when the mount is visible at its working directory.
fn require_consumer_completed(
    status: reqwest::StatusCode,
    response: &Value,
    expected_sha256: &str,
    what: &str,
) -> Result<()> {
    ensure!(
        status.is_success() && response.pointer("/thread/status") == Some(&json!("completed")),
        "{what}: consumer did not complete: {status}: {response}"
    );
    let seen = response
        .pointer("/result/result/sha256")
        .or_else(|| response.pointer("/result/result/stdout/sha256"))
        .cloned()
        .unwrap_or(Value::Null);
    eprintln!("{what}: consumer saw sha256={seen}");
    if let Some(seen) = seen.as_str() {
        ensure!(
            seen == expected_sha256,
            "{what}: consumer read {seen}, expected the original {expected_sha256}"
        );
    }
    Ok(())
}

fn store_root(state_path: &Path) -> PathBuf {
    ryeos_engine::roots::RuntimeRoot::new(state_path.to_path_buf())
        .state()
        .join("large-objects")
}

fn quarantined_names(state_path: &Path) -> Result<Vec<String>> {
    let directory = store_root(state_path).join("quarantine/objects");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut names = std::fs::read_dir(&directory)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

/// Flip one byte of the served object in place, as disk rot would.
fn rot_object(state_path: &Path, file_sha256: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = store_root(state_path).join("objects").join(file_sha256);
    ensure!(path.is_file(), "no served object at {}", path.display());
    let original = std::fs::metadata(&path)?.permissions();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    let mut bytes = std::fs::read(&path)?;
    bytes[PAYLOAD_BYTES / 2] ^= 0xff;
    std::fs::write(&path, &bytes)?;
    std::fs::set_permissions(&path, original)?;
    Ok(())
}

/// Run one stopped-node `ryeos` command against the harness node and return
/// its parsed JSON stdout, requiring success.
async fn ryeos_offline_json(h: &DaemonHarness, args: &[&str]) -> Result<Value> {
    let output = tokio::process::Command::new(common::ryeos_binary())
        .args(args)
        .arg("--json")
        .arg("--app-root")
        .arg(&h.state_path)
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("HOME", h.user_space.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn ryeos {args:?}"))?;
    ensure!(
        output.status.success(),
        "ryeos {args:?} exited {}; stdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "ryeos {args:?} printed non-JSON: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn scrub_quarantines_rotten_object_launch_refuses_reimport_heals_and_purge_discards()
-> Result<()> {
    let inputs = tempfile::tempdir()?;
    let bytes = payload();
    let file_sha256 = lillux::sha256_hex(&bytes);
    std::fs::write(inputs.path().join(PAYLOAD_NAME), &bytes)?;
    let input_root = inputs.path().to_path_buf();

    let (mut h, fixture) = DaemonHarness::start_fast_with(
        move |state, _user, fixture| admit_import_root(state, fixture, &input_root),
        |_| {},
    )
    .await?;
    let project = tempfile::tempdir()?;

    // 1. Import, bind, and launch the healthy consumer.
    let imported = import_payload(&h).await?;
    let manifest_hash = imported["manifest_hash"]
        .as_str()
        .context("import returned no manifest hash")?
        .to_owned();
    ensure!(
        store_root(&h.state_path)
            .join("objects")
            .join(&file_sha256)
            .is_file(),
        "the import did not publish the object into the large-object store"
    );
    write_consumer_tool(project.path(), &fixture, &manifest_hash)?;
    let snapshot_hash = snapshot_project(&h, project.path()).await?;
    let bound = service(
        &h,
        "service:external-content/bind",
        json!({
            "staging_id": imported["staging_id"], "request_digest": imported["request_digest"],
            "manifest_hash": manifest_hash, "consumer_ref": TOOL_REF,
            "consumer_kind": "pinned_project", "project_snapshot_hash": snapshot_hash,
            "project_path": project.path(),
        }),
    )
    .await?;
    let binding_subject_id = bound["binding_subject_id"]
        .as_str()
        .context("bind returned no binding subject")?
        .to_owned();
    let (status, response) = launch_consumer(&h, project.path(), &snapshot_hash).await?;
    require_consumer_completed(status, &response, &file_sha256, "healthy launch")?;

    // 2. Rot the stored bytes.
    rot_object(&h.state_path, &file_sha256)?;

    // 3. Scrub finds the damage and quarantines the object.
    let scrubbed = service(&h, "service:external-content/scrub", json!({})).await?;
    assert_eq!(
        scrubbed["objects_quarantined"],
        json!([file_sha256]),
        "scrub must quarantine exactly the rotten object: {scrubbed}"
    );
    assert!(
        !store_root(&h.state_path)
            .join("objects")
            .join(&file_sha256)
            .exists(),
        "the rotten object must leave the served namespace"
    );
    assert_eq!(
        quarantined_names(&h.state_path)?,
        vec![file_sha256.clone()],
        "the rotten object must be held in quarantine/objects"
    );
    assert!(
        store_root(&h.state_path)
            .join("quarantine/sidecars")
            .join(&file_sha256)
            .is_file(),
        "the object's sidecar must move into quarantine with it"
    );
    let findings = scrubbed["binding_findings"]
        .as_array()
        .context("scrub returned no binding findings")?;
    assert!(
        findings.iter().any(|finding| {
            finding["binding_subject_id"] == binding_subject_id.as_str()
                && finding["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("quarantined"))
        }),
        "scrub must report the binding that needs the quarantined object: {scrubbed}"
    );

    // 4. A launch that needs the object is refused typed, naming the cure.
    let (status, response) = launch_consumer(&h, project.path(), &snapshot_hash).await?;
    let rendered = response.to_string();
    assert!(
        !status.is_success() || response.pointer("/thread/status") != Some(&json!("completed")),
        "a launch over a quarantined object must not complete: {status}: {response}"
    );
    assert!(
        rendered.contains(QUARANTINED_CODE)
            && rendered.contains(&file_sha256)
            && rendered.contains("purge-quarantine"),
        "expected the typed `{QUARANTINED_CODE}` refusal naming the object and its cure: {status}: {response}"
    );

    // 5. Re-importing the original bytes publishes a clean copy and heals the
    //    launch; the damaged copy stays as evidence.
    let reimported = import_payload(&h).await?;
    assert_eq!(reimported["manifest_hash"], manifest_hash.as_str());
    assert!(
        store_root(&h.state_path)
            .join("objects")
            .join(&file_sha256)
            .is_file(),
        "the re-import must publish a clean served copy"
    );
    assert_eq!(
        std::fs::read(store_root(&h.state_path).join("objects").join(&file_sha256))?,
        bytes,
        "the served copy must hold the original bytes"
    );
    let (status, response) = launch_consumer(&h, project.path(), &snapshot_hash).await?;
    require_consumer_completed(status, &response, &file_sha256, "launch after re-import")?;
    assert_eq!(quarantined_names(&h.state_path)?, vec![file_sha256.clone()]);
    let rescrubbed = service(&h, "service:external-content/scrub", json!({})).await?;
    assert_eq!(
        rescrubbed["objects_quarantined"],
        json!([]),
        "the clean copy must scrub clean: {rescrubbed}"
    );

    // 6. With the daemon stopped, preview and then purge the quarantine.
    h.stop_gracefully().await?;
    let preview = ryeos_offline_json(
        &h,
        &["node", "large-objects", "purge-quarantine", "--dry-run"],
    )
    .await?;
    assert_eq!(preview["dry_run"], true, "{preview}");
    assert_eq!(preview["objects"], 1, "{preview}");
    assert_eq!(preview["bytes"], PAYLOAD_BYTES, "{preview}");
    assert_eq!(
        quarantined_names(&h.state_path)?,
        vec![file_sha256.clone()],
        "a dry run removes nothing"
    );
    let purged = ryeos_offline_json(
        &h,
        &["node", "large-objects", "purge-quarantine", "--confirm"],
    )
    .await?;
    let purged = purged.get("result").cloned().unwrap_or(purged);
    assert_eq!(purged["objects"], 1, "{purged}");
    assert_eq!(purged["sidecars"], 1, "{purged}");
    assert!(
        quarantined_names(&h.state_path)?.is_empty(),
        "purge must empty quarantine/objects"
    );
    assert!(
        !store_root(&h.state_path)
            .join("quarantine/sidecars")
            .join(&file_sha256)
            .exists(),
        "purge must remove the quarantined sidecar"
    );
    assert_eq!(
        std::fs::read(store_root(&h.state_path).join("objects").join(&file_sha256))?,
        bytes,
        "purge must leave the served clean copy untouched"
    );
    Ok(())
}
