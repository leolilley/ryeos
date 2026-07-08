use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::init_check::{init_state, InitDiagnostics, InitState};
use crate::metadata::DaemonMetadata;
use crate::LocalLifecycleEnv;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LifecycleStatus {
    NotInitialized {
        diagnostics: InitDiagnostics,
    },
    Stopped {
        app_root: PathBuf,
    },
    Running {
        metadata: DaemonMetadata,
    },
    Stale {
        metadata: DaemonMetadata,
        diagnostics: StaleDiagnostics,
    },
    /// A control probe reached a socket but the RPC bound elapsed without
    /// an answer — a live-but-busy daemon (e.g. under a launch burst), not
    /// a dead one. Distinct from `Stale` (refused/missing socket) because
    /// the remediations are opposite: this clears on its own; starting a
    /// replacement daemon would double-run.
    Unresponsive {
        metadata: DaemonMetadata,
        diagnostics: StaleDiagnostics,
    },
    /// The daemon process is alive (per its boot marker) but its control
    /// socket is not up yet — boot is in progress. Projection catch-up after
    /// a deploy can hold this window open for minutes. Clears on its own;
    /// starting a second daemon would double-run against the same state.
    Starting {
        pid: u32,
        started_at: String,
        app_root: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StaleDiagnostics {
    pub message: String,
}

/// Read-only lifecycle status probe.
///
/// Order of operations (no writes, no repairs):
///   1. Validate init-state; if not initialized, return NotInitialized.
///   2. Read `daemon.json` as a hint (may be missing).
///   3. Probe each UDS candidate (metadata hint first, configured next)
///      until one responds to `lifecycle.status` within the bounded
///      RPC timeout. Live response fields override stale metadata
///      fields.
///   4. If no UDS responds and metadata exists, return Stale.
///   5. Otherwise return Stopped.
pub async fn status(env: &LocalLifecycleEnv) -> Result<LifecycleStatus> {
    let config = env.config();
    if let InitState::NotInitialized { diagnostics } = init_state(&config.app_root)? {
        return Ok(LifecycleStatus::NotInitialized { diagnostics });
    }

    // Read `daemon.json` once via the env's best-effort accessor; a
    // malformed file becomes "no hint" rather than fatal.
    let metadata_hint = env.read_metadata_hint();
    let candidates = env.uds_candidates_from_hint(metadata_hint.as_ref());
    let timeout = env.rpc_timeout();

    // A probe that TIMES OUT (vs. refused/missing) means something is
    // listening but too busy to answer within the bound — classified
    // separately below so a busy daemon is never reported as stale.
    let mut probe_timed_out = false;
    for control_path in &candidates {
        let value = match crate::control::call(control_path, "lifecycle.status", json!({}), timeout)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                if err
                    .downcast_ref::<crate::control::ControlCallTimeout>()
                    .is_some()
                {
                    probe_timed_out = true;
                }
                continue;
            }
        };

        // Guard: a successful RPC must explicitly report "running"
        // before we trust it as a live daemon. If the contract changes
        // server-side, fail closed (treat as no response) rather than
        // misclassify.
        let reports_running = value
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s == "running")
            .unwrap_or(false);
        if !reports_running {
            continue;
        }

        let live_app_root = value
            .get("app_root")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let reports_requested_app_root = live_app_root
            .as_ref()
            .map(|path| path == &config.app_root)
            .unwrap_or_else(|| {
                metadata_hint
                    .as_ref()
                    .map(|metadata| metadata.app_root == config.app_root)
                    .unwrap_or(false)
            });
        if !reports_requested_app_root {
            continue;
        }

        // Live response — prefer live fields over the (possibly stale) hint.
        let hint = metadata_hint.clone();
        let metadata = DaemonMetadata {
            pid: value
                .get("pid")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .or_else(|| hint.as_ref().and_then(|m| m.pid)),
            bind: value
                .get("bind")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned)
                .or_else(|| hint.as_ref().and_then(|m| m.bind.clone()))
                .or_else(|| Some(config.bind.to_string())),
            uds_path: value
                .get("uds_path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .or_else(|| Some(control_path.clone())),
            started_at: value
                .get("started_at")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned)
                .or_else(|| hint.as_ref().and_then(|m| m.started_at.clone())),
            version: value
                .get("version")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned)
                .or_else(|| hint.as_ref().and_then(|m| m.version.clone())),
            revision: value
                .get("revision")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned)
                .or_else(|| hint.as_ref().and_then(|m| m.revision.clone())),
            build_date: value
                .get("build_date")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned)
                .or_else(|| hint.as_ref().and_then(|m| m.build_date.clone())),
            app_root: live_app_root
                .or_else(|| hint.as_ref().map(|m| m.app_root.clone()))
                .unwrap_or_else(|| config.app_root.clone()),
        };
        return Ok(LifecycleStatus::Running { metadata });
    }

    // No UDS responded. Before classifying from (possibly stale) daemon.json,
    // consult the boot marker: the daemon records it at process start, well
    // before its control socket exists (projection catch-up after a deploy
    // holds that window open for minutes). A live marker pid with a missing/
    // refused socket is a daemon mid-boot — reporting it "stopped" would
    // prescribe `ryeos start` at a live daemon. A probe that TIMED OUT is
    // excluded: a socket that accepted means the daemon is past boot, and
    // Unresponsive below carries the right remediation.
    if !probe_timed_out {
        let state_dir = config.app_root.join(ryeos_engine::AI_DIR).join("state");
        if let Some(crate::lifecycle_marker::LifecycleMarker::Running { pid, started_at }) =
            crate::lifecycle_marker::read(&state_dir)
        {
            if crate::lifecycle_marker::process_alive_as_ryeosd(pid) {
                return Ok(LifecycleStatus::Starting {
                    pid,
                    started_at,
                    app_root: config.app_root.clone(),
                });
            }
        }
    }

    match metadata_hint {
        Some(metadata) if probe_timed_out => {
            let probed: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            Ok(LifecycleStatus::Unresponsive {
                metadata,
                diagnostics: StaleDiagnostics {
                    message: format!(
                        "lifecycle control accepted a probe but did not answer within {timeout:?} \
                         at: {} — the daemon appears alive but busy",
                        probed.join(", ")
                    ),
                },
            })
        }
        Some(metadata) => {
            let probed: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            Ok(LifecycleStatus::Stale {
                metadata,
                diagnostics: StaleDiagnostics {
                    message: format!(
                        "daemon metadata exists but lifecycle control did not respond at any of: {}",
                        probed.join(", ")
                    ),
                },
            })
        }
        None => Ok(LifecycleStatus::Stopped {
            app_root: config.app_root.clone(),
        }),
    }
}

