use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
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
    let mut child = Command::new(&ryeosd)
        .arg("--system-space-dir")
        .arg(&config.system_space_dir)
        .arg("--bind")
        .arg(config.bind.to_string())
        .arg("--uds-path")
        .arg(&config.uds_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", ryeosd.display()))?;

    let mut child_stderr = child.stderr.take();
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
            let stderr = read_child_stderr(&mut child_stderr).await;
            if stderr.trim().is_empty() {
                bail!("ryeosd exited before lifecycle readiness: {exit}");
            }
            bail!("ryeosd exited before lifecycle readiness: {exit}\nstderr:\n{stderr}");
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

async fn read_child_stderr(stderr: &mut Option<tokio::process::ChildStderr>) -> String {
    let Some(mut stderr) = stderr.take() else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr.read_to_string(&mut buf)).await;
    buf
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
    pub fn try_acquire(system_space_dir: &Path) -> io::Result<Self> {
        let dir = system_space_dir.join(ryeos_engine::AI_DIR).join("state");
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
