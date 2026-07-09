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

    // A busy daemon (e.g. absorbing a launch burst) times out the status
    // probe transiently; that congestion is self-clearing, so give it a few
    // bounded retries before failing the launch. Every other outcome is
    // settled on the first probe.
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
            ryeos_node::LifecycleStatus::Starting { pid, .. } => {
                // Boot (projection catch-up after a deploy) runs for minutes,
                // far past the busy-retry budget — settle immediately with
                // the actual remediation: wait, don't start a second daemon.
                return Err(CliError::Local {
                    detail: format!(
                        "RyeOS daemon (pid {pid}) is starting up; its control socket is \
                         not available yet — wait for `ryeos node status` to report \
                         running, then retry"
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
            "RyeOS daemon is running but did not answer the control probe within the \
             timeout ({busy_message}); likely busy — retry shortly rather than starting \
             a replacement daemon"
        ),
    })
}
