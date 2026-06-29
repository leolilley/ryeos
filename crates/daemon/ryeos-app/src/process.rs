use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::env_contract::{DaemonRootEnv, EnvContractBuilder, EnvSourceKind, BASE_ALLOWLIST_NAMES};

/// Poll interval (ms) when waiting for a process group to exit after SIGTERM.
const KILL_POLL_INTERVAL_MS: u64 = 100;

/// Wait time (ms) after SIGKILL before checking if the process group is dead.
const POST_SIGKILL_WAIT_MS: u64 = 200;

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

/// Resolve the process-group id for `pid`. Runtimes are `setsid` session
/// leaders (so this equals `pid`), but `getpgid` is the correct general
/// derivation. Returns `pid` if the lookup fails (e.g. the process already
/// exited) so callers still record a usable group id rather than 0.
pub fn pgid_of(pid: i64) -> i64 {
    let g = unsafe { libc::getpgid(pid as libc::pid_t) };
    if g > 0 {
        g as i64
    } else {
        pid
    }
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
    let poll_interval = Duration::from_millis(KILL_POLL_INTERVAL_MS);
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
    std::thread::sleep(Duration::from_millis(POST_SIGKILL_WAIT_MS));

    KillResult {
        success: !pgid_alive(pgid),
        method: "killed",
    }
}

pub struct KillResult {
    pub success: bool,
    pub method: &'static str,
}

/// Policy for shutting down a thread's process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAction {
    /// SIGKILL immediately — no SIGTERM, no grace period.
    Hard,
    /// SIGTERM first, then SIGKILL after `grace` if the group survives.
    Graceful(std::time::Duration),
}

/// Map a tool-declared `CancellationMode` to a `ShutdownAction`.
///
/// - `Some(Hard)` → SIGKILL only.
/// - `Some(Graceful { grace_secs })` → SIGTERM, wait, then SIGKILL.
/// - `None` → default 3-second graceful.
pub fn resolve_shutdown_action(
    mode: Option<ryeos_engine::contracts::CancellationMode>,
) -> ShutdownAction {
    use ryeos_engine::contracts::CancellationMode;
    match mode {
        Some(CancellationMode::Hard) => ShutdownAction::Hard,
        Some(CancellationMode::Graceful { grace_secs }) => {
            ShutdownAction::Graceful(std::time::Duration::from_secs(grace_secs))
        }
        None => ShutdownAction::Graceful(std::time::Duration::from_secs(3)),
    }
}

/// Kill a process group according to the given shutdown action.
///
/// Skips if `pgid` matches the daemon's own PGID (would suicide).
pub fn kill_by_action(pgid: i64, action: ShutdownAction) -> KillResult {
    if pgid == daemon_pgid() {
        return KillResult {
            success: false,
            method: "skipped_daemon_pgid",
        };
    }
    match action {
        ShutdownAction::Hard => hard_kill_process_group(pgid),
        ShutdownAction::Graceful(grace) => kill_process_group(pgid, grace),
    }
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
    std::thread::sleep(Duration::from_millis(POST_SIGKILL_WAIT_MS));

    KillResult {
        success: !pgid_alive(pgid),
        method: "hard_killed",
    }
}

/// Validate an env var name before injecting it as a declared secret.
///
/// Declared secrets are real subprocess env vars, so they must not be
/// allowed to shadow daemon/runtime control env, root discovery, proxy,
/// CA, logging, or other inherited infrastructure names. Ordinary
/// application secrets such as `SUPABASE_SERVICE_KEY` and
/// `OXYLABS_PASSWORD` remain valid.
pub fn validate_spawn_secret_name(name: &str) -> anyhow::Result<()> {
    crate::env_contract::validate_secret_name(name)
        .map_err(|e| anyhow::anyhow!("invalid subprocess secret env name `{name}`: {e:#}"))
}

