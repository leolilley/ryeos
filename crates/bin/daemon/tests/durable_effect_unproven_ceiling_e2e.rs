//! The tool and graph kinds admit at most `recorded` from an item's own
//! `effects` declaration (`effect_class_ceiling.unproven_max`). A graph node
//! that asks for `sealed` against a tool or graph that self-declares
//! `effects: sealed` is refused at the daemon's callback dispatch boundary
//! with `durable_effect_sealed_unqualified`; the same node asking for
//! `recorded` passes that boundary.
//!
//! The durable request is a graph node's authored `effects:` class: the
//! daemon derives the admitted effect authorization from the signed graph and
//! enforces it against the callee's preflight before any child is launched.
//! A recorded dispatch that clears the effect gate then needs an admitted
//! retained-product launch to complete; these fixtures have none, so the
//! recorded runs are judged only on not being refused by the effect gate.

use ryeosd::test_support as common;

use std::path::Path;
use std::time::Duration;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

const SEALED_UNQUALIFIED: &str = "durable_effect_sealed_unqualified";

fn write_signed(path: &Path, body: &str, signer: &SigningKey) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("item path has a parent"))?;
    std::fs::write(
        path,
        lillux::signature::sign_content(body, signer, "#", None),
    )?;
    Ok(())
}

/// A Python tool whose author testifies `effects: sealed`.
fn plant_sealed_tool(project: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "test"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Self-declared sealed tool"
#   effects: "sealed"

import json
print(json.dumps({"sealed": True}))
"#;
    write_signed(&project.join(".ai/tools/test/sealed-tool.py"), body, signer)
}

/// A graph whose author testifies `effects: sealed` for itself.
fn plant_sealed_graph(project: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let body = r#"category: test
version: "1.0.0"
effects: sealed
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    write_signed(
        &project.join(".ai/graphs/test/sealed-inner.yaml"),
        body,
        signer,
    )
}

/// A caller graph with one inline durable action node of class `class`
/// targeting `target`.
fn plant_caller(
    project: &Path,
    name: &str,
    target: &str,
    class: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let (kind, bare) = target.split_once(':').expect("canonical target ref");
    let body = format!(
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.{kind}.{bare}
config:
  start: call
  max_steps: 2
  on_error: fail
  nodes:
    call:
      node_type: action
      effects: {class}
      cache_result: false
      action:
        item_id: "{target}"
        ref_bindings: {{}}
        params: {{}}
        thread: inline
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#
    );
    write_signed(
        &project.join(format!(".ai/graphs/test/{name}.yaml")),
        &body,
        signer,
    )
}

fn plant_project(project: &Path, fixture: &FastFixture) -> anyhow::Result<()> {
    let signer = &fixture.publisher;
    plant_sealed_tool(project, signer)?;
    plant_sealed_graph(project, signer)?;
    for (name, target, class) in [
        ("tool-sealed", "tool:test/sealed-tool", "sealed"),
        ("tool-recorded", "tool:test/sealed-tool", "recorded"),
        ("graph-sealed", "graph:test/sealed-inner", "sealed"),
        ("graph-recorded", "graph:test/sealed-inner", "recorded"),
    ] {
        plant_caller(project, name, target, class, signer)?;
    }
    Ok(())
}

/// Run `graph:test/<name>` to its terminal answer and return the response
/// together with every persisted event of its thread, rendered as one string
/// so a refusal is found wherever the daemon records it.
async fn run_caller(h: &mut DaemonHarness, project: &Path, name: &str) -> (Value, String) {
    let item_ref = format!("graph:test/{name}");
    let observed = tokio::time::timeout(
        Duration::from_secs(60),
        h.post_execute(&item_ref, project.to_str().unwrap(), json!({})),
    )
    .await;
    let (status, body) = match observed {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("{item_ref}: POST /execute failed: {error:#}\n--- daemon stderr ---\n{stderr}")
        }
        Err(_) => {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!("{item_ref}: POST /execute timed out\n--- daemon stderr ---\n{stderr}")
        }
    };
    let mut evidence = format!("status={status} body={body}");
    if let Some(thread_id) = body.pointer("/thread/thread_id").and_then(Value::as_str) {
        if let Ok(events) = h.existing_thread_events(thread_id).await {
            for (event_type, payload) in events {
                evidence.push_str(&format!("\n{event_type}: {payload}"));
            }
        }
    }
    (body, evidence)
}

fn graph_succeeded(body: &Value) -> bool {
    body.pointer("/result/success").and_then(Value::as_bool) == Some(true)
}

#[tokio::test(flavor = "multi_thread")]
async fn sealed_request_against_self_declared_sealed_callee_is_refused_unqualified() {
    let (mut h, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user, fixture| register_workflow_bundles(state_path, fixture),
        |_| {},
    )
    .await
    .expect("start daemon with the workflow bundles");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_project(project.path(), &fixture).expect("plant project items");

    for (name, callee) in [
        ("tool-sealed", "tool:test/sealed-tool"),
        ("graph-sealed", "graph:test/sealed-inner"),
    ] {
        let (body, evidence) = run_caller(&mut h, project.path(), name).await;
        assert!(
            !graph_succeeded(&body),
            "{name}: a sealed request against unproven `{callee}` must not complete; {evidence}"
        );
        assert!(
            evidence.contains(SEALED_UNQUALIFIED),
            "{name}: expected the typed `{SEALED_UNQUALIFIED}` refusal; {evidence}"
        );
        assert!(
            evidence.contains(callee) && evidence.contains("at most recorded"),
            "{name}: the refusal must name `{callee}` and the kind's recorded cap; {evidence}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recorded_request_against_self_declared_sealed_callee_clears_the_effect_gate() {
    let (mut h, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user, fixture| register_workflow_bundles(state_path, fixture),
        |_| {},
    )
    .await
    .expect("start daemon with the workflow bundles");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_project(project.path(), &fixture).expect("plant project items");

    for name in ["tool-recorded", "graph-recorded"] {
        let (_body, evidence) = run_caller(&mut h, project.path(), name).await;
        // A recorded request is within the kind's unproven cap and the
        // callee's sealed ceiling: none of the effect-class refusals applies.
        for refusal in [
            SEALED_UNQUALIFIED,
            "durable_effect_ceiling_exceeded",
            "durable_effect_subject_ineligible",
        ] {
            assert!(
                !evidence.contains(refusal),
                "{name}: a recorded request must clear the effect gate, got `{refusal}`; {evidence}"
            );
        }
        // The node really was dispatched as a durable action: the graph
        // validated and reached the callback boundary. Either it completed or
        // it stopped past the gate, at the retained-product launch these
        // fixtures do not provide.
        eprintln!("{name}: {evidence}");
    }
}
