use std::process;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rye-proc", about = "Cross-platform process lifecycle manager")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Spawn a detached/daemonized child process
    Spawn {
        /// Command to execute
        #[arg(long)]
        cmd: String,

        /// Arguments (repeatable)
        #[arg(long = "arg")]
        args: Vec<String>,

        /// File to redirect stdout/stderr to (optional)
        #[arg(long)]
        log: Option<String>,

        /// Environment variable to set (KEY=VALUE, repeatable)
        #[arg(long = "env")]
        envs: Vec<String>,
    },

    /// Kill a process by PID (graceful then force)
    Kill {
        /// Process ID to kill
        #[arg(long)]
        pid: u32,

        /// Seconds to wait for graceful shutdown before force kill
        #[arg(long, default_value_t = 3.0)]
        grace: f64,
    },

    /// Check if a process is alive
    Status {
        /// Process ID to check
        #[arg(long)]
        pid: u32,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Spawn {
            cmd,
            args,
            log,
            envs,
        } => do_spawn(&cmd, &args, log.as_deref(), &envs),
        Command::Kill { pid, grace } => do_kill(pid, grace),
        Command::Status { pid } => do_status(pid),
    };

    println!("{}", result);
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

fn do_spawn(cmd: &str, args: &[String], log: Option<&str>, envs: &[String]) -> serde_json::Value {
    let result = spawn_detached(cmd, args, log, envs);
    match result {
        Ok(pid) => serde_json::json!({ "success": true, "pid": pid }),
        Err(e) => serde_json::json!({ "success": false, "error": e }),
    }
}

#[cfg(unix)]
fn spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[String],
) -> Result<u32, String> {
    use std::fs::OpenOptions;
    use std::os::unix::process::CommandExt;

    let mut command = process::Command::new(cmd);
    command.args(args);

    // Set environment variables
    for env in envs {
        if let Some((key, value)) = env.split_once('=') {
            command.env(key, value);
        }
    }

    // Redirect I/O
    command.stdin(process::Stdio::null());
    if let Some(log_path) = log {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)
            .map_err(|e| format!("Failed to open log file: {e}"))?;
        let file2 = file.try_clone().map_err(|e| format!("Failed to clone log fd: {e}"))?;
        command.stdout(file);
        command.stderr(file2);
    } else {
        command.stdout(process::Stdio::null());
        command.stderr(process::Stdio::null());
    }

    // Create new session so child survives parent exit
    // SAFETY: setsid is async-signal-safe
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let child = command.spawn().map_err(|e| format!("Failed to spawn: {e}"))?;
    Ok(child.id())
}

#[cfg(windows)]
fn spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[String],
) -> Result<u32, String> {
    use std::fs::OpenOptions;
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const DETACHED_PROCESS: u32 = 0x00000008;

    let mut command = process::Command::new(cmd);
    command.args(args);

    for env in envs {
        if let Some((key, value)) = env.split_once('=') {
            command.env(key, value);
        }
    }

    command.stdin(process::Stdio::null());
    if let Some(log_path) = log {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)
            .map_err(|e| format!("Failed to open log file: {e}"))?;
        let file2 = file.try_clone().map_err(|e| format!("Failed to clone log handle: {e}"))?;
        command.stdout(file);
        command.stderr(file2);
    } else {
        command.stdout(process::Stdio::null());
        command.stderr(process::Stdio::null());
    }

    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);

    let child = command.spawn().map_err(|e| format!("Failed to spawn: {e}"))?;
    Ok(child.id())
}

// ---------------------------------------------------------------------------
// Kill
// ---------------------------------------------------------------------------

fn do_kill(pid: u32, grace: f64) -> serde_json::Value {
    match kill_process(pid, grace) {
        Ok(method) => serde_json::json!({ "success": true, "pid": pid, "method": method }),
        Err(e) => serde_json::json!({ "success": false, "pid": pid, "error": e }),
    }
}

#[cfg(unix)]
fn kill_process(pid: u32, grace: f64) -> Result<&'static str, String> {
    let pid = pid as i32;

    // Check if alive first
    if unsafe { libc::kill(pid, 0) } != 0 {
        return Ok("already_dead");
    }

    // Send SIGTERM
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(format!(
            "SIGTERM failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Wait for graceful shutdown
    let polls = (grace / 0.1).ceil() as u32;
    for _ in 0..polls {
        thread::sleep(Duration::from_millis(100));
        if unsafe { libc::kill(pid, 0) } != 0 {
            return Ok("terminated");
        }
    }

    // Force kill
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        // May have died between check and kill
        if unsafe { libc::kill(pid, 0) } != 0 {
            return Ok("terminated");
        }
        return Err(format!(
            "SIGKILL failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    Ok("killed")
}

#[cfg(windows)]
fn kill_process(pid: u32, grace: f64) -> Result<&'static str, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject,
        PROCESS_QUERY_INFORMATION, PROCESS_TERMINATE, SYNCHRONIZE,
    };

    let access = PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE;
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle == 0 {
        return Ok("already_dead");
    }

    // Try graceful wait first (Windows has no SIGTERM equivalent for arbitrary
    // processes, but we give it a grace period in case it exits on its own)
    let grace_ms = (grace * 1000.0) as u32;
    let wait_result = unsafe { WaitForSingleObject(handle, grace_ms) };
    if wait_result == WAIT_OBJECT_0 {
        unsafe { CloseHandle(handle) };
        return Ok("terminated");
    }

    // Force terminate
    let ok = unsafe { TerminateProcess(handle, 1) };
    unsafe { CloseHandle(handle) };

    if ok != 0 {
        Ok("killed")
    } else {
        Err(format!(
            "TerminateProcess failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

fn do_status(pid: u32) -> serde_json::Value {
    let alive = is_alive(pid);
    serde_json::json!({ "pid": pid, "alive": alive })
}

#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_INFORMATION, SYNCHRONIZE,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle == 0 {
        return false;
    }
    // Poll with 0ms timeout — returns WAIT_TIMEOUT (258) if still running
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    result != 0 // WAIT_OBJECT_0 (0) means exited, anything else means alive
}
