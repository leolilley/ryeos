//! Inline in-process child lineage e2e.
//!
//! A graph node that calls an in-process service inline (`thread: "inline"`)
//! gets a recorded service child that is a thread in the graph's chain with
//! the graph thread as upstream, exactly like an inline tool leaf. The
//! lineage must be visible through the ordinary thread readers with no
//! reader change: `threads.children`, `threads.get`, `threads.chain`, and the
//! graph thread's own `child_thread_spawned` event.

mod common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

/// Plant a graph whose single action node calls an in-process service inline.
fn plant_in_process_service_graph(project_dir: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let graphs_dir = project_dir.join(".ai/graphs");
    std::fs::create_dir_all(&graphs_dir)?;
    let body = r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.service.health/status
config:
  start: inspect
  nodes:
    inspect:
      action:
        item_id: "service:health/status"
        ref_bindings: {}
        params: {}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(graphs_dir.join("in_process.yaml"), signed)?;
    Ok(())
}

async fn service_result(h: &DaemonHarness, service_ref: &str, params: Value) -> Value {
    let (status, body) = h
        .post_execute(service_ref, ".", params)
        .await
        .unwrap_or_else(|error| panic!("post {service_ref}: {error:#}"));
    assert!(
        status.is_success(),
        "{service_ref} failed: status={status}; body={body:#}"
    );
    body.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{service_ref} response has no result: {body:#}"))
}

fn string_field<'a>(value: &'a Value, field: &str, ctx: &str) -> &'a str {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{ctx}: missing string `{field}`: {value:#}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_in_process_service_child_is_a_same_chain_child_of_the_graph_thread() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        Ok(())
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with the workflow bundles");

    let project = tempfile::tempdir().expect("project tempdir");
    plant_in_process_service_graph(project.path(), &fixture.publisher)
        .expect("plant in-process service graph");

    let (status, body) = h
        .post_execute(
            "graph:in_process",
            project.path().to_str().unwrap(),
            json!({}),
        )
        .await
        .expect("execute in-process service graph");
    if status != reqwest::StatusCode::OK
        || body.pointer("/result/status").and_then(Value::as_str) != Some("completed")
    {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "in-process service graph did not complete: status={status}; body={body:#}\n\
             --- daemon stderr ---\n{stderr}"
        );
    }
    let graph_thread = body
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("execute envelope has no graph thread_id: {body:#}"))
        .to_owned();

    // threads.get on the graph thread: it is its own chain root.
    let graph_view = service_result(&h, "service:threads/get", json!({ "thread_id": graph_thread }))
        .await;
    let graph_thread_view = graph_view
        .get("thread")
        .unwrap_or_else(|| panic!("threads.get has no thread: {graph_view:#}"));
    let graph_chain_root = string_field(graph_thread_view, "chain_root_id", "graph thread").to_owned();
    assert_eq!(
        graph_chain_root, graph_thread,
        "the graph thread is its own chain root"
    );

    // threads.children on the graph thread lists the inline in-process child.
    let children = service_result(
        &h,
        "service:threads/children",
        json!({ "thread_id": graph_thread }),
    )
    .await;
    let children = children
        .get("children")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("threads.children has no children array: {children:#}"));
    assert_eq!(
        children.len(),
        1,
        "one inline service action spawns exactly one child: {children:#?}"
    );
    let child = &children[0];
    let child_thread = string_field(child, "thread_id", "inline child").to_owned();
    assert_ne!(child_thread, graph_thread);
    assert_eq!(
        string_field(child, "item_ref", "inline child"),
        "service:health/status"
    );
    assert_eq!(
        string_field(child, "upstream_thread_id", "inline child"),
        graph_thread,
        "the inline in-process child has the graph thread as upstream"
    );
    assert_eq!(
        string_field(child, "chain_root_id", "inline child"),
        graph_chain_root,
        "the inline in-process child lives in the graph thread's chain"
    );

    // threads.get on the child reads the same placement and a settled terminal.
    let child_view = service_result(&h, "service:threads/get", json!({ "thread_id": child_thread }))
        .await;
    let child_thread_view = child_view
        .get("thread")
        .unwrap_or_else(|| panic!("threads.get has no thread: {child_view:#}"));
    assert_eq!(
        string_field(child_thread_view, "chain_root_id", "child threads.get"),
        graph_chain_root
    );
    assert_eq!(
        string_field(child_thread_view, "upstream_thread_id", "child threads.get"),
        graph_thread
    );
    assert_eq!(
        string_field(child_thread_view, "status", "child threads.get"),
        "completed"
    );

    // threads.chain on the graph thread carries the child as a member and the
    // projected dispatch edge from the graph thread to the child.
    let chain = service_result(&h, "service:threads/chain", json!({ "thread_id": graph_thread }))
        .await;
    let members = chain
        .get("threads")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("threads.chain has no threads: {chain:#}"));
    assert!(
        members
            .iter()
            .any(|member| member.get("thread_id").and_then(Value::as_str) == Some(&child_thread)),
        "the chain lists the inline in-process child: {members:#?}"
    );
    let edges = chain
        .get("edges")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("threads.chain has no edges: {chain:#}"));
    let edge = edges
        .iter()
        .find(|edge| {
            edge.get("source_thread_id").and_then(Value::as_str) == Some(&graph_thread)
                && edge.get("target_thread_id").and_then(Value::as_str) == Some(&child_thread)
        })
        .unwrap_or_else(|| panic!("no graph -> child edge in chain edges: {edges:#?}"));
    assert_eq!(string_field(edge, "edge_type", "chain edge"), "spawned");
    assert_eq!(
        string_field(edge, "chain_root_id", "chain edge"),
        graph_chain_root
    );

    // The graph thread's own braid carries the child_thread_spawned event.
    let spawned = h
        .principal_events()
        .await
        .into_iter()
        .filter(|(thread_id, event_type, _)| {
            thread_id == &graph_thread && event_type == "child_thread_spawned"
        })
        .map(|(_, _, payload)| payload)
        .collect::<Vec<_>>();
    assert_eq!(
        spawned.len(),
        1,
        "exactly one child_thread_spawned on the graph thread: {spawned:#?}"
    );
    assert_eq!(
        spawned[0].get("child_thread_id").and_then(Value::as_str),
        Some(child_thread.as_str())
    );
    assert_eq!(
        spawned[0].get("node").and_then(Value::as_str),
        Some("inspect")
    );
    assert_eq!(
        spawned[0].get("item_id").and_then(Value::as_str),
        Some("service:health/status")
    );
    assert_eq!(
        spawned[0].get("spawn_reason").and_then(Value::as_str),
        Some("dispatch")
    );

    h.stop_gracefully().await.expect("stop daemon");
}
