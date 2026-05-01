use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

pub fn remove_stale_socket(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
    }
    Ok(())
}

/// Check if a process group is alive.
pub fn pgid_alive(pgid: i64) -> bool {
    // kill(0, -pgid) checks if any process in the group exists
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

/// Return the daemon's own process group ID.
pub fn daemon_pgid() -> i64 {
    unsafe { libc::getpgid(0) as i64 }
}

/// Send SIGTERM to a process group, wait for grace period, then SIGKILL if needed.
pub fn kill_process_group(pgid: i64, grace: Duration) -> KillResult {
    let neg_pgid = -(pgid as i32);

    // SIGTERM the entire process group
    let term_result = unsafe { libc::kill(neg_pgid, libc::SIGTERM) };
    if term_result != 0 {
        let errno = std::io::Error::last_os_error();
        if errno.raw_os_error() == Some(libc::ESRCH) {
            return KillResult {
                success: true,
                method: "already_dead",
            };
        }
        return KillResult {
            success: false,
            method: "sigterm_failed",
        };
    }

    // Poll for death within grace period
    let deadline = std::time::Instant::now() + grace;
    let poll_interval = Duration::from_millis(100);
    while std::time::Instant::now() < deadline {
        if !pgid_alive(pgid) {
            return KillResult {
                success: true,
                method: "terminated",
            };
        }
        std::thread::sleep(poll_interval);
    }

    // Grace period expired — SIGKILL
    unsafe {
        libc::kill(neg_pgid, libc::SIGKILL);
    }

    // Brief wait for SIGKILL to take effect
    std::thread::sleep(Duration::from_millis(200));

    KillResult {
        success: !pgid_alive(pgid),
        method: "killed",
    }
}

pub struct KillResult {
    pub success: bool,
    pub method: &'static str,
}

/// SIGKILL the entire process group immediately — no SIGTERM, no grace.
///
/// Distinct from `kill_process_group(_, Duration::ZERO)` because the
/// latter still sends SIGTERM first; `Hard` cancellation mode requires
/// genuinely skipping that step.
pub fn hard_kill_process_group(pgid: i64) -> KillResult {
    let neg_pgid = -(pgid as i32);

    let kill_result = unsafe { libc::kill(neg_pgid, libc::SIGKILL) };
    if kill_result != 0 {
        let errno = std::io::Error::last_os_error();
        if errno.raw_os_error() == Some(libc::ESRCH) {
            return KillResult {
                success: true,
                method: "already_dead",
            };
        }
        return KillResult {
            success: false,
            method: "sigkill_failed",
        };
    }

    // Brief wait for SIGKILL to take effect.
    std::thread::sleep(Duration::from_millis(200));

    KillResult {
        success: !pgid_alive(pgid),
        method: "hard_killed",
    }
}

/// Allowlist of env keys the daemon propagates to subprocesses.
/// Plus declared secrets injected separately by the dispatch path.
const SPAWN_ENV_ALLOWLIST: &[&str] = &[
    "PATH",                  // libc/linker bootstrap
    "HOME",                  // libc/lib lookup
    "LANG", "LC_ALL", "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "USER_SPACE",            // root discovery (set by daemon)
    "RYE_SYSTEM_SPACE",      // root discovery (set by daemon)
    "RUST_LOG", "RUST_BACKTRACE",
    "RYEOSD_TEST_STDERR_DIR",
];

/// Build the env map for a daemon-spawned subprocess.
///
/// Composition:
///   * Allowlisted parent env, snapshotted at call time.
///   * Daemon-resolved roots (overrides allowlisted snapshot if
///     the daemon resolved a different value than the parent env
///     held).
///   * Declared secrets injected by the caller (e.g. dispatch from
///     `ItemMetadata.required_secrets`).
///
/// The result is meant to be passed verbatim into
/// `SubprocessRequest::envs`. After B's lillux contract change,
/// the subprocess sees ONLY these env entries — inherited parent
/// env is no longer available.
pub fn build_spawn_env(
    declared_secrets: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut env: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    for k in SPAWN_ENV_ALLOWLIST {
        if let Some(v) = std::env::var_os(k) {
            env.insert((*k).to_string(), v.to_string_lossy().into_owned());
        }
    }

    // Daemon-resolved roots override whatever the parent env held.
    let user_root = ryeos_engine::roots::user_root()
        .context("resolve user root for subprocess env")?;
    env.insert("USER_SPACE".to_string(), user_root.display().to_string());

    for (k, v) in declared_secrets {
        env.insert(k.clone(), v.clone());
    }

    Ok(env.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    /// Spawn a shell that:
    ///   1. Installs a SIGTERM trap which writes a marker file then exits 0.
    ///   2. Sleeps long enough that, absent any signal, the test would time out.
    ///      Returns (child, pgid, marker_path).
    fn spawn_signal_target(tmp: &TempDir) -> (std::process::Child, i64, std::path::PathBuf) {
        let marker = tmp.path().join("got_term");
        let marker_str = marker.display().to_string();
        let script = format!(
            r#"trap 'echo term > "{m}"; exit 0' TERM; (while true; do sleep 0.05; done)"#,
            m = marker_str
        );
        let child = unsafe {
            // process_group(0) starts the child in its own new process group
            // whose PGID equals its PID — exactly what the daemon kills.
            Command::new("sh")
                .arg("-c")
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                })
                .spawn()
                .expect("spawn signal target")
        };
        let pgid = child.id() as i64;
        // Give the shell a moment to install the trap before signalling.
        std::thread::sleep(Duration::from_millis(150));
        (child, pgid, marker)
    }

    fn read_marker(path: &std::path::Path) -> Option<String> {
        let mut f = std::fs::File::open(path).ok()?;
        let mut s = String::new();
        f.read_to_string(&mut s).ok()?;
        Some(s)
    }

    /// Run the kill in a background thread while we concurrently
    /// `waitpid` the child in the foreground. Without this, the child
    /// becomes a zombie that `kill(0, -pgid)` still reports as alive,
    /// confusing the daemon's `pgid_alive` poll. (In production the
    /// reaper is the engine's spawn handle.)
    fn run_kill_with_reaper<F: FnOnce() -> KillResult + Send + 'static>(
        kill: F,
        child: &mut std::process::Child,
    ) -> (KillResult, std::process::ExitStatus) {
        let kill_handle = std::thread::spawn(kill);
        let status = child.wait().expect("wait child");
        let result = kill_handle.join().expect("kill thread join");
        (result, status)
    }

    #[test]
    fn graceful_kill_sends_sigterm_first_and_marker_is_written() {
        let tmp = TempDir::new().unwrap();
        let (mut child, pgid, marker) = spawn_signal_target(&tmp);

        let (_result, status) = run_kill_with_reaper(
            move || kill_process_group(pgid, Duration::from_secs(2)),
            &mut child,
        );

        // SIGTERM trap fired → marker present, child exited 0.
        assert!(
            marker.exists(),
            "marker file missing — SIGTERM handler did not run"
        );
        assert_eq!(read_marker(&marker).as_deref(), Some("term\n"));
        assert!(status.success(), "expected clean exit, got {:?}", status);
    }

    #[test]
    fn hard_kill_skips_sigterm_and_marker_is_not_written() {
        use std::os::unix::process::ExitStatusExt;
        let tmp = TempDir::new().unwrap();
        let (mut child, pgid, marker) = spawn_signal_target(&tmp);

        let (result, status) =
            run_kill_with_reaper(move || hard_kill_process_group(pgid), &mut child);

        // result.method comes from hard_kill_process_group — either it
        // completed in time (`hard_killed`) or the kernel reaped the
        // entry between SIGKILL and the poll (`already_dead`).
        assert!(
            matches!(result.method, "hard_killed" | "already_dead"),
            "unexpected method: {}",
            result.method
        );
        // SIGKILL is uncatchable → trap MUST NOT have run → no marker.
        assert!(
            !marker.exists(),
            "marker file present — SIGTERM handler ran when it should not have"
        );
        // Child terminated by signal (no clean exit code), and that
        // signal was SIGKILL — not SIGTERM.
        assert!(status.code().is_none(), "expected signal exit, got {:?}", status);
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "expected SIGKILL, got {:?}",
            status.signal()
        );
    }

    #[test]
    fn graceful_kill_with_stubborn_process_falls_back_to_sigkill() {
        use std::os::unix::process::ExitStatusExt;
        // Trap SIGTERM but do NOT exit — forces grace expiry → SIGKILL path.
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("got_term");
        let marker_str = marker.display().to_string();
        // No subshell — the parent sh installs the trap and loops
        // directly so SIGTERM is fully absorbed by the trap (which
        // intentionally does not exit). The kernel's SIGKILL is then
        // the only thing that can terminate the process group, which
        // is exactly the path under test.
        let script = format!(
            r#"trap 'echo term > "{m}"' TERM; while true; do sleep 0.05; done"#,
            m = marker_str
        );
        let mut child = unsafe {
            Command::new("sh")
                .arg("-c")
                .arg(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                })
                .spawn()
                .expect("spawn stubborn target")
        };
        let pgid = child.id() as i64;
        std::thread::sleep(Duration::from_millis(150));

        let (_result, status) = run_kill_with_reaper(
            move || kill_process_group(pgid, Duration::from_millis(300)),
            &mut child,
        );

        // SIGTERM did fire (trap ran) — confirms graceful path tried first…
        assert!(
            marker.exists(),
            "trap marker missing — SIGTERM was not sent before SIGKILL"
        );
        // …and SIGKILL ultimately delivered the death blow.
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "expected SIGKILL fallback, got {:?}",
            status.signal()
        );
    }

    #[test]
    fn graceful_kill_already_dead_pgid_reports_already_dead() {
        let tmp = TempDir::new().unwrap();
        let (mut child, pgid, _marker) = spawn_signal_target(&tmp);
        // Reap it ourselves so the PGID is gone.
        let _ = unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(50));

        let result = kill_process_group(pgid, Duration::from_millis(50));
        assert!(result.success);
        assert_eq!(result.method, "already_dead");
    }
}
