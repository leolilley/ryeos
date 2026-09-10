use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::status::{LifecycleStatus, is_ready};
use crate::{LifecycleProgressObserver, LocalLifecycleEnv};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartReport {
    pub status: LifecycleStatus,
    pub already_running: bool,
}

/// Optional bootstrap endpoint selection attached to one start operation.
/// A differing value is durably published while the node is stopped before
/// either direct or supervised launch; it is never a transient child argument.
#[derive(Debug, Clone, Default)]
pub struct StartEndpointConfiguration {
    pub bind: Option<SocketAddr>,
    pub uds_path: Option<PathBuf>,
}

pub async fn start(env: &LocalLifecycleEnv, timeout: Duration) -> Result<StartReport> {
    start_with_endpoint_configuration(env, timeout, StartEndpointConfiguration::default(), None)
        .await
}

pub async fn start_with_progress(
    env: &LocalLifecycleEnv,
    timeout: Duration,
    observer: Option<&mut dyn LifecycleProgressObserver>,
) -> Result<StartReport> {
    start_with_endpoint_configuration(
        env,
        timeout,
        StartEndpointConfiguration::default(),
        observer,
    )
    .await
}

pub async fn start_with_endpoint_configuration(
    env: &LocalLifecycleEnv,
    timeout: Duration,
    endpoints: StartEndpointConfiguration,
    mut observer: Option<&mut dyn LifecycleProgressObserver>,
) -> Result<StartReport> {
    let app_root = env.config().app_root.clone();
    crate::init_check::require_initialized(&app_root)?;
    let deadline = Instant::now() + timeout;
    let mut start_lock = Some(loop {
        match env.try_acquire_start_lock()? {
            Some(lock) => break lock,
            None => {
                // Never return success merely because another operation's
                // daemon is live: supervised Up intent still needs this gate.
                let status = crate::status::status(env).await?;
                observe(&mut observer, &status);
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the active node lifecycle operation");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    });

    // Configuration may have changed while this caller waited for the
    // lifecycle lock. Reload it only after winning that lock; this snapshot is
    // the sole basis for endpoint comparison, status and launch.
    let locked_env =
        LocalLifecycleEnv::from_config(crate::NodeConfig::load_local(Some(app_root.clone()))?);
    let env = &locked_env;
    let original_config = env.config();

    // Resolve launch ownership only after acquiring the same lifecycle lock
    // retained by host association publication. Otherwise a start that first
    // observed no association could wait behind setup and then incorrectly
    // launch directly after setup completed. Discovery/control errors remain
    // terminal and never become permission to fall back to direct spawning.
    let service = crate::supervision::InstalledService::discover(original_config)?;
    if let Some(service) = &service {
        service.check_start_allowed()?;
    }

    let mut initial = crate::status::status(env).await?;
    observe(&mut observer, &initial);
    let changes_endpoint = endpoints
        .bind
        .is_some_and(|bind| bind != original_config.bind)
        || endpoints
            .uds_path
            .as_ref()
            .is_some_and(|path| path != &original_config.uds_path);
    let configured_env = if changes_endpoint {
        if !matches!(
            initial,
            LifecycleStatus::Stopped { .. } | LifecycleStatus::Failed { .. }
        ) {
            bail!("stop the node before changing its configured endpoints");
        }
        if let Some(service) = &service {
            // Failed supervised starts may still have Up intent or a process
            // in its bounded diagnostics grace. Establish Down before waiting
            // for the exact state authority used to publish the replacement.
            service.request_down()?;
        }
        let config = crate::supervision::configure_host_endpoints_while_locked(
            &app_root,
            endpoints.bind,
            endpoints.uds_path,
            start_lock
                .as_ref()
                .expect("endpoint configuration precedes lifecycle-lock release"),
        )?;
        let configured = LocalLifecycleEnv::from_config(config);
        initial = crate::status::status(&configured).await?;
        observe(&mut observer, &initial);
        Some(configured)
    } else {
        None
    };
    let env = configured_env.as_ref().unwrap_or(env);
    let config = env.config();
    // Retained failure is diagnostic history, not authority to prevent an
    // explicit retry. Only a different failure observed after Up belongs to
    // this start operation.
    let initial_failure = startup_failure_key(&initial);
    let initial_host_failure = match &service {
        Some(service) => read_host_launch_failure_or_inhibit(service)?,
        None => None,
    };
    if matches!(
        initial,
        LifecycleStatus::NotInitialized { .. } | LifecycleStatus::Unresponsive { .. }
    ) {
        bail!("node is uninitialized or its live control socket is unusable; refusing launch");
    }
    // Establish intent under the lifecycle gate even for an already-running
    // daemon. A readiness observation must not bypass a concurrent stop's Down.
    if let Some(service) = &service {
        service.request_up()?;
    }
    let already_running = matches!(
        initial,
        LifecycleStatus::Running { .. } | LifecycleStatus::Starting { .. }
    );
    if is_ready(&initial) {
        return Ok(StartReport {
            status: initial,
            already_running: true,
        });
    }
    release_launch_lock_after_ownership(&mut start_lock, &initial);

    // The same readiness loop serves native supervisors. Do not invent a
    // per-manager ready protocol or interpret successful `up` as node readiness.
    let mut direct = if service.is_some() || already_running {
        None
    } else {
        let ryeosd = resolve_ryeosd();
        let (stderr_log_path, stderr_log_start, stderr_log) = open_startup_stderr_log(env)?;
        let child = Command::new(&ryeosd)
            .arg("--app-root")
            .arg(&config.app_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_log))
            .spawn()
            .with_context(|| format!("spawn {}", ryeosd.display()))?;
        Some((child, stderr_log_path, stderr_log_start))
    };

    // Projection recovery can take minutes, but the lifecycle socket remains
    // live throughout. Publish each structured phase/counter snapshot to the
    // caller-owned observer.
    loop {
        let status = crate::status::status(env).await?;
        release_launch_lock_after_ownership(&mut start_lock, &status);
        observe(&mut observer, &status);
        if is_ready(&status) {
            return Ok(StartReport {
                status,
                already_running,
            });
        }
        if let Some(message) = startup_failure_message(&status)
            && startup_failure_key(&status) != initial_failure
        {
            if let Some(service) = &service {
                service
                    .request_down()
                    .context("inhibit supervisor after terminal startup failure")?;
            }
            if let Some((_, stderr_log_path, _)) = &direct {
                bail!(
                    "{message}\nstartup stderr log: {}",
                    stderr_log_path.display()
                );
            }
            bail!("{message}");
        }
        if let Some(service) = &service
            && let Some(failure) = read_host_launch_failure_or_inhibit(service)?
            && Some(&failure) != initial_host_failure.as_ref()
        {
            service
                .request_down()
                .context("inhibit supervisor after host-service launch failure")?;
            bail!(
                "host-service launch failed (pid {}) before node lifecycle control: {}",
                failure.pid,
                failure.error
            );
        }

        if let Some((child, stderr_log_path, stderr_log_start)) = &mut direct
            && let Some(exit) = child.try_wait().context("poll spawned ryeosd")?
        {
            // One last re-probe: a concurrent starter may have won and
            // our child may have exited because the lock was held by a
            // sibling that became Running.
            let status = crate::status::status(env).await?;
            if is_ready(&status) {
                return Ok(StartReport {
                    status,
                    already_running: false,
                });
            }
            // Child is gone and no live daemon is visible. Surface the
            // failure immediately rather than wedging concurrent
            // starters behind the start lock until the deadline.
            let stderr = read_startup_stderr_since(stderr_log_path, *stderr_log_start);
            if stderr.trim().is_empty() {
                bail!(
                    "ryeosd exited before lifecycle readiness: {exit}\nstartup stderr log: {}",
                    stderr_log_path.display()
                );
            }
            bail!(
                "ryeosd exited before lifecycle readiness: {exit}\nstartup stderr log: {}\nstderr tail:\n{stderr}",
                stderr_log_path.display()
            );
        }

        if Instant::now() >= deadline {
            let status = crate::status::status(env).await?;
            if is_ready(&status) {
                return Ok(StartReport {
                    status,
                    already_running: false,
                });
            }
            bail!("timed out waiting for RyeOS daemon lifecycle readiness");
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn read_host_launch_failure_or_inhibit(
    service: &crate::supervision::InstalledService,
) -> Result<Option<crate::supervision::HostLaunchFailure>> {
    match service.launch_failure() {
        Ok(failure) => Ok(failure),
        Err(error) => {
            if let Err(inhibit) = service.request_down() {
                bail!(
                    "host launch testimony is invalid: {error:#}; supervisor inhibition also failed: {inhibit:#}"
                );
            }
            Err(error).context("host launch testimony is invalid; supervisor was inhibited")
        }
    }
}

/// Keep concurrent starters excluded only until process ownership is visible.
/// Once Starting is authoritative, another starter joins it; retaining the lock
/// through slow recovery would prevent stop --force from cancelling that boot.
fn release_launch_lock_after_ownership(
    lock: &mut Option<LifecycleStartLock>,
    status: &LifecycleStatus,
) {
    if matches!(
        status,
        LifecycleStatus::Starting { .. }
            | LifecycleStatus::Running { .. }
            | LifecycleStatus::Unresponsive { .. }
    ) {
        drop(lock.take());
    }
}

fn observe(observer: &mut Option<&mut dyn LifecycleProgressObserver>, status: &LifecycleStatus) {
    if let Some(observer) = observer.as_deref_mut() {
        observer.observe(status);
    }
}

fn startup_failure_message(status: &LifecycleStatus) -> Option<String> {
    let LifecycleStatus::Failed { metadata, startup } = status else {
        return None;
    };
    Some(format!(
        "ryeosd startup failed{} during {}: {}",
        metadata
            .pid
            .map(|pid| format!(" (pid {pid})"))
            .unwrap_or_default(),
        startup.phase.as_str(),
        startup
            .error
            .as_deref()
            .unwrap_or("unknown startup failure"),
    ))
}

fn startup_failure_key(status: &LifecycleStatus) -> Option<(Option<u32>, String, String)> {
    let LifecycleStatus::Failed { metadata, startup } = status else {
        return None;
    };
    Some((
        metadata.pid,
        startup.started_at.clone(),
        startup.updated_at.clone(),
    ))
}

fn startup_stderr_log_path(env: &LocalLifecycleEnv) -> PathBuf {
    env.config()
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("ryeosd-start.stderr.log")
}

fn open_startup_stderr_log(env: &LocalLifecycleEnv) -> Result<(PathBuf, u64, File)> {
    let path = startup_stderr_log_path(env);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let start_len = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open startup stderr log {}", path.display()))?;
    Ok((path, start_len, file))
}

fn read_startup_stderr_since(path: &Path, offset: u64) -> String {
    const MAX_TAIL_BYTES: usize = 8192;

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) => return format!("<failed to open startup stderr log: {err}>"),
    };
    if let Err(err) = file.seek(SeekFrom::Start(offset)) {
        return format!("<failed to seek startup stderr log: {err}>");
    }
    let mut bytes = Vec::new();
    if let Err(err) = file.read_to_end(&mut bytes) {
        return format!("<failed to read startup stderr log: {err}>");
    }
    if bytes.len() > MAX_TAIL_BYTES {
        bytes = bytes[bytes.len() - MAX_TAIL_BYTES..].to_vec();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// RAII guard for the lifecycle operation lock. Lillux owns the pinned native
/// descriptor and platform locking mechanics; RyeOS observes only acquisition
/// or ordinary contention. Abrupt process exit releases the lease, so a crash
/// cannot wedge a subsequent lifecycle operation through a sentinel file.
pub struct LifecycleStartLock {
    state_directory: lillux::PinnedDirectory,
    _guard: lillux::PinnedDirectoryLock,
}

impl std::fmt::Debug for LifecycleStartLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleStartLock").finish_non_exhaustive()
    }
}

impl LifecycleStartLock {
    pub fn try_acquire(app_root: &Path) -> Result<Option<Self>> {
        let daemon_state = lillux::PinnedDirectory::open(app_root)?
            .context("open lifecycle app root")?
            .open_or_create_child(OsStr::new(ryeos_engine::AI_DIR), 0o700)?
            .open_or_create_child(OsStr::new("state"), 0o700)?
            .open_or_create_child(OsStr::new(ryeos_engine::roots::DAEMON_STATE_DIR), 0o700)?;
        let root = daemon_state
            .open_or_create_child(OsStr::new(ryeos_engine::roots::DAEMON_LIFECYCLE_DIR), 0o700)?;
        let Some(guard) = root.try_lock_exclusive()? else {
            return Ok(None);
        };
        Ok(Some(Self {
            state_directory: root,
            _guard: guard,
        }))
    }

    pub(crate) fn ensure_protects_app_root(&self, app_root: &Path) -> Result<()> {
        self.state_directory.ensure_path_binding()?;
        let expected =
            ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).daemon_lifecycle();
        if self.state_directory.path() != expected {
            bail!("lifecycle lock belongs to another app root");
        }
        Ok(())
    }
}

fn resolve_ryeosd() -> PathBuf {
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join("ryeosd");
        if sibling.exists() {
            return sibling;
        }
    }
    PathBuf::from("ryeosd")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_observation_releases_launch_gate_before_ready_so_stop_can_enter() {
        let root = tempfile::tempdir().unwrap();
        let mut lock = Some(
            LifecycleStartLock::try_acquire(root.path())
                .unwrap()
                .expect("initial lifecycle lock is available"),
        );
        release_launch_lock_after_ownership(
            &mut lock,
            &LifecycleStatus::Stopped {
                app_root: root.path().to_owned(),
            },
        );
        assert!(
            LifecycleStartLock::try_acquire(root.path())
                .unwrap()
                .is_none(),
            "pre-marker launch must remain serialized"
        );
        let retained_failure = LifecycleStatus::Failed {
            metadata: crate::DaemonMetadata {
                pid: Some(41),
                bind: None,
                uds_path: None,
                started_at: None,
                version: None,
                revision: None,
                build_date: None,
                app_root: root.path().to_owned(),
            },
            startup: crate::StartupSnapshot::failed_before_control(
                "2026-09-10T00:00:00Z",
                "2026-09-10T00:00:01Z",
                "old failure",
            ),
        };
        release_launch_lock_after_ownership(&mut lock, &retained_failure);
        assert!(
            LifecycleStartLock::try_acquire(root.path())
                .unwrap()
                .is_none(),
            "retained failure must not release a new start operation's gate"
        );
        let starting = LifecycleStatus::Starting {
            metadata: crate::DaemonMetadata {
                pid: Some(42),
                bind: None,
                uds_path: None,
                started_at: None,
                version: None,
                revision: None,
                build_date: None,
                app_root: root.path().to_owned(),
            },
            startup: crate::StartupSnapshot::bootstrapping("2026-09-10T00:00:00Z"),
            control_available: true,
        };
        assert!(!is_ready(&starting));
        release_launch_lock_after_ownership(&mut lock, &starting);
        let _stop_lock = LifecycleStartLock::try_acquire(root.path())
            .unwrap()
            .expect("stop must not wait for startup readiness");
        assert!(lock.is_none());
    }

    #[test]
    fn start_lock_is_exclusive_and_self_releasing() {
        let tmp = tempfile::tempdir().unwrap();
        let first = LifecycleStartLock::try_acquire(tmp.path())
            .unwrap()
            .expect("first lifecycle lock is available");
        assert!(
            LifecycleStartLock::try_acquire(tmp.path())
                .unwrap()
                .is_none()
        );
        drop(first);
        // Re-acquisition succeeds once dropped.
        let _again = LifecycleStartLock::try_acquire(tmp.path())
            .unwrap()
            .expect("released lifecycle lock is available");
    }

    #[test]
    fn live_runtime_state_lock_does_not_block_lifecycle_operations() {
        let root = tempfile::tempdir().unwrap();
        let state = lillux::PinnedDirectory::open_or_create(
            &ryeos_engine::roots::RuntimeRoot::new(root.path().to_path_buf()).state(),
        )
        .unwrap();
        let _runtime_lock = state.lock_exclusive().unwrap();
        let _lifecycle = LifecycleStartLock::try_acquire(root.path())
            .unwrap()
            .expect("live runtime authority must not own the lifecycle-operation lock");
    }
}
