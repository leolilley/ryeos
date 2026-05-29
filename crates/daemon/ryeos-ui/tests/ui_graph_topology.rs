// Smoke tests for `ui.graph.topology` with the live workspace bundles.

mod test_state;

use std::sync::Arc;

use ryeos_app::handler_context::HandlerContext;
use ryeos_ui::browser_session::LaunchContext;
use ryeos_ui::state::get_ui_state;

use test_state::build_test_state_with_live_bundles;

fn workspace_root() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/ directory")
        .to_string_lossy()
        .to_string()
}

#[tokio::test]
async fn graph_topology_returns_live_bundle_topology_for_browser_session() {
    let (_tmp, state) = build_test_state_with_live_bundles();
    let launch_context = LaunchContext {
        surface_ref: "surface:ryeos/cockpit/graph".into(),
        project_path: Some(workspace_root()),
        read_only: true,
        granted_caps: vec!["ui.read".into()],
    };
    let (session_id, _token) = get_ui_state(&state)
        .expect("ui state registered")
        .browser_sessions
        .mint_token(launch_context);

    let ctx = HandlerContext::new(
        format!("session:{session_id}"),
        vec!["ui.read".into()],
        false,
    );

    let result = (ryeos_ui::handlers::ui_graph_topology::DESCRIPTOR.handler)(
        serde_json::json!(null),
        ctx,
        Arc::new(state),
    )
    .await
    .expect("topology handler should succeed");

    assert_eq!(result["kind"], "topology_graph");
    assert_eq!(
        result["metadata"]["root_surface"],
        "surface:ryeos/cockpit/graph"
    );

    let nodes = result["nodes"].as_array().expect("nodes array");
    let edges = result["edges"].as_array().expect("edges array");
    assert!(
        nodes.len() > 10,
        "expected live bundle nodes, got {}",
        nodes.len()
    );
    assert!(
        edges.len() > 5,
        "expected live bundle edges, got {}",
        edges.len()
    );

    let has_node = |id: &str| {
        nodes
            .iter()
            .any(|node| node["id"].as_str().is_some_and(|node_id| node_id == id))
    };
    let has_edge = |from: &str, type_: &str, to: &str| {
        edges.iter().any(|edge| {
            edge["from"].as_str() == Some(from)
                && edge["type"].as_str() == Some(type_)
                && edge["to"].as_str() == Some(to)
        })
    };

    let node_ids: std::collections::BTreeSet<_> = nodes
        .iter()
        .filter_map(|node| node["id"].as_str())
        .collect();
    for edge in edges {
        let from = edge["from"].as_str().expect("edge.from string");
        let to = edge["to"].as_str().expect("edge.to string");
        assert!(node_ids.contains(from), "missing edge.from item: {from}");
        assert!(node_ids.contains(to), "missing edge.to item: {to}");
    }

    assert!(has_node("surface:ryeos/cockpit/graph"));
    assert!(has_node("client:ryeos/web"));
    assert!(has_node("kind:surface"));
    assert!(has_edge(
        "surface:ryeos/cockpit/graph",
        "extends",
        "surface:ryeos/cockpit/base"
    ));
    assert!(has_edge("client:ryeos/web", "serves_kind", "kind:surface"));
    assert!(has_edge(
        "kind:surface",
        "uses_handler",
        "handler:ryeos/core/extends-chain"
    ));
}
