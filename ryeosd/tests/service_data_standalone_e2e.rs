//! E2E for OfflineOnly services: bundle/install, bundle/remove, rebuild.
//! These services only work via `ryeosd run-service ...` (daemon must be
//! down). We assert each one actually performs its data work on disk.

mod common;

use common::{run_service_standalone_fresh};

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
    let result = parsed.get("result").or_else(|| Some(&parsed)).cloned().unwrap();
    // RebuildReport has chains_rebuilt/threads_restored/events_projected.
    assert!(result.get("chains_rebuilt").is_some()
        || result.get("threads_restored").is_some(),
        "rebuild result missing expected fields: {parsed}");
}

// ── 5.2 bundle/install then list then remove ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn standalone_bundle_install_then_list_then_remove() {
    // 1. Init a state dir via a no-op standalone call (system.status).
    let (init_out, sd, us) = run_service_standalone_fresh("service:system/status", None)
        .await
        .expect("run-service init");
    assert!(init_out.status.success(),
        "init failed: stdout={} stderr={}",
        String::from_utf8_lossy(&init_out.stdout),
        String::from_utf8_lossy(&init_out.stderr));

    // 2. Build a tiny dummy bundle directory: just a dir with a minimal
    //    `.ai/` tree. install.copy_dir copies whatever is at source_path.
    let src = tempfile::tempdir().expect("src tempdir");
    std::fs::create_dir_all(src.path().join(".ai")).unwrap();
    std::fs::write(src.path().join(".ai/marker.txt"), b"hello").unwrap();
    let src_path = src.path().to_str().unwrap().to_string();

    // 3. Install. We need to reuse the SAME state_dir across calls so the
    //    install persists. Re-using sd/us tempdirs requires a non-fresh
    //    helper, which the harness doesn't expose today. The simplest
    //    path: call the daemon binary directly with --state-dir = sd.path().
    //    Look at `run_service_standalone_fresh` in `common/mod.rs` to see
    //    the exact ryeosd invocation; replicate it inline here, but use
    //    the existing sd/us tempdirs so state survives between calls.
    let install_out = std::process::Command::new(common::ryeosd_binary())
        .arg("--init-if-missing")
        .arg("--state-dir").arg(sd.path().join("state"))
        .arg("--uds-path").arg(sd.path().join("state/ryeosd.sock"))
        .arg("run-service")
        .arg("service:bundle/install")
        .arg("--params")
        .arg(format!(r#"{{"name":"testbundle","source_path":"{src_path}"}}"#))
        .env("RYE_SYSTEM_SPACE", common::system_data_dir())
        .env("USER_SPACE", us.path())
        .env("HOME", us.path())
        .output()
        .expect("install");
    assert!(install_out.status.success(),
        "bundle.install failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&install_out.stdout),
        String::from_utf8_lossy(&install_out.stderr));

    // 4. Verify the bundle was actually copied to disk.
    let installed = sd.path().join("state/.ai/bundles/testbundle/.ai/marker.txt");
    assert!(installed.exists(),
        "expected installed marker at {} (install handler didn't copy)",
        installed.display());

    // 5. Verify the signed node-config item was written.
    let node_item = sd.path().join("state/.ai/node/bundles/testbundle.yaml");
    assert!(node_item.exists(),
        "expected node-config item at {}", node_item.display());

    // 6. Remove and verify both paths are gone.
    let remove_out = std::process::Command::new(common::ryeosd_binary())
        .arg("--state-dir").arg(sd.path().join("state"))
        .arg("--uds-path").arg(sd.path().join("state/ryeosd.sock"))
        .arg("run-service")
        .arg("service:bundle/remove")
        .arg("--params")
        .arg(r#"{"name":"testbundle"}"#)
        .env("RYE_SYSTEM_SPACE", common::system_data_dir())
        .env("USER_SPACE", us.path())
        .env("HOME", us.path())
        .output()
        .expect("remove");
    assert!(remove_out.status.success(),
        "bundle.remove failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&remove_out.stdout),
        String::from_utf8_lossy(&remove_out.stderr));
    assert!(!installed.exists(), "bundle dir should be gone after remove");
    assert!(!node_item.exists(), "node-config item should be gone after remove");
}
