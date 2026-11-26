//! E2E for stopped-node services: bundle/install, bundle/remove, and projection
//! maintenance.
//! These services only work via `ryeosd run-service ...` (daemon must be
//! down). We assert each one actually performs its data work on disk.

use ryeosd::test_support as common;

use common::{StandaloneHarness, run_service_standalone_fresh};

// ── 5.1 rebuild standalone — succeeds on fresh state ────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn standalone_rebuild_runs_on_fresh_state() {
    let (out, _node) = run_service_standalone_fresh("service:projection/rebuild", None)
        .await
        .expect("run-service rebuild");
    assert!(
        out.status.success(),
        "standalone rebuild failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // The CLI prints the result envelope to stdout, but tracing log lines
    // may precede it. Find the last JSON object (may span multiple lines)
    // by locating the start of the last line beginning with '{'.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json_start = stdout
        .lines()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with('{'))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    let json_str: String = stdout
        .lines()
        .skip(json_start)
        .collect::<Vec<&str>>()
        .join("\n");
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap_or_else(|e| {
        panic!("rebuild stdout not JSON: {e}\nextracted: {json_str}\nfull stdout: {stdout}")
    });
    let result = parsed.get("result").or(Some(&parsed)).cloned().unwrap();
    // RebuildReport has chains_rebuilt/threads_restored/events_projected.
    assert!(
        result.get("chains_rebuilt").is_some() || result.get("threads_restored").is_some(),
        "rebuild result missing expected fields: {parsed}"
    );
}

/// A minimal published candidate with its own kind namespace.
fn write_testbundle_source(harness: &StandaloneHarness) -> tempfile::TempDir {
    // Build a minimal published candidate with its own kind namespace. Reusing
    // a core kind would create two providers in the prospective graph, while an
    // unsigned or manifest-less source is intentionally not installable.
    let src = tempfile::tempdir().expect("src tempdir");
    let src_kinds = src.path().join(".ai/node/engine/kinds/testkind");
    std::fs::create_dir_all(&src_kinds).unwrap();
    // Core is installed as an ordinary bundle below the node app root.
    let core_kind = harness
        .app_root
        .join(".ai/bundles/core/.ai/node/engine/kinds/config/config.kind-schema.yaml");
    let config_schema = std::fs::read_to_string(&core_kind).expect("read core config kind schema");
    let test_schema = lillux::signature::strip_signature_lines(&config_schema)
        .replace(
            "category: \"engine/kinds/config\"",
            "category: \"engine/kinds/testkind\"",
        )
        .replace("  directory: config", "  directory: testkind");
    let signed_schema = lillux::signature::sign_content_at(
        &test_schema,
        &harness.fixture.publisher,
        "#",
        None,
        common::fast_fixture::FAST_FIXTURE_TIME,
    );
    std::fs::write(src_kinds.join("testkind.kind-schema.yaml"), signed_schema)
        .expect("write signed test kind schema");

    let manifest = "name: testbundle\nversion: '1.0'\nprovides_kinds:\n  - testkind\nrequires_kinds:\n  - config\nuses_kinds: []\n";
    let signed_manifest = lillux::signature::sign_content_at(
        manifest,
        &harness.fixture.publisher,
        "#",
        None,
        common::fast_fixture::FAST_FIXTURE_TIME,
    );
    std::fs::write(src.path().join(".ai/manifest.yaml"), signed_manifest)
        .expect("write signed bundle manifest");
    src
}