fn host_env_snapshot_lossy() -> Vec<(String, String)> {
    std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn compatibility_daemon_roots() -> anyhow::Result<DaemonRootEnv> {
    Ok(DaemonRootEnv {
        app_root: std::env::var_os("RYEOS_APP_ROOT").map(|p| p.to_string_lossy().into_owned()),
    })
}

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
    build_spawn_env_with_roots(declared_secrets, compatibility_daemon_roots()?)
}

pub fn build_spawn_env_with_roots(
    declared_secrets: &std::collections::BTreeMap<String, String>,
    roots: DaemonRootEnv,
) -> anyhow::Result<Vec<(String, String)>> {
    let secret_bindings = declared_secrets
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()));
    Ok(EnvContractBuilder::new()
        .with_base_allowlist(host_env_snapshot_lossy())?
        .with_daemon_roots(roots)?
        .with_bindings(EnvSourceKind::DeclaredSecret, secret_bindings)?
        .build())
}

/// Build the env contract for a daemon-spawned subprocess that is
/// NOT a directive-runtime launch (those go through the model-target
/// preflight in `execution/launch.rs`).
///
/// Composition:
///   * allowlisted parent env (PATH/HOME/proxy/CA/...) via
///     `build_spawn_env`
///   * caller-supplied per-spawn env (e.g. `RYEOSD_THREAD_AUTH_TOKEN`)
///     — wins over allowlist
///
/// This helper deliberately does NOT consult the vault or auto-discover
/// provider secrets. Provider-secret injection is owned by directive
/// launch preflight, where the resolved provider id is known.
pub fn build_subprocess_envs(
    declared_secrets: &std::collections::BTreeMap<String, String>,
    per_spawn_env: &[(String, String)],
) -> anyhow::Result<Vec<(String, String)>> {
    build_subprocess_envs_with_roots(
        declared_secrets,
        per_spawn_env,
        compatibility_daemon_roots()?,
    )
}

pub fn build_subprocess_envs_with_roots(
    declared_secrets: &std::collections::BTreeMap<String, String>,
    per_spawn_env: &[(String, String)],
    roots: DaemonRootEnv,
) -> anyhow::Result<Vec<(String, String)>> {
    let secret_bindings = declared_secrets
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()));
    Ok(EnvContractBuilder::new()
        .with_base_allowlist(host_env_snapshot_lossy())?
        .with_daemon_roots(roots)?
        .with_bindings(EnvSourceKind::DeclaredSecret, secret_bindings)?
        .with_bindings(EnvSourceKind::PerSpawnDaemon, per_spawn_env.iter().cloned())?
        .build())
}

pub fn subprocess_base_allowlist_names() -> &'static [&'static str] {
    BASE_ALLOWLIST_NAMES
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

    #[test]
    fn spawn_secret_policy_allows_application_secret_names() {
        validate_spawn_secret_name("SUPABASE_SERVICE_KEY").unwrap();
        validate_spawn_secret_name("OXYLABS_PASSWORD").unwrap();
    }

    #[test]
    fn spawn_secret_policy_rejects_protected_names() {
        for name in [
            "RYEOS_APP_ROOT",
            "RYEOSD_THREAD_AUTH_TOKEN",
            "RYEOS_PROJECT_SECRET",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "SSL_CERT_FILE",
        ] {
            let err = validate_spawn_secret_name(name).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("invalid subprocess secret env name"),
                "expected protected-name rejection for {name}, got: {msg}"
            );
        }
    }

    #[test]
    fn build_spawn_env_rejects_protected_secret_collision() {
        let mut secrets = std::collections::BTreeMap::new();
        secrets.insert("RYEOS_APP_ROOT".to_string(), "/tmp/evil".to_string());

        let err = build_spawn_env(&secrets).unwrap_err();
        let msg = format!("{err:#}");

        assert!(msg.contains("RYEOS_APP_ROOT"), "got: {msg}");
        assert!(
            msg.contains("protected") || msg.contains("blocked") || msg.contains("invalid"),
            "got: {msg}"
        );
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
        assert!(
            status.code().is_none(),
            "expected signal exit, got {:?}",
            status
        );
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
