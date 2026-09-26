//! Native, single-threaded private-source isolation probe. Enable explicitly
//! with LILLUX_PRIVATE_SOURCE_NATIVE=1 on a disposable qualified host. A
//! passing probe is only a filesystem-owner component result, not guest
//! launch or external-runtime qualification.

use std::ffi::OsStr;
use std::io::{BufRead as _, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn sealed_exec_child(name: &str) -> Result<(), String> {
    if !name.starts_with("lillux-private-source-")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("invalid sealed exec child name".into());
    }
    let path = std::path::Path::new("/tmp").join(name).join("payload");
    let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
    if dumpable < 0 {
        return Err(format!(
            "inspect exec child dumpability: {}",
            std::io::Error::last_os_error()
        ));
    }
    if dumpable != 0 {
        // This is a negative architectural probe, not a safe recovery path:
        // source authority is already exposed during the exec transition.
        std::process::exit(77);
    }
    if std::fs::read(&path).map_err(|error| error.to_string())? != b"private-source" {
        return Err("exec child observed different sealed source bytes".into());
    }
    if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
        return Err("exec child acquired writable sealed source descriptor".into());
    }
    if std::fs::create_dir(std::path::Path::new("/tmp").join(name).join("exec-write")).is_ok() {
        return Err("exec child created content under sealed source".into());
    }
    // Ordinary open refusal is insufficient if exec retained mount authority.
    // The test child must also fail to turn the source mount writable again.
    let remount = unsafe {
        libc::mount(
            std::ptr::null(),
            c"/tmp".as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV,
            std::ptr::null(),
        )
    };
    if remount == 0 {
        return Err("exec child regained writable source mount authority".into());
    }
    let remount_error = std::io::Error::last_os_error();
    if remount_error.raw_os_error() != Some(libc::EPERM) {
        return Err(format!(
            "exec child remount refusal was not a privilege boundary: {remount_error}"
        ));
    }
    Ok(())
}

