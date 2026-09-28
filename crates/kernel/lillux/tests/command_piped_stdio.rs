#![cfg(unix)]

use std::io::Read as _;
use std::process::Command;
use std::time::Duration;

fn subordinate_shell(script: &str) -> lillux::SubordinateProcess {
    lillux::SubordinateProcess::spawn(lillux::SubordinateProcessRequest {
        cmd: "/bin/sh".to_owned(),
        argv0: None,
        args: vec!["-c".to_owned(), script.to_owned()],
        cwd: "/".to_owned(),
        envs: Vec::new(),
        limits: None,
        inherited_fds: Vec::new(),
    })
    .unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn pinned_subordinate_launch_keeps_executable_and_cwd_after_path_rebind() {
    use lillux::time::MonotonicDeadline;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let selected = temp.path().join("selected");
    let moved = temp.path().join("moved");
    std::fs::create_dir(&selected).unwrap();
    let shell = selected.join("shell");
    std::fs::copy("/bin/sh", &shell).unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cwd = lillux::PinnedDirectory::open(&selected).unwrap().unwrap();
    let executable = cwd
        .open_pinned_regular(OsStr::new("shell"), false)
        .unwrap()
        .unwrap();

    std::fs::rename(&selected, &moved).unwrap();
    std::fs::create_dir(&selected).unwrap();
    std::fs::copy("/bin/false", &shell).unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut child =
        lillux::SubordinateProcess::spawn_pinned(lillux::PinnedSubordinateProcessRequest {
            executable,
            cwd,
            argv0: Some("sh".into()),
            args: vec!["-c".into(), "pwd".into()],
            envs: Vec::new(),
            limits: None,
            inherited_fds: Vec::new(),
        })
        .unwrap();
    let mut output = child.take_output().unwrap();
    let deadline = MonotonicDeadline::after(Duration::from_secs(5));
    assert_eq!(
        output.read_frame_until(b'\n', 4096, deadline).unwrap(),
        format!("{}\n", moved.display()).as_bytes()
    );
    assert!(
        child
            .wait_exact_child_until(deadline)
            .unwrap()
            .unwrap()
            .success
    );
}

#[cfg(target_os = "linux")]
#[test]
fn inherited_descriptor_paths_can_cross_one_subordinate_process_hop() {
    use lillux::time::MonotonicDeadline;
    use std::ffi::OsStr;

    let temp = tempfile::tempdir().unwrap();
    let selected = temp.path().join("selected");
    let moved = temp.path().join("moved");
    std::fs::create_dir(&selected).unwrap();
    std::fs::copy("/bin/sh", selected.join("guest")).unwrap();
    let cwd = lillux::PinnedDirectory::open(&selected).unwrap().unwrap();
    let executable = cwd
        .open_pinned_regular(OsStr::new("guest"), false)
        .unwrap()
        .unwrap()
        .into_inherited_descriptor_path()
        .unwrap();
    let cwd_authority = cwd.into_inherited_descriptor_path().unwrap();
    let program = executable.path().to_str().unwrap().to_owned();
    let guest_cwd = cwd_authority.path().to_str().unwrap().to_owned();
    std::fs::rename(&selected, &moved).unwrap();
    std::fs::create_dir(&selected).unwrap();
    std::fs::copy("/bin/false", selected.join("guest")).unwrap();

    let mut child = lillux::SubordinateProcess::spawn(lillux::SubordinateProcessRequest {
        cmd: "/bin/sh".into(),
        argv0: None,
        args: vec![
            "-c".into(),
            "cd \"$GUEST_CWD\" && \"$GUEST_PROGRAM\" -c 'pwd -P'".into(),
        ],
        cwd: "/".into(),
        envs: vec![
            ("GUEST_CWD".into(), guest_cwd),
            ("GUEST_PROGRAM".into(), program),
        ],
        limits: None,
        inherited_fds: vec![executable, cwd_authority],
    })
    .unwrap();
    let mut output = child.take_output().unwrap();
    let deadline = MonotonicDeadline::after(Duration::from_secs(5));
    assert_eq!(
        output.read_frame_until(b'\n', 4096, deadline).unwrap(),
        format!("{}\n", moved.display()).as_bytes()
    );
    assert!(
        child
            .wait_exact_child_until(deadline)
            .unwrap()
            .unwrap()
            .success
    );
}

#[cfg(target_os = "linux")]
#[test]
fn subordinate_timeout_retains_exact_child_for_explicit_escalation() {
    use lillux::time::{MonotonicDeadline, MonotonicTimer};
    let mut child = subordinate_shell("trap '' TERM; printf 'ready\\n'; IFS= read -r value");
    let mut output = child.take_output().unwrap();
    assert_eq!(
        output
            .read_frame_until(b'\n', 64, MonotonicDeadline::after(Duration::from_secs(2)))
            .unwrap(),
        b"ready\n"
    );
    let pending = child.cooperative_termination().unwrap().request().unwrap();
    let timer = MonotonicTimer::start();
    let deadline = MonotonicDeadline::after(Duration::from_millis(30));
    assert_eq!(child.wait_exact_child_until(deadline).unwrap(), None);
    assert_eq!(child.wait_exact_child_until(deadline).unwrap(), None);
    assert!(timer.elapsed() < Duration::from_secs(1));
    assert!(!pending.has_exited().unwrap());
    let exit = child
        .kill_exact_child_until(MonotonicDeadline::after(Duration::from_secs(2)))
        .unwrap()
        .unwrap();
    assert!(!exit.success);
    assert!(pending.has_exited().unwrap());
    // Repeated observation/escalation uses the cached reaped Child status;
    // it cannot signal a recycled PID.
    assert_eq!(child.kill_exact_child_until(deadline).unwrap(), Some(exit));
}

#[test]
fn subordinate_diagnostics_keep_draining_after_retention_limit() {
    use lillux::time::MonotonicDeadline;
    let mut child = subordinate_shell(
        "i=0; while [ $i -lt 20000 ]; do printf 'diagnostic\\n' >&2; i=$((i+1)); done",
    );
    let mut stderr = child.take_error().unwrap();
    let deadline = MonotonicDeadline::after(Duration::from_secs(5));
    let captured = stderr.drain_until(64, deadline).unwrap();
    assert_eq!(captured.bytes.len(), 64);
    assert!(captured.truncated);
    assert!(captured.eof);
    assert!(
        child
            .wait_exact_child_until(deadline)
            .unwrap()
            .unwrap()
            .success
    );
}

#[test]
fn subordinate_diagnostic_timeout_is_partial_evidence_not_eof() {
    use lillux::time::MonotonicDeadline;
    let mut child = subordinate_shell("printf private-partial >&2; IFS= read -r value");
    let mut stderr = child.take_error().unwrap();
    let mut input = child.take_input().unwrap();
    let deadline = MonotonicDeadline::after(Duration::from_millis(100));
    let captured = stderr.drain_until(64, deadline).unwrap();
    assert_eq!(captured.bytes, b"private-partial");
    assert!(!captured.truncated);
    assert!(!captured.eof);
    assert!(child.try_exit().unwrap().is_none());
    let cleanup = MonotonicDeadline::after(Duration::from_secs(2));
    input.write_all_until(b"done\n", cleanup).unwrap();
    let remainder = stderr.drain_until(64, cleanup).unwrap();
    assert!(remainder.eof);
    assert!(
        child
            .wait_exact_child_until(cleanup)
            .unwrap()
            .unwrap()
            .success
    );
}

#[test]
fn subordinate_reap_does_not_imply_descendant_diagnostic_eof() {
    use lillux::time::MonotonicDeadline;
    // The background shell retains stderr and an explicit inherited input.
    // Releasing the parent-held input lets it exit without signalling a group.
    let mut child = subordinate_shell("exec 3<&0; (IFS= read -r value <&3) & exit 0");
    let mut input = child.take_input().unwrap();
    let mut stderr = child.take_error().unwrap();
    assert!(
        child
            .wait_exact_child_until(MonotonicDeadline::after(Duration::from_secs(2)))
            .unwrap()
            .unwrap()
            .success
    );
    let captured = stderr
        .drain_until(64, MonotonicDeadline::after(Duration::from_millis(30)))
        .unwrap();
    assert!(
        !captured.eof,
        "reaped leader is not diagnostic/descendant completion"
    );
    let cleanup = MonotonicDeadline::after(Duration::from_secs(2));
    input.write_all_until(b"release\n", cleanup).unwrap();
    assert!(stderr.drain_until(64, cleanup).unwrap().eof);
}

#[test]
fn configured_pipes_survive_vacant_standard_descriptors() {
    // Never close the concurrent test runner's own standard descriptors.
    for mode in ["stdin", "all", "pinned-stdin", "pinned-all"] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "configured_pipe_child", "--nocapture"])
            .env("LILLUX_PIPE_STDIO_PROBE", mode)
            .output()
            .unwrap();
        assert!(result.status.success(), "{mode}: {result:?}");
    }
}

