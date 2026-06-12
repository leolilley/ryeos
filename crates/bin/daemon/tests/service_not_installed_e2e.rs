//! E2E: executing a `service:` ref whose item is not shipped by any
//! installed bundle returns the structured `service_not_installed`
//! error (404) with the installed-bundle list — not an opaque
//! `internal: not found` 500.
//!
//! Downstream motivation: a lean hosted-node image (core + hosted-node
//! policy, no standard) receiving `ryeos remote run <node>
//! service:scheduler/register` must tell the operator *why* the service
//! is missing and what is actually installed.

mod common;

use common::DaemonHarness;

/// Core-only node: `service:scheduler/register` ships in the standard
/// bundle, which is deliberately NOT registered here.
#[tokio::test]
async fn execute_missing_service_returns_service_not_installed() -> anyhow::Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(
        // No plant hook: only the core bundle is registered.
        |_state_path, _user_space, _fixture| Ok(()),
        |_| {},
    )
    .await?;

    let project = harness.state_path.display().to_string();
    let (status, body) = harness
        .post_execute(
            "service:scheduler/register",
            &project,
            serde_json::json!({}),
        )
        .await?;

    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "missing service item must be 404, got {status}: {body}"
    );
    assert_eq!(
        body["code"], "service_not_installed",
        "structured code expected, got: {body}"
    );
    assert_eq!(
        body["item_ref"], "service:scheduler/register",
        "payload must echo the requested ref, got: {body}"
    );

    let installed: Vec<String> = body["installed_bundles"]
        .as_array()
        .unwrap_or_else(|| panic!("installed_bundles missing from payload: {body}"))
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(
        installed.iter().any(|b| b == "core"),
        "installed_bundles must list core, got: {installed:?}"
    );
    assert!(
        !installed.iter().any(|b| b == "standard"),
        "standard must not be listed on a core-only node, got: {installed:?}"
    );

    assert!(
        body["hint"].as_str().is_some_and(|h| !h.is_empty()),
        "payload must carry an operator hint, got: {body}"
    );
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("service:scheduler/register")),
        "error message must name the missing ref, got: {body}"
    );

    Ok(())
}

/// A service that IS installed but fails for another reason must not
/// regress into `service_not_installed` (the mapping is scoped to
/// `EngineError::ItemNotFound` only).
#[tokio::test]
async fn installed_service_does_not_map_to_not_installed() -> anyhow::Result<()> {
    let (harness, _fixture) =
        DaemonHarness::start_fast_with(|_state_path, _user_space, _fixture| Ok(()), |_| {}).await?;

    let project = harness.state_path.display().to_string();
    // bundle.list ships in core and takes no params — must succeed.
    let (status, body) = harness
        .post_execute("service:bundle/list", &project, serde_json::json!({}))
        .await?;

    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "core-shipped service must execute, got {status}: {body}"
    );
    assert!(
        body["result"]["bundles"].is_array(),
        "bundle.list result expected, got: {body}"
    );

    Ok(())
}

/// `service:system/routes` reports the loaded route table and the
/// registered bundles with their installed paths.
#[tokio::test]
async fn system_routes_reports_routes_and_bundles() -> anyhow::Result<()> {
    let (harness, _fixture) =
        DaemonHarness::start_fast_with(|_state_path, _user_space, _fixture| Ok(()), |_| {}).await?;

    let project = harness.state_path.display().to_string();
    let (status, body) = harness
        .post_execute("service:system/routes", &project, serde_json::json!({}))
        .await?;

    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "system/routes must execute, got {status}: {body}"
    );

    let result = &body["result"];
    assert_eq!(
        result["routes_available"], true,
        "route snapshot must be published, got: {body}"
    );
    let routes = result["routes"]
        .as_array()
        .unwrap_or_else(|| panic!("routes array missing: {body}"));
    assert!(
        routes
            .iter()
            .any(|r| r["path"] == "/execute" && r["methods"].as_array().is_some()),
        "loaded routes must include /execute with methods, got: {routes:?}"
    );
    assert!(
        routes
            .iter()
            .all(|r| r["source_file"].as_str().is_some_and(|s| !s.is_empty())),
        "every route entry must carry its source file"
    );

    let bundles = result["bundles"]
        .as_array()
        .unwrap_or_else(|| panic!("bundles array missing: {body}"));
    let core = bundles
        .iter()
        .find(|b| b["name"] == "core")
        .unwrap_or_else(|| panic!("core bundle must be registered: {bundles:?}"));
    assert_eq!(
        core["path_exists"], true,
        "core bundle path must exist on disk, got: {core}"
    );

    // Path filter narrows the list.
    let (status, body) = harness
        .post_execute(
            "service:system/routes",
            &project,
            serde_json::json!({ "path": "/execute" }),
        )
        .await?;
    assert_eq!(status, reqwest::StatusCode::OK);
    let filtered = body["result"]["routes"].as_array().unwrap();
    assert!(
        !filtered.is_empty()
            && filtered
                .iter()
                .all(|r| r["path"].as_str().is_some_and(|p| p.contains("/execute"))),
        "path filter must narrow to /execute routes, got: {filtered:?}"
    );

    Ok(())
}
