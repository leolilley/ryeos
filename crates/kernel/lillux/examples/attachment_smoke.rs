#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use lillux::{is_alive, spawn_awaiting_attachment, SubprocessRequest};

#[cfg(target_os = "linux")]
fn shell_write(marker: &Path) -> SubprocessRequest {
    SubprocessRequest {
        cmd: "/bin/sh".to_string(),
        argv0: None,
        args: vec![
            "-c".to_string(),
            "printf executed > \"$LILLUX_ATTACHMENT_SMOKE_MARKER\"".to_string(),
        ],
        cwd: None,
        envs: vec![(
            "LILLUX_ATTACHMENT_SMOKE_MARKER".to_string(),
            marker.as_os_str().to_string_lossy().into_owned(),
        )],
        stdin_data: None,
        timeout: 30.0,
        limits: None,
        inherited_fds: Vec::new(),
        supervised_status: None,
    }
}

#[cfg(target_os = "linux")]
fn marker_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "lillux-attachment-smoke-{}-{label}",
        std::process::id()
    ))
}

#[cfg(target_os = "linux")]
fn remove_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), String> {
    let mut require_pid_1 = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--require-pid-1" if !require_pid_1 => require_pid_1 = true,
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let own_pid = std::process::id();
    if require_pid_1 && own_pid != 1 {
        return Err(format!(
            "expected smoke process PID 1, observed PID {own_pid}"
        ));
    }

    let release_marker = marker_path("release");
    remove_if_present(&release_marker)?;
    let pending = spawn_awaiting_attachment(shell_write(&release_marker))
        .map_err(|error| format!("spawn release target: {}", error.stderr))?;
    std::thread::sleep(Duration::from_millis(100));
    if release_marker.exists() {
        return Err("target executed before attachment release".to_string());
    }
    if pending.pid() as i64 != pending.pgid() {
        return Err(format!(
            "attachment identity is not a process-group leader: pid={}, pgid={}",
            pending.pid(),
            pending.pgid()
        ));
    }
    let released_pid = pending.pid();
    let result = pending
        .release_after_attachment()
        .map_err(|error| format!("release target: {}", error.result.stderr))?
        .wait();
    if !result.success {
        return Err(format!("released target failed: {}", result.stderr));
    }
    if result.pid != released_pid
        || std::fs::read_to_string(&release_marker).ok().as_deref() != Some("executed")
    {
        return Err("released target did not execute with its pinned identity".to_string());
    }
    remove_if_present(&release_marker)?;

    let abort_marker = marker_path("abort");
    remove_if_present(&abort_marker)?;
    let pending = spawn_awaiting_attachment(shell_write(&abort_marker))
        .map_err(|error| format!("spawn abort target: {}", error.stderr))?;
    let aborted_pid = pending.pid();
    let aborted = pending
        .abort_and_reap()
        .map_err(|error| format!("abort target: {error}"))?;
    if aborted.pid != aborted_pid || abort_marker.exists() || is_alive(aborted_pid) {
        return Err("aborted target executed or was not reaped".to_string());
    }

    println!("lillux attachment smoke passed (pid {own_pid})");
    Ok(())
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = run() {
        eprintln!("lillux attachment smoke failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("lillux attachment smoke is supported only on Linux");
    std::process::exit(2);
}
