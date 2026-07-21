#![cfg(target_os = "linux")]

use std::time::Duration;

use lillux::{
    is_alive, retain_fork_sensitive_descriptors, spawn, spawn_awaiting_attachment,
    supervised_launcher_attachment_status_pipe, supervised_launcher_status_pipe, SubprocessLimits,
    SubprocessRequest,
};

fn shell(script: String) -> SubprocessRequest {
    SubprocessRequest {
        cmd: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), script],
        cwd: None,
        envs: vec![],
        stdin_data: None,
        timeout: 30.0,
        limits: None,
        inherited_fds: Vec::new(),
        supervised_status: None,
    }
}

#[test]
fn direct_target_cannot_execute_before_attachment_release() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("executed");
    let pending =
        spawn_awaiting_attachment(shell(format!("printf executed > {}", marker.display())))
            .expect("spawn awaiting attachment");

    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !marker.exists(),
        "target executed before attachment release"
    );
    assert_eq!(pending.pid() as i64, pending.pgid());
    let pending_pid = pending.pid();

    let running = pending
        .release_after_attachment()
        .expect("release after attachment");
    let result = running.wait();
    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.pid, pending_pid);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "executed");
}

#[test]
fn abort_pending_direct_target_never_executes_and_reaps() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("executed");
    let pending =
        spawn_awaiting_attachment(shell(format!("printf executed > {}", marker.display())))
            .expect("spawn awaiting attachment");
    let pid = pending.pid();

    let aborted = pending.abort_and_reap().expect("abort and reap");
    assert_eq!(aborted.pid, pid);
    assert!(!marker.exists(), "aborted target executed");
    assert!(!is_alive(pid), "aborted target was not reaped");
}

#[test]
fn dropping_pending_direct_target_never_executes_and_reaps() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("executed");
    let pending =
        spawn_awaiting_attachment(shell(format!("printf executed > {}", marker.display())))
            .expect("spawn awaiting attachment");
    let pid = pending.pid();

    drop(pending);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while is_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!marker.exists(), "dropped pending target executed");
    assert!(!is_alive(pid), "dropped pending target was not reaped");
}

#[test]
fn direct_setup_failure_returns_no_pending_process() {
    let mut request = shell("exit 0".to_string());
    request.cwd = Some("/definitely/not/a/real/lillux-working-directory".to_string());

    let error = match spawn_awaiting_attachment(request) {
        Ok(pending) => {
            pending.abort_and_reap().expect("abort unexpected process");
            panic!("invalid cwd must fail before returning a pending process")
        }
        Err(error) => error,
    };
    assert!(error.stderr.contains("open cwd"), "{}", error.stderr);
}

#[test]
fn exec_failure_is_reported_by_release_transition() {
    let request = SubprocessRequest {
        cmd: "/definitely/not/a/real/lillux-executable".to_string(),
        args: vec![],
        cwd: None,
        envs: vec![],
        stdin_data: None,
        timeout: 30.0,
        limits: None,
        inherited_fds: Vec::new(),
        supervised_status: None,
    };
    let pending = spawn_awaiting_attachment(request).expect("child reaches final pre-exec hold");
    let error = match pending.release_after_attachment() {
        Ok(running) => {
            running.abort();
            panic!("exec must fail after release")
        }
        Err(error) => error,
    };

    assert_eq!(error.phase, "exec after attachment release");
    assert!(error.result.stderr.contains("Failed to spawn"));
}

#[test]
fn attachment_deadline_expiry_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("executed");
    let mut request = shell(format!("printf executed > {}", marker.display()));
    // Leave enough setup budget for other attachment tests to exercise the
    // process-wide fork barrier concurrently; expiry is asserted after this
    // call returns, at the held target boundary.
    request.timeout = 0.5;
    let pending = spawn_awaiting_attachment(request).expect("spawn awaiting attachment");
    let pid = pending.pid();
    std::thread::sleep(Duration::from_millis(550));

    let error = match pending.release_after_attachment() {
        Ok(running) => {
            running.abort();
            panic!("expired attachment must not release")
        }
        Err(error) => error,
    };
    assert!(error.result.stderr.contains("deadline expired"));
    assert!(!marker.exists(), "expired target executed");
    assert!(!is_alive(pid), "expired target was not reaped");
}

