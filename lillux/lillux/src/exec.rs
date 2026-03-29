use std::io::{Read, Write};
use std::process::{self, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::Subcommand;

#[derive(Subcommand)]
pub enum ExecAction {
    /// Run a command, wait for completion, capture output
    Run {
        #[arg(long)]
        cmd: String,
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        stdin: Option<String>,
        #[arg(long)]
        stdin_pipe: bool,
        #[arg(long = "env")]
        envs: Vec<String>,
        #[arg(long, default_value_t = 300.0)]
        timeout: f64,
    },
    /// Spawn a detached/daemonized child process
    Spawn {
        #[arg(long)]
        cmd: String,
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long)]
        log: Option<String>,
        #[arg(long = "env")]
        envs: Vec<String>,
        #[arg(long)]
        stdin: Option<String>,
        #[arg(long)]
        stdin_pipe: bool,
    },
    /// Kill a process by PID
    Kill {
        #[arg(long)]
        pid: u32,
        #[arg(long, default_value_t = 3.0)]
        grace: f64,
    },
    /// Check if a process is alive
    Status {
        #[arg(long)]
        pid: u32,
    },
}

fn resolve_stdin(stdin_arg: Option<String>, stdin_pipe: bool) -> Option<String> {
    if let Some(data) = stdin_arg {
        return Some(data);
    }
    if stdin_pipe {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        if !buf.is_empty() { return Some(buf); }
    }
    None
}

fn set_envs(command: &mut process::Command, envs: &[String]) {
    for env in envs {
        if let Some((k, v)) = env.split_once('=') { command.env(k, v); }
    }
}

fn write_stdin(child: &mut process::Child, data: Option<&str>) {
    if let Some(data) = data {
        if let Some(mut s) = child.stdin.take() { let _ = s.write_all(data.as_bytes()); }
    }
}

fn setup_log(command: &mut process::Command, log: Option<&str>) -> Result<(), String> {
    if let Some(path) = log {
        let file = std::fs::OpenOptions::new()
            .create(true).write(true).truncate(true).open(path)
            .map_err(|e| format!("Failed to open log file: {e}"))?;
        let file2 = file.try_clone().map_err(|e| format!("Failed to clone log fd: {e}"))?;
        command.stdout(file).stderr(file2);
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    Ok(())
}

pub fn run(action: ExecAction) -> serde_json::Value {
    match action {
        ExecAction::Run { cmd, args, cwd, stdin, stdin_pipe, envs, timeout } => {
            do_exec(&cmd, &args, cwd.as_deref(), resolve_stdin(stdin, stdin_pipe).as_deref(), &envs, timeout)
        }
        ExecAction::Spawn { cmd, args, log, envs, stdin, stdin_pipe } => {
            match spawn_detached(&cmd, &args, log.as_deref(), &envs, resolve_stdin(stdin, stdin_pipe).as_deref()) {
                Ok(pid) => serde_json::json!({ "success": true, "pid": pid }),
                Err(e) => serde_json::json!({ "success": false, "error": e }),
            }
        }
        ExecAction::Kill { pid, grace } => match kill_process(pid, grace) {
            Ok(method) => serde_json::json!({ "success": true, "pid": pid, "method": method }),
            Err(e) => serde_json::json!({ "success": false, "pid": pid, "error": e }),
        },
        ExecAction::Status { pid } => serde_json::json!({ "pid": pid, "alive": is_alive(pid) }),
    }
}

fn do_exec(cmd: &str, args: &[String], cwd: Option<&str>, stdin_data: Option<&str>, envs: &[String], timeout: f64) -> serde_json::Value {
    let start = Instant::now();
    let mut command = process::Command::new(cmd);
    command.args(args);
    set_envs(&mut command, envs);
    if let Some(dir) = cwd { command.current_dir(dir); }
    command.stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() });
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return serde_json::json!({
            "success": false, "stdout": "", "stderr": format!("Failed to spawn: {e}"),
            "return_code": -1, "duration_ms": start.elapsed().as_secs_f64() * 1000.0,
        }),
    };
    write_stdin(&mut child, stdin_data);

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();
    let stdout_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout_handle { let _ = out.read_to_end(&mut buf); }
        buf
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut err) = stderr_handle { let _ = err.read_to_end(&mut buf); }
        buf
    });

    let timeout_dur = Duration::from_secs_f64(timeout);
    let (tx, rx) = std::sync::mpsc::channel();
    let _timer = thread::spawn(move || { thread::sleep(timeout_dur); let _ = tx.send(()); });

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (out, err) = (stdout_thread.join().unwrap_or_default(), stderr_thread.join().unwrap_or_default());
                let code = status.code().unwrap_or(-1);
                return serde_json::json!({
                    "success": code == 0, "stdout": String::from_utf8_lossy(&out),
                    "stderr": String::from_utf8_lossy(&err), "return_code": code,
                    "duration_ms": start.elapsed().as_secs_f64() * 1000.0,
                });
            }
            Ok(None) => {
                if rx.try_recv().is_ok() {
                    let _ = child.kill();
                    let _ = child.wait();
                    let (out, err) = (stdout_thread.join().unwrap_or_default(), stderr_thread.join().unwrap_or_default());
                    return serde_json::json!({
                        "success": false, "stdout": String::from_utf8_lossy(&out),
                        "stderr": format!("Command timed out after {timeout} seconds\n{}", String::from_utf8_lossy(&err)),
                        "return_code": -1, "duration_ms": start.elapsed().as_secs_f64() * 1000.0,
                    });
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = (stdout_thread.join(), stderr_thread.join());
                return serde_json::json!({
                    "success": false, "stdout": "", "stderr": format!("Wait failed: {e}"),
                    "return_code": -1, "duration_ms": start.elapsed().as_secs_f64() * 1000.0,
                });
            }
        }
    }
}

