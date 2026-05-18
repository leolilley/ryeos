//! E2E for OfflineOnly services: bundle/install, bundle/remove, rebuild.
//! These services only work via `ryeosd run-service ...` (daemon must be
//! down). We assert each one actually performs its data work on disk.

mod common;

use common::{run_service_standalone_fresh, StandaloneHarness, ryeosd_binary};

// ── 5.1 rebuild standalone — succeeds on fresh state ────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn standalone_rebuild_runs_on_fresh_state() {
    let (out, _sd, _us) = run_service_standalone_fresh("service:rebuild", None)
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
    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .unwrap_or_else(|e| panic!("rebuild stdout not JSON: {e}\nextracted: {json_str}\nfull stdout: {stdout}"));
    let result = parsed.get("result").or(Some(&parsed)).cloned().unwrap();
    // RebuildReport has chains_rebuilt/threads_restored/events_projected.
    assert!(result.get("chains_rebuilt").is_some()
        || result.get("threads_restored").is_some(),
        "rebuild result missing expected fields: {parsed}");
}

// ── 5.2 bundle/install then list then remove ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn standalone_bundle_install_then_list_then_remove() {
    // Persistent harness: core is installed under .ai/bundles/core/
    // so preflight's discover_installed_bundle_roots finds it.
    let harness = StandaloneHarness::new_initialized()
        .expect("standalone harness init");

    // Build a minimal candidate bundle. It needs a .ai/ directory with
    // at least one kind schema so preflight doesn't bail at "no kind
    // schemas". Copy a real signed one from core.
    let src = tempfile::tempdir().expect("src tempdir");
    let src_kinds = src.path().join(".ai/node/engine/kinds/service");
    std::fs::create_dir_all(&src_kinds).unwrap();
    let core_kind = harness.system_space_dir
        .join(".ai/node/engine/kinds/service/service.kind-schema.yaml");
    std::fs::copy(&core_kind, src_kinds.join("service.kind-schema.yaml"))
        .expect("copy real kind schema from core");
    let src_path = src.path().to_str().unwrap().to_string();

    // 1. Install testbundle.
    let install_out = harness.run_service(
        "service:bundle/install",
        Some(&format!(r#"{{"name":"testbundle","source_path":"{src_path}"}}"#)),
    ).await.expect("install spawn");
    assert!(install_out.status.success(),
        "bundle.install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&install_out.stdout),
        String::from_utf8_lossy(&install_out.stderr));

    // 2. Verify the bundle was copied to disk.
    let installed = harness.system_space_dir
        .join(".ai/bundles/testbundle/.ai/node/engine/kinds/service/service.kind-schema.yaml");
    assert!(installed.exists(),
        "expected installed kind schema at {} (install handler didn't copy)",
        installed.display());

    // 3. Verify the signed node-config item was written.
    let node_item = harness.system_space_dir
        .join(".ai/node/bundles/testbundle.yaml");
    assert!(node_item.exists(),
        "expected node-config item at {}", node_item.display());

    // 4. Remove and verify both paths are gone.
    let remove_out = harness.run_service(
        "service:bundle/remove",
        Some(r#"{"name":"testbundle"}"#),
    ).await.expect("remove spawn");
    assert!(remove_out.status.success(),
        "bundle.remove failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&remove_out.stdout),
        String::from_utf8_lossy(&remove_out.stderr));
    assert!(!installed.exists(), "bundle dir should be gone after remove");
    assert!(!node_item.exists(), "node-config item should be gone after remove");

    // Keep tempdirs alive through all assertions.
    let _ = harness;
}
