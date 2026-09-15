#![cfg(target_os = "linux")]

use std::io::Read;
use std::sync::mpsc;
use std::time::Duration;

use lillux::{OutputLimitExceeded, SubprocessLimits, SubprocessRequest};

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
        timeout: 3.0,
        limits: None,
        inherited_fds: vec![],
        inherited_fd_mappings: vec![],
        supervised_status: None,
    }
}

#[test]
fn scoped_stdout_observer_settles_silent_child_at_existing_deadline() {
    let mut request = shell("/bin/sleep 30");
    request.timeout = 0.1;
    let process = lillux::spawn(request).unwrap();
    let (completion, observation) = process.wait_with_stdout(|mut reader| {
        let mut bytes = vec![];
        reader.read_to_end(&mut bytes).map(|_| bytes)
    });
    assert!(completion.timed_out, "{completion:?}");
    assert!(observation.unwrap().is_empty());
}

#[test]
fn scoped_stdout_observer_failure_and_panic_interrupt_exact_wait() {
    for panic in [false, true] {
        let process = lillux::spawn(shell("printf broken; /bin/sleep 30")).unwrap();
        let (completion, observation) = process.wait_with_stdout(|mut reader| {
            reader.read_exact(&mut [0; 6]).unwrap();
            if panic {
                panic!("fixture observation panic");
            }
            Err::<(), _>("fixture publication refusal")
        });
        assert!(!completion.success);
        assert!(
            !completion.timed_out,
            "failure must interrupt before deadline"
        );
        assert!(completion.stderr.contains("process observation failed"));
        if panic {
            assert!(matches!(
                observation,
                Err(lillux::ProcessObservationError::Panicked)
            ));
        } else {
            assert!(matches!(
                observation,
                Err(lillux::ProcessObservationError::Observation(
                    "fixture publication refusal"
                ))
            ));
        }
    }
}

#[test]
fn scoped_stdout_observer_refuses_duplicate_without_stranding_child() {
    let mut process = lillux::spawn(shell("/bin/sleep 30")).unwrap();
    let mut original = process.take_stdout_reader().unwrap();
    let (completion, observation) = process.wait_with_stdout(|_| Ok::<(), ()>(()));
    assert!(!completion.success);
    assert!(!completion.timed_out);
    assert!(matches!(
        observation,
        Err(lillux::ProcessObservationError::AlreadyConsumed)
    ));
    assert_eq!(original.read(&mut [0]).unwrap(), 0);
}

#[test]
fn fatal_observer_failure_interrupts_the_existing_wait_owner() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let mut process = lillux::spawn(shell("printf broken; /bin/sleep 30")).unwrap();
    let mut reader = process.take_stdout_reader().unwrap();
    let failed = Arc::new(AtomicBool::new(false));
    let observer_failed = Arc::clone(&failed);
    let observer = std::thread::spawn(move || {
        let mut bytes = [0; 6];
        reader.read_exact(&mut bytes).unwrap();
        observer_failed.store(true, Ordering::Release);
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
    });
    let completion = process.wait_interruptible(|| failed.load(Ordering::Acquire));
    assert!(!completion.success);
    assert!(
        !completion.timed_out,
        "observer failure must not wait for deadline"
    );
    assert!(completion.stderr.contains("process observation failed"));
    observer.join().unwrap();
}

#[test]
fn raw_stdout_observer_preserves_binary_header_before_process_exit() {
    let mut process = lillux::spawn(shell("printf '\\000\\000\\000\\200'; /bin/sleep 5"))
        .expect("spawn binary-output fixture");
    let mut reader = process.take_stdout_reader().expect("first stdout observer");
    assert!(process.take_stdout_reader().is_none());
    let (sent, received) = mpsc::channel();
    let observer = std::thread::spawn(move || {
        let mut header = [0; 4];
        let result = reader.read_exact(&mut header);
        sent.send((result, header)).unwrap();
    });
    let (result, header) = received
        .recv_timeout(Duration::from_secs(2))
        .expect("header must arrive while child is still running");
    result.unwrap();
    assert_eq!(header, [0, 0, 0, 0x80]);
    let process = match process.wait_for_natural_exit(Duration::ZERO) {
        Err(process) => process,
        Ok(result) => panic!("observer waited for process exit: {result:?}"),
    };
    process.abort_and_reap_checked().unwrap();
    observer.join().unwrap();
}

