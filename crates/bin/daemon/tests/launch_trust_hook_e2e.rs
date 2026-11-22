//! The launch trust floor on a graph hook action, end to end through the
//! daemon.
//!
//! A hook action is a real dispatch on the runtime callback path: the daemon
//! preflights the hook's item exactly as it preflights a node action, so an
//! unsigned project tool named by a hook is refused by the trust gate. Graph
//! hooks are observers, so the refusal is a routing-inert warning on a graph
//! that still completes, and the tool never runs.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;
use serde_json::Value;

/// The audit's gap-B tool from `launch_trust_floor_e2e.rs`, left unsigned.
const GAP_ECHO_TOOL: &str = "category: gap
executor_id: \"@subprocess\"
execution_protocol: \"protocol:ryeos/core/opaque\"
config:
  command: /bin/echo
  args: [\"gap-b-check\"]
";

fn plant_unsigned_gap_echo(project: &Path) {
    let dir = project.join(".ai/tools/gap");
    std::fs::create_dir_all(&dir).expect("create project tools dir");
    std::fs::write(dir.join("echo.yaml"), GAP_ECHO_TOOL).expect("write gap echo tool");
}

/// A pinned-signed graph whose `graph_started` hook dispatches the unsigned
/// tool. The graph declares the tool's execute capability, so the hook's
/// captured grants cover it and `enforce_callback_caps` lets the dispatch
/// through to preflight: only the trust gate is left to refuse it.
fn plant_hooked_graph(project: &Path, signer: &SigningKey) {
    let dir = project.join(".ai/graphs");
    std::fs::create_dir_all(&dir).expect("create project graphs dir");
    let body = r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.gap/echo
config:
  start: done
  hooks:
    - id: gap_probe
      event: graph_started
      result: discard
      action:
        item_id: "tool:gap/echo"
        ref_bindings: {}
        params: {}
  nodes:
    done:
      node_type: return
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("hooked.yaml"), signed).expect("write hooked graph");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graph_hook_naming_an_unsigned_project_tool_is_refused_by_the_trust_gate() {
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        register_workflow_bundles(state_path, fixture)
    };
    let (mut h, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
        .await
        .expect("start daemon with the workflow bundles");
    let project = tempfile::tempdir().expect("project tempdir");
    plant_unsigned_gap_echo(project.path());
    plant_hooked_graph(project.path(), &fixture.publisher);

    let (status, body) = h
        .post_execute(
            "graph:hooked",
            project.path().to_str().unwrap(),
            serde_json::json!({}),
        )
        .await
        .expect("post graph:hooked");
    if !status.is_success() {
        let stderr = h.drain_stderr_nonblocking().await;
        panic!(
            "the trusted graph itself must launch; only its hook's item is refused: \
             status={status}; body={body:#}\n--- daemon stderr ---\n{stderr}"
        );
    }

    let result = body.get("result").unwrap_or(&body);
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("completed"),
        "a refused observer hook must not fail the graph: {body:#}"
    );
    let warnings: Vec<&str> = result
        .get("warnings")
        .and_then(Value::as_array)
        .map(|warnings| warnings.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let hook_warning = warnings
        .iter()
        .find(|warning| warning.contains("graph_started"))
        .unwrap_or_else(|| panic!("the refused hook must leave a graph_started warning: {body:#}"));
    assert!(
        hook_warning.contains("effective_trust_unsigned"),
        "the hook must be refused by the trust gate, not a capability or policy check: \
         {hook_warning}"
    );
    assert!(
        hook_warning.contains("tool:gap/echo"),
        "the refusal must name the hook's tool: {hook_warning}"
    );
    assert!(
        !body.to_string().contains("gap-b-check\\n"),
        "the refused tool must not have run: {body:#}"
    );
}
