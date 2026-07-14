use std::io::{Read, Write};
use std::process::{self, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::Subcommand;

// ---------------------------------------------------------------------------
// Library types — clean Rust API, no JSON
// ---------------------------------------------------------------------------

/// Request to run a subprocess synchronously.
///
/// Env handling: `envs` is **authoritative**. The runner clears the
/// subprocess environment with `Command::env_clear()` before applying
/// `envs`, so callers MUST populate every env var the subprocess
/// needs. Inheriting parent env is not supported. This contract
/// closes the secret-leak hole where shell-exported variables on the
/// daemon process bypassed `required_secrets` scoping.
pub struct SubprocessRequest {
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub envs: Vec<(String, String)>,
    pub stdin_data: Option<String>,
    pub timeout: f64,
    /// Optional limits installed in the child immediately before `exec`.
    pub limits: Option<SubprocessLimits>,
    /// Open descriptors intentionally kept alive and inherited through exec.
    /// Lillux retains the handles and clears `FD_CLOEXEC` only in the forked
    /// child. The sandbox uses these only for Bubblewrap's fd-based mounts.
    pub inherited_fds: Vec<std::sync::Arc<std::fs::File>>,
}

/// Resource limits applied to a spawned subprocess.
///
/// Limits are fail-closed: a configured limit that is unsupported, invalid,
/// or cannot be installed prevents the subprocess from spawning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubprocessLimits {
    /// Maximum number of file descriptors the subprocess may open.
    pub max_open_files: Option<u64>,
}

/// Result of a synchronous subprocess execution.
pub struct SubprocessResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: f64,
    pub pid: u32,
    pub timed_out: bool,
}

