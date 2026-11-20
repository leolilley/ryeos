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
    // observation. A single launch faster than that is only possible when the
    // wait ends at the exit. The minimum over many launches is compared, so
    // slow launches on a loaded host cannot fail it, and a fresh batch is
    // tried if a whole batch was starved.
    let mut best = Duration::MAX;
    for _attempt in 0..3 {
        for _ in 0..40 {
            let started = Instant::now();
            let result = lillux::run(shell("printf ok"));
            let elapsed = started.elapsed();
            assert!(result.success, "{result:?}");
            assert_eq!(result.stdout, "ok");
            best = best.min(elapsed);
        }
        if best < Duration::from_millis(9) {
            return;
        }
    }
    panic!("fastest of 120 launches took {best:?}, not below the old 10 ms floor");
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
fn exit_with_the_pipe_still_open_in_an_escaped_grandchild_does_not_wait_for_the_pipe() {
    // The shell exits at once while a grandchild in its own session keeps both
    // capture pipes open. The group kill cannot reach it, so only the stop
    // wake-up can end the drains: settlement must follow the exit, not the
    // grandchild's EOF 6 s later.
    let started = Instant::now();
    let result = lillux::run(shell("setsid sleep 6 & printf early"));
    assert!(started.elapsed() < Duration::from_secs(4), "{result:?}");
    assert_eq!(result.stdout, "early");
    assert!(!result.timed_out);
}

#[test]
fn dropping_a_running_process_wakes_and_joins_its_drains() {
    let process = lillux::spawn(shell("printf up; sleep 30")).unwrap();
    let pid = process.pid;
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

// ── waits that follow events: stdin pressure, supervised status, natural exit ──

/// Fastest of `attempts * per_attempt` runs of `launch`; stops early once a
/// run beats `bound`. The minimum is compared so slow launches on a loaded
/// host cannot fail a test whose point is that a fast one is possible.
fn fastest_launch(
    attempts: usize,
    per_attempt: usize,
    bound: Duration,
    launch: impl Fn(),
) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..attempts {
        for _ in 0..per_attempt {
            let started = Instant::now();
            launch();
            best = best.min(started.elapsed());
        }
        if best < bound {
            break;
        }
    }
    best
}

#[test]
fn large_stdin_is_not_paced_by_a_fixed_interval() {
    // A 1 MiB payload crosses a 64 KiB pipe sixteen times. With a fixed 10 ms
    // wait on every full pipe that costs at least 160 ms; waiting for the
    // child to drain the pipe costs only the transfer itself.
    let bound = Duration::from_millis(100);
    let best = fastest_launch(3, 5, bound, || {
        let mut request = shell("wc -c");
        request.stdin_data = Some("x".repeat(1024 * 1024));
        let result = lillux::run(request);
        assert!(result.success, "{result:?}");
        assert_eq!(result.stdout.trim(), "1048576");
    });
    assert!(best < bound, "fastest 1 MiB stdin launch took {best:?}");
}

/// A launcher-shaped request: the wrapper reports its target's pid on the
/// supervised status channel, then waits for the target.
fn supervised_shell(target_script: &str) -> SubprocessRequest {
    let status = lillux::supervised_launcher_status_pipe().expect("status pipe");
    let status_fd = status.writer_descriptor().unwrap() as i32;
    let wrapper_script = format!(
        "/bin/sh -c '{target_script}' & target=$!; \
         printf '{{\"child-pid\":%s}}\\n' \"$target\" > /proc/self/fd/{status_fd}; wait \"$target\""
    );
    let mut request = shell(&wrapper_script);
    request.envs = vec![("PATH".into(), "/usr/bin:/bin".into())];
    request.inherited_fds.push(status.writer);
    request.supervised_status = Some(status.reader);
    request
}

#[test]
fn supervised_launch_does_not_wait_on_status_or_exit_timers() {
    // The old status reader slept 10 ms whenever the channel was empty, and
    // the wrapper wait slept 10 ms per check, so a supervised launch could not
    // finish under 10 ms.
    let bound = Duration::from_millis(10);
    let best = fastest_launch(3, 30, bound, || {
        let running = lillux::spawn(supervised_shell("exit 0"))
            .unwrap_or_else(|failure| panic!("supervised spawn: {}", failure.stderr));
        let result = running.wait();
        assert!(result.success, "{result:?}");
    });
    assert!(best < bound, "fastest supervised launch took {best:?}");
}

#[test]
fn silent_supervised_launcher_still_fails_at_the_request_deadline() {
    // The status wait is now an event wait; its deadline must be unchanged.
    let status = lillux::supervised_launcher_status_pipe().expect("status pipe");
    let status_fd = status.writer_descriptor().unwrap() as i32;
    let mut request = shell(&format!("sleep 30; : > /proc/self/fd/{status_fd}"));
    request.envs = vec![("PATH".into(), "/usr/bin:/bin".into())];
    request.timeout = 1.0;
    request.inherited_fds.push(status.writer);
    request.supervised_status = Some(status.reader);
    let started = Instant::now();
    let failure = match lillux::spawn(request) {
        Ok(running) => {
            drop(running.wait());
            panic!("a launcher that never reports must not spawn");
        }
        Err(failure) => failure,
    };
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "{failure:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8), "{failure:?}");
}

#[test]
fn natural_wait_ends_at_the_exit_not_at_a_timer() {
    let bound = Duration::from_millis(10);
    let best = fastest_launch(3, 30, bound, || {
        let running = lillux::spawn(shell("printf done")).unwrap();
        let result = running
            .wait_for_natural_exit(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("natural exit was not observed"));
        assert_eq!(result.stdout, "done");
    });
    assert!(best < bound, "fastest natural wait took {best:?}");
}

#[test]
fn natural_wait_gives_the_process_back_at_its_own_deadline() {
    let running = lillux::spawn(shell("sleep 30")).unwrap();
    let started = Instant::now();
    let Err(owner) = running.wait_for_natural_exit(Duration::from_millis(300)) else {
        panic!("a running process cannot have exited naturally");
    };
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    let pid = owner.pid;
    drop(owner);
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
    assert!(!alive, "released owner left its process alive");
}