#[test]
fn supervised_target_uses_the_same_typed_attachment_transition() {
    use std::os::fd::AsRawFd as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("executed");
    let pipe = supervised_launcher_attachment_status_pipe().expect("attachment status pipe");
    let status_fd = pipe.writer_fd();
    let release_reader = pipe.attachment_release_reader.as_raw_fd();
    let script = format!(
        "(/bin/dd bs=1 count=1 <&{release_reader} >/dev/null 2>&1; printf executed > {}) & target=$!; printf '{{\"child-pid\":%s}}\\n' \"$target\" >&{status_fd}; wait \"$target\"",
        marker.display()
    );
    let mut request = shell(script);
    request.envs = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
    request.inherited_fds.push(pipe.writer);
    request.inherited_fds.push(pipe.attachment_release_reader);
    request
        .inherited_fds
        .push(pipe.attachment_release_keepalive_writer);
    request.supervised_status = Some(pipe.reader);

    let pending = spawn_awaiting_attachment(request).expect("supervised pending target");
    assert_ne!(pending.pid() as i64, pending.pgid());
    std::thread::sleep(Duration::from_millis(100));
    assert!(!marker.exists(), "supervised target crossed its hold early");

    let running = pending
        .release_after_attachment()
        .expect("release supervised target");
    let result = running.wait();
    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "executed");
}

#[test]
fn normal_spawn_rejects_an_attachment_bearing_supervisor() {
    let pipe = supervised_launcher_attachment_status_pipe().expect("attachment status pipe");
    let mut request = shell("exit 0".to_string());
    request.inherited_fds.push(pipe.writer);
    request.inherited_fds.push(pipe.attachment_release_reader);
    request
        .inherited_fds
        .push(pipe.attachment_release_keepalive_writer);
    request.supervised_status = Some(pipe.reader);

    let error = match spawn(request) {
        Ok(running) => {
            running.abort();
            panic!("normal spawn must reject an attachment-bearing supervisor")
        }
        Err(error) => error,
    };
    assert!(
        error.stderr.contains("spawn_awaiting_attachment"),
        "{}",
        error.stderr
    );
}

#[test]
fn attachment_spawn_rejects_supervision_without_a_target_boundary() {
    let pipe = supervised_launcher_status_pipe().expect("status pipe");
    let mut request = shell("exit 0".to_string());
    request.inherited_fds.push(pipe.writer);
    request.supervised_status = Some(pipe.reader);

    let error = match spawn_awaiting_attachment(request) {
        Ok(pending) => {
            pending
                .abort_and_reap()
                .expect("abort unexpected pending target");
            panic!("attachment spawn must reject a supervisor without a target boundary")
        }
        Err(error) => error,
    };
    assert!(
        error
            .stderr
            .contains("omitted its required target attachment boundary"),
        "{}",
        error.stderr
    );
}

#[test]
fn direct_attachment_preserves_exact_environment_and_cwd() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut request = shell("printf '%s:%s' \"$EXACT\" \"$PWD\" > observed".to_string());
    request.cwd = Some(temp.path().to_string_lossy().into_owned());
    request.envs = vec![("EXACT".to_string(), "authority".to_string())];

    let pending = spawn_awaiting_attachment(request).expect("spawn awaiting attachment");
    let running = pending
        .release_after_attachment()
        .expect("release after attachment");
    let result = running.wait();
    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("observed")).unwrap(),
        format!("authority:{}", temp.path().display())
    );
}