/// Validate retained descriptors and make them inheritable only inside this
/// command's forked child. Callers must keep the Arc handles alive through
/// `spawn`/`status`.
pub fn configure_inherited_fds(
    command: &mut process::Command,
    inherited_fds: &[std::sync::Arc<std::fs::File>],
) -> Result<(), String> {
    #[cfg(not(unix))]
    {
        let _ = command;
        if inherited_fds.is_empty() {
            return Ok(());
        }
        return Err("inherited descriptors are unsupported on this platform".to_string());
    }

    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::process::CommandExt as _;

        let mut raw = Vec::with_capacity(inherited_fds.len());
        for file in inherited_fds {
            let fd = file.as_raw_fd();
            if fd <= libc::STDERR_FILENO {
                return Err(format!("inherited descriptor {fd} overlaps stdio"));
            }
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                return Err(format!(
                    "inherited descriptor {fd} cannot be inspected: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if flags & libc::FD_CLOEXEC == 0 {
                return Err(format!(
                    "inherited descriptor {fd} is not protected by FD_CLOEXEC"
                ));
            }
            raw.push(fd);
        }
        unsafe {
            command.pre_exec(move || {
                for fd in &raw {
                    let flags = libc::fcntl(*fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }
}

/// Result of a detached spawn.
pub struct SpawnResult {
    pub pid: u32,
}

/// A running subprocess that can be waited on later.
pub struct RunningProcess {
    pub pid: u32,
    pub pgid: i64,
    child: process::Child,
    stdout_thread: thread::JoinHandle<Vec<u8>>,
    stderr_thread: thread::JoinHandle<Vec<u8>>,
    start: Instant,
    timeout: f64,
}

impl RunningProcess {
    /// Wait for the process to finish (or time out) and return the result.
    pub fn wait(mut self) -> SubprocessResult {
        let timeout_rx = if self.timeout > 0.0 {
            let timeout_dur = Duration::from_secs_f64(self.timeout);
            let (tx, rx) = std::sync::mpsc::channel();
            let _timer = thread::spawn(move || {
                thread::sleep(timeout_dur);
                let _ = tx.send(());
            });
            Some(rx)
        } else {
            None
        };

        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    let (out, err) = (
                        self.stdout_thread.join().unwrap_or_default(),
                        self.stderr_thread.join().unwrap_or_default(),
                    );
                    let code = status.code().unwrap_or(-1);
                    return SubprocessResult {
                        success: code == 0,
                        stdout: String::from_utf8_lossy(&out).into_owned(),
                        stderr: String::from_utf8_lossy(&err).into_owned(),
                        exit_code: code,
                        duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
                        pid: self.pid,
                        timed_out: false,
                    };
                }
                Ok(None) => {
                    if timeout_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
                        #[cfg(unix)]
                        {
                            // Kill the entire process group (child + grandchildren)
                            unsafe {
                                libc::kill(-(self.pgid as i32), libc::SIGKILL);
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = self.child.kill();
                        }
                        let _ = self.child.wait();
                        let (out, err) = (
                            self.stdout_thread.join().unwrap_or_default(),
                            self.stderr_thread.join().unwrap_or_default(),
                        );
                        return SubprocessResult {
                            success: false,
                            stdout: String::from_utf8_lossy(&out).into_owned(),
                            stderr: format!(
                                "Command timed out after {} seconds\n{}",
                                self.timeout,
                                String::from_utf8_lossy(&err)
                            ),
                            exit_code: -1,
                            duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
                            pid: self.pid,
                            timed_out: true,
                        };
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    let _ = (self.stdout_thread.join(), self.stderr_thread.join());
                    return SubprocessResult {
                        success: false,
                        stdout: String::new(),
                        stderr: format!("Wait failed: {e}"),
                        exit_code: -1,
                        duration_ms: self.start.elapsed().as_secs_f64() * 1000.0,
                        pid: self.pid,
                        timed_out: false,
                    };
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Library functions — public API for in-process callers
// ---------------------------------------------------------------------------

/// Spawn a subprocess and return a handle that can be waited on later.
pub fn lib_spawn(request: SubprocessRequest) -> Result<RunningProcess, SubprocessResult> {
    let start = Instant::now();
    let timeout = request.timeout;

    #[cfg(unix)]
    let inherited_fds = {
        use std::os::fd::AsRawFd as _;

        let mut raw = Vec::with_capacity(request.inherited_fds.len());
        for file in &request.inherited_fds {
            let fd = file.as_raw_fd();
            if fd <= libc::STDERR_FILENO {
                return Err(spawn_failure(
                    start,
                    format!("Failed to spawn: inherited descriptor {fd} overlaps stdio"),
                ));
            }
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: inherited descriptor {fd} cannot be inspected: {}",
                        std::io::Error::last_os_error()
                    ),
                ));
            }
            if flags & libc::FD_CLOEXEC == 0 {
                return Err(spawn_failure(
                    start,
                    format!(
                        "Failed to spawn: inherited descriptor {fd} is not protected by FD_CLOEXEC"
                    ),
                ));
            }
            raw.push(fd);
        }
        raw
    };
    #[cfg(not(unix))]
    if !request.inherited_fds.is_empty() {
        return Err(spawn_failure(
            start,
            "Failed to spawn: inherited descriptors are unsupported on this platform",
        ));
    }

    let envs_str: Vec<String> = request
        .envs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    let mut command = process::Command::new(&request.cmd);
    command.args(&request.args);
    command.env_clear();
    set_envs(&mut command, &envs_str);
    if let Some(ref dir) = request.cwd {
        command.current_dir(dir);
    }
    command.stdin(if request.stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    // Keep caller-pinned descriptors alive through `Command::spawn`. They stay
    // CLOEXEC in the multithreaded parent and are made inheritable only in the
    // forked child, preventing unrelated concurrent spawns from receiving them.
    let _inherited_fds = &request.inherited_fds;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in &inherited_fds {
                    let flags = libc::fcntl(*fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    if let Err(reason) = configure_subprocess_limits(&mut command, request.limits.as_ref()) {
        return Err(spawn_failure(
            start,
            format!("Failed to spawn: invalid resource limits: {reason}"),
        ));
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Err(spawn_failure(start, format!("Failed to spawn: {e}"))),
    };
    let pid = child.id();

    // On Unix with setsid, pid == pgid since the child is its own process group leader.
    #[cfg(unix)]
    let pgid = pid as i64;
    #[cfg(not(unix))]
    let pgid = -1i64;

    write_stdin(&mut child, request.stdin_data.as_deref());

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();
    let stdout_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout_handle {
            let _ = out.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut err) = stderr_handle {
            let _ = err.read_to_end(&mut buf);
        }
        buf
    });

    Ok(RunningProcess {
        pid,
        pgid,
        child,
        stdout_thread,
        stderr_thread,
        start,
        timeout,
    })
}

fn spawn_failure(start: Instant, reason: impl Into<String>) -> SubprocessResult {
    SubprocessResult {
        success: false,
        stdout: String::new(),
        stderr: reason.into(),
        exit_code: -1,
        duration_ms: start.elapsed().as_secs_f64() * 1000.0,
        pid: 0,
        timed_out: false,
    }
}

/// Validate subprocess resource limits without changing process state.
///
/// This checks platform support, finite representation, and the current
/// process's hard limit. It does not install any limit.
pub fn validate_subprocess_limits(limits: Option<&SubprocessLimits>) -> Result<(), String> {
    #[cfg(unix)]
    {
        validated_max_open_files(limits).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        if let Some(max_open_files) = limits.and_then(|limits| limits.max_open_files) {
            return Err(format!(
                "max_open_files {max_open_files} is unsupported on this platform"
            ));
        }
        Ok(())
    }
}

/// Validate and attach subprocess resource limits to `command`.
///
/// On Unix the limits are installed in a `pre_exec` hook. A failure to install
/// them aborts the spawn, so callers cannot accidentally run without the
/// configured cap. Unsupported or invalid limits are rejected immediately.
pub fn configure_subprocess_limits(
    command: &mut process::Command,
    limits: Option<&SubprocessLimits>,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let max_open_files = validated_max_open_files(limits)?;
        if let Some(max_open_files) = max_open_files {
            unsafe {
                command.pre_exec(move || {
                    let limit = libc::rlimit {
                        rlim_cur: max_open_files,
                        rlim_max: max_open_files,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        validate_subprocess_limits(limits)?;
    }
    Ok(())
}

#[cfg(unix)]
fn validated_max_open_files(
    limits: Option<&SubprocessLimits>,
) -> Result<Option<libc::rlim_t>, String> {
    let Some(max_open_files) = limits.and_then(|limits| limits.max_open_files) else {
        return Ok(None);
    };
    let max_open_files: libc::rlim_t = max_open_files.try_into().map_err(|_| {
        format!("max_open_files {max_open_files} cannot be represented on this platform")
    })?;
    if max_open_files == libc::RLIM_INFINITY {
        return Err("max_open_files must be finite".to_string());
    }

    let mut parent_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut parent_limit) } != 0 {
        return Err(format!(
            "failed to inspect parent RLIMIT_NOFILE: {}",
            std::io::Error::last_os_error()
        ));
    }
    if parent_limit.rlim_max != libc::RLIM_INFINITY && max_open_files > parent_limit.rlim_max {
        return Err(format!(
            "max_open_files {max_open_files} exceeds parent hard limit {}",
            parent_limit.rlim_max
        ));
    }

    Ok(Some(max_open_files))
}

#[cfg(all(test, not(unix)))]
mod resource_limit_tests {
    use super::*;

    #[test]
    fn configured_open_file_limit_is_refused() {
        let limits = SubprocessLimits {
            max_open_files: Some(64),
        };

        let error = validate_subprocess_limits(Some(&limits)).unwrap_err();

        assert!(error.contains("unsupported"), "{error}");

        let mut command = process::Command::new("unused");
        let error = configure_subprocess_limits(&mut command, Some(&limits)).unwrap_err();
        assert!(error.contains("unsupported"), "{error}");
    }
}

/// Run a subprocess synchronously and return structured results.
pub fn lib_run(request: SubprocessRequest) -> SubprocessResult {
    match lib_spawn(request) {
        Ok(running) => running.wait(),
        Err(result) => result,
    }
}

/// Spawn a detached subprocess.
pub fn lib_spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[(String, String)],
) -> Result<SpawnResult, String> {
    let envs_str: Vec<String> = envs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    spawn_detached(cmd, args, log, &envs_str, None).map(|pid| SpawnResult { pid })
}

/// Kill a process by PID. Returns the method used: "terminated", "killed", or "already_dead".
pub fn lib_kill(pid: u32, grace: f64) -> Result<String, String> {
    kill_process(pid, grace).map(|s| s.to_string())
}

/// Check if a process is alive.
pub fn lib_is_alive(pid: u32) -> bool {
    is_alive(pid)
}

// ---------------------------------------------------------------------------
// CLI types and entry point
// ---------------------------------------------------------------------------

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
    /// Stream a command's output with raw passthrough (no JSON wrapping)
    Stream {
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
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    None
}

/// Apply env key=value pairs to a Command. Callers should call
/// `command.env_clear()` before this to ensure `envs` is authoritative.
fn set_envs(command: &mut process::Command, envs: &[String]) {
    for env in envs {
        if let Some((k, v)) = env.split_once('=') {
            command.env(k, v);
        }
    }
}

fn write_stdin(child: &mut process::Child, data: Option<&str>) {
    if let Some(data) = data {
        if let Some(mut s) = child.stdin.take() {
            let _ = s.write_all(data.as_bytes());
        }
    }
}

fn setup_log(command: &mut process::Command, log: Option<&str>) -> Result<(), String> {
    if let Some(path) = log {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| format!("Failed to open log file: {e}"))?;
        let file2 = file
            .try_clone()
            .map_err(|e| format!("Failed to clone log fd: {e}"))?;
        command.stdout(file).stderr(file2);
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    Ok(())
}

pub fn run(action: ExecAction) -> serde_json::Value {
    match action {
        ExecAction::Run {
            cmd,
            args,
            cwd,
            stdin,
            stdin_pipe,
            envs,
            timeout,
        } => do_exec(
            &cmd,
            &args,
            cwd.as_deref(),
            resolve_stdin(stdin, stdin_pipe).as_deref(),
            &envs,
            timeout,
        ),
        ExecAction::Spawn {
            cmd,
            args,
            log,
            envs,
            stdin,
            stdin_pipe,
        } => {
            match spawn_detached(
                &cmd,
                &args,
                log.as_deref(),
                &envs,
                resolve_stdin(stdin, stdin_pipe).as_deref(),
            ) {
                Ok(pid) => serde_json::json!({ "success": true, "pid": pid }),
                Err(e) => serde_json::json!({ "success": false, "error": e }),
            }
        }
        ExecAction::Stream {
            cmd,
            args,
            cwd,
            stdin,
            stdin_pipe,
            envs,
            timeout,
        } => {
            let code = do_stream(
                &cmd,
                &args,
                cwd.as_deref(),
                resolve_stdin(stdin, stdin_pipe).as_deref(),
                &envs,
                timeout,
            );
            process::exit(code);
        }
        ExecAction::Kill { pid, grace } => match kill_process(pid, grace) {
            Ok(method) => serde_json::json!({ "success": true, "pid": pid, "method": method }),
            Err(e) => serde_json::json!({ "success": false, "pid": pid, "error": e }),
        },
        ExecAction::Status { pid } => serde_json::json!({ "pid": pid, "alive": is_alive(pid) }),
    }
}

fn do_exec(
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    stdin_data: Option<&str>,
    envs: &[String],
    timeout: f64,
) -> serde_json::Value {
    let r = lib_run(SubprocessRequest {
        cmd: cmd.to_string(),
        args: args.to_vec(),
        cwd: cwd.map(|s| s.to_string()),
        envs: envs
            .iter()
            .filter_map(|e| {
                e.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect(),
        stdin_data: stdin_data.map(|s| s.to_string()),
        timeout,
        limits: None,
        inherited_fds: Vec::new(),
    });
    serde_json::json!({
        "success": r.success, "stdout": r.stdout, "stderr": r.stderr,
        "return_code": r.exit_code, "duration_ms": r.duration_ms,
    })
}

/// Stream mode: raw passthrough of child stdout/stderr, no JSON wrapping.
/// Returns: child exit code, 124 on timeout, 125 on spawn failure.
fn do_stream(
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    stdin_data: Option<&str>,
    envs: &[String],
    timeout: f64,
) -> i32 {
    let mut command = process::Command::new(cmd);
    command.args(args);
    command.env_clear();
    set_envs(&mut command, envs);
    // Set PYTHONUNBUFFERED for Python children to ensure streaming latency
    command.env("PYTHONUNBUFFERED", "1");
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to spawn: {e}");
            return 125;
        }
    };
    write_stdin(&mut child, stdin_data);

    let stderr_handle = child.stderr.take();
    let stderr_thread = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        if let Some(mut err) = stderr_handle {
            let mut stderr_out = std::io::stderr();
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = stderr_out.write_all(&buf[..n]);
                        let _ = stderr_out.flush();
                    }
                    Err(_) => break,
                }
            }
        }
    });

    // Forward stdout: raw chunks with flush
    let stdout_handle = child.stdout.take();
    let stdout_thread = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        if let Some(mut out) = stdout_handle {
            let mut stdout_out = std::io::stdout();
            loop {
                match out.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = stdout_out.write_all(&buf[..n]);
                        let _ = stdout_out.flush();
                    }
                    Err(_) => break,
                }
            }
        }
    });

    // Wait with timeout. A non-positive timeout is the no-timeout sentinel.
    let timeout_rx = if timeout > 0.0 {
        let timeout_dur = Duration::from_secs_f64(timeout);
        let (tx, rx) = std::sync::mpsc::channel();
        let _timer = thread::spawn(move || {
            thread::sleep(timeout_dur);
            let _ = tx.send(());
        });
        Some(rx)
    } else {
        None
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return status.code().unwrap_or(1);
            }
            Ok(None) => {
                if timeout_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    eprintln!("Command timed out after {timeout} seconds");
                    return 124;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                eprintln!("Wait failed: {e}");
                return 125;
            }
        }
    }
}

