use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::status::LifecycleStatus;
use crate::{LifecycleProgressObserver, LocalLifecycleEnv};

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
    stop_with_progress(env, opts, None).await
}

pub async fn stop_with_progress(
    env: &LocalLifecycleEnv,
    opts: StopOptions,
    mut observer: Option<&mut dyn LifecycleProgressObserver>,
) -> Result<StopReport> {
    let initial = crate::status::status(env).await?;
    observe(&mut observer, &initial);
    match initial {
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
        LifecycleStatus::Running { .. } => {}
        // Busy-but-alive: proceed with the normal stop flow — the graceful
        // shutdown call may itself time out, after which the deadline/force
        // path below applies.
        LifecycleStatus::Unresponsive { .. } => {}
        LifecycleStatus::Starting {
            control_available: true,
            ..
        }
        | LifecycleStatus::Failed { .. } => {}
        LifecycleStatus::Starting { ref metadata, .. } => {
            // No authenticated daemon socket exists yet, so the signal path
            // cannot pin and verify the live peer. Booting clears on its own.
            bail!(
                "a daemon (pid {}) is starting but its control socket is not available yet; \
                 wait briefly, then retry stop",
                metadata.pid.unwrap_or_default(),
            )
        }
    }

    // Runtime isolationes receive the callback UDS, so privileged lifecycle
    // control must never be routed over that socket. Signal the positively
    // identified local daemon instead; SIGTERM enters the same graceful
    // shutdown coordinator as Ctrl-C.
    let target = pin_live_daemon(env).await?;
    target.signal(libc::SIGTERM)?;

    let mut deadline = Instant::now() + opts.timeout;
    let mut forced = false;
    loop {
        let status = crate::status::status(env).await?;
        observe(&mut observer, &status);

        // Socket and metadata cleanup happen before Tokio's blocking worker
        // pool has necessarily stopped. Lifecycle status is therefore useful
        // progress, but only the pidfd can prove the daemon process is gone.
        if target.has_exited()? {
            let status = crate::status::status(env).await?;
            observe(&mut observer, &status);
            return Ok(StopReport {
                status,
                already_stopped: false,
            });
        }

        if Instant::now() >= deadline {
            if opts.force && !forced {
                // The daemon normally removes its socket early in graceful
                // shutdown. Escalate through the pidfd captured before SIGTERM
                // so this can neither miss the old process nor hit a replacement.
                target.signal(libc::SIGKILL)?;
                forced = true;
                deadline = Instant::now() + Duration::from_secs(2);
                continue;
            }
            if forced {
                bail!("daemon remained live after pidfd SIGKILL escalation");
            }
            bail!("timed out waiting for graceful shutdown; try: ryeos stop --force");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn observe(observer: &mut Option<&mut dyn LifecycleProgressObserver>, status: &LifecycleStatus) {
    if let Some(observer) = observer.as_deref_mut() {
        observer.observe(status);
    }
}

/// Connect to the configured live control/callback socket, take the kernel's
/// peer PID (rather than trusting daemon.json or an RPC field), pin that exact
/// incarnation with a pidfd, verify it is ryeosd, and signal through the pidfd.
struct LiveDaemonTarget {
    pid: u32,
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
}

impl LiveDaemonTarget {
    fn signal(&self, signal: libc::c_int) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0u32,
                )
            };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error)
                        .with_context(|| format!("signal pinned ryeosd pid {}", self.pid));
                }
            }
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        bail!("pidfd lifecycle stop is not supported on this platform")
    }

    fn has_exited(&self) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            let mut pollfd = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
            if result < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("poll pinned ryeosd pid {}", self.pid));
            }
            Ok(result > 0)
        }

        #[cfg(not(target_os = "linux"))]
        bail!("pidfd lifecycle stop is not supported on this platform")
    }
}

async fn pin_live_daemon(env: &LocalLifecycleEnv) -> Result<LiveDaemonTarget> {
    let timeout = env.rpc_timeout();
    for candidate in env.uds_candidates() {
        let stream = match tokio::time::timeout(
            timeout,
            tokio::net::UnixStream::connect(&candidate),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            _ => continue,
        };
        let Some(pid) = stream
            .peer_cred()
            .context("read daemon socket peer credentials")?
            .pid()
        else {
            continue;
        };
        if let Ok(target) = pin_verified_ryeosd_peer(&stream, pid as u32) {
            return Ok(target);
        }
    }
    Err(anyhow::anyhow!(
        "cannot stop: no configured socket had a verifiable live ryeosd peer"
    ))
}

fn pin_verified_ryeosd_peer(stream: &tokio::net::UnixStream, pid: u32) -> Result<LiveDaemonTarget> {
    #[cfg(target_os = "linux")]
    {
        let mut raw_pidfd: libc::c_int = -1;
        let mut value_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut raw_pidfd as *mut libc::c_int).cast(),
                &mut value_len,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("capture daemon socket peer with SO_PEERPIDFD");
        }
        if value_len as usize != std::mem::size_of::<libc::c_int>() || raw_pidfd < 0 {
            bail!("SO_PEERPIDFD returned an invalid daemon descriptor");
        }
        // SAFETY: successful SO_PEERPIDFD installed a new owned descriptor.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw_pidfd) };
        verify_expected_ryeosd_pid(pid)?;
        Ok(LiveDaemonTarget { pid, pidfd })
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (stream, pid);
        bail!("pidfd lifecycle stop is not supported on this platform")
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