fn sandbox_target(name: &str) -> Result<(), String> {
    if !name.starts_with("lillux-private-source-")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("invalid sandbox target name".into());
    }
    if std::fs::read("/source/payload").map_err(|error| error.to_string())? != b"private-source" {
        return Err("sandbox target observed different source bytes".into());
    }
    if std::fs::OpenOptions::new()
        .write(true)
        .open("/source/payload")
        .is_ok()
    {
        return Err("sandbox target opened source writable".into());
    }
    if std::fs::create_dir("/source/mutation").is_ok() {
        return Err("sandbox target created source content".into());
    }
    if std::fs::read(format!("/tmp/{name}/payload")).is_ok() {
        return Err("sandbox target retained ambient source path".into());
    }
    let remount = unsafe {
        libc::mount(
            std::ptr::null(),
            c"/source".as_ptr(),
            std::ptr::null(),
            libc::MS_REMOUNT | libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if remount == 0 {
        return Err("sandbox target restored writable source mount".into());
    }
    writeln!(std::io::stdout(), "TARGET").map_err(|error| error.to_string())?;
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut release = [0u8; 1];
    std::io::stdin()
        .read_exact(&mut release)
        .map_err(|error| error.to_string())?;
    if release != [2] {
        return Err("invalid executed target release acknowledgement".into());
    }
    Ok(())
}

fn wait_bounded(process: &mut std::process::Child, label: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if process
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = process.kill();
            return Err(format!("{label} deadline expired"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Test-only failure guard. The exact namespace-init PID is killed before its
/// dedicated owner, so a failing assertion cannot leave a child holding the
/// phase pipes while the parent waits for output.
struct ProbeKillGuard {
    owner_pidfd: OwnedFd,
    target_pidfd: Option<OwnedFd>,
    armed: bool,
}

fn probe_pidfd(pid: u32) -> Result<OwnedFd, String> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
    if fd < 0 {
        return Err(format!(
            "pin probe process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn probe_kill(pidfd: &OwnedFd) {
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        );
    }
}

impl Drop for ProbeKillGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Some(pidfd) = &self.target_pidfd {
                probe_kill(pidfd);
            }
            probe_kill(&self.owner_pidfd);
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn held_source_target(
    source: lillux::sandbox::LinuxSealedPrivateSourceFilesystem,
    name: &str,
    writable_source: bool,
    wrong_source: bool,
) -> Result<(), String> {
    use lillux::sandbox::{
        LinuxSandboxExit, LinuxSandboxLifecycle, LinuxSandboxMount, LinuxSandboxMountAccess,
        LinuxSandboxNetwork, LinuxSandboxProcFilesystem, LinuxSandboxRequest,
    };
    let executable_path = std::env::current_exe().map_err(|error| error.to_string())?;
    let executable =
        lillux::pin_canonical_mount_source(&executable_path).map_err(|error| error.to_string())?;
    let libraries = lillux::pin_canonical_mount_source(std::path::Path::new("/usr/lib"))
        .map_err(|error| error.to_string())?;
    let loader =
        lillux::pin_canonical_mount_source(std::path::Path::new("/usr/lib/ld-linux-x86-64.so.2"))
            .map_err(|error| error.to_string())?;
    let mount_name = if wrong_source { "wrong-source" } else { name };
    let source_mount =
        lillux::pin_canonical_mount_source(&std::path::Path::new("/tmp").join(mount_name))
            .map_err(|error| error.to_string())?;
    let readonly = LinuxSandboxMountAccess::ReadOnly;
    let request = LinuxSandboxRequest {
        executable: PathBuf::from("/bin/probe"),
        argv0: "private-source-target".into(),
        arguments: vec!["child-sandbox-target".into(), name.into()],
        cwd: PathBuf::from("/"),
        environment: std::collections::BTreeMap::new(),
        mounts: vec![
            LinuxSandboxMount {
                source_fd: executable
                    .inherited_descriptor()
                    .map_err(|error| error.to_string())?,
                destination: PathBuf::from("/bin/probe"),
                access: readonly,
                layer: 0,
            },
            LinuxSandboxMount {
                source_fd: libraries
                    .inherited_descriptor()
                    .map_err(|error| error.to_string())?,
                destination: PathBuf::from("/usr/lib"),
                access: readonly,
                layer: 0,
            },
            LinuxSandboxMount {
                source_fd: loader
                    .inherited_descriptor()
                    .map_err(|error| error.to_string())?,
                destination: PathBuf::from("/lib64/ld-linux-x86-64.so.2"),
                access: readonly,
                layer: 0,
            },
            LinuxSandboxMount {
                source_fd: source_mount
                    .inherited_descriptor()
                    .map_err(|error| error.to_string())?,
                destination: PathBuf::from("/source"),
                access: if writable_source {
                    LinuxSandboxMountAccess::Writable
                } else {
                    readonly
                },
                layer: 0,
            },
        ],
        fixed_parent_views: Vec::new(),
        overlay: None,
        network: LinuxSandboxNetwork::Isolated,
        private_tmp: true,
        proc_filesystem: LinuxSandboxProcFilesystem::Empty,
        minimal_devices: true,
        character_devices: Vec::new(),
        target_channels: Vec::new(),
        lifecycle: LinuxSandboxLifecycle::Run,
        contain_process_group: true,
        nested_sandbox: false,
        aggregate_limits: None,
    };
    let expected = request.clone();
    let mut held = lillux::sandbox::prepare_linux_sandbox_from_sealed_source(
        source,
        OsStr::new(name),
        std::path::Path::new("/source"),
        request,
    );
    let outcome = (|| -> Result<(), String> {
        if unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err("source owner became dumpable during held preparation".into());
        }
        if std::fs::read(format!("/tmp/{name}/payload")).is_ok() {
            return Err("source owner's old path survived private target root setup".into());
        }
        let receipt = held.held().mount_preparation_receipt()?;
        if receipt.owned_child_pid != held.held().child_pid()
            || !receipt.matches_request(&expected)?
        {
            return Err("held source target mount preparation differed from request".into());
        }
        let unreleased = held.held().try_observe_target_exit().unwrap_err();
        if !unreleased.contains("not been successfully released") {
            return Err(format!(
                "held source target gave wrong pre-release refusal: {unreleased}"
            ));
        }
        writeln!(std::io::stdout(), "HELD {}", held.held().child_pid())
            .map_err(|error| error.to_string())?;
        std::io::stdout()
            .flush()
            .map_err(|error| error.to_string())?;
        let mut release = [0u8; 1];
        std::io::stdin()
            .read_exact(&mut release)
            .map_err(|error| error.to_string())?;
        if release != [1] {
            return Err("invalid held target release acknowledgement".into());
        }
        held.held().release_once()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut applied = false;
        loop {
            if let Some(receipt) = held.held().try_observe_applied_launch()? {
                if !receipt.matches_request(&expected)? {
                    return Err("held source target applied launch differed from request".into());
                }
                applied = true;
            }
            if let Some(exit) = held.held().try_observe_target_exit()? {
                if !applied || exit.into_termination().exit() != LinuxSandboxExit::Code(0) {
                    return Err("held source target failed or lacked applied evidence".into());
                }
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("held source target completion deadline expired".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if let Err(error) = &outcome {
        if let Err(cleanup) = held
            .held()
            .terminate_namespace_for_export(Duration::from_secs(5))
        {
            eprintln!(
                "held source probe failed ({error}) and exact namespace cleanup failed ({cleanup})"
            );
            lillux::sandbox::exit_with_linux_sandbox_status(LinuxSandboxExit::Code(125));
        }
    }
    outcome
}

#[cfg(not(target_arch = "x86_64"))]
fn held_source_target(
    _source: lillux::sandbox::LinuxSealedPrivateSourceFilesystem,
    _name: &str,
    _writable_source: bool,
    _wrong_source: bool,
) -> Result<(), String> {
    Err("held private source native probe currently requires x86_64 loader fixture".into())
}

fn child(
    name: &str,
    hold_writer: bool,
    writable_mount: bool,
    wrong_source: bool,
) -> Result<(), String> {
    if !name.starts_with("lillux-private-source-")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("invalid test child name".into());
    }
    let owner = lillux::sandbox::enter_linux_private_source_filesystem(
        lillux::sandbox::LinuxPrivateSourceLimits {
            max_bytes: 4 * 1024 * 1024,
            max_inodes: 128,
        },
    )?;
    if lillux::sandbox::enter_linux_private_source_filesystem(
        lillux::sandbox::LinuxPrivateSourceLimits {
            max_bytes: 4 * 1024 * 1024,
            max_inodes: 128,
        },
    )
    .is_ok()
    {
        return Err("private source owner admitted a second namespace entry".into());
    }
    let child = owner
        .root()
        .create_child(OsStr::new(name), 0o700)
        .map_err(|error| error.to_string())?;
    child
        .atomic_create_pinned_regular(OsStr::new("payload"), b"private-source", 0o600)
        .map_err(|error| error.to_string())?
        .ok_or("private source payload already exists")?;
    if wrong_source {
        owner
            .root()
            .create_child(OsStr::new("wrong-source"), 0o700)
            .map_err(|error| error.to_string())?;
    }
    let preopened = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/tmp/{name}/payload"))
        .map_err(|error| format!("open private source before seal: {error}"))?;
    if hold_writer {
        let _held = preopened;
        let _ = owner.seal_read_only()?;
        return Err("private source seal accepted an open writable descriptor".into());
    }
    drop(preopened);
    let owner = owner.seal_read_only()?;
    if std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/tmp/{name}/payload"))
        .is_ok()
    {
        return Err("sealed private source accepted a new writable descriptor".into());
    }
    if owner
        .root()
        .create_child(OsStr::new("post-seal"), 0o700)
        .is_ok()
    {
        return Err("sealed private source accepted a new child".into());
    }
    let mut exec_child = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg("child-sealed-view")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn sealed-source exec child: {error}"))?;
    wait_bounded(&mut exec_child, "sealed-source exec child")?;
    let exec_output = exec_child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    let exec_stderr = String::from_utf8_lossy(&exec_output.stderr);
    if exec_output.status.success() {
        // A future platform may preserve non-dumpability across this exec.
        // The child then verifies the inherited mount remains read-only.
    } else if exec_output.status.code() == Some(77) {
        // Refusal is expected on kernels that reset dumpability at exec.
    } else {
        return Err(format!(
            "sealed-source exec child failed: exit={}, stderr={}",
            exec_output.status, exec_stderr
        ));
    }
    held_source_target(owner, name, writable_mount, wrong_source)?;
    std::io::stdout()
        .write_all(b"READY\n")
        .map_err(|error| error.to_string())?;
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut release = [0u8; 1];
    std::io::stdin()
        .read_exact(&mut release)
        .map_err(|error| error.to_string())?;
    if release != [1] {
        return Err("invalid private source test release".into());
    }
    Ok(())
}

fn parent() -> Result<(), String> {
    let name = format!("lillux-private-source-{:032x}", rand::random::<u128>());
    let mut writer_probe =
        Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
            .arg("child-held-writer")
            .arg(&name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| error.to_string())?;
    wait_bounded(&mut writer_probe, "private source held-writer refusal")?;
    let writer_output = writer_probe
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if writer_output.status.code() != Some(125)
        || !String::from_utf8_lossy(&writer_output.stderr)
            .contains("seal private source mount read-only")
    {
        return Err(format!(
            "held writable descriptor did not fail closed: exit={}, stderr={}",
            writer_output.status,
            String::from_utf8_lossy(&writer_output.stderr)
        ));
    }
    let mut writable_mount_probe =
        Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
            .arg("child-writable-mount")
            .arg(&name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| error.to_string())?;
    wait_bounded(&mut writable_mount_probe, "writable source mount refusal")?;
    let writable_mount_output = writable_mount_probe
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if writable_mount_output.status.code() != Some(125)
        || !String::from_utf8_lossy(&writable_mount_output.stderr)
            .contains("private source mount cannot grant writable target access")
    {
        return Err(format!(
            "writable source mount did not refuse before target: exit={}, stderr={}",
            writable_mount_output.status,
            String::from_utf8_lossy(&writable_mount_output.stderr)
        ));
    }
    let mut wrong_source_probe =
        Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
            .arg("child-wrong-source")
            .arg(&name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| error.to_string())?;
    wait_bounded(&mut wrong_source_probe, "wrong sealed source refusal")?;
    let wrong_source_output = wrong_source_probe
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if wrong_source_output.status.code() != Some(125)
        || !String::from_utf8_lossy(&wrong_source_output.stderr)
            .contains("private source mount is not the exact selected child and destination")
    {
        return Err(format!(
            "wrong sealed source did not refuse before target: exit={}, stderr={}",
            wrong_source_output.status,
            String::from_utf8_lossy(&wrong_source_output.stderr)
        ));
    }
    let mut process = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg("child")
        .arg(&name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let owner_pidfd = match probe_pidfd(process.id()) {
        Ok(pidfd) => pidfd,
        Err(error) => {
            // The just-spawned child is still retained and unreaped, so its
            // numeric PID cannot be recycled on this refusal path.
            let _ = process.kill();
            return Err(error);
        }
    };
    let mut probe_guard = ProbeKillGuard {
        owner_pidfd,
        target_pidfd: None,
        armed: true,
    };
    let stdout = process
        .stdout
        .take()
        .ok_or("private source child stdout missing")?;
    let (line_tx, line_rx) = std::sync::mpsc::sync_channel(3);
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        for _ in 0..3 {
            let mut line = String::new();
            let result = reader
                .read_line(&mut line)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    if bytes == 0 {
                        Err("private source child closed its phase channel".into())
                    } else {
                        Ok(line)
                    }
                });
            if line_tx.send(result).is_err() {
                break;
            }
        }
    });
    let held_line = match line_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(line)) => line,
        other => {
            return Err(format!(
                "private source child did not reach held phase: {other:?}"
            ));
        }
    };
    let held_pid = held_line
        .strip_prefix("HELD ")
        .and_then(|text| text.trim_end().parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| format!("invalid held target phase: {held_line:?}"))?;
    probe_guard.target_pidfd = Some(probe_pidfd(held_pid)?);
    let target_proc = std::path::Path::new("/proc").join(held_pid.to_string());
    let held_alive_before = unsafe { libc::kill(held_pid as libc::pid_t, 0) } == 0;
    let fd_error = std::fs::read_dir(target_proc.join("fd"))
        .err()
        .and_then(|error| error.raw_os_error());
    let source_error = std::fs::read(target_proc.join("root/source/payload"))
        .err()
        .and_then(|error| error.raw_os_error());
    let ambient_error = std::fs::read(target_proc.join("root/tmp").join(&name).join("payload"))
        .err()
        .and_then(|error| error.raw_os_error());
    let ptrace = unsafe { libc::ptrace(libc::PTRACE_ATTACH, held_pid as libc::pid_t, 0, 0) };
    let ptrace_error = (ptrace != 0)
        .then(|| std::io::Error::last_os_error().raw_os_error())
        .flatten();
    if ptrace == 0 {
        unsafe { libc::ptrace(libc::PTRACE_DETACH, held_pid as libc::pid_t, 0, 0) };
    }
    let held_alive_after = unsafe { libc::kill(held_pid as libc::pid_t, 0) } == 0;
    let permission =
        |error| matches!(error, Some(code) if code == libc::EACCES || code == libc::EPERM);
    if !held_alive_before
        || !held_alive_after
        || !permission(fd_error)
        || !permission(source_error)
        || !matches!(ambient_error, Some(code) if code == libc::ENOENT || code == libc::EACCES || code == libc::EPERM)
        || !permission(ptrace_error)
    {
        return Err(format!(
            "held target authority probe differed: alive_before={held_alive_before}, alive_after={held_alive_after}, fd_error={fd_error:?}, source_error={source_error:?}, ambient_error={ambient_error:?}, ptrace_error={ptrace_error:?}"
        ));
    }
    process
        .stdin
        .as_mut()
        .ok_or("private source child stdin missing")?
        .write_all(&[1])
        .map_err(|error| error.to_string())?;
    let target = line_rx
        .recv_timeout(Duration::from_secs(15))
        .map_err(|error| format!("executed target did not report its live phase: {error}"))??;
    if target != "TARGET\n" {
        return Err(format!("unexpected executed target phase: {target:?}"));
    }
    let live_target_proc = std::path::Path::new("/proc").join(held_pid.to_string());
    let post_exec_path = live_target_proc.join("root/source/payload");
    let post_exec_source = std::fs::read(&post_exec_path);
    let post_exec_write = std::fs::OpenOptions::new()
        .write(true)
        .open(&post_exec_path);
    let target_alive = unsafe { libc::kill(held_pid as libc::pid_t, 0) } == 0;
    // Exec can reset target dumpability, so this profile cannot promise
    // source confidentiality against an untrusted same-UID outer peer. A
    // host that denies the read is stronger; either way the source must not
    // become writable through the target's proc view.
    let readable_exact =
        matches!(post_exec_source.as_deref(), Ok(bytes) if bytes == b"private-source");
    let read_denied = permission(
        post_exec_source
            .as_ref()
            .err()
            .and_then(|error| error.raw_os_error()),
    );
    let write_error = post_exec_write
        .as_ref()
        .err()
        .and_then(|error| error.raw_os_error());
    if !target_alive
        || !(readable_exact || read_denied)
        || !matches!(write_error, Some(code) if code == libc::EROFS || code == libc::EACCES || code == libc::EPERM)
    {
        return Err(format!(
            "executed target source integrity observation differed: target_alive={target_alive}; read={post_exec_source:?}; write_error={:?}",
            write_error
        ));
    }
    process
        .stdin
        .as_mut()
        .ok_or("private source child stdin missing after target exec")?
        .write_all(&[2])
        .map_err(|error| error.to_string())?;
    let ready = match line_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(line)) => line,
        other => {
            return Err(format!(
                "private source child did not reach completed phase: {other:?}"
            ));
        }
    };
    if ready != "READY\n" {
        return Err(format!(
            "private source child sent invalid readiness {ready:?}"
        ));
    }
    let host_path = std::path::Path::new("/tmp").join(&name).join("payload");
    let host_cannot_read = std::fs::read(&host_path).is_err();
    let host_cannot_open_writable = std::fs::OpenOptions::new()
        .write(true)
        .open(&host_path)
        .is_err();
    let proc_fd_directory = std::path::Path::new("/proc")
        .join(process.id().to_string())
        .join("fd");
    let same_uid_cannot_enumerate_fds = std::fs::read_dir(&proc_fd_directory).is_err();
    let proc_path = std::path::Path::new("/proc")
        .join(process.id().to_string())
        .join("root/tmp")
        .join(&name)
        .join("payload");
    let same_uid_cannot_read_through_proc = std::fs::read(&proc_path).is_err();
    let same_uid_cannot_open_proc_writable = std::fs::OpenOptions::new()
        .write(true)
        .open(&proc_path)
        .is_err();
    process
        .stdin
        .take()
        .ok_or("private source child stdin missing")?
        .write_all(&[1])
        .map_err(|error| error.to_string())?;
    let exit_deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if process
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            break;
        }
        if std::time::Instant::now() >= exit_deadline {
            return Err("private source child exit deadline expired".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = process
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if ready != "READY\n"
        || !output.status.success()
        || !host_cannot_read
        || !host_cannot_open_writable
        || !same_uid_cannot_enumerate_fds
        || !same_uid_cannot_read_through_proc
        || !same_uid_cannot_open_proc_writable
    {
        return Err(format!(
            "private source isolation failed: ready={ready:?}, exit={}, host_readable={}, host_writable={}, proc_fd_enumerable={}, proc_readable={}, proc_writable={}, stderr={}",
            output.status,
            !host_cannot_read,
            !host_cannot_open_writable,
            !same_uid_cannot_enumerate_fds,
            !same_uid_cannot_read_through_proc,
            !same_uid_cannot_open_proc_writable,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    probe_guard.armed = false;
    Ok(())
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let result = if args.get(1).is_some_and(|arg| arg == "child") {
        child(
            args.get(2).map(String::as_str).unwrap_or(""),
            false,
            false,
            false,
        )
    } else if args.get(1).is_some_and(|arg| arg == "child-held-writer") {
        child(
            args.get(2).map(String::as_str).unwrap_or(""),
            true,
            false,
            false,
        )
    } else if args.get(1).is_some_and(|arg| arg == "child-writable-mount") {
        child(
            args.get(2).map(String::as_str).unwrap_or(""),
            false,
            true,
            false,
        )
    } else if args.get(1).is_some_and(|arg| arg == "child-wrong-source") {
        child(
            args.get(2).map(String::as_str).unwrap_or(""),
            false,
            false,
            true,
        )
    } else if args.get(1).is_some_and(|arg| arg == "child-sealed-view") {
        sealed_exec_child(args.get(2).map(String::as_str).unwrap_or(""))
    } else if args.get(1).is_some_and(|arg| arg == "child-sandbox-target") {
        sandbox_target(args.get(2).map(String::as_str).unwrap_or(""))
    } else if std::env::var("LILLUX_PRIVATE_SOURCE_NATIVE").as_deref() == Ok("1") {
        parent()
    } else {
        println!("private source native probe skipped; set LILLUX_PRIVATE_SOURCE_NATIVE=1");
        Ok(())
    };
    if let Err(error) = result {
        eprintln!("private source owner probe failed: {error}");
        std::process::exit(1);
    }
}