#[test]
fn configured_pipe_child() {
    let Ok(mode) = std::env::var("LILLUX_PIPE_STDIO_PROBE") else {
        return;
    };
    let mut temp_guard = None;
    let pinned = if mode == "pinned-all" || mode == "pinned-stdin" {
        let temp = tempfile::tempdir().unwrap();
        let shell = temp.path().join("shell");
        std::fs::copy("/bin/sh", &shell).unwrap();
        let cwd = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let executable = cwd
            .open_pinned_regular(std::ffi::OsStr::new("shell"), false)
            .unwrap()
            .unwrap();
        temp_guard = Some(temp);
        Some((cwd, executable))
    } else {
        None
    };
    let count = if mode == "all" || mode == "pinned-all" {
        3
    } else {
        1
    };
    for fd in 0..count {
        // This test runs alone in its disposable child process.
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }
    let script = "IFS= read -r value; printf '%s' \"$value\"; printf 'private-stderr' >&2";
    let mut child = if let Some((cwd, executable)) = pinned {
        lillux::SubordinateProcess::spawn_pinned(lillux::PinnedSubordinateProcessRequest {
            executable,
            cwd,
            argv0: Some("sh".into()),
            args: vec!["-c".into(), script.into()],
            envs: Vec::new(),
            limits: None,
            inherited_fds: Vec::new(),
        })
    } else {
        lillux::SubordinateProcess::spawn(lillux::SubordinateProcessRequest {
            cmd: "/bin/sh".to_owned(),
            argv0: None,
            args: vec!["-c".into(), script.into()],
            cwd: "/".to_owned(),
            envs: Vec::new(),
            limits: None,
            inherited_fds: Vec::new(),
        })
    }
    .unwrap();
    let mut input = child.take_input().unwrap();
    let mut output = child.take_output().unwrap();
    let mut error = child.take_error().unwrap();
    input
        .write_all_until(
            b"pipe-survived\n",
            lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(2)),
        )
        .unwrap();
    drop(input);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    output.read_to_end(&mut stdout).unwrap();
    error.read_to_end(&mut stderr).unwrap();
    let status = child.wait_exact_child().unwrap();
    assert!(status.success);
    assert_eq!(stdout, b"pipe-survived");
    assert_eq!(stderr, b"private-stderr");
    drop(temp_guard);
}

