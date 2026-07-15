use std::path::Path;

use crate::error::CliError;

pub async fn lifecycle_preflight(app_root: &Path) -> Result<(), CliError> {
    // A deliberate remote override is still valid for normal daemon-backed
    // dispatch. Lifecycle reads/mutations themselves ignore this env var.
    if std::env::var_os("RYEOSD_URL").is_some() {
        return Ok(());
    }

    let env = ryeos_node::LocalLifecycleEnv::load(Some(app_root.to_path_buf())).map_err(|e| {
        CliError::Local {
            detail: format!("resolve local node lifecycle env: {e:#}"),
        }
    })?;
    let controller = ryeos_node::LifecycleController::from_env(env);

    // A busy or incompatible live daemon cannot provide a usable status.
    // Retry a few bounded times, but never reclassify it as stopped and start
    // a replacement against the same state.
    const BUSY_ATTEMPTS: u32 = 3;
    let mut busy_message = String::new();
    for attempt in 1..=BUSY_ATTEMPTS {
        match controller.status().await.map_err(|e| CliError::Local {
            detail: format!("read lifecycle status: {e:#}"),
        })? {
            ryeos_node::LifecycleStatus::Running { .. } => return Ok(()),
            ryeos_node::LifecycleStatus::NotInitialized { diagnostics } => {
                return Err(CliError::Local {
                    detail: format!(
                        "RyeOS is not initialized. Run: ryeos init\nDetail: {}",
                        diagnostics.message
                    ),
                });
            }
            ryeos_node::LifecycleStatus::Stopped { .. } => {
                return Err(CliError::Local {
                    detail: "RyeOS is initialized but not running. Run: ryeos start".into(),
                });
            }
            ryeos_node::LifecycleStatus::Stale { diagnostics, .. } => {
                return Err(CliError::Local {
                    detail: format!(
                        "RyeOS daemon metadata is stale: {}\nRun: ryeos start",
                        diagnostics.message
                    ),
                });
            }
            ryeos_node::LifecycleStatus::Starting {
                metadata, startup, ..
            } => {
                // Boot (including a projection rebuild/recovery after a deploy) can run for minutes,
                // far past the busy-retry budget — settle immediately with
                // the actual remediation: wait, don't start a second daemon.
                let pid = metadata.pid.unwrap_or_default();
                return Err(CliError::Local {
                    detail: format!(
                        "RyeOS daemon (pid {pid}) is starting: {} ({}ms elapsed) — \
                         wait for `ryeos node status` to report running, then retry",
                        startup.phase.as_str(),
                        startup.elapsed_ms,
                    ),
                });
            }
            ryeos_node::LifecycleStatus::Failed { metadata, startup } => {
                return Err(CliError::Local {
                    detail: format!(
                        "RyeOS daemon startup failed{} during {}: {}",
                        metadata
                            .pid
                            .map(|pid| format!(" (pid {pid})"))
                            .unwrap_or_default(),
                        startup.phase.as_str(),
                        startup
                            .error
                            .as_deref()
                            .unwrap_or("unknown startup failure"),
                    ),
                });
            }
            ryeos_node::LifecycleStatus::Unresponsive { diagnostics, .. } => {
                busy_message = diagnostics.message;
                if attempt < BUSY_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                }
            }
        }
    }
    Err(CliError::Local {
        detail: format!(
            "RyeOS daemon control is live but unusable ({busy_message}); retry shortly \
             if it is busy, otherwise inspect or stop the existing daemon — never start \
             a replacement against the same state"
        ),
    })
}