#[test]
fn direct_attachment_preserves_limits_and_exact_identity_after_exec() {
    let mut request =
        shell("printf '%s %s %s' \"$$\" \"$(ps -o pgid= -p $$)\" \"$(ulimit -n)\"".to_string());
    request.envs = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
    request.limits = Some(SubprocessLimits {
        max_open_files: Some(64),
        ..SubprocessLimits::default()
    });

    let pending = spawn_awaiting_attachment(request).expect("spawn awaiting attachment");
    let pid = pending.pid();
    let pgid = pending.pgid();
    let result = pending
        .release_after_attachment()
        .expect("release after attachment")
        .wait();
    assert!(result.success, "stderr: {}", result.stderr);
    let observed = result
        .stdout
        .split_ascii_whitespace()
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()
        .expect("numeric identity and limit output");
    assert_eq!(observed, vec![i64::from(pid), pgid, 64]);
}

#[test]
fn concurrent_attachment_boundaries_release_only_their_own_targets() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut pending = Vec::new();
    for index in 0..12 {
        let marker = temp.path().join(format!("executed-{index}"));
        let process =
            spawn_awaiting_attachment(shell(format!("printf executed > {}", marker.display())))
                .expect("concurrent spawn awaiting attachment");
        pending.push((index, marker, process));
    }

    std::thread::sleep(Duration::from_millis(100));
    assert!(pending.iter().all(|(_, marker, _)| !marker.exists()));
    for (index, marker, process) in pending {
        if index % 2 == 0 {
            let result = process
                .release_after_attachment()
                .expect("release selected target")
                .wait();
            assert!(result.success, "stderr: {}", result.stderr);
            assert!(marker.exists(), "released target did not execute");
        } else {
            process.abort_and_reap().expect("abort unselected target");
            assert!(!marker.exists(), "one release crossed into another target");
        }
    }
}

#[test]
fn direct_fork_waits_for_fork_sensitive_descriptor_scopes() {
    let lease = retain_fork_sensitive_descriptors();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let pending = spawn_awaiting_attachment(shell("exit 0".to_string()))
            .expect("spawn after descriptor scope quiesces");
        sender.send(pending).expect("publish pending process");
    });

    assert!(
        receiver.recv_timeout(Duration::from_millis(100)).is_err(),
        "direct child forked while fork-sensitive descriptors were retained"
    );
    drop(lease);

    let pending = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("direct fork resumes after descriptor scope");
    pending.abort_and_reap().expect("abort held test target");
    worker.join().expect("spawn worker");
}

#[test]
fn direct_fork_fails_loudly_for_same_thread_descriptor_scope() {
    let lease = retain_fork_sensitive_descriptors();
    let error = match spawn_awaiting_attachment(shell("exit 0".to_string())) {
        Ok(pending) => {
            pending.abort_and_reap().expect("abort unexpected process");
            panic!("same-owner fork-sensitive scope must not wait on itself")
        }
        Err(error) => error,
    };
    assert!(
        error
            .stderr
            .contains("calling thread retains fork-sensitive descriptor authority"),
        "{}",
        error.stderr
    );
    drop(lease);
}

#[test]
fn simultaneous_attachment_forks_keep_release_authority_per_child() {
    const CHILDREN: usize = 8;
    let start = std::sync::Arc::new(std::sync::Barrier::new(CHILDREN));
    let (sender, receiver) = std::sync::mpsc::channel();
    let workers = (0..CHILDREN)
        .map(|_| {
            let start = std::sync::Arc::clone(&start);
            let sender = sender.clone();
            std::thread::spawn(move || {
                start.wait();
                let pending = spawn_awaiting_attachment(shell("exit 0".to_string()))
                    .expect("simultaneous attachment fork");
                sender.send(pending).expect("publish pending process");
            })
        })
        .collect::<Vec<_>>();
    drop(sender);

    let pending = receiver.into_iter().collect::<Vec<_>>();
    assert_eq!(pending.len(), CHILDREN);
    for process in pending {
        process
            .abort_and_reap()
            .expect("each held child retains independent abort authority");
    }
    for worker in workers {
        worker.join().expect("simultaneous spawn worker");
    }
}

