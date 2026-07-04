use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::status::{is_running, LifecycleStatus};
use crate::LocalLifecycleEnv;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartReport {
    pub status: LifecycleStatus,
    pub already_running: bool,
}

pub async fn start(env: &LocalLifecycleEnv, timeout: Duration) -> Result<StartReport> {
    let config = env.config();
    let deadline = Instant::now() + timeout;

    match crate::status::status(env).await? {
        LifecycleStatus::NotInitialized { .. } => {
            bail!("RyeOS is not initialized. Run: ryeos init")
        }
        status @ LifecycleStatus::Running { .. } => {
            return Ok(StartReport {
                status,
                already_running: true,
            })
        }
        LifecycleStatus::Stopped { .. } | LifecycleStatus::Stale { .. } => {}
        LifecycleStatus::Unresponsive { diagnostics, .. } => {
            // A busy daemon is still a daemon — starting a second one here
            // would double-run against the same state.
            bail!(
                "a daemon appears to be running but did not answer the control probe \
                 ({}); retry shortly, or `ryeos stop` it first",
                diagnostics.message
            )
        }
    }

    let _start_lock = loop {
        match env.try_acquire_start_lock() {
            Ok(lock) => break lock,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                // Another `ryeos start` is in flight; let it converge.
                let status = crate::status::status(env).await?;
                if is_running(&status) {
                    return Ok(StartReport {
                        status,
                        already_running: true,
                    });
                }
                if Instant::now() >= deadline {
                    bail!("timed out waiting for concurrent RyeOS daemon start");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(err) => return Err(err).context("acquire lifecycle start lock"),
        }
    };

    let ryeosd = resolve_ryeosd();
    let (stderr_log_path, stderr_log_start, stderr_log) = open_startup_stderr_log(env)?;
    let mut child = Command::new(&ryeosd)
        .arg("--app-root")
        .arg(&config.app_root)
        .arg("--bind")
        .arg(config.bind.to_string())
        .arg("--uds-path")
        .arg(&config.uds_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .with_context(|| format!("spawn {}", ryeosd.display()))?;

    loop {
        let status = crate::status::status(env).await?;
        if is_running(&status) {
            return Ok(StartReport {
                status,
                already_running: false,
            });
        }

        if let Some(exit) = child.try_wait().context("poll spawned ryeosd")? {
            // One last re-probe: a concurrent starter may have won and
            // our child may have exited because the lock was held by a
            // sibling that became Running.
            let status = crate::status::status(env).await?;
            if is_running(&status) {
                return Ok(StartReport {
                    status,
                    already_running: false,
                });
            }
            // Child is gone and no live daemon is visible. Surface the
            // failure immediately rather than wedging concurrent
            // starters behind the start lock until the deadline.
            let stderr = read_startup_stderr_since(&stderr_log_path, stderr_log_start);
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
            if is_running(&status) {
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

/// RAII guard for the lifecycle start lock.
///
/// Backed by an OS-level `flock(LOCK_EX | LOCK_NB)`. The lock is
/// released by closing the file descriptor, so an abrupt process exit
/// (crash, SIGKILL) cannot wedge subsequent starts the way a sentinel
/// file would.
pub struct LifecycleStartLock {
    _file: File,
}

impl std::fmt::Debug for LifecycleStartLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecycleStartLock").finish_non_exhaustive()
    }
}

impl LifecycleStartLock {
    pub fn try_acquire(app_root: &Path) -> io::Result<Self> {
        let dir = app_root.join(ryeos_engine::AI_DIR).join("state");
        fs::create_dir_all(&dir)?;
        let path = dir.join("lifecycle-start.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        flock_exclusive_nb(&file)?;
        // Record holder PID for diagnostics; ignore errors.
        #[cfg(unix)]
        {
            use std::io::Write;
            let _ = (&file).set_len(0);
            let _ = writeln!(&file, "{}", std::process::id());
        }
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn flock_exclusive_nb(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, err));
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(not(unix))]
fn flock_exclusive_nb(_file: &File) -> io::Result<()> {
    Ok(())
}

fn resolve_ryeosd() -> PathBuf {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let sibling = dir.join("ryeosd");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("ryeosd")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_lock_is_exclusive_and_self_releasing() {
        let tmp = tempfile::tempdir().unwrap();
        let first = LifecycleStartLock::try_acquire(tmp.path()).unwrap();
        let err = LifecycleStartLock::try_acquire(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(first);
        // Re-acquisition succeeds once dropped.
        let _again = LifecycleStartLock::try_acquire(tmp.path()).unwrap();
    }
}