#[test]
fn subordinate_input_backpressure_obeys_one_absolute_deadline() {
    let mut child = lillux::SubordinateProcess::spawn(lillux::SubordinateProcessRequest {
        cmd: "/bin/sh".to_owned(),
        argv0: None,
        args: vec!["-c".to_owned(), "exec sleep 30".to_owned()],
        cwd: "/".to_owned(),
        envs: Vec::new(),
        limits: None,
        inherited_fds: Vec::new(),
    })
    .unwrap();
    let mut input = child.take_input().unwrap();
    let error = input
        .write_all_until(
            &vec![b'x'; 8 * 1024 * 1024],
            lillux::time::MonotonicDeadline::after(Duration::from_millis(30)),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn diagnostic_drain_timeout_retains_owner_then_observes_eof() {
    use lillux::{SubordinateDiagnosticDrainEnd as End, time::MonotonicDeadline};
    let mut child = subordinate_shell("IFS= read -r value; printf private >&2");
    let mut input = child.take_input().unwrap();
    let mut drain = child.take_error().unwrap().start_discarding().unwrap();
    let expired = MonotonicDeadline::after(Duration::ZERO);
    assert_eq!(drain.finish_until(expired), None);
    assert_eq!(drain.finish_until(expired), None);
    assert!(child.try_exit().unwrap().is_none());
    let deadline = MonotonicDeadline::after(Duration::from_secs(2));
    input.write_all_until(b"finish\n", deadline).unwrap();
    assert_eq!(drain.finish_until(deadline), Some(End::Eof));
    assert_eq!(drain.cancel_until(expired), Some(End::Eof));
    assert!(
        child
            .wait_exact_child_until(deadline)
            .unwrap()
            .unwrap()
            .success
    );
}

#[test]
fn diagnostic_drain_cancels_silent_pipe_without_claiming_child_exit() {
    use lillux::{SubordinateDiagnosticDrainEnd as End, time::MonotonicDeadline};
    let mut child = subordinate_shell("IFS= read -r value");
    let mut input = child.take_input().unwrap();
    let mut drain = child.take_error().unwrap().start_discarding().unwrap();
    let deadline = MonotonicDeadline::after(Duration::from_secs(2));
    assert_eq!(drain.cancel_until(deadline), Some(End::Cancelled));
    assert!(child.try_exit().unwrap().is_none());
    assert_eq!(drain.finish_until(deadline), Some(End::Cancelled));
    input.write_all_until(b"finish\n", deadline).unwrap();
    assert!(
        child
            .wait_exact_child_until(deadline)
            .unwrap()
            .unwrap()
            .success
    );
}

#[test]
fn diagnostic_drain_cancellation_is_not_starved_by_continuous_output() {
    use lillux::{SubordinateDiagnosticDrainEnd as End, time::MonotonicDeadline};
    let mut child = subordinate_shell(
        "i=0; while [ $i -lt 20000 ]; do printf diagnostic >&2; i=$((i+1)); done; printf 'ready\\n'; while :; do printf diagnostic >&2 || exit 1; done",
    );
    let mut drain = child.take_error().unwrap().start_discarding().unwrap();
    let deadline = MonotonicDeadline::after(Duration::from_secs(5));
    assert_eq!(
        child
            .take_output()
            .unwrap()
            .read_frame_until(b'\n', 64, deadline)
            .unwrap(),
        b"ready\n"
    );
    assert_eq!(drain.cancel_until(deadline), Some(End::Cancelled));
    // Pipe closure is deliberately requested; this is not forceful termination
    // authority. The producer observes the closed reader and exits itself.
    assert!(
        !child
            .wait_exact_child_until(deadline)
            .unwrap()
            .unwrap()
            .success
    );
}

#[test]
fn dropped_diagnostic_owner_requests_cancellation_without_join_evidence() {
    use lillux::time::MonotonicDeadline;
    let mut child =
        subordinate_shell("IFS= read -r value; while :; do printf diagnostic >&2 || exit 1; done");
    let mut input = child.take_input().unwrap();
    let drain = child.take_error().unwrap().start_discarding().unwrap();
    drop(drain);
    let deadline = MonotonicDeadline::after(Duration::from_secs(2));
    input.write_all_until(b"start\n", deadline).unwrap();
    // Producer failure proves the reader eventually closed. It is not a join
    // receipt: dropping the owner deliberately abandoned that observation.
    assert!(
        !child
            .wait_exact_child_until(deadline)
            .unwrap()
            .unwrap()
            .success
    );
}

#[test]
fn subordinate_frame_limit_includes_delimiter_and_accepts_exact_eof() {
    use lillux::time::MonotonicDeadline;
    for (script, limit, expected) in [
        ("printf 'abc\\n'", 4, Some(b"abc\n".as_slice())),
        ("printf 'abc\\n'", 3, None),
        ("printf abc", 3, Some(b"abc".as_slice())),
        ("printf abcd", 3, None),
        ("printf '\\n'", 0, None),
        ("exit 0", 0, Some(b"".as_slice())),
    ] {
        let mut child = subordinate_shell(script);
        let deadline = MonotonicDeadline::after(Duration::from_secs(2));
        let result = child
            .take_output()
            .unwrap()
            .read_frame_until(b'\n', limit, deadline);
        match expected {
            Some(bytes) => assert_eq!(result.unwrap(), bytes, "script={script}, limit={limit}"),
            None => assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::InvalidData,
                "script={script}, limit={limit}"
            ),
        }
        assert!(
            child
                .wait_exact_child_until(deadline)
                .unwrap()
                .unwrap()
                .success
        );
    }
}

#[test]
fn subordinate_output_frame_does_not_renew_its_deadline_after_partial_data() {
    let mut child = lillux::SubordinateProcess::spawn(lillux::SubordinateProcessRequest {
        cmd: "/bin/sh".to_owned(),
        argv0: None,
        args: vec![
            "-c".to_owned(),
            "printf partial-output; exec sleep 30".to_owned(),
        ],
        cwd: "/".to_owned(),
        envs: Vec::new(),
        limits: None,
        inherited_fds: Vec::new(),
    })
    .unwrap();
    let mut output = child.take_output().unwrap();
    let error = output
        .read_frame_until(
            b'\n',
            1024,
            lillux::time::MonotonicDeadline::after(Duration::from_millis(30)),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}