// ── 5.2 bundle/install then list then remove ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn standalone_bundle_install_then_list_then_remove() {
    // Persistent harness: core is installed under .ai/bundles/core/
    // so preflight's discover_installed_bundle_roots finds it.
    // `testbundle` is granted explicit (empty) command-registration
    // authority in the sealed node policy; install refuses any active bundle
    // the node policy does not name.
    let harness = StandaloneHarness::new_initialized_authorizing(&["testbundle"])
        .await
        .expect("standalone harness init");

    let src = write_testbundle_source(&harness);
    let src_path = src.path().to_str().unwrap().to_string();

    // 1. Install testbundle.
    let install_out = harness
        .run_service(
            "service:bundle/install",
            Some(&format!(
                r#"{{"name":"testbundle","source_path":"{src_path}"}}"#
            )),
        )
        .await
        .expect("install spawn");
    assert!(
        install_out.status.success(),
        "bundle.install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&install_out.stdout),
        String::from_utf8_lossy(&install_out.stderr)
    );

    // 2. Verify the bundle was copied to disk.
    let installed = harness
        .app_root
        .join(".ai/bundles/testbundle/.ai/node/engine/kinds/testkind/testkind.kind-schema.yaml");
    assert!(
        installed.exists(),
        "expected installed kind schema at {} (install handler didn't copy)",
        installed.display()
    );

    // 3. Verify the signed node-config item was written.
    let node_item = harness.app_root.join(".ai/node/bundles/testbundle.yaml");
    assert!(
        node_item.exists(),
        "expected node-config item at {}",
        node_item.display()
    );

    // 4. Remove and verify both paths are gone.
    let remove_out = harness
        .run_service("service:bundle/remove", Some(r#"{"name":"testbundle"}"#))
        .await
        .expect("remove spawn");
    assert!(
        remove_out.status.success(),
        "bundle.remove failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&remove_out.stdout),
        String::from_utf8_lossy(&remove_out.stderr)
    );
    assert!(
        !installed.exists(),
        "bundle dir should be gone after remove"
    );
    assert!(
        !node_item.exists(),
        "node-config item should be gone after remove"
    );

    // Keep tempdirs alive through all assertions.
    let _ = harness;
}

/// A stopped node has no HTTP route: the refusal's exact body must reach
/// stderr after the one-line error.
#[tokio::test(flavor = "multi_thread")]
async fn standalone_structured_refusal_prints_its_body() {
    let harness = StandaloneHarness::new_initialized_authorizing(&["testbundle"])
        .await
        .expect("standalone harness init");
    let src = write_testbundle_source(&harness);
    let src_path = src.path().to_str().unwrap().to_string();
    let install = harness
        .run_service(
            "service:bundle/install",
            Some(&format!(
                r#"{{"name":"testbundle","source_path":"{src_path}"}}"#
            )),
        )
        .await
        .expect("install spawn");
    assert!(install.status.success());

    // Nothing is pinned, so naming a binding is a typed refusal.
    let refused = harness
        .run_service(
            "service:bundle/install",
            Some(&format!(
                r#"{{"name":"testbundle","source_path":"{src_path}","replace":true,"invalidate_bindings":["runtime_snapshot_production/render-snapshot"]}}"#
            )),
        )
        .await
        .expect("replace spawn");
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("structured service error: invalidated_binding_not_stranded"),
        "one-line error kept:\n{stderr}"
    );
    assert!(
        stderr.contains("\"not_stranded\"")
            && stderr.contains("runtime_snapshot_production/render-snapshot"),
        "body printed:\n{stderr}"
    );
}

// ── 5.3 a stopped-node install or removal leaves a node that boots ──────

/// The signed init completion record pins every bundle registration, so each
/// stopped-node registration change must reseal it. The node is started again
/// after each change; a refusal to boot fails the test at the restart.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_boots_after_a_stopped_node_install_and_after_a_stopped_node_remove() {
    let mut harness = StandaloneHarness::new_initialized_authorizing(&["testbundle"])
        .await
        .expect("standalone harness init");
    let src = write_testbundle_source(&harness);
    let src_path = src.path().to_str().unwrap().to_string();

    let install_out = harness
        .run_service(
            "service:bundle/install",
            Some(&format!(
                r#"{{"name":"testbundle","source_path":"{src_path}"}}"#
            )),
        )
        .await
        .expect("install spawn");
    assert!(
        install_out.status.success(),
        "bundle.install failed: {}",
        String::from_utf8_lossy(&install_out.stderr)
    );
    harness
        .restart_node()
        .await
        .expect("the node boots after a stopped-node install");
    let names = harness.listed_bundle_names().await.expect("list");
    assert!(names.iter().any(|name| name == "testbundle"), "{names:?}");
    harness
        .stop_node()
        .await
        .expect("stop the node before the next stopped-node change");

    let remove_out = harness
        .run_service("service:bundle/remove", Some(r#"{"name":"testbundle"}"#))
        .await
        .expect("remove spawn");
    assert!(
        remove_out.status.success(),
        "bundle.remove failed: {}",
        String::from_utf8_lossy(&remove_out.stderr)
    );
    harness
        .restart_node()
        .await
        .expect("the node boots after a stopped-node remove");
    let names = harness.listed_bundle_names().await.expect("list");
    assert!(!names.iter().any(|name| name == "testbundle"), "{names:?}");
}
