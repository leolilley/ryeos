//! A graph's own `fail` return node end to end: the child graph ends failed
//! with the author's code, and a graph that follows it reads the code and who
//! chose it (`failure_origin`) in `state._outcome`.
//!
//! The code is deliberately one the platform itself uses, to show that a
//! reader tells the author's failure from the platform's by its origin.
//! Mechanism only: nothing here shows that a graph should fail rather than
//! complete.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

use ryeosd::test_support as common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;

fn plant_vault_with_zen_key(state_path: &Path) -> anyhow::Result<()> {
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

fn plant(project: &Path, signer: &SigningKey, name: &str, body: &str) {
    let dir = project.join(".ai/graphs");
    std::fs::create_dir_all(&dir).expect("graphs dir");
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join(format!("{name}.yaml")), signed).expect("write graph");
}

/// The followed child: reaches a `fail` return node with a code the platform
/// also uses, and a message built from its own state.
const CHILD: &str = r#"category: test
version: "1.0.0"
config:
  start: decide
  state: {goal: "write a poem"}
  nodes:
    decide:
      node_type: gate
      next:
        type: conditional
        branches:
          - to: stop
    stop:
      node_type: return
      fail: {code: depth_limit_exceeded, message: "the goal was not met: ${state.goal}"}
"#;

/// The parent follows the child, routes its failure to `recover`, and returns
/// what it read in `state._outcome`.
const PARENT: &str = r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.graph.child_failing
config:
  start: fetch
  nodes:
    fetch:
      node_type: action
      follow: true
      action:
        item_id: "graph:child_failing"
        ref_bindings: {}
        params: {}
      on_error: recover
      next: {type: unconditional, to: unreached}
    recover:
      node_type: return
      output:
        code: "${state._outcome.failure_code}"
        origin: "${state._outcome.failure_origin}"
        kind: "${state._outcome.outcome_kind}"
    unreached:
      node_type: return
      output: {reached: "the success branch"}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_graph_failing_with_its_own_code_is_read_by_its_follower_with_its_origin() {
    let plant_state =
        |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
            register_workflow_bundles(state_path, fixture)?;
            plant_vault_with_zen_key(state_path)?;
            Ok(())
        };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant_state, |_| {})
        .await
        .expect("start daemon");

    let project = tempfile::tempdir().expect("project tempdir");
    plant(project.path(), &fixture.publisher, "child_failing", CHILD);
    plant(project.path(), &fixture.publisher, "parent_failing", PARENT);

    let (status, body) = tokio::time::timeout(
        Duration::from_secs(60),
        h.post_execute(
            "graph:parent_failing",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        ),
    )
    .await
    .expect("execute timed out")
    .expect("post /execute");
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    let parent = body["thread"]["thread_id"]
        .as_str()
        .expect("parent thread id")
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(120);
    let (events, successor) = loop {
        let events = h.principal_events().await;
        let successor = events
            .iter()
            .find(|(tid, ty, _)| ty == "graph_completed" && *tid != parent)
            .map(|(tid, _, _)| tid.clone());
        if let Some(successor) = successor {
            break (events, successor);
        }
        if Instant::now() >= deadline {
            let stderr = h.drain_stderr_nonblocking().await;
            panic!(
                "the parent never completed.\nevents={events:#?}\n--- daemon stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    // The child ended failed, never completed, and its thread says why.
    let child_failed = events.iter().any(|(_, ty, payload)| {
        ty == "thread_failed" && payload.to_string().contains("depth_limit_exceeded")
    });
    assert!(
        child_failed,
        "the child thread failed with its code; events={events:#?}"
    );

    assert!(
        h.wait_for_thread_terminal_event(&successor, Duration::from_secs(60))
            .await,
        "successor {successor} never reached a terminal event"
    );
    let (get_status, get_body) = h
        .post_execute(
            "service:threads/get",
            project.path().to_str().unwrap(),
            serde_json::json!({ "thread_id": successor }),
        )
        .await
        .expect("post threads/get");
    assert!(get_status.is_success(), "body={get_body:#}");
    let text = get_body.to_string();
    // The follower read the code, the origin that tells it from the platform's
    // own `depth_limit_exceeded`, and the outcome class.
    assert!(text.contains("depth_limit_exceeded"), "{text}");
    assert!(text.contains("author"), "{text}");
    assert!(!text.contains("the success branch"), "{text}");
}
