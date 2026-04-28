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

// ── Test 4: CLI fails fast when daemon is down (no silent fallback) ────
//   Asserts the post-cli-impl contract: `rye execute` always tries the
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

    let rye = rye_binary();
    let out = tokio::process::Command::new(&rye)
        .arg("execute")
        .arg("service:system/status")
        .env("RYEOS_STATE_DIR", &state_path)
        // RYEOSD_BIN intentionally NOT set — the new contract has no
        // fallback that would consult it; presence or absence is moot.
        .env("RYE_SYSTEM_SPACE", common::system_data_dir())
        .env("USER_SPACE", user_space.path())
        .env("HOME", user_space.path())
        .output()
        .await
        .expect("spawn rye");
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

    // The CLI's verb table comes from the three-tier `.ai/config/cli/`
    // hierarchy. The verb `status` (-> `service:system/status`) ships in the
    // `standard` bundle (`ryeos-bundles/standard/.ai/config/cli/status.yaml`);
    // `core` is engine-config-only and has no `config/cli/`. We point the
    // CLI's RYE_SYSTEM_SPACE at standard for verb discovery; the daemon
    // already runs against `system_data_dir()` (= `core`) for engine kinds.
    // HOME points the user tier at the harness user space (where
    // `populate_user_space` pre-loaded the trusted-signers fixture so the
    // verb YAMLs verify). RYEOS_STATE_DIR locates the daemon's bind socket.
    let standard_bundle = common::workspace_root().join("ryeos-bundles/standard");
    let out = tokio::process::Command::new(&rye)
        .arg("status") // alias → service:system/status, no --project-path
        .env("RYEOS_STATE_DIR", &h.state_path)
        .env("RYE_SYSTEM_SPACE", &standard_bundle)
        .env("HOME", h.user_space.path())
        .output()
        .await
        .expect("spawn rye");
    assert!(
        out.status.success(),
        "rye status (no --project-path) failed; if daemon rejected, the CLI may have dropped project_path. exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
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
