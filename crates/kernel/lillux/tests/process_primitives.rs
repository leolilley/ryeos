//! Process-primitive hardening for `lillux::exec` (Unix).
//!
//! Covers the synchronous runner (`run`), the spawn/wait split
//! (`spawn` → `RunningProcess::wait`), detached spawn (`spawn_detached`),
//! liveness (`is_alive`), and termination (`kill`). The most load-bearing
//! contract exercised here is `env_clear`: `SubprocessRequest::envs` is
//! authoritative, so a variable exported into the parent must NOT leak
//! into the child — this is the secret-scoping guarantee documented on
//! `SubprocessRequest`.

#![cfg(unix)]

use lillux::{
    configure_subprocess_limits, is_alive, kill, run, spawn, spawn_detached,
    validate_subprocess_limits, SubprocessLimits, SubprocessRequest,
};

/// A `/bin/sh -c <args>` request with a generous default timeout and an
/// empty (authoritative) environment.
fn sh(args: &[&str]) -> SubprocessRequest {
    SubprocessRequest {
        cmd: "/bin/sh".to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        envs: vec![],
        stdin_data: None,
        timeout: 30.0,
        limits: None,
    }
}

/// A PATH sufficient for the child to resolve coreutils (`sleep`, `cat`).
fn path_env() -> Vec<(String, String)> {
    vec![("PATH".to_string(), "/usr/bin:/bin".to_string())]
}

// ── run: output, exit codes, stdin ─────────────────────────────────────

#[test]
fn run_captures_stdout_and_zero_exit() {
    let r = run(sh(&["-c", "printf hello"]));
    assert!(r.success);
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.stdout, "hello");
    assert!(!r.timed_out);
}

#[test]
fn run_propagates_nonzero_exit_code() {
    let r = run(sh(&["-c", "exit 3"]));
    assert!(!r.success);
    assert_eq!(r.exit_code, 3);
}

#[test]
fn run_writes_stdin_to_child() {
    let mut request = sh(&["-c", "cat"]);
    request.envs = path_env();
    request.stdin_data = Some("piped-input".to_string());
    let r = run(request);
    assert!(r.success, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "piped-input");
}

#[test]
fn run_installs_max_open_files_before_exec() {
    let mut request = sh(&["-c", "ulimit -n"]);
    request.limits = Some(SubprocessLimits {
        max_open_files: Some(64),
    });

    let result = run(request);

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.stdout.trim(), "64");
}

#[test]
fn configure_limits_applies_to_an_arbitrary_command() {
    let limits = SubprocessLimits {
        max_open_files: Some(64),
    };
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "ulimit -n"]);

    configure_subprocess_limits(&mut command, Some(&limits)).unwrap();
    let output = command.output().unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "64");
}

#[test]
fn validation_is_side_effect_free_and_rejects_an_unbounded_limit() {
    let limits = SubprocessLimits {
        max_open_files: Some(u64::MAX),
    };

    let error = validate_subprocess_limits(Some(&limits)).unwrap_err();

    assert!(error.contains("max_open_files"), "{error}");
}

#[test]
fn spawn_rejects_an_unbounded_open_file_limit_before_fork() {
    let mut request = sh(&["-c", "true"]);
    request.limits = Some(SubprocessLimits {
        max_open_files: Some(u64::MAX),
    });

    let Err(result) = spawn(request) else {
        panic!("an invalid resource limit must prevent spawn");
    };

    assert_eq!(result.pid, 0);
    assert!(
        result.stderr.contains("max_open_files"),
        "{}",
        result.stderr
    );
}

// ── env_clear: the secret-scoping contract ─────────────────────────────

#[test]
fn run_env_is_authoritative_no_parent_leak() {
    // Export a probe into THIS process, then confirm the child cannot see
    // it (env_clear) unless it is passed explicitly through `envs`.
    std::env::set_var("LILLUX_LEAK_PROBE", "leaked");

    let absent = run(sh(&["-c", "printf %s \"${LILLUX_LEAK_PROBE:-absent}\""]));
    assert_eq!(
        absent.stdout, "absent",
        "parent env must not leak into the child"
    );

    let mut with_env = sh(&["-c", "printf %s \"$LILLUX_LEAK_PROBE\""]);
    with_env.envs = vec![("LILLUX_LEAK_PROBE".to_string(), "explicit".to_string())];
    let explicit = run(with_env);
    assert_eq!(
        explicit.stdout, "explicit",
        "an explicitly-passed env var must reach the child"
    );

    std::env::remove_var("LILLUX_LEAK_PROBE");
}

// ── timeout kills the process group ────────────────────────────────────

#[test]
fn run_times_out_and_terminates() {
    let start = std::time::Instant::now();
    let mut request = sh(&["-c", "sleep 10"]);
    request.envs = path_env();
    request.timeout = 0.5;

    let r = run(request);

    assert!(r.timed_out, "expected timeout; stderr: {}", r.stderr);
    assert!(!r.success);
    assert!(
        start.elapsed().as_secs_f64() < 5.0,
        "timeout must fire well before the 10s sleep completes"
    );
    assert!(
        r.stderr.contains("timed out"),
        "stderr must explain the timeout; got: {}",
        r.stderr
    );
}

// ── spawn/wait split ───────────────────────────────────────────────────

#[test]
fn spawn_then_wait_returns_output() {
    let Ok(running) = spawn(sh(&["-c", "printf done"])) else {
        panic!("spawn failed");
    };
    let r = running.wait();
    assert!(r.success);
    assert_eq!(r.stdout, "done");
}

#[test]
fn spawned_process_is_alive_then_reaped() {
    let mut request = sh(&["-c", "sleep 1"]);
    request.envs = path_env();
    let Ok(running) = spawn(request) else {
        panic!("spawn failed");
    };
    let pid = running.pid;

    assert!(is_alive(pid), "process must be alive while running");

    let r = running.wait();
    assert!(r.success);

    // wait() reaps the child; give the kernel a beat to clear the entry.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        !is_alive(pid),
        "process must be gone once wait has reaped it"
    );
}

// ── detached spawn ─────────────────────────────────────────────────────

#[test]
fn spawn_detached_writes_to_log_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let log = tmp.path().join("out.log");

    spawn_detached(
        "/bin/sh",
        &["-c".to_string(), "printf detached-ok".to_string()],
        Some(log.to_str().unwrap()),
        &path_env(),
    )
    .expect("spawn detached");

    // Poll briefly for the detached child to run and flush into the log.
    let mut content = String::new();
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&log) {
            if !s.is_empty() {
                content = s;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(content, "detached-ok");
}

// ── liveness / termination on unused PIDs ──────────────────────────────

#[test]
fn is_alive_false_for_unused_pid() {
    // Well above any Linux pid_max (default 4194304) yet inside the i32
    // range `kill(2)` accepts as a plain PID, so it resolves to ESRCH.
    assert!(!is_alive(2_000_000_000));
}

#[test]
fn kill_reports_already_dead_for_unused_pid() {
    let method = kill(2_000_000_000, 0.1).expect("kill");
    assert_eq!(method, "already_dead");
}
