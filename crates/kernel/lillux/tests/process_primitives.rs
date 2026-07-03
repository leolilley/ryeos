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

use lillux::{is_alive, kill, run, spawn, spawn_detached, SubprocessRequest};

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
    let running = spawn(sh(&["-c", "printf done"])).expect("spawn");
    let r = running.wait();
    assert!(r.success);
    assert_eq!(r.stdout, "done");
}

#[test]
fn spawned_process_is_alive_then_reaped() {
    let mut request = sh(&["-c", "sleep 1"]);
    request.envs = path_env();
    let running = spawn(request).expect("spawn");
    let pid = running.pid;

    assert!(is_alive(pid), "process must be alive while running");

    let r = running.wait();
    assert!(r.success);

    // wait() reaps the child; give the kernel a beat to clear the entry.
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!is_alive(pid), "process must be gone once wait has reaped it");
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
