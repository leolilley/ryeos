//! Under enforced isolation a pinned copy-on-write run is sealed from the
//! isolation adapter's workspace scan (protocol v15). An entry the adapter
//! cannot represent (an absolute symlink, a FIFO, a non-UTF-8 name) is
//! reported as `unrepresentable { reason }`; the executor skips it under a
//! path the node's ingest-ignore policy excludes and the seal succeeds, and
//! refuses it under an included path with the typed
//! `UnrepresentableWorkspaceEntry`, naming the path and the reason.
//!
//! The fast fixture runs its `local` host class under `enforce` with the
//! core `linux-lillux` backend, so a launched tool's writes reach the seal
//! only through the adapter. On a node whose fixture policy is `disabled`
//! the seal is the native copy capture, which this feature does not touch,
//! and the test says so and returns, as the other isolation-dependent tests
//! branch on `node_isolation_mode`.

use ryeosd::test_support as common;

use std::path::Path;
use std::time::Duration;

use common::DaemonHarness;
use common::fast_fixture::FastFixture;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use serde_json::{Value, json};

const TOOL_REF: &str = "tool:test/plant-entries";

/// A tool that plants, under the run's project, what its `mode` parameter
/// names, plus one ordinary included file so the run always changes the
/// tree.
fn plant_tool(project: &Path, fixture: &FastFixture) -> anyhow::Result<()> {
    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "test"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Plant entries the isolation adapter cannot represent"

import json
import os
import sys

project = sys.argv[sys.argv.index("--project-path") + 1]
raw = sys.stdin.read()
params = json.loads(raw) if raw.strip() else {}
mode = params.get("mode")


def under(*parts):
    path = os.path.join(project, *parts)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    return path


with open(under("out.txt"), "w", encoding="utf-8") as handle:
    handle.write("sealed output\n")

if mode == "ignored":
    # The motivating case: a virtualenv interpreter link, plus a FIFO and a
    # non-UTF-8 name, all under paths the ingest-ignore policy excludes.
    os.symlink("/usr/bin/python3", under(".venv", "bin", "python"))
    os.mkfifo(under("__pycache__", "pipe"))
    os.makedirs(under("node_modules", "x"), exist_ok=True)
    fd = os.open(os.path.join(os.fsencode(project), b"node_modules", b"bad-\xff"),
                 os.O_CREAT | os.O_WRONLY, 0o644)
    os.close(fd)
elif mode == "symlink":
    os.symlink("/usr/bin/python3", under("bin", "python"))
elif mode == "fifo":
    os.mkfifo(under("run", "pipe"))
elif mode == "non_utf8":
    os.makedirs(under("data", "x"), exist_ok=True)
    fd = os.open(os.path.join(os.fsencode(project), b"data", b"bad-\xff"),
                 os.O_CREAT | os.O_WRONLY, 0o644)
    os.close(fd)
else:
    raise SystemExit("unknown mode: %r" % (mode,))

print(json.dumps({"planted": mode}))
"#;
    let path = project.join(".ai/tools/test/plant-entries.py");
    std::fs::create_dir_all(path.parent().expect("tool parent"))?;
    std::fs::write(
        path,
        lillux::signature::sign_content(body, &fixture.publisher, "#", None),
    )?;
    Ok(())
}

/// Capture the live project, run the tool copy-on-write, and seal its result.
async fn run(h: &mut DaemonHarness, project: &Path, mode: &str) -> (reqwest::StatusCode, Value) {
    let observed = tokio::time::timeout(
        Duration::from_secs(120),
        h.post_json(
            "/execute",
            json!({
                "item_ref": TOOL_REF, "ref_bindings": {}, "project_path": project,
                "parameters": {"mode": mode},
                "execution_policy": ExecutionPolicy::local_pinned_capture(ExecutionResponse::Wait),
            }),
        ),
    )
    .await;
    match observed {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("{mode}: POST /execute failed: {error:#}\n--- daemon stderr ---\n{stderr}")
        }
        Err(_) => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("{mode}: POST /execute timed out\n--- daemon stderr ---\n{stderr}")
        }
    }
}

fn completed(status: reqwest::StatusCode, body: &Value) -> bool {
    status.is_success() && body.pointer("/thread/status") == Some(&json!("completed"))
}

/// The fast-fixture node, or `None` when its isolation policy does not route
/// the seal through the adapter.
async fn enforced_node() -> Option<(DaemonHarness, FastFixture)> {
    let (h, fixture) = DaemonHarness::start_fast()
        .await
        .expect("start the fast-fixture daemon");
    let mode = common::fast_fixture::node_isolation_mode(&h.state_path)
        .expect("read the node's isolation mode");
    match mode.as_str() {
        "enforce" => Some((h, fixture)),
        "disabled" => {
            eprintln!(
                "SKIP: node isolation is `disabled`; the seal is the native copy capture, not \
                 the isolation adapter scan this test covers"
            );
            None
        }
        other => panic!("node isolation mode {other:?} is neither enforce nor disabled"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unrepresentable_entries_under_ignored_paths_are_skipped_and_the_seal_succeeds() {
    let Some((mut h, fixture)) = enforced_node().await else {
        return;
    };
    let project = tempfile::tempdir().expect("project tempdir");
    plant_tool(project.path(), &fixture).expect("plant tool");

    let (status, body) = run(&mut h, project.path(), "ignored").await;
    let rendered = body.to_string();
    assert!(
        completed(status, &body),
        "ignored unrepresentable entries must not fail the seal: {status}: {body}"
    );
    assert!(
        !rendered.contains("unrepresentable"),
        "nothing ignored may be reported unrepresentable: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unrepresentable_entries_under_included_paths_refuse_the_seal_naming_path_and_reason() {
    let Some((mut h, fixture)) = enforced_node().await else {
        return;
    };
    let project = tempfile::tempdir().expect("project tempdir");
    plant_tool(project.path(), &fixture).expect("plant tool");

    for (mode, reason, path) in [
        ("symlink", "unportable_symlink_target", "bin/python"),
        ("fifo", "unsupported_entry_type", "run/pipe"),
        // The adapter reports a non-UTF-8 name with U+FFFD in its place.
        ("non_utf8", "non_utf8_name", "data/bad-\u{fffd}"),
    ] {
        let (status, body) = run(&mut h, project.path(), mode).await;
        let rendered = body.to_string();
        assert!(
            !completed(status, &body),
            "{mode}: an included unrepresentable entry must refuse the seal: {status}: {body}"
        );
        let expected =
            format!("unrepresentable workspace entry ({reason}) at included path {path}");
        assert!(
            rendered.contains(&expected),
            "{mode}: expected the typed refusal `{expected}`: {status}: {body}"
        );
    }
}
