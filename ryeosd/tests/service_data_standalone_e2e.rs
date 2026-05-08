//! E2E for OfflineOnly services: bundle/install, bundle/remove, rebuild.
//! These services only work via `ryeosd run-service ...` (daemon must be
//! down). We assert each one actually performs its data work on disk.

mod common;

use common::{run_service_standalone_fresh, copy_core_to_temp, populate_user_space, ryeosd_binary};

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
    // The daemon's `--system-space-dir` is treated as writable system
    // space (init writes node identity, vault, signed bundle items into
    // `.ai/node/...`). Use a writable copy of the core bundle as the
    // shared system_space_dir across init / install / remove so state
    // persists between standalone invocations.
    let (_core_tmp, system_space_dir) = copy_core_to_temp();
    let user_space = tempfile::tempdir().expect("user_space tempdir");
    populate_user_space(user_space.path());

    let uds_path = system_space_dir.join("ryeosd.sock");

    // 1. Init via a no-op standalone call (system.status) using the
    //    writable core copy as system_space_dir.
    let init_out = std::process::Command::new(ryeosd_binary())
        .arg("--init-if-missing")
        .arg("--system-space-dir").arg(&system_space_dir)
        .arg("--uds-path").arg(&uds_path)
        .arg("run-service")
        .arg("service:system/status")
        .env("HOSTNAME", "testhost")
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .output()
        .expect("init");
    assert!(init_out.status.success(),
        "init failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&init_out.stdout),
        String::from_utf8_lossy(&init_out.stderr));

    // 2. Build a tiny dummy bundle directory: just a dir with a minimal
    //    `.ai/` tree. install.copy_dir copies whatever is at source_path.
    let src = tempfile::tempdir().expect("src tempdir");
    std::fs::create_dir_all(src.path().join(".ai")).unwrap();
    std::fs::write(src.path().join(".ai/marker.txt"), b"hello").unwrap();
    let src_path = src.path().to_str().unwrap().to_string();

    // 3. Install against the same system_space_dir.
    let install_out = std::process::Command::new(ryeosd_binary())
        .arg("--system-space-dir").arg(&system_space_dir)
        .arg("--uds-path").arg(&uds_path)
        .arg("run-service")
        .arg("service:bundle/install")
        .arg("--params")
        .arg(format!(r#"{{"name":"testbundle","source_path":"{src_path}"}}"#))
        .env("HOSTNAME", "testhost")
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .output()
        .expect("install");
    assert!(install_out.status.success(),
        "bundle.install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&install_out.stdout),
        String::from_utf8_lossy(&install_out.stderr));

    // 4. Verify the bundle was actually copied to disk.
    let installed = system_space_dir.join(".ai/bundles/testbundle/.ai/marker.txt");
    assert!(installed.exists(),
        "expected installed marker at {} (install handler didn't copy)",
        installed.display());

    // 5. Verify the signed node-config item was written.
    let node_item = system_space_dir.join(".ai/node/bundles/testbundle.yaml");
    assert!(node_item.exists(),
        "expected node-config item at {}", node_item.display());

    // 6. Remove and verify both paths are gone.
    let remove_out = std::process::Command::new(ryeosd_binary())
        .arg("--system-space-dir").arg(&system_space_dir)
        .arg("--uds-path").arg(&uds_path)
        .arg("run-service")
        .arg("service:bundle/remove")
        .arg("--params")
        .arg(r#"{"name":"testbundle"}"#)
        .env("HOSTNAME", "testhost")
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .output()
        .expect("remove");
    assert!(remove_out.status.success(),
        "bundle.remove failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&remove_out.stdout),
        String::from_utf8_lossy(&remove_out.stderr));
    assert!(!installed.exists(), "bundle dir should be gone after remove");
    assert!(!node_item.exists(), "node-config item should be gone after remove");
}
