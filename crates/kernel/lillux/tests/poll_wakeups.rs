#![cfg(target_os = "linux")]

//! Launch latency and teardown: waits end at the event (child exit, pipe
//! readiness, stop wake-up), never at a later timer tick.

use std::time::{Duration, Instant};

use lillux::{SubprocessLimits, SubprocessRequest};

// Host shell programs are deliberately the low-level process test fixtures,
// not a fallback for an admitted worker or development realization.
fn shell(script: &str) -> SubprocessRequest {
    SubprocessRequest {
        cmd: "/bin/sh".into(),
        argv0: None,
        args: vec!["-c".into(), script.into()],
        cwd: None,
        envs: vec![],
        stdin_data: None,
        timeout: 30.0,
        limits: None,
        inherited_fds: vec![],
        inherited_fd_mappings: vec![],
        supervised_status: None,
    }
}

#[test]
fn short_launches_finish_below_the_old_fixed_sleep_floor() {
    // Before event-driven waits every launch slept at least one 10 ms wrapper
    // interval, because the child cannot have exited by the first
    // observation. A launch faster than that is only possible when the wait
    // ends at the exit. The fast tail is compared, not the mean, so a loaded
    // runner's slow launches cannot fail it.
    let mut durations = Vec::new();
    for _ in 0..60 {
        let started = Instant::now();
        let result = lillux::run(shell("printf ok"));
        durations.push(started.elapsed());
        assert!(result.success, "{result:?}");
        assert_eq!(result.stdout, "ok");
    }
    durations.sort();
    let fast_tail = durations[durations.len() / 10];
    assert!(
        fast_tail < Duration::from_millis(9),
        "10th percentile launch took {fast_tail:?}; durations {durations:?}"
    );
}

#[test]
fn large_stdout_and_stderr_are_captured_without_loss_or_deadlock() {
    const BYTES: usize = 4 * 1024 * 1024;
    let script = format!(
        "head -c {BYTES} /dev/zero | tr '\\0' a & head -c {BYTES} /dev/zero | tr '\\0' b >&2; wait"
    );
    let started = Instant::now();
    let result = lillux::run(shell(&script));
    assert!(result.success, "{:?}", result.stderr.len());
    assert_eq!(result.stdout.len(), BYTES);
    assert!(result.stdout.bytes().all(|byte| byte == b'a'));
    assert_eq!(result.stderr.len(), BYTES);
    assert!(result.stderr.bytes().all(|byte| byte == b'b'));
    assert!(!result.stdout_truncated && !result.stderr_truncated);
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn output_beyond_the_retention_limit_is_bounded_and_terminated() {
    let mut request = shell("head -c 8388608 /dev/zero | tr '\\0' a; sleep 30");
    request.limits = Some(SubprocessLimits {
        max_stdout_bytes: Some(1024 * 1024),
        ..SubprocessLimits::default()
    });
    let started = Instant::now();
    let result = lillux::run(request);
    assert!(!result.success);
    assert!(result.output_limit_exceeded.is_some(), "{result:?}");
    assert!(result.stdout_truncated);
    assert_eq!(result.stdout.len(), 1024 * 1024);
    assert!(!result.timed_out);
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn timeout_kills_a_silent_child_on_time() {
    let mut request = shell("sleep 30");
    request.timeout = 0.3;
    let started = Instant::now();
    let result = lillux::run(request);
    let elapsed = started.elapsed();
    assert!(result.timed_out, "{result:?}");
    assert!(!result.success);
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    let alive = unsafe { libc::kill(result.pid as i32, 0) } == 0;
    assert!(!alive, "timed-out child remained alive");
}

#[test]
fn timeout_applies_while_output_keeps_arriving() {
    let mut request = shell("while :; do printf x; sleep 0.01; done");
    request.timeout = 0.3;
    let started = Instant::now();
    let result = lillux::run(request);
    assert!(result.timed_out, "{result:?}");
    assert!(!result.stdout.is_empty());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn exit_with_the_pipe_still_open_in_a_grandchild_does_not_wait_for_the_pipe() {
    // The shell exits at once while the background sleep keeps both capture
    // pipes open. Settlement must follow the exit and the stop wake-up, not
    // the grandchild's EOF 30 s later.
    let started = Instant::now();
    let result = lillux::run(shell("sleep 30 & printf early"));
    assert!(started.elapsed() < Duration::from_secs(5), "{result:?}");
    assert_eq!(result.stdout, "early");
    assert!(!result.timed_out);
}

#[test]
fn dropping_a_running_process_wakes_and_joins_its_drains() {
    let process = lillux::spawn(shell("printf up; sleep 30")).unwrap();
    let pid = process.pid();
    let started = Instant::now();
    drop(process);
    assert!(started.elapsed() < Duration::from_secs(5));
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
    assert!(!alive, "dropped process remained alive");
}

#[test]
fn stdin_reaches_the_child_in_full() {
    let mut request = shell("wc -c");
    request.stdin_data = Some("x".repeat(1024 * 1024));
    let result = lillux::run(request);
    assert!(result.success, "{result:?}");
    assert_eq!(result.stdout.trim(), "1048576");

    let mut small = shell("cat");
    small.stdin_data = Some("hello".into());
    let result = lillux::run(small);
    assert_eq!(result.stdout, "hello");
}

#[test]
fn child_that_never_reads_stdin_is_still_settled_at_exit() {
    let mut request = shell("printf done");
    request.stdin_data = Some("y".repeat(4 * 1024 * 1024));
    let started = Instant::now();
    let result = lillux::run(request);
    assert_eq!(result.stdout, "done");
    assert!(started.elapsed() < Duration::from_secs(5));
}
