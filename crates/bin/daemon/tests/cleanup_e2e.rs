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
//! interference. The daemon binds `127.0.0.1:0` (kernel-assigned port).

mod common;

use common::{run_service_standalone_fresh, ryeos_binary, ryeosd_binary, DaemonHarness};

// ── Test 1: live /execute over TCP ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn live_execute_service_node_status_over_tcp() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = h
        .post_execute("service:node/status", ".", serde_json::json!({}))
        .await
        .expect("post /execute");
    assert!(status.is_success(), "status was {status}, body={body}");
    // Just assert it returned a JSON object (shape is asserted in invariant tests).
    assert!(body.is_object(), "expected object, got {body}");
}

// ── Test 2: standalone run-service ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn standalone_run_service_node_status() {
    let (out, _sd, _us) = run_service_standalone_fresh("service:node/status", None)
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
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let ryeos = ryeos_binary();

    let out = tokio::process::Command::new(&ryeos)
        .arg("execute")
        .arg("service:node/status")
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("RYEOSD_BIN", ryeosd_binary())
        .env("HOME", h.user_space.path())
        .output()
        .await
        .expect("spawn ryeos");
    assert!(
        out.status.success(),
        "ryeos exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── Test 4: CLI fails fast when initialized but daemon is down ────────
//   Lifecycle preflight should explain the stopped local node state rather
//   than surfacing daemon.json/signing/transport noise or silently spawning
//   `ryeosd run-service`.

#[tokio::test(flavor = "multi_thread")]
async fn cli_initialized_but_stopped_suggests_start() {
    // Core bundle content must exist at the registered path so the offline
    // verified snapshot can resolve `execute` — otherwise command resolution
    // fails before lifecycle preflight can report the stopped node.
    let (_core_tmp, state_path) = common::copy_core_to_temp();
    let user_space = tempfile::tempdir().expect("user tempdir");
    let fixture = common::fast_fixture::populate_initialized_state(&state_path, user_space.path())
        .expect("populate initialized state");
    common::fast_fixture::register_core_bundle_at_state(&state_path, &fixture)
        .expect("register core");
    common::fast_fixture::register_standard_bundle(&state_path, &fixture)
        .expect("register standard");

    let ryeos = ryeos_binary();
    let out = tokio::process::Command::new(&ryeos)
        .arg("execute")
        .arg("service:node/status")
        .env("RYEOS_APP_ROOT", &state_path)
        .env("HOME", user_space.path())
        .output()
        .await
        .expect("spawn ryeos");
    assert!(!out.status.success(), "daemon-down CLI should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("RyeOS is initialized but not running. Run: ryeos start"),
        "expected lifecycle stopped guidance, got: {stderr}"
    );
    drop(user_space);
}

// ── Test 5: OfflineOnly service errors when daemon is up ───────────────

#[tokio::test(flavor = "multi_thread")]
async fn offline_only_service_errors_when_daemon_up() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let (status, body) = h
        .post_execute("service:projection/rebuild", ".", serde_json::json!({}))
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
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let ryeos = ryeos_binary();

    // The CLI sends raw tokens (`["status"]`) to the daemon's /execute
    // endpoint. The daemon resolves via its command registry. The app root
    // locates daemon metadata and the operator signing key.
    let out = tokio::process::Command::new(&ryeos)
        .arg("status") // alias → service:node/status, no --project-path
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("HOME", h.user_space.path())
        .output()
        .await
        .expect("spawn ryeos");
    assert!(
        out.status.success(),
        "ryeos status (no --project-path) failed; if daemon rejected, the CLI may have dropped project_path. exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── Test 7b: maintenance/gc token command defaults params ───────────────
//   Catches the token-command path for `ryeos maintenance gc` specifically;
//   service:maintenance/gc direct execution is covered elsewhere.

#[tokio::test(flavor = "multi_thread")]
async fn cli_maintenance_gc_token_command_runs_with_default_params() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");
    let ryeos = ryeos_binary();

    let cas_root = h.state_path.join(".ai/state/objects");
    let orphan_hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let orphan_object = lillux::shard_path(&cas_root, "objects", orphan_hash, ".json");
    std::fs::create_dir_all(orphan_object.parent().expect("orphan parent"))
        .expect("create orphan CAS shard");
    std::fs::write(&orphan_object, br#"{"kind":"orphan"}"#).expect("write orphan CAS object");
    assert!(
        orphan_object.is_file(),
        "orphan object should exist before GC"
    );

    let out = tokio::process::Command::new(&ryeos)
        .args(["maintenance", "gc"])
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("RYEOSD_BIN", ryeosd_binary())
        .env("HOME", h.user_space.path())
        .output()
        .await
        .expect("spawn ryeos maintenance gc");

    assert!(
        out.status.success(),
        "ryeos maintenance gc failed; exit={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("freed_bytes") && stdout.contains("duration_ms"),
        "maintenance gc should print GC result fields, stdout={stdout}"
    );
    assert!(
        stdout.contains("\"deleted_objects\": 1") || stdout.contains("\"deleted_objects\":1"),
        "maintenance gc should report the planted orphan object as deleted, stdout={stdout}"
    );
    assert!(
        stdout.contains("\"freed_bytes\": 17") || stdout.contains("\"freed_bytes\":17"),
        "maintenance gc should report the planted orphan object's bytes as freed, stdout={stdout}"
    );
    assert!(
        !orphan_object.exists(),
        "real maintenance gc should delete the planted orphan object at {}",
        orphan_object.display()
    );

    let gc_log = h.state_path.join(".ai/state/logs/gc.jsonl");
    let gc_log_body = std::fs::read_to_string(&gc_log)
        .unwrap_or_else(|err| panic!("read GC event log {}: {err}", gc_log.display()));
    assert!(
        gc_log_body.contains("\"dry_run\":false") && gc_log_body.contains("\"compact\":false"),
        "bare maintenance gc should write a real non-compact runtime-state GC event, log={gc_log_body}"
    );
}

// ── Test 8: daemon init surface must not exist or mutate system space ────────
//   (Catches regressions that reintroduce daemon-side init and pollute bundles/core.)

#[tokio::test(flavor = "multi_thread")]
async fn daemon_init_only_is_rejected_and_does_not_mutate_app_root() {
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

    // Try the removed daemon init surface against the COPIED system bundle.
    let init = std::process::Command::new(ryeosd_binary())
        .arg("--init-only")
        .arg("--app-root")
        .arg(&state_path)
        .arg("--uds-path")
        .arg(state_path.join("ryeosd.sock"))
        .env("RYEOS_APP_ROOT", &state_path)
        .env("HOME", user_space.path())
        .output()
        .expect("ryeosd --init-only rejection");
    assert!(
        !init.status.success(),
        "removed --init-only should fail, stdout={} stderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );

    let after = snapshot(&sys_root);
    assert_eq!(
        before, after,
        "RYEOS_APP_ROOT was mutated by removed --init-only — daemon bootstrap must NEVER touch operator-managed bundle content"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_daemon_start_on_fresh_state_creates_no_runtime_state() {
    let outer = tempfile::tempdir().expect("state tempdir");
    let state_path = outer.path().join("state");
    let runtime_path = outer.path().join("runtime");

    let out = std::process::Command::new(ryeosd_binary())
        .arg("--app-root")
        .arg(&state_path)
        .arg("--uds-path")
        .arg(runtime_path.join("ryeosd.sock"))
        .output()
        .expect("ryeosd fresh start rejection");

    assert!(
        !out.status.success(),
        "fresh daemon start must fail before init, stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Run: ryeos init"), "stderr={stderr}");
    assert!(
        !state_path.exists(),
        "fresh daemon start must not create system state before init verification"
    );
    assert!(
        !runtime_path.exists(),
        "fresh daemon start must not create runtime socket dirs before init verification"
    );
}

// ── Test 9: UDS namespace rejects service methods (Gate 3 daemon-spawn) ──
//   The bare UDS server must only expose health/lifecycle control and `runtime.*`
//   methods. A service method like `node/status` must get "unknown_method".
//   This closes the TODO at cleanup_invariants.rs:171.

#[tokio::test(flavor = "multi_thread")]
async fn uds_namespace_rejects_service_methods() {
    let (h, _fixture) = DaemonHarness::start_fast().await.expect("start daemon");

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(&h.uds_path)
        .await
        .expect("connect to UDS socket");

    let mut stream = stream;

    let request = serde_json::json!({
        "request_id": 1u64,
        "method": "node/status",
        "params": {}
    });
    let payload = rmp_serde::to_vec(&request).expect("encode rpc request");
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len).await.expect("write frame length");
    stream.write_all(&payload).await.expect("write frame body");
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
        "UDS should reject 'node/status' with an error, got: {response}"
    );
    let error = &response["error"];
    assert_eq!(
        error["code"], "unknown_method",
        "expected 'unknown_method' error code, got: {response}"
    );
    let msg = error["message"].as_str().unwrap_or("").to_lowercase();
    assert!(
        msg.contains("unknown") || msg.contains("node/status"),
        "error message should mention unknown method, got: {}",
        error["message"]
    );
}

// ── Test 10: State lock prevents concurrent daemons (Gate 8 daemon-spawn) ──
//   Two daemons sharing the same app_root: the second must fail to acquire
//   the state lock and exit with an error. This closes the TODO at
//   cleanup_invariants.rs:289.

#[tokio::test(flavor = "multi_thread")]
async fn state_lock_prevents_concurrent_daemons() {
    let (_core_tmp, state_path) = common::copy_core_to_temp();
    let user_space = tempfile::tempdir().expect("user tempdir");
    let fixture = common::fast_fixture::populate_initialized_state(&state_path, user_space.path())
        .expect("populate initialized state");
    common::fast_fixture::register_core_bundle_at_state(&state_path, &fixture)
        .expect("register core bundle");

    let _state_lock = ryeos_app::state_lock::StateLock::acquire(
        &ryeos_app::state_lock::default_lock_path(&state_path),
    )
    .expect("hold first daemon state lock");

    let uds_outer = tempfile::tempdir().expect("uds tempdir");
    let uds_path = uds_outer.path().join("ryeosd.sock");
    let _live_uds = std::os::unix::net::UnixListener::bind(&uds_path).expect("bind live UDS");

    // Test expects the second daemon to fail on state-lock contention;
    // any free port works — `:0` lets the kernel pick one.
    let bind: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    // Point the second daemon at the SAME state dir and UDS socket as the
    // first. It must fail on the state lock before touching the live daemon's
    // socket path.
    let mut cmd = tokio::process::Command::new(common::ryeosd_binary());
    cmd.arg("--app-root")
        .arg(&state_path)
        .arg("--bind")
        .arg(bind.to_string())
        .arg("--uds-path")
        .arg(&uds_path)
        .env("HOSTNAME", "testhost")
        .env("RYEOS_APP_ROOT", &state_path)
        .env("HOME", user_space.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = cmd.output().await.expect("spawn second daemon");

    // The second daemon should fail on the state lock, not by deleting or
    // rebinding the first daemon's UDS socket.
    assert!(
        !output.status.success(),
        "second daemon should fail when sharing state_dir with first daemon; \
         exit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("state lock held") || stderr.contains("failed to acquire state lock"),
        "second daemon should fail on state lock before touching UDS; stderr={stderr}"
    );
    assert!(
        uds_path.exists(),
        "failed second daemon start must not unlink the live daemon UDS socket"
    );
    std::os::unix::net::UnixStream::connect(&uds_path)
        .expect("live UDS listener should still accept connections");
}

// ── Test 11: Symlinked node config items are rejected (Gate 12 daemon-spawn) ──
//   The node_config loader must reject symlinks in `.ai/node/bundles/`.
//   A symlinked YAML placed in the state bundles dir must cause daemon
//   startup to fail. This closes the TODO at cleanup_invariants.rs:405.

#[tokio::test(flavor = "multi_thread")]
async fn symlinked_node_config_rejected_at_startup() {
    let result = DaemonHarness::start_fast_with(
        |state_path, _user_space, _fixture| {
            let bundles_dir = state_path.join(".ai").join("node").join("bundles");
            std::fs::create_dir_all(&bundles_dir)?;
            let link_target = bundles_dir.join("adversarial.yaml");
            #[cfg(unix)]
            std::os::unix::fs::symlink("/etc/passwd", &link_target)?;
            Ok(())
        },
        |_cmd| {},
    )
    .await;

    match result {
        Ok(_) => panic!("daemon should reject symlinked node config items in bundles dir"),
        Err(e) => {
            let err_msg = format!("{:#}", e);
            let rejected_symlink = err_msg.contains("symlink")
                || err_msg.contains("not a regular file")
                || err_msg.contains("Too many levels of symbolic links")
                || err_msg.contains("os error 40");
            assert!(
                rejected_symlink,
                "error should report the fail-closed symlink rejection, got: {err_msg}"
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
    let (h, _fixture) = DaemonHarness::start_fast()
        .await
        .expect("daemon should start — bundle YAMLs must all parse");
    // If we got here, the daemon completed Phase 1 + Phase 2 node_config
    // bootstrap and the self-check. Every bundle YAML in the system bundle
    // was loaded and verified. Verify the daemon is actually healthy.
    let (status, body) = h
        .post_execute("service:node/status", ".", serde_json::json!({}))
        .await
        .expect("post /execute");
    assert!(
        status.is_success(),
        "daemon healthy check failed: {status}, body={body}"
    );
}

// ── Test 13: legacy structural fields rejected by daemon startup ──
//   Node config identity is derived from the path. A node YAML body that still
//   declares `section` must fail during startup rather than being tolerated as
//   a legacy ref shape.

#[tokio::test(flavor = "multi_thread")]
async fn legacy_section_field_rejected_at_startup() {
    let result = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| {
            let routes_dir = state_path.join(".ai").join("node").join("routes");
            std::fs::create_dir_all(&routes_dir)?;
            let body = r#"section: routes
id: legacy/section
path: /legacy/section
methods:
  - GET
auth: none
limits:
  body_bytes_max: 1024
  timeout_ms: 1000
  concurrent_max: 1
response:
  mode: json
  body_b64: e30=
"#;
            let signed = lillux::signature::sign_content_at(
                body,
                &fixture.publisher,
                "#",
                None,
                common::fast_fixture::FAST_FIXTURE_TIME,
            );
            std::fs::write(routes_dir.join("legacy-section.yaml"), signed)?;
            Ok(())
        },
        |_cmd| {},
    )
    .await;

    match result {
        Ok(_) => panic!("daemon should reject node config item declaring legacy section field"),
        Err(e) => {
            let err_msg = format!("{:#}", e);
            assert!(
                err_msg.contains("legacy structural field 'section'"),
                "error should mention legacy section rejection, got: {err_msg}"
            );
        }
    }
}
