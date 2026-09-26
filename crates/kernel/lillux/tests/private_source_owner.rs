//! Native, single-threaded private-source isolation probe. Enable explicitly
//! with LILLUX_PRIVATE_SOURCE_NATIVE=1 on a disposable qualified host. A
//! passing probe is only a filesystem-owner component result, not guest
//! launch or external-runtime qualification.

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
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
            let _ = process.wait();
            return Err(format!("{label} deadline expired"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child(name: &str, hold_writer: bool) -> Result<(), String> {
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
    let mut process = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg("child")
        .arg(&name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdout = process
        .stdout
        .take()
        .ok_or("private source child stdout missing")?;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut ready = [0u8; 6];
        let result = stdout.read_exact(&mut ready).map(|()| ready);
        let _ = ready_tx.send(result);
    });
    let ready = match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(ready)) => ready,
        Ok(Err(error)) => {
            let _ = process.kill();
            let output = process
                .wait_with_output()
                .map_err(|wait| wait.to_string())?;
            return Err(format!(
                "private source child did not reach readiness: {error}; stderr={}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Err(error) => {
            let _ = process.kill();
            let output = process
                .wait_with_output()
                .map_err(|wait| wait.to_string())?;
            return Err(format!(
                "private source child readiness deadline expired: {error}; stderr={}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    };
    if ready != *b"READY\n" {
        let _ = process.kill();
        let output = process
            .wait_with_output()
            .map_err(|wait| wait.to_string())?;
        return Err(format!(
            "private source child sent invalid readiness {ready:?}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
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
            let _ = process.kill();
            let output = process
                .wait_with_output()
                .map_err(|error| error.to_string())?;
            return Err(format!(
                "private source child exit deadline expired; stderr={}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = process
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if ready != *b"READY\n"
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
    Ok(())
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let result = if args.get(1).is_some_and(|arg| arg == "child") {
        child(args.get(2).map(String::as_str).unwrap_or(""), false)
    } else if args.get(1).is_some_and(|arg| arg == "child-held-writer") {
        child(args.get(2).map(String::as_str).unwrap_or(""), true)
    } else if args.get(1).is_some_and(|arg| arg == "child-sealed-view") {
        sealed_exec_child(args.get(2).map(String::as_str).unwrap_or(""))
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
