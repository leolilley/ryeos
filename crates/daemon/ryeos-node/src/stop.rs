use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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

/// Inhibit an already configured native service when complete bootstrap
/// configuration is unreadable. Without endpoints, only the protected host
/// association owns a safe process transition; a direct node must be repaired
/// before it can be authenticated and stopped.
pub async fn stop_supervised_with_invalid_config(
    app_root: &std::path::Path,
    timeout: Duration,
) -> Result<StopReport> {
    crate::init_check::require_initialized(app_root)?;
    let deadline = Instant::now() + timeout;
    let _lifecycle_lock = loop {
        match crate::LifecycleStartLock::try_acquire(app_root)? {
            Some(lock) => break lock,
            None if Instant::now() >= deadline => {
                bail!("timed out waiting for the active node lifecycle operation")
            }
            None => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    };
    let service = crate::supervision::InstalledService::discover_app_root(app_root)?.context(
        "node configuration is invalid and no protected host association exists; refusing to guess a direct daemon endpoint",
    )?;
    service.check_supervisor()?;
    service.request_down()?;
    Ok(StopReport {
        status: LifecycleStatus::Stopped {
            app_root: app_root.to_path_buf(),
        },
        already_stopped: false,
    })
}

pub async fn stop_with_progress(
    env: &LocalLifecycleEnv,
    opts: StopOptions,
    mut observer: Option<&mut dyn LifecycleProgressObserver>,
) -> Result<StopReport> {
    crate::init_check::require_initialized(&env.config().app_root)?;
    // Serialize start and stop through the existing node lifecycle lock. A
    // native supervisor still needs durable down intent before daemon exit;
    // this lock alone cannot inhibit its independent restart machinery.
    let lock_deadline = Instant::now() + opts.timeout;
    let _lifecycle_lock = loop {
        match env.try_acquire_start_lock()? {
            Some(lock) => break lock,
            None => {
                if Instant::now() >= lock_deadline {
                    bail!("timed out waiting for the active node lifecycle operation");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    let service = crate::supervision::InstalledService::discover(env.config())?;
    if let Some(service) = &service {
        service.check_supervisor()?;
    }
    let initial = crate::status::status(env).await?;
    observe(&mut observer, &initial);
    let pre_pinned_target = if matches!(initial, LifecycleStatus::Failed { .. }) {
        if let Some(service) = &service {
            service.request_down()?;
        }
        // A pre-control failure has already exited and therefore has no peer
        // to pin. Down intent is sufficient to inhibit another supervisor
        // attempt; retained failure testimony remains available for diagnosis.
        match pin_live_daemon(env).await {
            Ok(target) => Some(target),
            Err(_) => {
                return Ok(StopReport {
                    status: LifecycleStatus::Stopped {
                        app_root: env.config().app_root.clone(),
                    },
                    already_stopped: false,
                });
            }
        }
    } else {
        None
    };
    match initial {
        LifecycleStatus::NotInitialized { .. } => {
            bail!("RyeOS is not initialized. Run: ryeos init")
        }
        status @ LifecycleStatus::Stopped { .. } => {
            if let Some(service) = &service {
                service.request_down()?;
            }
            return Ok(StopReport {
                status,
                already_stopped: true,
            });
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
            let Some(service) = &service else {
                // A directly launched pre-control daemon has no authenticated
                // peer and no native supervisor authority. Its diagnostic PID
                // must never be promoted into signal authority.
                bail!(
                    "a directly launched daemon (pid {}) is starting but its control socket is not available yet; wait briefly, then retry stop",
                    metadata.pid.unwrap_or_default(),
                )
            };
            // The configured native controller owns this exact service
            // process and its restart disposition. Its successful Down
            // transition is sufficient to cancel pre-control startup without
            // trusting a marker PID. Worker-tree settlement remains a
            // separate retained scope-recovery obligation.
            service.request_down()?;
            return Ok(StopReport {
                status: LifecycleStatus::Stopped {
                    app_root: env.config().app_root.clone(),
                },
                already_stopped: false,
            });
        }
    }

    // Runtime isolationes receive the callback UDS, so privileged lifecycle
    // control must never be routed over that socket. Signal the positively
    // identified local daemon instead; SIGTERM enters the same graceful
    // shutdown coordinator as Ctrl-C.
    let target = match pre_pinned_target {
        Some(target) => target,
        None => pin_live_daemon(env).await?,
    };
    if let Some(service) = &service {
        // Pin first: native down may make the authenticated socket disappear.
        // Its durable intent prevents a later service restart from launching.
        service.request_down()?;
    }
    target.request_termination()?;

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
            if !matches!(
                status,
                LifecycleStatus::Stopped { .. } | LifecycleStatus::Stale { .. }
            ) {
                bail!(
                    "the pinned daemon exited but another node process is visible; refusing successful stop"
                );
            }
            // This is daemon-exit evidence, not worker-tree settlement. Forced
            // shutdown must leave existing scope recovery obligations intact.
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
                target.force_termination()?;
                forced = true;
                // SIGKILL is definitive process authority, but a task leaving
                // uninterruptible filesystem I/O may not become pidfd-readable
                // within an arbitrary two-second grace. Reuse the caller's
                // already-bounded stop timeout for the exact pinned process;
                // this changes neither the target nor the escalation policy.
                deadline = Instant::now() + opts.timeout;
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
pub(crate) struct LiveDaemonTarget {
    pid: u32,
    peer: lillux::local_ipc::AuthenticatedUnixPeer,
}

impl LiveDaemonTarget {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    fn request_termination(&self) -> Result<()> {
        self.peer
            .request_termination()
            .with_context(|| format!("terminate pinned ryeosd pid {}", self.pid))
    }

    fn force_termination(&self) -> Result<()> {
        self.peer
            .force_termination()
            .with_context(|| format!("force terminate pinned ryeosd pid {}", self.pid))
    }

    fn has_exited(&self) -> Result<bool> {
        self.peer.has_exited()
    }

    pub(crate) fn executable_digest_exact(&self, expected_bytes: u64) -> Result<String> {
        self.peer.executable_digest_exact(expected_bytes)
    }
}
pub(crate) async fn pin_live_daemon(env: &LocalLifecycleEnv) -> Result<LiveDaemonTarget> {
    let timeout = env.rpc_timeout();
    for candidate in env.uds_candidates() {
        let pinned = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || {
                let stream = lillux::LocalDuplexStream::connect(&candidate)?;
                pin_verified_ryeosd_peer(&stream)
            }),
        )
        .await
        .ok()
        .and_then(Result::ok);
        let Some(pinned) = pinned else {
            continue;
        };
        return Ok(pinned?);
    }
    Err(anyhow::anyhow!(
        "cannot stop: no configured socket had a verifiable live ryeosd peer"
    ))
}

fn pin_verified_ryeosd_peer(stream: &lillux::LocalDuplexStream) -> Result<LiveDaemonTarget> {
    let peer = stream.authenticated_peer()?;
    let pid = u32::try_from(peer.pid()).context("invalid daemon peer PID")?;
    peer.require_executable_name(std::ffi::OsStr::new("ryeosd"))?;
    Ok(LiveDaemonTarget { pid, peer })
}
