//! What a followed directive read, end to end: a parent graph follows a
//! directive whose model calls a tool that declares `reads` (twice on one ref)
//! and a tool that does not; the graph then sees `dispatch.reads` built by the
//! daemon from its own records.
//!
//! The mechanism, not the model: the refs are what the model asked for in calls
//! that succeeded through a tool the daemon executed and recorded at launch.
//! The planted tool lives in the project, so its entries carry `space: project`.
//! These need populated bundles (`-f bundles=branch`).

use ryeosd::test_support as common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::DaemonHarness;
use common::fast_fixture::{
    FastFixture, register_config_fixture_bundle, register_workflow_bundles,
};
use common::mock_provider::{MockProvider, MockResponse};
use lillux::crypto::SigningKey;

/// Plant ZEN_API_KEY in the sealed vault so the graph runtime preflight passes
/// (mirrors `graph_spawn_smoke`).
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

/// Plant the mock `chat_completions` provider (the mock returns a fixed
/// `usage: {prompt_tokens: 10, completion_tokens: 5}` on every call).
fn plant_mock_provider(
    root: &Path,
    mock_base_url: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime/model-providers");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        r#"family: chat_completions
transport:
  kind: remote_http
  base_url: "{mock_base_url}"
body_template:
  model: "{{model}}"
  messages: "{{messages}}"
  tools: "{{tools}}"
  stream: "{{stream}}"
auth: {{}}
headers: {{}}
schemas:
  accounting:
    require_usage: true
  streaming:
    mode: delta_merge
  output_limit:
    path: max_tokens
    semantics: provider_native_output_tokens
pricing:
  input_per_million: "0.0"
  output_per_million: "0.0"
"#
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

/// Map the `general` tier to the mock provider.
fn plant_model_routing(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/config/ryeos-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = r#"tiers:
  general:
    provider: mock
    model: mock-model
    context_window: 200000
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

/// A project tool whose header optionally declares `reads`.
fn plant_tool(
    root: &Path,
    name: &str,
    reads_argument: Option<&str>,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let dir = root.join(format!(".ai/tools/{name}"));
    std::fs::create_dir_all(&dir)?;
    let reads = reads_argument
        .map(|argument| format!("#   reads:\n#     argument: \"{argument}\"\n"))
        .unwrap_or_default();
    let body = format!(
        "#!/usr/bin/env python3\n# ryeos-tool:\n#   category: \"{name}\"\n#   version: \"1.0.0\"\n#   executor_id: \"tool:ryeos/core/runtimes/python/script\"\n#   execution_protocol: \"protocol:ryeos/core/opaque\"\n#   description: \"reads e2e tool {name}\"\n{reads}\nimport json\nprint(json.dumps({{\"ok\": True}}))\n"
    );
    let signed = lillux::signature::sign_content(&body, signer, "#", None);
    std::fs::write(dir.join(format!("{name}.py")), signed)?;
    Ok(())
}

fn plant_reader_directive(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/directives");
    std::fs::create_dir_all(&dir)?;
    let body = r#"---
name: reader_child
category: ""
description: "reads e2e child"
model:
  tier: general
requires:
  capabilities:
    declared:
      - "ryeos.execute.tool.*"
---
Read the notes.
"#;
    let signed = lillux::signature::sign_content(body, signer, "<!--", Some("-->"));
    std::fs::write(dir.join("reader_child.md"), signed)?;
    Ok(())
}

fn plant_parent_graph(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/graphs");
    std::fs::create_dir_all(&dir)?;
    let body = r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.reader_child
      - "ryeos.execute.tool.*"
config:
  start: fetch
  nodes:
    fetch:
      node_type: action
      follow: true
      action:
        item_id: "directive:reader_child"
        ref_bindings:
          model: "directive:reader_child"
        params: {}
      assign:
        reads: "${dispatch.reads}"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
      output:
        reads: "${state.reads}"
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("reads_parent.yaml"), signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_followed_directives_reads_are_projected_from_the_daemons_own_records() {
    let mock = MockProvider::start(vec![
        MockResponse::ToolCall {
            id: "c1".into(),
            name: "reader_reader".into(),
            arguments: r#"{"path":"notes/a"}"#.into(),
        },
        MockResponse::ToolCall {
            id: "c2".into(),
            name: "reader_reader".into(),
            arguments: r#"{"path":"notes/a"}"#.into(),
        },
        MockResponse::ToolCall {
            id: "c3".into(),
            name: "other_other".into(),
            arguments: r#"{"path":"notes/b"}"#.into(),
        },
        MockResponse::Text("done".into()),
    ])
    .await;
    let mock_url = mock.base_url.clone();
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)?;
        register_config_fixture_bundle(
            state_path,
            "fixture-reads-model-config",
            fixture,
            |bundle_root| plant_mock_provider(bundle_root, &mock_url, &fixture.publisher),
        )?;
        plant_vault_with_zen_key(state_path)?;
        Ok(())
    };
    let (h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with mock provider + workflow bundles");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_model_routing(project.path(), &fixture.publisher).expect("plant routing");
    plant_tool(project.path(), "reader", Some("path"), &fixture.publisher).expect("plant reader");
    plant_tool(project.path(), "other", None, &fixture.publisher).expect("plant other");
    plant_reader_directive(project.path(), &fixture.publisher).expect("plant child");
    plant_parent_graph(project.path(), &fixture.publisher).expect("plant parent");

    let (status, body) = h
        .post_execute(
            "graph:reads_parent",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post /execute");
    assert_eq!(status, reqwest::StatusCode::OK, "body={body:#}");
    let parent_tid = body["thread"]["thread_id"]
        .as_str()
        .expect("parent thread")
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(120);
    let successor_tid = loop {
        let events = h.principal_events().await;
        if let Some(tid) = events
            .iter()
            .find(|(tid, et, _)| et == "graph_completed" && *tid != parent_tid)
            .map(|(tid, _, _)| tid.clone())
        {
            break tid;
        }
        assert!(
            Instant::now() < deadline,
            "resumed parent never completed: {events:#?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(
        h.wait_for_thread_terminal_event(&successor_tid, Duration::from_secs(60))
            .await,
        "successor never reached a terminal event"
    );

    // The daemon recorded the read-declaring tool at launch, with its space.
    let events = h.principal_events().await;
    let table: Vec<_> = events
        .iter()
        .filter(|(_, et, _)| et == "launch_tool_reads")
        .collect();
    assert_eq!(
        table.len(),
        1,
        "one launch table for the directive: {events:#?}"
    );
    let tools = table[0].2["tools"].as_array().expect("tools array");
    let entry = |item_ref: &str| tools.iter().find(|tool| tool["item_ref"] == item_ref);
    let reader = entry("tool:reader/reader").expect("the project reader is in the table");
    assert_eq!(reader["argument"], "path");
    assert_eq!(reader["space"], "project");
    // The bundle's own read tools are in it too, each in the bundle space.
    let fetch = entry("tool:ryeos/core/fetch").expect("the bundle fetch tool is in the table");
    assert_eq!(fetch["argument"], "item_ref");
    assert_eq!(fetch["space"], "bundle");
    assert!(
        entry("tool:other/other").is_none(),
        "the undeclared tool is not in it: {tools:?}"
    );

    let (get_status, get_body) = h
        .post_execute(
            "service:threads/get",
            project.path().to_str().unwrap(),
            serde_json::json!({ "thread_id": successor_tid }),
        )
        .await
        .expect("threads/get");
    assert!(get_status.is_success(), "{get_body:#}");
    let text = serde_json::to_string(&get_body).unwrap_or_default();
    assert!(
        text.contains("notes/a"),
        "the declared read reached the graph: {text}"
    );
    assert!(text.contains("requested"), "{text}");
    assert!(text.contains("declared-read-tools-only"), "{text}");
    assert!(
        !text.contains("notes/b"),
        "an undeclared tool's argument is not a read: {text}"
    );
    drop(mock);
}
