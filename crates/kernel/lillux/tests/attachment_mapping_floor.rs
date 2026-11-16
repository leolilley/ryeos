#![cfg(target_os = "linux")]
//! Its own test binary, so this process holds no other pending child whose
//! control descriptors the new child would also have to close: the test is
//! about the launch's own attachment coordinates and must be deterministic.

use lillux::{SubprocessRequest, spawn_awaiting_attachment};

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
#[test]
fn mapped_targets_at_the_lowest_free_descriptors_leave_attachment_control_intact() {
    use std::os::fd::AsRawFd as _;

    // The attachment gate's own pipes and cwd used to take the lowest free
    // descriptors after the mappings were prepared, so a mapping target there
    // was refused as aliasing process-control authority. Map a dense range
    // starting at the lowest free descriptor: every gate coordinate must now
    // sit above it and the launch must succeed with every mapping installed.
    // The sources are opened FIRST, so they take the low numbers; the
    // targets are then the lowest numbers still free in this parent, which is
    // exactly where a newly created control pipe or cwd descriptor would land.
    let count = 16_u32;
    let sources: Vec<_> = (0..count)
        .map(|_| {
            lillux::sealed_memfd(c"ryeos-mapping-probe", b"mapped").expect("sealed memfd")
        })
        .collect();
    let probe = std::fs::File::open("/dev/null").expect("probe descriptor");
    let lowest = u32::try_from(probe.as_raw_fd()).unwrap().max(3);
    drop(probe);
    let last = lowest + count - 1;
    let mut request = shell(format!(
        "[ -e /proc/self/fd/{lowest} ] && [ -e /proc/self/fd/{last} ]"
    ));
    request.cwd = Some("/".to_string());
    for (offset, source) in (0..count).zip(&sources) {
        source
            .bind_to_subprocess_request(&mut request, lowest + offset)
            .expect("bind mapping");
    }
    drop(sources);
    let pending = spawn_awaiting_attachment(request).expect("spawn awaiting attachment");
    let result = pending
        .release_after_attachment()
        .expect("release after attachment")
        .wait();
    assert!(result.success, "stderr: {}", result.stderr);
}
