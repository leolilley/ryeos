//! V5.2 closeout end-to-end gate.
//!
//! These tests spawn the actual `ryeosd` binary (and optionally `ryeos`)
//! as real subprocesses and exercise them over HTTP/TCP and via
//! `run-service`. They catch the regressions that the prior in-process
//! "e2e" file (now `cleanup_invariants.rs`) could not.
//!
//! Architecture facts these tests assert:
//! - `/execute` is HTTP/TCP only (UDS is system.health + runtime.* only).
//! - CLI dispatches `service:*` to the daemon; on "daemon not running"
//!   it falls back to spawning `ryeosd run-service` (NOT `current_exe`).
//! - ServiceAvailability::OfflineOnly errors when the daemon is up.
//! - ServiceAvailability::DaemonOnly errors when the daemon is down.
//! - Standalone `bundle.install` persists across daemon restart and is
//!   visible to the live `bundle.list` (regression: standalone must
//!   load the full node-config snapshot, not an empty one).
//! - CLI propagates `project_path` in the `/execute` request body.
//!
//! Each test brings up its own daemon in a tempdir to avoid cross-test
//! interference. TCP ports come from `pick_free_port()`.

mod common;

use common::{
    ryos_binary, run_service_standalone_fresh, ryeosd_binary, DaemonHarness,
};

// ── Test 1: live /execute over TCP ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn live_execute_service_system_status_over_tcp() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = h
        .post_execute("service:system/status", ".", serde_json::json!({}))
        .await
        .expect("post /execute");
    assert!(status.is_success(), "status was {status}, body={body}");
    // Just assert it returned a JSON object (shape is asserted in invariant tests).
    assert!(body.is_object(), "expected object, got {body}");
}

