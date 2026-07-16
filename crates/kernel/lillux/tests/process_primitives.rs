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
    configure_subprocess_limits, is_alive, kill, run, sealed_memfd, spawn, spawn_detached,
    supervised_launcher_status_pipe, validate_subprocess_limits, OutputLimitExceeded,
    SubprocessLimits, SubprocessRequest,
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
        inherited_fds: Vec::new(),
        supervised_status: None,
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
        ..SubprocessLimits::default()
    });

    let result = run(request);

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.stdout.trim(), "64");
}

#[test]
fn configure_limits_applies_to_an_arbitrary_command() {
    let limits = SubprocessLimits {
        max_open_files: Some(64),
        ..SubprocessLimits::default()
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
        ..SubprocessLimits::default()
    };

    let error = validate_subprocess_limits(Some(&limits)).unwrap_err();

    assert!(error.contains("max_open_files"), "{error}");
}

#[test]
fn spawn_rejects_an_unbounded_open_file_limit_before_fork() {
    let mut request = sh(&["-c", "true"]);
    request.limits = Some(SubprocessLimits {
        max_open_files: Some(u64::MAX),
        ..SubprocessLimits::default()
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

#[test]
fn normal_completion_cleans_up_background_group_members() {
    let mut request = sh(&["-c", "sleep 30 & printf %s $!"]);
    request.envs = path_env();

    let result = run(request);

    assert!(result.success, "stderr: {}", result.stderr);
    let background_pid: u32 = result.stdout.parse().expect("background pid");
    for _ in 0..50 {
        if !is_alive(background_pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !is_alive(background_pid),
        "background group member survived synchronous completion"
    );
}

// ── bounded daemon-side output retention ──────────────────────────────

#[test]
fn stdout_limit_is_an_explicit_failed_outcome() {
    let mut request = sh(&["-c", "printf 0123456789"]);
    request.limits = Some(SubprocessLimits {
        max_stdout_bytes: Some(5),
        ..SubprocessLimits::default()
    });

    let result = run(request);

    assert!(!result.success);
    assert!(!result.timed_out);
    assert_eq!(
        result.output_limit_exceeded,
        Some(OutputLimitExceeded::Stdout)
    );
    assert!(result.stdout_truncated);
    assert!(!result.stderr_truncated);
    assert_eq!(result.stdout, "01234");
    assert!(result.stderr.contains("output retention limit"));
}

#[test]
fn output_overflow_terminates_a_continuously_writing_group() {
    let mut request = sh(&["-c", "while :; do printf 0123456789; done"]);
    request.timeout = 10.0;
    request.limits = Some(SubprocessLimits {
        max_stdout_bytes: Some(1024),
        ..SubprocessLimits::default()
    });
    let started = std::time::Instant::now();

    let result = run(request);

    assert_eq!(
        result.output_limit_exceeded,
        Some(OutputLimitExceeded::Stdout)
    );
    assert!(result.stdout_truncated);
    assert_eq!(result.stdout.len(), 1024);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "output overflow must terminate before the ordinary timeout"
    );
}

// ── immutable descriptor-backed protocol data ─────────────────────────

#[test]
#[cfg(target_os = "linux")]
fn sealed_memfd_is_rewound_cloexec_and_immutable() {
    use std::io::{Read as _, Seek as _, Write as _};
    use std::os::fd::AsRawFd as _;

    let file = sealed_memfd(c"lillux-test", b"sealed protocol bytes").expect("sealed memfd");
    let fd = file.as_raw_fd();
    assert!(fd > libc::STDERR_FILENO);

    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(descriptor_flags >= 0);
    assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);

    let required_seals =
        libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    let observed_seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    assert_eq!(observed_seals & required_seals, required_seals);

    let mut view = file.try_clone().expect("clone sealed memfd");
    assert_eq!(view.stream_position().expect("position"), 0);
    let mut bytes = Vec::new();
    view.read_to_end(&mut bytes).expect("read sealed memfd");
    assert_eq!(bytes, b"sealed protocol bytes");

    let error = view.write_all(b"mutation").unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EPERM));
}

#[test]
#[cfg(not(target_os = "linux"))]
fn sealed_memfd_fails_closed_off_linux() {
    let error = sealed_memfd(c"lillux-test", b"payload").unwrap_err();
    assert!(error.contains("only on Linux"), "{error}");
}

// ── trusted-launcher target identity supervision ──────────────────────

#[cfg(target_os = "linux")]
fn supervised_launcher_protocol_shell(script: &str, timeout: f64) -> SubprocessRequest {
    let status = supervised_launcher_status_pipe().expect("status pipe");
    let status_fd = status.writer_fd();
    // Like the real sandbox launch, the mock target inherits the retained
    // wrapper's Lillux-owned session/process group. The status PID identifies
    // the target for accounting, while the wrapper keeps the shared PGID owned.
    let wrapper_script = format!(
        "/bin/sh -c 'exec /bin/sh -c \"$1\"' \
         ryeos-target '{script}' & target=$!; \
         printf '{{\"child-pid\":%s}}\\n' \"$target\" >&{status_fd}; wait \"$target\""
    );
    let mut request = sh(&["-c", &wrapper_script]);
    request.envs = path_env();
    request.timeout = timeout;
    request.inherited_fds.push(status.writer);
    request.supervised_status = Some(status.reader);
    request
}

#[test]
#[cfg(target_os = "linux")]
fn reported_target_group_is_exposed_and_killed_on_timeout() {
    let running = spawn(supervised_launcher_protocol_shell("sleep 30", 0.25)).expect("spawn");
    let target_pid = running.pid;

    assert_ne!(running.pgid, target_pid as i64);
    assert_eq!(
        unsafe { libc::getpgid(target_pid as libc::pid_t) } as i64,
        running.pgid
    );
    let result = running.wait();

    assert!(result.timed_out, "stderr: {}", result.stderr);
    assert_eq!(result.pid, target_pid);
    for _ in 0..50 {
        if !is_alive(target_pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !is_alive(target_pid),
        "reported target survived timeout cleanup"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn reported_target_exit_still_cleans_up_same_group_descendants() {
    let running = spawn(supervised_launcher_protocol_shell(
        "sleep 30 & printf %s $!",
        2.0,
    ))
    .expect("spawn");
    let target_pid = running.pid;
    let retained_pgid = running.pgid;

    let result = running.wait();

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.pid, target_pid);
    let background_pid: u32 = result.stdout.parse().expect("background pid");
    assert_ne!(retained_pgid, target_pid as i64);
    for _ in 0..50 {
        if !is_alive(background_pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !is_alive(background_pid),
        "same-group descendant survived after the reported target exited"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn malformed_launcher_status_fails_closed_and_kills_wrapper() {
    let status = supervised_launcher_status_pipe().expect("status pipe");
    let status_fd = status.writer_fd();
    let wrapper_script = format!("printf 'not-json\\n' >&{status_fd}; sleep 30");
    let mut request = sh(&["-c", &wrapper_script]);
    request.envs = path_env();
    request.inherited_fds.push(status.writer);
    request.supervised_status = Some(status.reader);

    let Err(result) = spawn(request) else {
        panic!("malformed trusted-launcher status must refuse the spawn");
    };

    assert!(
        result.stderr.contains("invalid JSON status document"),
        "{}",
        result.stderr
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