pub fn is_running(status: &LifecycleStatus) -> bool {
    matches!(status, LifecycleStatus::Running { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeConfig;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_config(root: &std::path::Path) -> NodeConfig {
        NodeConfig {
            app_root: root.join("state"),
            bind: "127.0.0.1:7400".parse::<SocketAddr>().unwrap(),
            uds_path: root.join("runtime/ryeosd.sock"),
        }
    }

    fn test_env(root: &std::path::Path) -> LocalLifecycleEnv {
        LocalLifecycleEnv::from_config(test_config(root))
    }

    fn mark_initialized(app_root: &std::path::Path) {
        let bundles = app_root.join(".ai/node/bundles");
        std::fs::create_dir_all(&bundles).unwrap();
        std::fs::write(
            bundles.join("core.yaml"),
            "# ryeos:signed:test\nkind: node\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn status_on_fresh_tempdir_does_not_create_files() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let status = status(&env).await.unwrap();
        assert!(matches!(status, LifecycleStatus::NotInitialized { .. }));
        assert!(!env.config().app_root.exists());
        assert!(!tmp.path().join("runtime").exists());
    }

    #[tokio::test]
    async fn stale_daemon_metadata_without_live_control_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        DaemonMetadata {
            pid: Some(999_999),
            bind: Some(config.bind.to_string()),
            uds_path: Some(config.uds_path.clone()),
            started_at: Some("now".to_string()),
            version: Some("test".to_string()),
            revision: None,
            build_date: None,
            app_root: config.app_root.clone(),
        }
        .write(&config.app_root)
        .unwrap();

        let status = status(&env).await.unwrap();
        assert!(matches!(status, LifecycleStatus::Stale { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_boot_marker_without_control_socket_is_starting_not_stopped() {
        // A daemon writes its boot marker at process start, minutes before
        // the control socket exists on a heavy boot (projection catch-up).
        // Misclassifying that window as Stopped prescribes `ryeos start` at
        // a live daemon.
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        let state_dir = config.app_root.join(ryeos_engine::AI_DIR).join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // Stand in for the booting daemon with a process whose comm really
        // is "ryeosd" (the classifier verifies the name, not just liveness):
        // a copy of /bin/sh idling under that name.
        let fake_ryeosd = tmp.path().join("ryeosd");
        std::fs::copy("/bin/sh", &fake_ryeosd).unwrap();
        // Two commands, so the shell cannot exec-optimize into `sleep`
        // (which would change the comm out from under the test).
        let mut daemon = std::process::Command::new(&fake_ryeosd)
            .args(["-c", "sleep 30; exit 0"])
            .spawn()
            .unwrap();
        std::fs::write(
            state_dir.join("lifecycle.json"),
            format!(
                r#"{{"state":"running","pid":{},"started_at":"2026-01-01T00:00:00Z"}}"#,
                daemon.id()
            ),
        )
        .unwrap();

        let status = status(&env).await;
        let _ = daemon.kill();
        let _ = daemon.wait();
        let LifecycleStatus::Starting { pid, .. } = status.unwrap() else {
            panic!("live boot marker without socket should be Starting");
        };
        assert_eq!(pid, daemon.id());
    }

    // The recycled-pid discrimination needs /proc to inspect the comm.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn boot_marker_pid_recycled_by_another_process_is_not_starting() {
        // A crash leaves a `running` marker; if the OS recycles that pid onto
        // an unrelated process, classifying it as a live daemon would block
        // `ryeos start` indefinitely. This test's own pid is alive but is not
        // a ryeosd.
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        let state_dir = config.app_root.join(ryeos_engine::AI_DIR).join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("lifecycle.json"),
            format!(
                r#"{{"state":"running","pid":{},"started_at":"2026-01-01T00:00:00Z"}}"#,
                std::process::id()
            ),
        )
        .unwrap();

        let status = status(&env).await.unwrap();
        assert!(
            matches!(status, LifecycleStatus::Stopped { .. }),
            "recycled marker pid should not classify as Starting, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn boot_marker_with_dead_pid_is_stopped() {
        // A crashed daemon leaves a `running` marker behind; a dead marker
        // pid must NOT read as Starting — `ryeos start` is the right
        // remediation there.
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        let state_dir = config.app_root.join(ryeos_engine::AI_DIR).join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("lifecycle.json"),
            format!(
                r#"{{"state":"running","pid":{},"started_at":"2026-01-01T00:00:00Z"}}"#,
                u32::MAX - 1
            ),
        )
        .unwrap();

        let status = status(&env).await.unwrap();
        assert!(
            matches!(status, LifecycleStatus::Stopped { .. }),
            "dead marker pid should be Stopped, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn accepted_but_silent_control_socket_is_unresponsive_not_stale() {
        // A listener that accepts the probe but never answers is a busy live
        // daemon; misclassifying it as Stale prescribes `ryeos start` — the
        // wrong remediation (it would double-run).
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        DaemonMetadata {
            pid: Some(4242),
            bind: Some(config.bind.to_string()),
            uds_path: Some(config.uds_path.clone()),
            started_at: Some("now".to_string()),
            version: Some("test".to_string()),
            revision: None,
            build_date: None,
            app_root: config.app_root.clone(),
        }
        .write(&config.app_root)
        .unwrap();

        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            std::future::pending::<()>().await;
        });

        let status = status(&env).await.unwrap();
        server.abort();
        assert!(
            matches!(status, LifecycleStatus::Unresponsive { .. }),
            "accepted-but-silent socket should be Unresponsive, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn live_control_without_daemon_json_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let uds_path = config.uds_path.clone();
        let app_root = config.app_root.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = serde_json::json!({
                "request_id": 1u64,
                "result": {
                    "status": "running",
                    "pid": 1234u64,
                    "bind": "127.0.0.1:7400",
                    "uds_path": uds_path.display().to_string(),
                    "app_root": app_root.display().to_string(),
                    "started_at": "now",
                    "version": "test"
                }
            });
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Running { metadata } = status else {
            panic!("expected running")
        };
        assert_eq!(metadata.pid, Some(1234));
        assert!(!DaemonMetadata::path(&config.app_root).exists());
    }

    #[tokio::test]
    async fn status_uses_daemon_metadata_uds_path_as_liveness_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        let hinted_uds_path = tmp.path().join("hinted/ryeosd.sock");
        std::fs::create_dir_all(hinted_uds_path.parent().unwrap()).unwrap();
        DaemonMetadata {
            pid: Some(5678),
            bind: Some(config.bind.to_string()),
            uds_path: Some(hinted_uds_path.clone()),
            started_at: Some("metadata".to_string()),
            version: Some("metadata".to_string()),
            revision: None,
            build_date: None,
            app_root: config.app_root.clone(),
        }
        .write(&config.app_root)
        .unwrap();

        let listener = tokio::net::UnixListener::bind(&hinted_uds_path).unwrap();
        let app_root = config.app_root.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = serde_json::json!({
                "request_id": 1u64,
                "result": {
                    "status": "running",
                    "app_root": app_root.display().to_string()
                }
            });
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Running { metadata } = status else {
            panic!("expected running")
        };
        assert_eq!(metadata.pid, Some(5678));
        assert_eq!(metadata.uds_path, Some(hinted_uds_path));
    }

    #[tokio::test]
    async fn live_response_overrides_stale_metadata_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        DaemonMetadata {
            pid: Some(1),
            bind: Some("0.0.0.0:1".to_string()),
            uds_path: Some(config.uds_path.clone()),
            started_at: Some("stale".to_string()),
            version: Some("stale".to_string()),
            revision: None,
            build_date: None,
            app_root: config.app_root.clone(),
        }
        .write(&config.app_root)
        .unwrap();

        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let app_root = config.app_root.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = serde_json::json!({
                "request_id": 1u64,
                "result": {
                    "status": "running",
                    "pid": 9999u64,
                    "bind": "127.0.0.1:7400",
                    "app_root": app_root.display().to_string(),
                    "started_at": "live",
                    "version": "live"
                }
            });
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Running { metadata } = status else {
            panic!("expected running")
        };
        assert_eq!(metadata.pid, Some(9999), "live pid wins over stale");
        assert_eq!(
            metadata.bind.as_deref(),
            Some("127.0.0.1:7400"),
            "live bind wins"
        );
        assert_eq!(metadata.started_at.as_deref(), Some("live"));
        assert_eq!(metadata.version.as_deref(), Some("live"));
    }

    #[tokio::test]
    async fn non_running_status_response_is_not_trusted_as_live() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        DaemonMetadata {
            pid: Some(42),
            bind: Some(config.bind.to_string()),
            uds_path: Some(config.uds_path.clone()),
            started_at: Some("hint".to_string()),
            version: Some("hint".to_string()),
            revision: None,
            build_date: None,
            app_root: config.app_root.clone(),
        }
        .write(&config.app_root)
        .unwrap();

        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            // Successful RPC but status is NOT "running" — must be
            // treated as no live daemon (fail-closed).
            let response = serde_json::json!({
                "request_id": 1u64,
                "result": { "status": "draining", "pid": 9999u64 }
            });
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        assert!(
            matches!(status, LifecycleStatus::Stale { .. }),
            "non-running response with metadata hint should be Stale, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn live_control_for_different_app_root_is_not_trusted_as_live() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let other_app_root = tmp.path().join("other-state");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = serde_json::json!({
                "request_id": 1u64,
                "result": {
                    "status": "running",
                    "pid": 9999u64,
                    "app_root": other_app_root.display().to_string()
                }
            });
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        assert!(
            matches!(status, LifecycleStatus::Stopped { .. }),
            "response for a different app root should be ignored, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn malformed_daemon_json_does_not_fail_status() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        // Plant a daemon.json that is valid file but invalid JSON.
        std::fs::write(
            DaemonMetadata::path(&config.app_root),
            "{ this is : not json",
        )
        .unwrap();

        // status must NOT propagate the parse error; with no live daemon
        // and an unreadable hint, it should report Stopped.
        let status = status(&env).await.unwrap();
        assert!(
            matches!(status, LifecycleStatus::Stopped { .. }),
            "malformed daemon.json should be treated as no hint, got: {status:?}"
        );
    }

    async fn read_frame(stream: &mut tokio::net::UnixStream) -> Vec<u8> {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).await.unwrap();
        payload
    }

    async fn write_frame(stream: &mut tokio::net::UnixStream, payload: &[u8]) {
        let len = payload.len() as u32;
        stream.write_all(&len.to_be_bytes()).await.unwrap();
        stream.write_all(payload).await.unwrap();
    }
}
