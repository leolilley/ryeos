//! A tool on a runtime that declares `source_scope` runs from a private
//! admitted-input root and never sees the live project. Launching one with
//! live project authority is refused with a typed code instead of exiting 0
//! with its project writes discarded; a pinned (copy-on-write) launch of the
//! same tool runs, and an `@subprocess` tool without `source_scope` still
//! gets the live project.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use serde_json::{Value, json};

const CODE: &str = "live_project_unavailable_to_sourced_runtime";

const PYTHON_TOOL: &str = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "livecheck"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "writes into the project it was given"

import os, sys

project = sys.argv[sys.argv.index("--project-path") + 1]
with open(os.path.join(project, "written-by-tool.txt"), "w") as handle:
    handle.write("hello")
print("done")
"#;

fn subprocess_tool(project: &Path) -> String {
    format!(
        "category: livecheck
executor_id: \"@subprocess\"
execution_protocol: \"protocol:ryeos/core/opaque\"
config:
  command: /bin/sh
  args: [\"-c\", \"echo live > {}/written-by-subprocess.txt\"]
",
        project.display()
    )
}

fn plant(project: &Path, name: &str, ext: &str, body: &str, signer: &SigningKey) {
    let dir = project.join(".ai/tools/livecheck");
    std::fs::create_dir_all(&dir).expect("create tools dir");
    std::fs::write(
        dir.join(format!("{name}.{ext}")),
        lillux::signature::sign_content(body, signer, "#", None),
    )
    .expect("write tool");
}

async fn run(
    h: &DaemonHarness,
    item_ref: &str,
    project: &Path,
    policy: ExecutionPolicy,
) -> (reqwest::StatusCode, Value) {
    h.post_json(
        "/execute",
        json!({
            "item_ref": item_ref,
            "ref_bindings": {},
            "project_path": project,
            "parameters": {},
            "execution_policy": policy,
        }),
    )
    .await
    .expect("post /execute")
}

#[tokio::test(flavor = "multi_thread")]
async fn live_authority_on_a_source_scoped_runtime_is_refused_typed() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "py", "py", PYTHON_TOOL, &fixture.user);

    let (status, body) = run(
        &h,
        "tool:livecheck/py",
        project.path(),
        ExecutionPolicy::local_live(ExecutionResponse::Wait),
    )
    .await;
    let text = body.to_string();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some(CODE),
        "{body:#}"
    );
    assert!(text.contains("tool:livecheck/py"), "{body:#}");
    assert!(
        text.contains("tool:ryeos/core/runtimes/python/script"),
        "{body:#}"
    );
    assert!(text.contains("--pin-project"), "{body:#}");
    assert!(
        !project.path().join("written-by-tool.txt").exists(),
        "the refused tool must not have run"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_tool_runs_pinned_and_a_subprocess_tool_still_writes_live() {
    let (h, fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), "py", "py", PYTHON_TOOL, &fixture.user);
    plant(
        project.path(),
        "sub",
        "yaml",
        &subprocess_tool(project.path()),
        &fixture.user,
    );

    let (status, body) = run(
        &h,
        "tool:livecheck/py",
        project.path(),
        ExecutionPolicy::local_pinned_capture(ExecutionResponse::Wait),
    )
    .await;
    assert!(status.is_success(), "pinned launch: {status}: {body:#}");
    assert_eq!(
        body.pointer("/thread/status"),
        Some(&json!("completed")),
        "{body:#}"
    );

    let (status, body) = run(
        &h,
        "tool:livecheck/sub",
        project.path(),
        ExecutionPolicy::local_live(ExecutionResponse::Wait),
    )
    .await;
    assert!(status.is_success(), "subprocess launch: {status}: {body:#}");
    assert!(
        project.path().join("written-by-subprocess.txt").is_file(),
        "an @subprocess tool without source_scope must write the live project: {body:#}"
    );
}
