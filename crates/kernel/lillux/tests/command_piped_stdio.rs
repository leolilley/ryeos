#![cfg(unix)]

use std::io::Write as _;
use std::process::Command;

#[test]
fn configured_pipes_survive_vacant_standard_descriptors() {
    // Never close the concurrent test runner's own standard descriptors.
    for mode in ["stdin", "all"] {
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
    let count = if mode == "all" { 3 } else { 1 };
    for fd in 0..count {
        // This test runs alone in its disposable child process.
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "IFS= read -r value; printf '%s' \"$value\"; printf 'private-stderr' >&2",
    ]);
    lillux::configure_command_piped_stdio(&mut command);
    lillux::configure_owner_private_creation_mask(&mut command);
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"pipe-survived\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"pipe-survived");
    assert_eq!(output.stderr, b"private-stderr");
}