#[cfg(unix)]
fn spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[String],
    stdin_data: Option<&str>,
) -> Result<u32, String> {
    use std::os::unix::process::CommandExt;
    let mut command = process::Command::new(cmd);
    command.args(args);
    command.env_clear();
    set_envs(&mut command, envs);
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    setup_log(&mut command, log)?;
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn: {e}"))?;
    write_stdin(&mut child, stdin_data);
    Ok(child.id())
}

#[cfg(windows)]
fn spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[String],
    stdin_data: Option<&str>,
) -> Result<u32, String> {
    use std::os::windows::process::CommandExt;
    let mut command = process::Command::new(cmd);
    command.args(args);
    command.env_clear();
    set_envs(&mut command, envs);
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    setup_log(&mut command, log)?;
    command.creation_flags(0x00000200 | 0x00000008); // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn: {e}"))?;
    write_stdin(&mut child, stdin_data);
    Ok(child.id())
}

#[cfg(unix)]
fn kill_process(pid: u32, grace: f64) -> Result<&'static str, String> {
    let pid = pid as i32;
    if unsafe { libc::kill(pid, 0) } != 0 {
        return Ok("already_dead");
    }
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(format!(
            "SIGTERM failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    for _ in 0..(grace / 0.1).ceil() as u32 {
        thread::sleep(Duration::from_millis(100));
        if unsafe { libc::kill(pid, 0) } != 0 {
            return Ok("terminated");
        }
    }
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
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
    use windows_sys::Win32::System::Threading::*;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle == 0 {
        return Ok("already_dead");
    }
    if unsafe { WaitForSingleObject(handle, (grace * 1000.0) as u32) } == WAIT_OBJECT_0 {
        unsafe { CloseHandle(handle) };
        return Ok("terminated");
    }
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

#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::*;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle == 0 {
        return false;
    }
    let result = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    result != 0
}
