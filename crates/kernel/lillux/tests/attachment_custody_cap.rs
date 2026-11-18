#![cfg(target_os = "linux")]
//! Own test binary: this test holds the process-wide fork lease for ~5 s,
//! which would fail concurrent spawns in any binary that shares the process.

use std::time::Duration;

use lillux::{SubprocessRequest, spawn_awaiting_attachment, try_retain_fork_sensitive_descriptors};

fn shell(script: String) -> SubprocessRequest {
    SubprocessRequest {
        cmd: "/bin/sh".to_string(),
        argv0: None,
        args: vec!["-c".to_string(), script],
        cwd: None,
        envs: vec![],
        stdin_data: None,
        timeout: 30.0,
        limits: None,
        inherited_fds: Vec::new(),
        inherited_fd_mappings: Vec::new(),
        supervised_status: None,
    }
}

// Takes a real ~5 s wait: the fork-custody bound is the fixed 5 s cap, and
// the only way to show it ignores a long request timeout is to wait it out.
// There is no seam for a held child that never reports READY (the child is
// Lillux's own fork), so this covers the custody wait that holds the barrier.
#[test]
fn custody_wait_keeps_five_second_cap_under_a_long_request_timeout() {
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let lease = try_retain_fork_sensitive_descriptors().unwrap();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(lease);
    });
    held_rx.recv().unwrap();

    let mut request = shell("exit 0".to_string());
    request.timeout = 600.0;
    let started = std::time::Instant::now();
    let result = spawn_awaiting_attachment(request);
    let elapsed = started.elapsed();
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    match result {
        Ok(pending) => {
            pending.abort_and_reap().expect("abort unexpected target");
            panic!("spawn must refuse while a lease stays held past the 5 s cap");
        }
        Err(error) => assert!(error.stderr.contains("Failed to spawn"), "{}", error.stderr),
    }
    assert!(
        elapsed >= Duration::from_secs(4) && elapsed < Duration::from_secs(15),
        "custody wait must follow the 5 s cap, not the 600 s request: {elapsed:?}"
    );
}
