use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::init_check::{init_state, InitDiagnostics, InitState};
use crate::lifecycle_wire::{LifecycleResponse, LifecycleWireState, StartupSnapshot};
use crate::metadata::DaemonMetadata;
use crate::LocalLifecycleEnv;

/// The daemon records its running marker immediately before publishing the
/// startup control listener. Beyond this grace period, a live marker without a
/// reachable listener is a wedged/unresponsive daemon, not ordinary startup.
const MARKER_ONLY_BOOTSTRAP_GRACE: Duration = Duration::from_secs(30);

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
        ready_at: String,
        startup: StartupSnapshot,
    },
    Failed {
        metadata: DaemonMetadata,
        startup: StartupSnapshot,
    },
    Stale {
        metadata: DaemonMetadata,
        diagnostics: StaleDiagnostics,
    },
    /// Live daemon ownership is established, but no usable lifecycle response
    /// is available (timeout, malformed/incompatible wire, wrong app identity,
    /// or a marker-only bootstrap that exceeded its publication grace).
    /// Distinct from `Stale` because starting a replacement could double-run.
    Unresponsive {
        metadata: DaemonMetadata,
        diagnostics: StaleDiagnostics,
    },
    /// The daemon process is alive but has not opened external admission.
    /// When the UDS is already serving, `startup` carries current phase and
    /// progress. Marker-only discovery reports the Bootstrapping phase.
    Starting {
        metadata: DaemonMetadata,
        startup: StartupSnapshot,
        control_available: bool,
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
///   4. If a UDS answers but its current lifecycle response is unusable,
///      return Unresponsive so replacement startup remains forbidden.
///   5. If no UDS answers and metadata exists, return Stale.
///   6. Otherwise return Stopped.
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

    // A timeout or invalid reply still proves that something owns the live
    // socket. Classify it separately so a busy/incompatible daemon is never
    // reported as stale and replaced.
    let mut unusable_control_paths: Vec<(PathBuf, String)> = Vec::new();
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
                    unusable_control_paths.push((
                        control_path.clone(),
                        format!("no answer within {timeout:?}"),
                    ));
                } else if let Some(live_error) =
                    err.downcast_ref::<crate::control::ControlLivePeerError>()
                {
                    unusable_control_paths.push((control_path.clone(), live_error.to_string()));
                } else if let Some(connect_error) =
                    err.downcast_ref::<crate::control::ControlConnectError>()
                {
                    if !matches!(
                        connect_error.kind,
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) {
                        unusable_control_paths.push((
                            control_path.clone(),
                            format!("lifecycle ownership is uncertain because {connect_error}"),
                        ));
                    }
                }
                continue;
            }
        };

        let response: LifecycleResponse = match serde_json::from_value(value) {
            Ok(response) => response,
            Err(error) => {
                unusable_control_paths.push((
                    control_path.clone(),
                    format!(
                        "{} returned an invalid lifecycle payload: {error}",
                        control_path.display()
                    ),
                ));
                continue;
            }
        };
        if let Err(error) = response.validate() {
            unusable_control_paths.push((
                control_path.clone(),
                format!(
                    "{} returned an inconsistent lifecycle payload: {error}",
                    control_path.display()
                ),
            ));
            continue;
        }

        if response.identity.app_root != config.app_root {
            unusable_control_paths.push((
                control_path.clone(),
                format!("{} belongs to a different app root", control_path.display()),
            ));
            continue;
        }
        if response.identity.uds_path != *control_path {
            unusable_control_paths.push((
                control_path.clone(),
                format!(
                    "{} returned mismatched lifecycle UDS identity {}",
                    control_path.display(),
                    response.identity.uds_path.display()
                ),
            ));
            continue;
        }

        let metadata = DaemonMetadata {
            pid: Some(response.identity.pid),
            bind: Some(response.identity.bind.clone()),
            uds_path: Some(response.identity.uds_path.clone()),
            started_at: Some(response.identity.started_at.clone()),
            version: Some(response.identity.version.clone()),
            revision: response.identity.revision.clone(),
            build_date: response.identity.build_date.clone(),
            app_root: response.identity.app_root.clone(),
        };
        return Ok(match response.status {
            LifecycleWireState::Starting => LifecycleStatus::Starting {
                metadata,
                startup: response.startup,
                control_available: true,
            },
            LifecycleWireState::Running => LifecycleStatus::Running {
                metadata,
                ready_at: response
                    .ready_at
                    .expect("validated running lifecycle response has ready_at"),
                startup: response.startup,
            },
            LifecycleWireState::Failed => LifecycleStatus::Failed {
                metadata,
                startup: response.startup,
            },
        });
    }

    // No UDS responded. Before classifying from (possibly stale) daemon.json,
    // consult the boot marker: the daemon records it before listener
    // publication. A fresh live marker with a missing/refused socket is a
    // daemon in that narrow bootstrap window; an older one is Unresponsive.
    // Reporting either "stopped" would prescribe `ryeos start` at a live
    // daemon. Any live or ownership-uncertain control path excludes marker
    // fallback; Unresponsive below carries the fail-closed remediation.
    if unusable_control_paths.is_empty() {
        let state_dir = config.app_root.join(ryeos_engine::AI_DIR).join("state");
        if let Some(crate::lifecycle_marker::LifecycleMarker::Running { pid, started_at }) =
            crate::lifecycle_marker::read(&state_dir)
        {
            if crate::lifecycle_marker::process_alive_as_ryeosd(pid) {
                let marker_age = crate::lifecycle_marker::age(&state_dir).unwrap_or_default();
                let mut startup = StartupSnapshot::bootstrapping(started_at.clone());
                startup.elapsed_ms = marker_age.as_millis().try_into().unwrap_or(u64::MAX);
                startup.updated_at = lillux::time::iso8601_now();
                let metadata = DaemonMetadata {
                    pid: Some(pid),
                    bind: Some(config.bind.to_string()),
                    uds_path: Some(config.uds_path.clone()),
                    started_at: Some(started_at),
                    version: None,
                    revision: None,
                    build_date: None,
                    app_root: config.app_root.clone(),
                };
                if marker_age > MARKER_ONLY_BOOTSTRAP_GRACE {
                    return Ok(LifecycleStatus::Unresponsive {
                        metadata,
                        diagnostics: StaleDiagnostics {
                            message: format!(
                                "live ryeosd pid {pid} has not published usable lifecycle control after {}ms",
                                startup.elapsed_ms
                            ),
                        },
                    });
                }
                return Ok(LifecycleStatus::Starting {
                    metadata,
                    startup,
                    control_available: false,
                });
            }
        }
    }

    if !unusable_control_paths.is_empty() {
        let live_control_path = unusable_control_paths
            .first()
            .expect("non-empty unusable control set")
            .0
            .clone();
        let mut metadata = metadata_hint.unwrap_or_else(|| DaemonMetadata {
            pid: None,
            bind: Some(config.bind.to_string()),
            uds_path: Some(live_control_path.clone()),
            started_at: None,
            version: None,
            revision: None,
            build_date: None,
            app_root: config.app_root.clone(),
        });
        // The path that supplied live/uncertain ownership evidence is more
        // authoritative for stop diagnostics than daemon.json's stale hint.
        metadata.uds_path = Some(live_control_path);
        let detail = unusable_control_paths
            .iter()
            .map(|(_, detail)| detail.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Ok(LifecycleStatus::Unresponsive {
            metadata,
            diagnostics: StaleDiagnostics {
                message: format!("lifecycle control is live but unusable: {detail}"),
            },
        });
    }

    match metadata_hint {
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

/// True only once the daemon's lifecycle response has passed the strict
/// Running + ready validation. Starting and Failed are always false.
pub fn is_ready(status: &LifecycleStatus) -> bool {
    matches!(status, LifecycleStatus::Running { .. })
}

pub fn is_running(status: &LifecycleStatus) -> bool {
    is_ready(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle_wire::StartupPhase;
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
        // A daemon writes its boot marker just before it publishes the stable
        // startup control socket. Misclassifying that narrow window as Stopped
        // prescribes `ryeos start` at a live daemon.
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
        let LifecycleStatus::Starting { metadata, .. } = status.unwrap() else {
            panic!("live boot marker without socket should be Starting");
        };
        assert_eq!(metadata.pid, Some(daemon.id()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn old_live_boot_marker_without_control_socket_is_unresponsive() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        let state_dir = config.app_root.join(ryeos_engine::AI_DIR).join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let fake_ryeosd = tmp.path().join("ryeosd");
        std::fs::copy("/bin/sh", &fake_ryeosd).unwrap();
        let mut daemon = std::process::Command::new(&fake_ryeosd)
            .args(["-c", "sleep 30; exit 0"])
            .spawn()
            .unwrap();
        let marker = state_dir.join("lifecycle.json");
        std::fs::write(
            &marker,
            format!(
                r#"{{"state":"running","pid":{},"started_at":"2026-01-01T00:00:00Z"}}"#,
                daemon.id()
            ),
        )
        .unwrap();
        let old = std::time::SystemTime::now()
            .checked_sub(MARKER_ONLY_BOOTSTRAP_GRACE + Duration::from_secs(1))
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let status = status(&env).await;
        let _ = daemon.kill();
        let _ = daemon.wait();
        let LifecycleStatus::Unresponsive {
            metadata,
            diagnostics,
        } = status.unwrap()
        else {
            panic!("old live marker without control must be Unresponsive");
        };
        assert_eq!(metadata.pid, Some(daemon.id()));
        assert!(diagnostics.message.contains("has not published"));
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
    async fn connected_peer_that_closes_mid_rpc_is_unresponsive_not_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();

        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            // Drop the connected stream without returning an RPC frame.
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        assert!(
            matches!(status, LifecycleStatus::Unresponsive { .. }),
            "a connected peer with an unusable RPC exchange must block replacement: {status:?}"
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
            let response = ready_wire_result(1234, app_root, uds_path, "now", "test");
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Running { metadata, .. } = status else {
            panic!("expected running")
        };
        assert_eq!(metadata.pid, Some(1234));
        assert!(!DaemonMetadata::path(&config.app_root).exists());
    }

    #[tokio::test]
    async fn live_starting_response_preserves_phase_and_does_not_report_running() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let response = starting_wire_result(
            1234,
            config.app_root.clone(),
            config.uds_path.clone(),
            "2026-07-14T00:00:00Z",
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Starting {
            metadata, startup, ..
        } = status
        else {
            panic!("expected starting")
        };
        assert_eq!(metadata.pid, Some(1234));
        assert_eq!(startup.phase, StartupPhase::OpeningProjection);
        assert!(!is_ready(&LifecycleStatus::Starting {
            metadata,
            startup,
            control_available: true,
        }));
    }

    #[tokio::test]
    async fn live_failed_response_surfaces_concrete_startup_error() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let response = failed_wire_result(
            1234,
            config.app_root.clone(),
            config.uds_path.clone(),
            "2026-07-14T00:00:00Z",
            "projection checksum mismatch",
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Failed { startup, .. } = status else {
            panic!("expected failed")
        };
        assert_eq!(startup.phase, StartupPhase::Failed);
        assert_eq!(
            startup.error.as_deref(),
            Some("projection checksum mismatch")
        );
        assert!(startup.failed_at.is_some());
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
        let live_uds_path = hinted_uds_path.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = ready_wire_result(5678, app_root, live_uds_path, "metadata", "metadata");
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Running { metadata, .. } = status else {
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
        let uds_path = config.uds_path.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = ready_wire_result(9999, app_root, uds_path, "live", "live");
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Running { metadata, .. } = status else {
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
    async fn incompatible_live_status_blocks_replacement_start() {
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
            // Successful RPC with an incompatible lifecycle wire still proves
            // a live owner of the socket. It must block replacement startup.
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
            matches!(status, LifecycleStatus::Unresponsive { .. }),
            "incompatible live response should be Unresponsive, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn oversized_live_status_frame_blocks_replacement_without_reading_its_body() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();

        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let oversized = crate::LIFECYCLE_FRAME_MAX_BYTES + 1;
            stream.write_all(&oversized.to_be_bytes()).await.unwrap();
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Unresponsive { diagnostics, .. } = status else {
            panic!("oversized live frame must block replacement: {status:?}")
        };
        assert!(diagnostics.message.contains("frame too large"));
    }

    #[tokio::test]
    async fn live_control_for_different_app_root_blocks_replacement_start() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let other_app_root = tmp.path().join("other-state");
        let uds_path = config.uds_path.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = ready_wire_result(9999, other_app_root, uds_path, "now", "test");
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        assert!(
            matches!(status, LifecycleStatus::Unresponsive { .. }),
            "a live conflicting socket should be Unresponsive, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn mismatched_live_uds_identity_blocks_replacement_start() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.app_root);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let app_root = config.app_root.clone();
        let advertised_uds = tmp.path().join("different/ryeosd.sock");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = ready_wire_result(9999, app_root, advertised_uds, "now", "test");
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        let LifecycleStatus::Unresponsive {
            metadata,
            diagnostics,
        } = status
        else {
            panic!("a mismatched live UDS identity must block replacement: {status:?}")
        };
        assert_eq!(metadata.uds_path, Some(config.uds_path.clone()));
        assert!(diagnostics
            .message
            .contains("mismatched lifecycle UDS identity"));
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

    fn ready_wire_result(
        pid: u32,
        app_root: PathBuf,
        uds_path: PathBuf,
        started_at: &str,
        version: &str,
    ) -> serde_json::Value {
        let identity = crate::LifecycleIdentity {
            pid,
            bind: "127.0.0.1:7400".into(),
            uds_path,
            app_root,
            started_at: started_at.into(),
            version: version.into(),
            revision: None,
            build_date: None,
        };
        let response = crate::LifecycleResponse::running(
            identity,
            "2026-07-14T00:00:01Z",
            StartupSnapshot::bootstrapping(started_at),
        );
        serde_json::json!({ "request_id": 1u64, "result": response })
    }

    fn starting_wire_result(
        pid: u32,
        app_root: PathBuf,
        uds_path: PathBuf,
        started_at: &str,
    ) -> serde_json::Value {
        let identity = crate::LifecycleIdentity {
            pid,
            bind: "127.0.0.1:7400".into(),
            uds_path,
            app_root,
            started_at: started_at.into(),
            version: "test".into(),
            revision: None,
            build_date: None,
        };
        let mut startup = StartupSnapshot::bootstrapping(started_at);
        startup.sequence = 1;
        startup.phase = StartupPhase::OpeningProjection;
        startup.updated_at = "2026-07-14T00:00:01Z".into();
        startup.phase_started_at = startup.updated_at.clone();
        serde_json::json!({
            "request_id": 1u64,
            "result": crate::LifecycleResponse::starting(identity, startup),
        })
    }

    fn failed_wire_result(
        pid: u32,
        app_root: PathBuf,
        uds_path: PathBuf,
        started_at: &str,
        error: &str,
    ) -> serde_json::Value {
        let starting = starting_wire_result(pid, app_root, uds_path, started_at);
        let response: crate::LifecycleResponse =
            serde_json::from_value(starting["result"].clone()).unwrap();
        let response = crate::LifecycleResponse::failed(
            response.identity,
            "2026-07-14T00:00:02Z",
            error,
            response.startup,
        );
        serde_json::json!({ "request_id": 1u64, "result": response })
    }

    async fn write_frame(stream: &mut tokio::net::UnixStream, payload: &[u8]) {
        let len = payload.len() as u32;
        stream.write_all(&len.to_be_bytes()).await.unwrap();
        stream.write_all(payload).await.unwrap();
    }
}