#[cfg(unix)]
fn spawn_detached(cmd: &str, args: &[String], log: Option<&str>, envs: &[String], stdin_data: Option<&str>) -> Result<u32, String> {
    use std::os::unix::process::CommandExt;
    let mut command = process::Command::new(cmd);
    command.args(args);
    set_envs(&mut command, envs);
    command.stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() });
    setup_log(&mut command, log)?;
    unsafe { command.pre_exec(|| { libc::setsid(); Ok(()) }); }
    let mut child = command.spawn().map_err(|e| format!("Failed to spawn: {e}"))?;
    write_stdin(&mut child, stdin_data);
    Ok(child.id())
}

#[cfg(windows)]
fn spawn_detached(cmd: &str, args: &[String], log: Option<&str>, envs: &[String], stdin_data: Option<&str>) -> Result<u32, String> {
    use std::os::windows::process::CommandExt;
    let mut command = process::Command::new(cmd);
    command.args(args);
    set_envs(&mut command, envs);
    command.stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() });
    setup_log(&mut command, log)?;
    command.creation_flags(0x00000200 | 0x00000008); // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
    let mut child = command.spawn().map_err(|e| format!("Failed to spawn: {e}"))?;
    write_stdin(&mut child, stdin_data);
    Ok(child.id())
}

#[cfg(unix)]
fn kill_process(pid: u32, grace: f64) -> Result<&'static str, String> {
    let pid = pid as i32;
    if unsafe { libc::kill(pid, 0) } != 0 { return Ok("already_dead"); }
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(format!("SIGTERM failed: {}", std::io::Error::last_os_error()));
    }
    for _ in 0..(grace / 0.1).ceil() as u32 {
        thread::sleep(Duration::from_millis(100));
        if unsafe { libc::kill(pid, 0) } != 0 { return Ok("terminated"); }
    }
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        if unsafe { libc::kill(pid, 0) } != 0 { return Ok("terminated"); }
        return Err(format!("SIGKILL failed: {}", std::io::Error::last_os_error()));
    }
    Ok("killed")
}

#[cfg(windows)]
fn kill_process(pid: u32, grace: f64) -> Result<&'static str, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::*;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
    if handle == 0 { return Ok("already_dead"); }
    if unsafe { WaitForSingleObject(handle, (grace * 1000.0) as u32) } == WAIT_OBJECT_0 {
        unsafe { CloseHandle(handle) };
        return Ok("terminated");
    }
    let ok = unsafe { TerminateProcess(handle, 1) };
    unsafe { CloseHandle(handle) };
    if ok != 0 { Ok("killed") } else { Err(format!("TerminateProcess failed: {}", std::io::Error::last_os_error())) }
}

#[cfg(unix)]
fn is_alive(pid: u32) -> bool { unsafe { libc::kill(pid as i32, 0) == 0 } }

#[cfg(windows)]
fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::*;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle == 0 { return false; }
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    result != 0
}
