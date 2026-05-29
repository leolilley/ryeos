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
        system_space_dir: PathBuf,
    },
    Running {
        metadata: DaemonMetadata,
    },
    Stale {
        metadata: DaemonMetadata,
        diagnostics: StaleDiagnostics,
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
    if let InitState::NotInitialized { diagnostics } = init_state(&config.system_space_dir)? {
        return Ok(LifecycleStatus::NotInitialized { diagnostics });
    }

    // Read `daemon.json` once via the env's best-effort accessor; a
    // malformed file becomes "no hint" rather than fatal.
    let metadata_hint = env.read_metadata_hint();
    let candidates = env.uds_candidates_from_hint(metadata_hint.as_ref());
    let timeout = env.rpc_timeout();

    for control_path in &candidates {
        let value = match crate::control::call(control_path, "lifecycle.status", json!({}), timeout)
            .await
        {
            Ok(value) => value,
            Err(_) => continue,
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

        let live_system_space_dir = value
            .get("system_space_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let reports_requested_system_space = live_system_space_dir
            .as_ref()
            .map(|path| path == &config.system_space_dir)
            .unwrap_or_else(|| {
                metadata_hint
                    .as_ref()
                    .map(|metadata| metadata.system_space_dir == config.system_space_dir)
                    .unwrap_or(false)
            });
        if !reports_requested_system_space {
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
            system_space_dir: live_system_space_dir
                .or_else(|| hint.as_ref().map(|m| m.system_space_dir.clone()))
                .unwrap_or_else(|| config.system_space_dir.clone()),
        };
        return Ok(LifecycleStatus::Running { metadata });
    }

    // No UDS responded.
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
            system_space_dir: config.system_space_dir.clone(),
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
            system_space_dir: root.join("state"),
            user_root: root.join("user"),
            bind: "127.0.0.1:7400".parse::<SocketAddr>().unwrap(),
            uds_path: root.join("runtime/ryeosd.sock"),
        }
    }

    fn test_env(root: &std::path::Path) -> LocalLifecycleEnv {
        LocalLifecycleEnv::from_config(test_config(root))
    }

    fn mark_initialized(system_space_dir: &std::path::Path) {
        let bundles = system_space_dir.join(".ai/node/bundles");
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
        assert!(!env.config().system_space_dir.exists());
        assert!(!tmp.path().join("runtime").exists());
    }

    #[tokio::test]
    async fn stale_daemon_metadata_without_live_control_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.system_space_dir);
        DaemonMetadata {
            pid: Some(999_999),
            bind: Some(config.bind.to_string()),
            uds_path: Some(config.uds_path.clone()),
            started_at: Some("now".to_string()),
            version: Some("test".to_string()),
            system_space_dir: config.system_space_dir.clone(),
        }
        .write(&config.system_space_dir)
        .unwrap();

        let status = status(&env).await.unwrap();
        assert!(matches!(status, LifecycleStatus::Stale { .. }));
    }

    #[tokio::test]
    async fn live_control_without_daemon_json_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.system_space_dir);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let uds_path = config.uds_path.clone();
        let system_space_dir = config.system_space_dir.clone();
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
                    "system_space_dir": system_space_dir.display().to_string(),
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
        assert!(!DaemonMetadata::path(&config.system_space_dir).exists());
    }

    #[tokio::test]
    async fn status_uses_daemon_metadata_uds_path_as_liveness_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.system_space_dir);
        let hinted_uds_path = tmp.path().join("hinted/ryeosd.sock");
        std::fs::create_dir_all(hinted_uds_path.parent().unwrap()).unwrap();
        DaemonMetadata {
            pid: Some(5678),
            bind: Some(config.bind.to_string()),
            uds_path: Some(hinted_uds_path.clone()),
            started_at: Some("metadata".to_string()),
            version: Some("metadata".to_string()),
            system_space_dir: config.system_space_dir.clone(),
        }
        .write(&config.system_space_dir)
        .unwrap();

        let listener = tokio::net::UnixListener::bind(&hinted_uds_path).unwrap();
        let system_space_dir = config.system_space_dir.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = serde_json::json!({
                "request_id": 1u64,
                "result": {
                    "status": "running",
                    "system_space_dir": system_space_dir.display().to_string()
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
        mark_initialized(&config.system_space_dir);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        DaemonMetadata {
            pid: Some(1),
            bind: Some("0.0.0.0:1".to_string()),
            uds_path: Some(config.uds_path.clone()),
            started_at: Some("stale".to_string()),
            version: Some("stale".to_string()),
            system_space_dir: config.system_space_dir.clone(),
        }
        .write(&config.system_space_dir)
        .unwrap();

        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let system_space_dir = config.system_space_dir.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = serde_json::json!({
                "request_id": 1u64,
                "result": {
                    "status": "running",
                    "pid": 9999u64,
                    "bind": "127.0.0.1:7400",
                    "system_space_dir": system_space_dir.display().to_string(),
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
        mark_initialized(&config.system_space_dir);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        DaemonMetadata {
            pid: Some(42),
            bind: Some(config.bind.to_string()),
            uds_path: Some(config.uds_path.clone()),
            started_at: Some("hint".to_string()),
            version: Some("hint".to_string()),
            system_space_dir: config.system_space_dir.clone(),
        }
        .write(&config.system_space_dir)
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
    async fn live_control_for_different_system_space_is_not_trusted_as_live() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.system_space_dir);
        std::fs::create_dir_all(config.uds_path.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&config.uds_path).unwrap();
        let other_system_space = tmp.path().join("other-state");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_frame(&mut stream).await;
            let response = serde_json::json!({
                "request_id": 1u64,
                "result": {
                    "status": "running",
                    "pid": 9999u64,
                    "system_space_dir": other_system_space.display().to_string()
                }
            });
            let encoded = rmp_serde::to_vec_named(&response).unwrap();
            write_frame(&mut stream, &encoded).await;
        });

        let status = status(&env).await.unwrap();
        server.await.unwrap();
        assert!(
            matches!(status, LifecycleStatus::Stopped { .. }),
            "response for a different system space should be ignored, got: {status:?}"
        );
    }

    #[tokio::test]
    async fn malformed_daemon_json_does_not_fail_status() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let config = env.config();
        mark_initialized(&config.system_space_dir);
        // Plant a daemon.json that is valid file but invalid JSON.
        std::fs::write(
            DaemonMetadata::path(&config.system_space_dir),
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