#[test]
fn released_target_timeout_still_terminates_its_process_group() {
    let temp = tempfile::tempdir().expect("tempdir");
    let background_pid = temp.path().join("background-pid");
    let mut request = shell(format!(
        "sleep 30 & child=$!; printf '%s' \"$child\" > {}; wait \"$child\"",
        background_pid.display()
    ));
    request.timeout = 0.2;
    let pending = spawn_awaiting_attachment(request).expect("spawn awaiting attachment");
    let result = pending
        .release_after_attachment()
        .expect("release after attachment")
        .wait();
    assert!(
        result.timed_out,
        "target did not retain its request timeout"
    );
    let child_pid = std::fs::read_to_string(background_pid)
        .expect("background pid")
        .parse::<u32>()
        .expect("numeric background pid");
    assert!(
        !is_alive(child_pid),
        "timed-out group member survived cleanup"
    );
}

#[test]
fn supervised_output_overflow_before_release_fails_closed() {
    use std::os::fd::AsRawFd as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("executed");
    let pipe = supervised_launcher_attachment_status_pipe().expect("attachment status pipe");
    let status_fd = pipe.writer_fd();
    let release_reader = pipe.attachment_release_reader.as_raw_fd();
    let script = format!(
        "(/bin/dd bs=1 count=1 <&{release_reader} >/dev/null 2>&1; printf executed > {}) & target=$!; printf '{{\"child-pid\":%s}}\\n' \"$target\" >&{status_fd}; /bin/dd if=/dev/zero bs=1024 count=4 2>/dev/null; wait \"$target\"",
        marker.display()
    );
    let mut request = shell(script);
    request.envs = vec![("PATH".to_string(), "/usr/bin:/bin".to_string())];
    request.limits = Some(SubprocessLimits {
        max_stdout_bytes: Some(16),
        ..SubprocessLimits::default()
    });
    request.inherited_fds.push(pipe.writer);
    request.inherited_fds.push(pipe.attachment_release_reader);
    request
        .inherited_fds
        .push(pipe.attachment_release_keepalive_writer);
    request.supervised_status = Some(pipe.reader);

    let pending = spawn_awaiting_attachment(request).expect("supervised pending target");
    let pid = pending.pid();
    std::thread::sleep(Duration::from_millis(100));
    let error = match pending.release_after_attachment() {
        Ok(running) => {
            running.abort();
            panic!("overflowed launcher output must prevent target release")
        }
        Err(error) => error,
    };
    assert!(error.result.stderr.contains("output exceeded"));
    assert!(!marker.exists(), "overflowed supervised target executed");
    assert!(
        !is_alive(pid),
        "overflowed supervised target survived cleanup"
    );
}

#[test]
fn attachment_parent_death_fixture() {
    let Ok(marker) = std::env::var("LILLUX_ATTACHMENT_PARENT_DEATH_MARKER") else {
        return;
    };
    let pid_file = std::env::var("LILLUX_ATTACHMENT_PARENT_DEATH_PID").expect("pid file");
    let pending = spawn_awaiting_attachment(shell(format!("printf executed > {marker}")))
        .expect("spawn parent-death fixture");
    std::fs::write(pid_file, pending.pid().to_string()).expect("publish held child pid");
    // Deliberately bypass Rust destructors. The held child's temporary
    // PR_SET_PDEATHSIG must make process death fail closed on its own.
    std::process::exit(0);
}

#[test]
fn direct_parent_death_kills_unreleased_target() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("executed");
    let pid_file = temp.path().join("pid");
    let status = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .args(["--exact", "attachment_parent_death_fixture", "--nocapture"])
        .env("LILLUX_ATTACHMENT_PARENT_DEATH_MARKER", &marker)
        .env("LILLUX_ATTACHMENT_PARENT_DEATH_PID", &pid_file)
        .status()
        .expect("run parent-death fixture");
    assert!(status.success());
    let pid = std::fs::read_to_string(&pid_file)
        .expect("fixture pid")
        .parse::<u32>()
        .expect("numeric fixture pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while is_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!is_alive(pid), "held target survived its parent process");
    assert!(!marker.exists(), "held target executed after parent death");
}
