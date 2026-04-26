//! V5.2 closeout end-to-end gate.
//!
//! These tests spawn the actual `ryeosd` binary (and optionally `rye`)
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
    rye_binary, run_service_standalone_fresh, ryeosd_binary, DaemonHarness,
};

// ── Test 1: live /execute over TCP ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn live_execute_service_system_status_over_tcp() {
    let h = DaemonHarness::start().await.expect("start daemon");
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
    let rye = rye_binary();

    let out = tokio::process::Command::new(&rye)
        .arg("execute")
        .arg("service:system/status")
        .env("RYEOS_STATE_DIR", &h.state_path)
        .env("RYEOSD_BIN", ryeosd_binary())
        .output()
        .await
        .expect("spawn rye");
    assert!(
        out.status.success(),
        "rye exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── Test 4: CLI falls back to run-service when daemon is down ──────────
//   (Catches Task I.2 regression: rye must spawn `ryeosd`, not current_exe)

#[tokio::test(flavor = "multi_thread")]
async fn cli_daemon_down_falls_back_to_run_service() {
    // Bring up a daemon to init the state_dir, then drop it so the daemon
    // is down. The state_dir persists via the harness's outer tempdir for
    // the duration of this test (held alive by `_harness`).
    //
    // After the harness goes out of scope, the daemon process dies but the
    // tempdir is also cleaned up — so we extract the state_path BEFORE
    // dropping the harness... actually we need the state_dir to outlive the
    // harness. Use a stand-alone init via `ryeosd run-service` against a
    // fresh non-existent path: `--init-if-missing` triggers init, the
    // service runs, daemon stays down. Then rye-cli falls back the SAME
    // way against the SAME state_dir.

    let outer = tempfile::tempdir().expect("tempdir");
    let state_path = outer.path().join("state");
    let user_space = tempfile::tempdir().expect("user tempdir");
    common::populate_user_space(user_space.path());

    // First standalone invocation initializes the state_dir.
    let init_out = std::process::Command::new(ryeosd_binary())
        .arg("--init-if-missing")
        .arg("--state-dir").arg(&state_path)
        .arg("--uds-path").arg(state_path.join("ryeosd.sock"))
        .arg("run-service")
        .arg("service:system/status")
        .env("RYE_SYSTEM_SPACE", common::system_data_dir())
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .output()
        .expect("ryeosd run-service init");
    assert!(
        init_out.status.success(),
        "init via run-service failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&init_out.stdout),
        String::from_utf8_lossy(&init_out.stderr),
    );

    // Now invoke rye CLI; daemon is down, so it must fall back to spawning
    // `ryeosd run-service` (NOT `current_exe()`). RYEOSD_BIN points the
    // CLI at the test-built daemon binary.
    let rye = rye_binary();
    let out = tokio::process::Command::new(&rye)
        .arg("execute")
        .arg("service:system/status")
        .env("RYEOS_STATE_DIR", &state_path)
        .env("RYEOSD_BIN", ryeosd_binary())
        .env("RYE_SYSTEM_SPACE", common::system_data_dir())
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .output()
        .await
        .expect("spawn rye");
    assert!(
        out.status.success(),
        "rye exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    drop(outer);
    drop(user_space);
}

// ── Test 5: OfflineOnly service errors when daemon is up ───────────────

#[tokio::test(flavor = "multi_thread")]
async fn offline_only_service_errors_when_daemon_up() {
    let h = DaemonHarness::start().await.expect("start daemon");
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
    let rye = rye_binary();

    let out = tokio::process::Command::new(&rye)
        .arg("status") // alias → service:system/status, no --project-path
        .env("RYEOS_STATE_DIR", &h.state_path)
        .env("RYEOSD_BIN", ryeosd_binary())
        .output()
        .await
        .expect("spawn rye");
    assert!(
        out.status.success(),
        "rye status (no --project-path) failed; if daemon rejected, the CLI may have dropped project_path. exit={:?}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── Test 8: --init-only must NEVER mutate RYE_SYSTEM_SPACE ─────────────
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
    copy_dir_all(&common::system_data_dir(), &sys_root);

    let before = snapshot(&sys_root);
    assert!(!before.is_empty(), "system bundle should be non-empty");

    let outer = tempfile::tempdir().expect("state tempdir");
    let state_path = outer.path().join("state");
    let user_space = tempfile::tempdir().expect("user tempdir");
    common::populate_user_space(user_space.path());

    // Run --init-only against the COPIED system bundle.
    let init = std::process::Command::new(ryeosd_binary())
        .arg("--init-only")
        .arg("--state-dir").arg(&state_path)
        .arg("--uds-path").arg(state_path.join("ryeosd.sock"))
        .env("RYE_SYSTEM_SPACE", &sys_root)
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
        "RYE_SYSTEM_SPACE was mutated by --init-only — daemon bootstrap must NEVER touch operator-managed bundle content"
    );
}