// ── Test 2: standalone run-service ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn standalone_run_service_system_status() {
    let (out, _sd, _us) = run_service_standalone_fresh("service:system/status", None)
        .await
        .expect("run-service");
    assert!(
        out.status.success(),
        "exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── Test 3: CLI uses TCP /execute when daemon is up ────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn cli_daemon_up_uses_execute() {
    let h = DaemonHarness::start().await.expect("start daemon");
    let ryos = ryos_binary();

    let out = tokio::process::Command::new(&ryos)
        .arg("execute")
        .arg("service:system/status")
        .env("RYEOS_SYSTEM_SPACE_DIR", &h.state_path)
        .env("RYEOSD_BIN", ryeosd_binary())
        .env("HOME", h.user_space.path())
        .output()
        .await
        .expect("spawn ryos");
    assert!(
        out.status.success(),
        "ryos exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── Test 4: CLI fails fast when daemon is down (no silent fallback) ────
//   Asserts the post-cli-impl contract: `ryeos execute` always tries the
//   daemon. If `daemon.json` is absent (daemon down), it exits 75
//   (EX_TEMPFAIL) with a typed error, NOT a silent fallback to spawning
//   `ryeosd run-service`. The offline path is now an explicit, separate
//   verb (`ryeosd run-service ...`); the CLI must not paper over it.

#[tokio::test(flavor = "multi_thread")]
async fn cli_daemon_down_fails_fast_with_exit_75() {
    let outer = tempfile::tempdir().expect("tempdir");
    // `state_path` does NOT exist; no `daemon.json` is anywhere on disk.
    let state_path = outer.path().join("state-never-initialized");
    let user_space = tempfile::tempdir().expect("user tempdir");
    common::populate_user_space(user_space.path());

    let ryos = ryos_binary();
    let out = tokio::process::Command::new(&ryos)
        .arg("execute")
        .arg("service:system/status")
        // RYEOSD_BIN intentionally NOT set — the new contract has no
        // fallback that would consult it; presence or absence is moot.
        .env("RYEOS_SYSTEM_SPACE_DIR", common::workspace_core_dir())
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .output()
        .await
        .expect("spawn ryos");
    assert_eq!(
        out.status.code(),
        Some(75),
        "expected EX_TEMPFAIL (75) for daemon-down, got exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("daemon.json not found"),
        "expected stderr to mention 'daemon.json not found' (typed fail-loud), got: {stderr}"
    );
    drop(outer);
    drop(user_space);
}

// ── Test 5: OfflineOnly service errors when daemon is up ───────────────

#[tokio::test(flavor = "multi_thread")]
async fn offline_only_service_errors_when_daemon_up() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = h
        .post_execute("service:rebuild", ".", serde_json::json!({}))
        .await
        .expect("post /execute");
    assert!(
        !status.is_success(),
        "expected failure for OfflineOnly in live mode, got {status}: {body}"
    );
    let body_str = body.to_string().to_lowercase();
    assert!(
        body_str.contains("offline") || body_str.contains("standalone"),
        "expected error to mention OfflineOnly/standalone, got: {body}"
    );
}

// ── Test 6: DaemonOnly service errors via run-service ──────────────────

#[tokio::test(flavor = "multi_thread")]
async fn daemon_only_service_errors_via_run_service() {
    let (out, _sd, _us) = run_service_standalone_fresh(
        "service:commands/submit",
        Some(r#"{"thread_id":"T-test","command_type":"noop","params":{}}"#),
    )
    .await
    .expect("run-service");
    assert!(
        !out.status.success(),
        "expected failure for DaemonOnly in standalone, exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
    .to_lowercase();
    assert!(
        combined.contains("daemon") || combined.contains("daemononly"),
        "expected error to mention daemon-only, got: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── Test 7: CLI defaults project_path to "." ───────────────────────────
//   (Catches Task M.1 regression — body must include project_path.)
//
// We assert this end-to-end by hitting the live daemon and confirming
// the request succeeds with no explicit --project-path. The daemon's
// /execute handler requires project_path; if the CLI dropped it, the
// request would 4xx.

#[tokio::test(flavor = "multi_thread")]
async fn cli_execute_defaults_project_path_to_dot() {
    let h = DaemonHarness::start().await.expect("start daemon");
    let ryos = ryos_binary();

    // The CLI sends raw tokens (`["status"]`) to the daemon's /execute
    // endpoint. The daemon resolves via its AliasRegistry (loaded from
    // the core bundle's `node/aliases/`). RYEOS_SYSTEM_SPACE_DIR locates
    // the daemon's bind socket. HOME points the user tier at the harness
    // user space (where `populate_user_space` pre-loaded the
    // trusted-signers fixture).
    let out = tokio::process::Command::new(&ryos)
        .arg("status") // alias → service:system/status, no --project-path
        .env("RYEOS_SYSTEM_SPACE_DIR", &h.state_path)
        .env("HOME", h.user_space.path())
        .output()
        .await
        .expect("spawn ryos");
    assert!(
        out.status.success(),
        "ryos status (no --project-path) failed; if daemon rejected, the CLI may have dropped project_path. exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── Test 8: --init-only must NEVER mutate RYEOS_SYSTEM_SPACE_DIR ─────────────
//   (Catches the V5.2-CLOSEOUT bug that polluted ryeos-bundles/core.)

#[tokio::test(flavor = "multi_thread")]
async fn init_only_does_not_mutate_system_space() {
    use std::collections::BTreeMap;
    use std::fs;

    fn snapshot(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        fn walk(dir: &std::path::Path, out: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
            let entries = match fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if let Ok(bytes) = fs::read(&p) {
                    out.insert(p, bytes);
                }
            }
        }
        walk(root, &mut out);
        out
    }

    // Make a per-test copy of the system bundle so even a regression
    // can't corrupt the workspace.
    let sys_outer = tempfile::tempdir().expect("system tempdir");
    let sys_root = sys_outer.path().join("core");
    fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_dir_all(&from, &to);
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }
    }
    copy_dir_all(&common::workspace_core_dir(), &sys_root);

    let before = snapshot(&sys_root);
    assert!(!before.is_empty(), "system bundle should be non-empty");

    let outer = tempfile::tempdir().expect("state tempdir");
    let state_path = outer.path().join("state");
    let user_space = tempfile::tempdir().expect("user tempdir");
    common::populate_user_space(user_space.path());

    // Run --init-only against the COPIED system bundle.
    let init = std::process::Command::new(ryeosd_binary())
        .arg("--init-only")
        .arg("--system-space-dir").arg(&state_path)
        .arg("--uds-path").arg(state_path.join("ryeosd.sock"))
        .env("RYEOS_SYSTEM_SPACE_DIR", &sys_root)
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .output()
        .expect("ryeosd --init-only");
    assert!(
        init.status.success(),
        "init-only failed:\nstderr={}",
        String::from_utf8_lossy(&init.stderr)
    );

    let after = snapshot(&sys_root);
    assert_eq!(
        before, after,
        "RYEOS_SYSTEM_SPACE_DIR was mutated by --init-only — daemon bootstrap must NEVER touch operator-managed bundle content"
    );
}

// ── Test 9: UDS namespace rejects service methods (Gate 3 daemon-spawn) ──
//   The bare UDS server must only expose `system.health` and `runtime.*`
//   methods. A service method like `system/status` must get "unknown_method".
//   This closes the TODO at cleanup_invariants.rs:171.

#[tokio::test(flavor = "multi_thread")]
async fn uds_namespace_rejects_service_methods() {
    let h = DaemonHarness::start().await.expect("start daemon");

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(&h.uds_path)
        .await
        .expect("connect to UDS socket");

    let mut stream = stream;

    let request = serde_json::json!({
        "request_id": 1u64,
        "method": "system/status",
        "params": {}
    });
    let payload = rmp_serde::to_vec(&request).expect("encode rpc request");
    let len = (payload.len() as u32).to_be_bytes();
    stream
        .write_all(&len)
        .await
        .expect("write frame length");
    stream
        .write_all(&payload)
        .await
        .expect("write frame body");
    stream.shutdown().await.expect("shutdown write side");

    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .expect("read response length");
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp_buf = vec![0u8; resp_len];
    stream
        .read_exact(&mut resp_buf)
        .await
        .expect("read response body");
    let response: serde_json::Value =
        rmp_serde::from_slice(&resp_buf).expect("decode rpc response");

    assert!(
        response.get("error").is_some(),
        "UDS should reject 'system/status' with an error, got: {response}"
    );
    let error = &response["error"];
    assert_eq!(
        error["code"], "unknown_method",
        "expected 'unknown_method' error code, got: {response}"
    );
    let msg = error["message"]
        .as_str()
        .unwrap_or("")
        .to_lowercase();
    assert!(
        msg.contains("unknown") || msg.contains("system/status"),
        "error message should mention unknown method, got: {}",
        error["message"]
    );
}

// ── Test 10: State lock prevents concurrent daemons (Gate 8 daemon-spawn) ──
//   Two daemons sharing the same system_space_dir: the second must fail to acquire
//   the state lock and exit with an error. This closes the TODO at
//   cleanup_invariants.rs:289.

#[tokio::test(flavor = "multi_thread")]
async fn state_lock_prevents_concurrent_daemons() {
    let h1 = DaemonHarness::start().await.expect("start first daemon");

    let state_dir_outer = tempfile::tempdir().expect("state tempdir");
    let user_space = tempfile::tempdir().expect("user tempdir");
    common::populate_user_space(user_space.path());
    let state_path = state_dir_outer.path().join("state");

    let port = common::pick_free_port();
    let bind: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let uds_path = state_path.join("ryeosd.sock");

    // Point the second daemon at the SAME state dir as the first
    let mut cmd = tokio::process::Command::new(common::ryeosd_binary());
    cmd.arg("--system-space-dir").arg(&h1.state_path)
        .arg("--bind").arg(bind.to_string())
        .arg("--uds-path").arg(&uds_path)
        .env("HOSTNAME", "testhost")
        .env("RYEOS_SYSTEM_SPACE_DIR", common::workspace_core_dir())
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd.output().await.expect("spawn second daemon");

    // The second daemon should fail — either the state lock blocks it,
    // or the UDS bind fails because the first daemon already owns it.
    assert!(
        !output.status.success(),
        "second daemon should fail when sharing state_dir with first daemon; \
         exit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

// ── Test 11: Symlinked node config items are rejected (Gate 12 daemon-spawn) ──
//   The node_config loader must reject symlinks in `.ai/node/bundles/`.
//   A symlinked YAML placed in the state bundles dir must cause daemon
//   startup to fail. This closes the TODO at cleanup_invariants.rs:405.

#[tokio::test(flavor = "multi_thread")]
async fn symlinked_node_config_rejected_at_startup() {
    let result = DaemonHarness::start_with_pre_init(|state_path, _user_space| {
        let bundles_dir = state_path.join(".ai").join("node").join("bundles");
        std::fs::create_dir_all(&bundles_dir)?;
        let link_target = bundles_dir.join("adversarial.yaml");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", &link_target)?;
        Ok(())
    }, |_cmd| {}).await;

    match result {
        Ok(_) => panic!(
            "daemon should reject symlinked node config items in bundles dir"
        ),
        Err(e) => {
            let err_msg = format!("{:#}", e);
            assert!(
                err_msg.contains("symlink") || err_msg.contains("not a regular file"),
                "error should mention symlink rejection, got: {err_msg}"
            );
        }
    }
}

// ── Test 12: Daemon loads all bundle YAMLs successfully (Gate 13 daemon-spawn) ──
//   A successful daemon start proves the full node_config + engine pipeline
//   loaded every bundle YAML from the system bundle without parse errors.
//   If any YAML is malformed, the daemon would fail at bootstrap.
//   This closes the TODO at cleanup_invariants.rs:437.

#[tokio::test(flavor = "multi_thread")]
async fn daemon_startup_proves_bundle_yamls_parse() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("daemon should start — bundle YAMLs must all parse");
    // If we got here, the daemon completed Phase 1 + Phase 2 node_config
    // bootstrap and the self-check. Every bundle YAML in the system bundle
    // was loaded and verified. Verify the daemon is actually healthy.
    let (status, body) = h
        .post_execute("service:system/status", ".", serde_json::json!({}))
        .await
        .expect("post /execute");
    assert!(status.is_success(), "daemon healthy check failed: {status}, body={body}");
}

// ── Test 13: Path=section invariant enforced by daemon (Gate 15 daemon-spawn) ──
//   A node config YAML whose `section` field doesn't match its parent
//   directory must cause daemon startup to fail. We plant a valid
//   signed route YAML (section: routes) into the bundles directory
//   to trigger the mismatch. This closes the TODO at
//   cleanup_invariants.rs:521.

#[tokio::test(flavor = "multi_thread")]
async fn path_section_mismatch_rejected_at_startup() {
    let workspace = common::workspace_root();
    let route_yaml = workspace
        .join("ryeos-bundles/core/.ai/node/routes/execute-stream.yaml");
    assert!(route_yaml.is_file(), "route fixture must exist");

    let result = DaemonHarness::start_with_pre_init(move |state_path, _user_space| {
        let bundles_dir = state_path.join(".ai").join("node").join("bundles");
        std::fs::create_dir_all(&bundles_dir)?;
        let dest = bundles_dir.join("execute-stream.yaml");
        std::fs::copy(&route_yaml, &dest)?;
        Ok(())
    }, |_cmd| {}).await;

    match result {
        Ok(_) => panic!(
            "daemon should reject node config item with section != parent directory"
        ),
        Err(e) => {
            let err_msg = format!("{:#}", e);
            assert!(
                err_msg.contains("path = section invariant") || err_msg.contains("declares section"),
                "error should mention path=section invariant violation, got: {err_msg}"
            );
        }
    }
}