#[test]
fn raw_stdout_observer_retains_unread_bytes_after_settlement() {
    let mut process =
        lillux::spawn(shell("printf '\\377\\000\\200end'")).expect("spawn binary-output fixture");
    let mut reader = process.take_stdout_reader().unwrap();
    let result = process.wait();
    assert!(result.success, "{result:?}");
    let mut bytes = vec![];
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"\xff\0\x80end");
    assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
}

#[test]
fn raw_stdout_observer_reports_existing_output_bound() {
    let mut request = shell("printf abcdefgh");
    request.limits = Some(SubprocessLimits {
        max_stdout_bytes: Some(3),
        ..Default::default()
    });
    let mut process = lillux::spawn(request).unwrap();
    let mut reader = process.take_stdout_reader().unwrap();
    let result = process.wait();
    assert!(!result.success);
    assert_eq!(
        result.output_limit_exceeded,
        Some(OutputLimitExceeded::Stdout)
    );
    let mut bytes = vec![];
    let error = reader.read_to_end(&mut bytes).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(bytes, b"abc");
}

#[test]
fn raw_stdout_observer_is_woken_by_abort_without_owning_process() {
    let mut process = lillux::spawn(shell("/bin/sleep 5")).unwrap();
    let mut reader = process.take_stdout_reader().unwrap();
    let (sent, received) = mpsc::channel();
    let observer = std::thread::spawn(move || {
        sent.send(reader.read(&mut [0; 1])).unwrap();
    });
    process.abort_and_reap_checked().unwrap();
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap(),
        0
    );
    observer.join().unwrap();
}

#[test]
fn raw_stdout_eof_does_not_settle_a_running_process() {
    let mut process = lillux::spawn(shell("exec 1>&-; /bin/sleep 5")).unwrap();
    let mut reader = process.take_stdout_reader().unwrap();
    let (sent, received) = mpsc::channel();
    let observer = std::thread::spawn(move || {
        sent.send(reader.read(&mut [0; 1])).unwrap();
    });
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap(),
        0
    );
    let process = match process.wait_for_natural_exit(Duration::ZERO) {
        Err(process) => process,
        Ok(result) => panic!("stdout EOF incorrectly settled process: {result:?}"),
    };
    process.abort_and_reap_checked().unwrap();
    observer.join().unwrap();
}

#[test]
fn dropping_raw_stdout_observer_does_not_stop_capture() {
    let mut process = lillux::spawn(shell("printf retained")).unwrap();
    drop(process.take_stdout_reader().unwrap());
    let result = process.wait();
    assert!(result.success, "{result:?}");
    assert_eq!(result.stdout, "retained");
}

#[test]
fn raw_stdout_observer_is_woken_by_existing_process_deadline() {
    let mut request = shell("/bin/sleep 5");
    request.timeout = 0.1;
    let mut process = lillux::spawn(request).unwrap();
    let mut reader = process.take_stdout_reader().unwrap();
    let owner = std::thread::spawn(move || process.wait());
    assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
    let result = owner.join().unwrap();
    assert!(result.timed_out);
    assert!(!result.success);
}

#[test]
fn raw_stdout_observer_uses_the_existing_attachment_release() {
    let pending = lillux::spawn_awaiting_attachment(shell("printf '\\200attached'"))
        .expect("spawn held fixture");
    // There is deliberately no observer or alternate execution path on the
    // pending authority. Only the existing release yields a running process.
    let mut process = pending.release_after_attachment().unwrap();
    let mut reader = process.take_stdout_reader().unwrap();
    let owner = std::thread::spawn(move || process.wait());
    let mut bytes = vec![];
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"\x80attached");
    assert!(owner.join().unwrap().success);
}
