use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::status::LifecycleStatus;
use crate::LocalLifecycleEnv;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopOptions {
    pub force: bool,
    pub timeout: Duration,
}

impl Default for StopOptions {
    fn default() -> Self {
        Self {
            force: false,
            timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopReport {
    pub status: LifecycleStatus,
    pub already_stopped: bool,
}

pub async fn stop(env: &LocalLifecycleEnv, opts: StopOptions) -> Result<StopReport> {
    let initial = crate::status::status(env).await?;
    let initial_live_uds: Option<PathBuf> = match initial {
        LifecycleStatus::NotInitialized { .. } => {
            bail!("RyeOS is not initialized. Run: ryeos init")
        }
        status @ LifecycleStatus::Stopped { .. } => {
            return Ok(StopReport {
                status,
                already_stopped: true,
            })
        }
        LifecycleStatus::Stale { diagnostics, .. } => {
            bail!("stale daemon metadata: {}", diagnostics.message)
        }
        LifecycleStatus::Running { ref metadata } => metadata.uds_path.clone(),
        // Busy-but-alive: proceed with the normal stop flow — the graceful
        // shutdown call may itself time out, after which the deadline/force
        // path below applies.
        LifecycleStatus::Unresponsive { ref metadata, .. } => metadata.uds_path.clone(),
    };

    // Send shutdown to the UDS that just proved the daemon is alive,
    // falling back to the configured path only if status did not record
    // one. Never blind-fire at a stale `daemon.json`-derived path.
    let shutdown_uds = initial_live_uds
        .clone()
        .unwrap_or_else(|| env.config().uds_path.clone());
    let _ = crate::control::call(
        &shutdown_uds,
        "lifecycle.shutdown",
        json!({}),
        env.rpc_timeout(),
    )
    .await;

    let deadline = Instant::now() + opts.timeout;
    loop {
        let status = crate::status::status(env).await?;
        match status {
            status @ LifecycleStatus::Stopped { .. } | status @ LifecycleStatus::Stale { .. } => {
                return Ok(StopReport {
                    status,
                    already_stopped: false,
                })
            }
            LifecycleStatus::NotInitialized { .. } => {
                return Ok(StopReport {
                    status,
                    already_stopped: false,
                })
            }
            LifecycleStatus::Running { .. } | LifecycleStatus::Unresponsive { .. } => {}
        }

        if Instant::now() >= deadline {
            if opts.force {
                // Re-confirm the LIVE pid + uds path right before
                // signalling. The pid captured at the start of stop()
                // may already be stale if the daemon restarted during
                // the graceful-stop window. Fail closed if reconfirm
                // fails or if the live daemon does not report a pid.
                let (pid, _) = reconfirm_live_pid(env).await?;
                force_kill(pid)?;
                continue;
            }
            bail!("timed out waiting for graceful shutdown; try: ryeos stop --force");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Re-probe `lifecycle.status` against the currently live UDS and
/// return its `(pid, uds_path)`. Fails when no live daemon responds,
/// when the live daemon does not report a pid, or when both candidate
/// UDS paths refuse the RPC.
async fn reconfirm_live_pid(env: &LocalLifecycleEnv) -> Result<(u32, PathBuf)> {
    let timeout = env.rpc_timeout();
    for candidate in env.uds_candidates() {
        let value =
            match crate::control::call(&candidate, "lifecycle.status", json!({}), timeout).await {
                Ok(value) => value,
                Err(_) => continue,
            };

        // Fail-closed mirror of status.rs's guard: a successful RPC
        // must explicitly report "running" before we trust the pid.
        // Otherwise we could SIGTERM based on a deceptive or off-spec
        // response.
        let reports_running = value
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s == "running")
            .unwrap_or(false);
        if !reports_running {
            continue;
        }

        let pid = value.get("pid").and_then(|v| v.as_u64()).map(|v| v as u32);
        let live_uds = value
            .get("uds_path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| candidate.clone());
        return match pid {
            Some(pid) => Ok((pid, live_uds)),
            None => Err(anyhow::anyhow!(
                "cannot force stop: live daemon at {} did not report a pid",
                live_uds.display()
            )),
        };
    }
    Err(anyhow::anyhow!(
        "cannot force stop: no daemon responded to live status reconfirmation"
    ))
}

fn force_kill(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        // Fail-closed PID verification: if we cannot positively confirm
        // the target pid is a `ryeosd`, do not signal it.
        verify_expected_ryeosd_pid(pid)?;
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            // ESRCH: pid is gone — benign. The daemon may have just
            // exited gracefully between reconfirm and signal; let the
            // caller loop re-probe status.
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        bail!("force stop is not supported on this platform")
    }
}

#[cfg(unix)]
fn verify_expected_ryeosd_pid(pid: u32) -> Result<()> {
    let comm_path = std::path::PathBuf::from(format!("/proc/{pid}/comm"));
    if let Ok(comm) = std::fs::read_to_string(&comm_path) {
        let comm = comm.trim();
        if comm != "ryeosd" {
            bail!("refusing force stop: pid {pid} is '{comm}', not ryeosd")
        }
        return Ok(());
    }

    let exe_path = std::path::PathBuf::from(format!("/proc/{pid}/exe"));
    match std::fs::read_link(&exe_path) {
        Ok(exe) => {
            if exe.file_name().and_then(|name| name.to_str()) != Some("ryeosd") {
                bail!(
                    "refusing force stop: pid {pid} executable is {}, not ryeosd",
                    exe.display()
                )
            }
            Ok(())
        }
        Err(err) => bail!(
            "refusing force stop: cannot verify pid {pid} is ryeosd ({err}); \
             /proc/<pid>/comm and /proc/<pid>/exe both unavailable"
        ),
    }
}
