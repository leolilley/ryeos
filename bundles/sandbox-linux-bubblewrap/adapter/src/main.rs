use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ryeos_isolation_protocol::{
    from_json_slice_strict, AdapterInspectionRequest, AdapterInspectionResponse,
    AdapterLaunchRequest, InspectedArtifact, IsolationAdapterProtocolVersion,
    IsolationArtifactRole, IsolationCapability, IsolationDiagnostic, IsolationDiagnosticCode,
    IsolationMountAccess, IsolationNetwork, IsolationTargetTriple, LauncherRefusalDocument,
    MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
};
use sha2::{Digest as _, Sha256};

const ADAPTER_BUILD: &str = env!("CARGO_PKG_VERSION");
const BACKEND_ID: &str = "linux-bubblewrap";
const LAUNCHER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

fn main() {
    let mut args = std::env::args_os();
    let _program = args.next();
    let Some(mode) = args.next() else {
        fail_process("missing adapter mode");
    };
    let Some(request_fd) = args.next() else {
        fail_process("missing request descriptor");
    };
    if args.next().is_some() {
        fail_process("unexpected adapter argument");
    }
    let request_fd = parse_fd(&request_fd).unwrap_or_else(|error| fail_process(&error));

    match mode.to_str() {
        Some("inspect") => {
            let result = inspect(request_fd);
            match result {
                Ok(response) => write_response(&response),
                Err(error) => fail_process(&error),
            }
        }
        Some("launch") => launch(request_fd),
        _ => fail_process("unsupported adapter mode"),
    }
}

fn inspect(request_fd: RawFd) -> Result<AdapterInspectionResponse, String> {
    let request: AdapterInspectionRequest = read_sealed_request(request_fd)?;
    if request.protocol != IsolationAdapterProtocolVersion::V1 {
        return Err("unsupported isolation adapter protocol".to_string());
    }
    validate_inspection_identity(&request)?;
    let launcher_fd = *request
        .artifacts
        .get(&IsolationArtifactRole::Launcher)
        .ok_or_else(|| "inspection request is missing launcher artifact".to_string())?
        as RawFd;
    validate_inherited_fd(launcher_fd, "launcher artifact")?;

    let version_output = run_launcher_probe(launcher_fd, "--version")?;
    if version_output.stdout.len() + version_output.stderr.len() > MAX_RESPONSE_BYTES {
        return Err("launcher version response exceeds adapter limit".to_string());
    }
    if !version_output.status.success() {
        return Err(format!(
            "launcher version inspection failed: {}",
            String::from_utf8_lossy(&version_output.stderr).trim()
        ));
    }
    let version = String::from_utf8(version_output.stdout)
        .map_err(|_| "launcher version is not UTF-8".to_string())?
        .trim()
        .to_string();
    if !version.starts_with("bubblewrap ") {
        return Err("launcher did not identify as Bubblewrap".to_string());
    }
    let version_number = version
        .strip_prefix("bubblewrap ")
        .ok_or_else(|| "launcher returned an invalid Bubblewrap version".to_string())?;
    require_launcher_version(version_number)?;

    let help_output = run_launcher_probe(launcher_fd, "--help")?;
    if help_output.stdout.len() + help_output.stderr.len() > MAX_RESPONSE_BYTES {
        return Err("launcher feature response exceeds adapter limit".to_string());
    }
    if !help_output.status.success() {
        return Err(format!(
            "launcher feature inspection failed: {}",
            String::from_utf8_lossy(&help_output.stderr).trim()
        ));
    }
    let help = format!(
        "{}\n{}",
        String::from_utf8_lossy(&help_output.stdout),
        String::from_utf8_lossy(&help_output.stderr)
    );
    let tokens = help.split_whitespace().collect::<BTreeSet<_>>();
    for required in [
        "--args",
        "--argv0",
        "--bind-fd",
        "--chdir",
        "--clearenv",
        "--dev",
        "--dir",
        "--json-status-fd",
        "--ro-bind-fd",
        "--setenv",
        "--tmpfs",
        "--unshare-ipc",
        "--unshare-net",
        "--unshare-user",
        "--unshare-uts",
    ] {
        if !tokens.contains(required) {
            return Err(format!(
                "launcher does not support required option {required}"
            ));
        }
    }

    let digest = digest_fd(launcher_fd)?;
    Ok(AdapterInspectionResponse {
        protocol: IsolationAdapterProtocolVersion::V1,
        adapter_build: ADAPTER_BUILD.to_string(),
        effective_capabilities: supported_capabilities(),
        artifacts: BTreeMap::from([(
            IsolationArtifactRole::Launcher,
            InspectedArtifact { version, digest },
        )]),
    })
}

fn validate_inspection_identity(request: &AdapterInspectionRequest) -> Result<(), String> {
    request
        .validate()
        .map_err(|error| format!("invalid inspection request: {error}"))?;
    if request.backend_id != BACKEND_ID {
        return Err(format!(
            "adapter implements backend `{BACKEND_ID}`, not `{}`",
            request.backend_id
        ));
    }
    if Some(request.target) != host_target() {
        return Err("inspection target does not match this adapter build".to_string());
    }
    Ok(())
}

struct LauncherProbeOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_launcher_probe(launcher_fd: RawFd, argument: &str) -> Result<LauncherProbeOutput, String> {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new(format!("/proc/self/fd/{launcher_fd}"));
    command
        .arg(argument)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // An ELF image is opened before close-on-exec descriptors are discarded,
    // while a script interpreter reopens the kernel-provided /proc/self/fd/N
    // name after exec. Retain the exact inspected artifact explicitly so both
    // forms execute the same pinned inode and do not depend on ambient flags.
    unsafe {
        command.pre_exec(move || set_cloexec(launcher_fd, false).map_err(std::io::Error::other));
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("execute exact launcher for {argument} inspection: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "capture launcher probe stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "capture launcher probe stderr".to_string())?;
    let stdout_reader = std::thread::spawn(move || read_probe_stream(stdout, "stdout"));
    let stderr_reader = std::thread::spawn(move || read_probe_stream(stderr, "stderr"));
    let started = Instant::now();
    let status = loop {
        match child
            .try_wait()
            .map_err(|error| format!("wait for launcher {argument} inspection: {error}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= LAUNCHER_PROBE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("launcher {argument} inspection timed out"));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "launcher probe stdout reader panicked".to_string())??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "launcher probe stderr reader panicked".to_string())??;
    Ok(LauncherProbeOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_probe_stream(stream: impl Read, label: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read launcher probe {label}: {error}"))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "launcher probe {label} exceeds {MAX_RESPONSE_BYTES} bytes"
        ));
    }
    Ok(bytes)
}

fn require_launcher_version(version: &str) -> Result<(), String> {
    let mut components = version.split('.');
    let parsed = (
        parse_version_component(&mut components, "major")?,
        parse_version_component(&mut components, "minor")?,
        parse_version_component(&mut components, "patch")?,
    );
    if components.next().is_some() {
        return Err("launcher version must use major.minor.patch".to_string());
    }
    if parsed < (0, 11, 0) {
        return Err("launcher version 0.11.0 or newer is required".to_string());
    }
    Ok(())
}

fn host_target() -> Option<IsolationTargetTriple> {
    if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Some(IsolationTargetTriple::X86_64UnknownLinuxGnu)
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Some(IsolationTargetTriple::Aarch64UnknownLinuxGnu)
    } else {
        None
    }
}

fn parse_version_component(
    components: &mut std::str::Split<'_, char>,
    label: &str,
) -> Result<u64, String> {
    let value = components
        .next()
        .ok_or_else(|| format!("launcher version is missing its {label} component"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("launcher version has an invalid {label} component"));
    }
    value
        .parse()
        .map_err(|_| format!("launcher version has an invalid {label} component"))
}

fn launch(request_fd: RawFd) -> ! {
    let request = match read_sealed_request::<AdapterLaunchRequest>(request_fd) {
        Ok(request) => request,
        Err(error) => fail_process(&error),
    };
    let status_fd = request.status_fd as RawFd;
    let result = prepare_launch(&request).and_then(exec_launcher);
    let error = result.unwrap_err_or_else();
    emit_refusal(status_fd, error);
}

trait NeverResultExt {
    fn unwrap_err_or_else(self) -> String;
}

impl NeverResultExt for Result<std::convert::Infallible, String> {
    fn unwrap_err_or_else(self) -> String {
        match self {
            Ok(never) => match never {},
            Err(error) => error,
        }
    }
}

#[derive(Debug)]
struct PreparedLaunch {
    launcher_fd: RawFd,
    inherited_fds: BTreeSet<RawFd>,
    arguments: Vec<String>,
}

fn prepare_launch(request: &AdapterLaunchRequest) -> Result<PreparedLaunch, String> {
    if request.protocol != IsolationAdapterProtocolVersion::V1 {
        return Err("unsupported isolation adapter protocol".to_string());
    }
    let required = request
        .validate()
        .map_err(|error| format!("invalid launch request: {error}"))?;
    let supported = supported_capabilities();
    let missing: Vec<_> = required.difference(&supported).collect();
    if !missing.is_empty() {
        return Err(format!(
            "adapter is missing required capabilities: {missing:?}"
        ));
    }
    let launcher_fd = *request
        .artifacts
        .get(&IsolationArtifactRole::Launcher)
        .ok_or_else(|| "launch request is missing launcher artifact".to_string())?
        as RawFd;
    validate_inherited_fd(launcher_fd, "launcher artifact")?;
    validate_inherited_fd(request.status_fd as RawFd, "status writer")?;
    for authority in &request.authorities {
        validate_inherited_fd(authority.inherited_fd as RawFd, "plan authority")?;
    }
    for descriptor in request.artifacts.values().copied() {
        validate_inherited_fd(descriptor as RawFd, "isolation artifact")?;
    }

    if request
        .artifacts
        .keys()
        .any(|role| *role != IsolationArtifactRole::Launcher)
    {
        return Err("this adapter build does not support dynamic-loader artifacts".to_string());
    }

    let authority_by_id: BTreeMap<_, _> = request
        .authorities
        .iter()
        .map(|authority| (authority.id.clone(), authority))
        .collect();
    let target_mount = request
        .plan
        .mounts
        .iter()
        .find(|mount| mount.source == request.plan.target.executable)
        .ok_or_else(|| "target executable authority is not mounted".to_string())?;

    let mut arguments = vec![
        "--json-status-fd".to_string(),
        request.status_fd.to_string(),
        "--clearenv".to_string(),
        "--unshare-user".to_string(),
        "--unshare-ipc".to_string(),
        "--unshare-uts".to_string(),
    ];
    if request.plan.network == IsolationNetwork::Isolated {
        arguments.push("--unshare-net".to_string());
    }
    arguments.extend(["--tmpfs".to_string(), "/".to_string()]);
    arguments.extend(["--dir".to_string(), "/etc".to_string()]);
    arguments.extend(["--dir".to_string(), "/proc".to_string()]);
    arguments.extend(["--dev".to_string(), "/dev".to_string()]);
    if request.plan.private_tmp {
        arguments.extend(["--tmpfs".to_string(), "/tmp".to_string()]);
    }

    let mut created_directories = BTreeSet::new();
    for mount in &request.plan.mounts {
        append_parent_directories(
            &mut arguments,
            mount.destination.as_str(),
            &mut created_directories,
        );
    }
    for mount in &request.plan.mounts {
        let authority = authority_by_id
            .get(&mount.source)
            .ok_or_else(|| "mount authority disappeared after validation".to_string())?;
        arguments.extend([
            match mount.access {
                IsolationMountAccess::ReadOnly => "--ro-bind-fd",
                IsolationMountAccess::Writable => "--bind-fd",
            }
            .to_string(),
            authority.inherited_fd.to_string(),
            mount.destination.as_str().to_string(),
        ]);
    }
    for (name, value) in &request.plan.environment.values {
        arguments.extend(["--setenv".to_string(), name.clone(), value.clone()]);
    }
    arguments.extend([
        "--chdir".to_string(),
        request.plan.target.cwd.as_str().to_string(),
        "--argv0".to_string(),
        request.plan.target.argv0.clone(),
        "--".to_string(),
        target_mount.destination.as_str().to_string(),
    ]);
    arguments.extend(request.plan.target.arguments.iter().cloned());

    let inherited_fds = request
        .authorities
        .iter()
        .map(|authority| authority.inherited_fd as RawFd)
        .chain([launcher_fd, request.status_fd as RawFd])
        .collect();
    Ok(PreparedLaunch {
        launcher_fd,
        inherited_fds,
        arguments,
    })
}

fn exec_launcher(mut prepared: PreparedLaunch) -> Result<std::convert::Infallible, String> {
    let bytes = encode_nul_arguments(&prepared.arguments)?;
    let argument_file = create_sealed_memfd(c"ryeos-bwrap-args", &bytes)?;
    let argument_fd = argument_file.as_raw_fd();
    prepared.inherited_fds.insert(argument_fd);

    seal_descriptor_boundary(prepared.launcher_fd, &prepared.inherited_fds)?;

    let error = exact_launcher_command(prepared.launcher_fd, argument_fd).exec();
    Err(format!("exec exact Bubblewrap launcher: {error}"))
}

fn seal_descriptor_boundary(
    launcher_fd: RawFd,
    inherited_fds: &BTreeSet<RawFd>,
) -> Result<(), String> {
    let open_fds = std::fs::read_dir("/proc/self/fd")
        .map_err(|error| format!("enumerate adapter descriptors: {error}"))?
        .map(|entry| {
            entry
                .map_err(|error| format!("enumerate adapter descriptor: {error}"))?
                .file_name()
                .to_string_lossy()
                .parse::<RawFd>()
                .map_err(|error| format!("parse adapter descriptor: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Mark every ambient non-stdio descriptor close-on-exec first. Only the
    // signed plan's authorities, launcher argument file, and status channel
    // are then made inheritable. The launcher remains CLOEXEC: `/proc/self/fd`
    // resolves it for the initial exec and the descriptor disappears in the
    // Bubblewrap image.
    for fd in open_fds.into_iter().filter(|fd| *fd > libc::STDERR_FILENO) {
        set_cloexec_if_open(fd)?;
    }
    for fd in inherited_fds {
        if *fd != launcher_fd {
            set_cloexec(*fd, false)?;
        }
    }
    set_cloexec(launcher_fd, true)
}

fn set_cloexec_if_open(fd: RawFd) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EBADF) {
            return Ok(());
        }
        return Err(format!("inspect ambient descriptor {fd}: {error}"));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(format!(
            "protect ambient descriptor {fd}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn exact_launcher_command(launcher_fd: RawFd, argument_fd: RawFd) -> Command {
    let mut command = Command::new(format!("/proc/self/fd/{launcher_fd}"));
    command
        .args(["--args", &argument_fd.to_string()])
        .env_clear();
    command
}

fn read_sealed_request<T: serde::de::DeserializeOwned>(fd: RawFd) -> Result<T, String> {
    validate_inherited_fd(fd, "request")?;
    require_seals(fd)?;
    // SAFETY: the adapter process owns this inherited request descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let length = file
        .metadata()
        .map_err(|error| format!("inspect request descriptor: {error}"))?
        .len() as usize;
    if length > MAX_REQUEST_BYTES {
        return Err(format!("request exceeds {MAX_REQUEST_BYTES} bytes"));
    }
    let mut bytes = Vec::with_capacity(length);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read request descriptor: {error}"))?;
    from_json_slice_strict(&bytes).map_err(|error| format!("parse strict request JSON: {error}"))
}

fn create_sealed_memfd(name: &std::ffi::CStr, bytes: &[u8]) -> Result<File, String> {
    let fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd <= libc::STDERR_FILENO {
        if fd >= 0 {
            close_fd(fd);
        }
        return Err("create argument memfd above stdio failed".to_string());
    }
    // SAFETY: memfd_create returned a unique owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)
        .map_err(|error| format!("write argument memfd: {error}"))?;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| format!("rewind argument memfd: {error}"))?;
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } < 0 {
        return Err(format!(
            "seal argument memfd: {}",
            std::io::Error::last_os_error()
        ));
    }
    require_seals(fd)?;
    Ok(file)
}

fn require_seals(fd: RawFd) -> Result<(), String> {
    let required = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    let observed = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if observed < 0 || observed & required != required {
        return Err("descriptor is not sealed against mutation".to_string());
    }
    Ok(())
}

fn encode_nul_arguments(arguments: &[String]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for argument in arguments {
        if argument.as_bytes().contains(&0) {
            return Err("Bubblewrap argument contains an interior NUL".to_string());
        }
        bytes.extend_from_slice(argument.as_bytes());
        bytes.push(0);
    }
    Ok(bytes)
}

fn append_parent_directories(
    arguments: &mut Vec<String>,
    destination: &str,
    created: &mut BTreeSet<String>,
) {
    let mut parents = Vec::new();
    let mut current = std::path::Path::new(destination).parent();
    while let Some(parent) = current {
        if parent == std::path::Path::new("/") {
            break;
        }
        parents.push(parent.to_string_lossy().into_owned());
        current = parent.parent();
    }
    parents.reverse();
    for parent in parents {
        if created.insert(parent.clone()) {
            arguments.extend(["--dir".to_string(), parent]);
        }
    }
}

fn supported_capabilities() -> BTreeSet<IsolationCapability> {
    BTreeSet::from([
        IsolationCapability::FilesystemPrivateRoot,
        IsolationCapability::FilesystemFdReadOnly,
        IsolationCapability::FilesystemFdWritable,
        IsolationCapability::FilesystemOrderedOverlays,
        IsolationCapability::FilesystemPrivateTmp,
        IsolationCapability::DevicesMinimal,
        IsolationCapability::EnvironmentExact,
        IsolationCapability::NetworkHost,
        IsolationCapability::NetworkIsolated,
        IsolationCapability::ProcessHostPidNamespace,
        IsolationCapability::ProcessTargetPidReporting,
        IsolationCapability::LifecycleSharedProcessGroup,
    ])
}

fn digest_fd(fd: RawFd) -> Result<String, String> {
    let path = format!("/proc/self/fd/{fd}");
    let mut file =
        File::open(path).map_err(|error| format!("open artifact for digest: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read artifact for digest: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn write_response(response: &AdapterInspectionResponse) -> ! {
    response
        .validate()
        .unwrap_or_else(|error| fail_process(&format!("validate inspection response: {error}")));
    let bytes = serde_json::to_vec(response)
        .unwrap_or_else(|error| fail_process(&format!("serialize inspection response: {error}")));
    if bytes.len() > MAX_RESPONSE_BYTES {
        fail_process("inspection response exceeds protocol limit");
    }
    if std::io::stdout().write_all(&bytes).is_err() {
        std::process::exit(1);
    }
    std::process::exit(0)
}

fn emit_refusal(status_fd: RawFd, message: String) -> ! {
    let diagnostic = IsolationDiagnostic {
        code: IsolationDiagnosticCode::LaunchRefused,
        message,
        details: BTreeMap::new(),
    };
    let document = LauncherRefusalDocument {
        refused: diagnostic,
    };
    if validate_inherited_fd(status_fd, "status writer").is_ok() {
        if let Ok(mut bytes) = serde_json::to_vec(&document) {
            bytes.push(b'\n');
            // SAFETY: failure is terminal and this process owns the inherited writer.
            let mut writer = unsafe { File::from_raw_fd(status_fd) };
            let _ = writer.write_all(&bytes);
        }
    }
    std::process::exit(126)
}

fn parse_fd(value: &OsString) -> Result<RawFd, String> {
    let text = value
        .to_str()
        .ok_or_else(|| "request descriptor is not UTF-8".to_string())?;
    let fd: RawFd = text
        .parse()
        .map_err(|_| "request descriptor is not numeric".to_string())?;
    validate_inherited_fd(fd, "request")?;
    Ok(fd)
}

fn validate_inherited_fd(fd: RawFd, kind: &str) -> Result<(), String> {
    if fd <= libc::STDERR_FILENO {
        return Err(format!("{kind} descriptor overlaps stdio"));
    }
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(format!("{kind} descriptor is invalid"));
    }
    Ok(())
}

fn set_cloexec(fd: RawFd, enabled: bool) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(format!(
            "inspect descriptor {fd}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let updated = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, updated) } < 0 {
        return Err(format!(
            "configure descriptor {fd}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

fn fail_process(message: &str) -> ! {
    eprintln!("ryeos-bubblewrap-adapter: {message}");
    std::process::exit(125)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd as _;

    use ryeos_isolation_protocol::{
        IsolationAuthority, IsolationAuthorityId, IsolationAuthorityPurpose,
        IsolationDeviceSurface, IsolationEnvironment, IsolationMount, IsolationPath, IsolationPlan,
        IsolationTarget,
    };

    fn valid_inspection_request() -> AdapterInspectionRequest {
        AdapterInspectionRequest {
            protocol: IsolationAdapterProtocolVersion::V1,
            target: host_target().expect("adapter tests require a supported Linux GNU target"),
            backend_id: BACKEND_ID.to_string(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 3)]),
        }
    }

    fn valid_launch_request() -> (AdapterLaunchRequest, Vec<File>) {
        let launcher = File::open("/dev/null").unwrap();
        let target = File::open("/dev/null").unwrap();
        let project = File::open("/").unwrap();
        let workspace = File::open("/tmp").unwrap();
        let status = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();

        let target_id = IsolationAuthorityId::new("target").unwrap();
        let project_id = IsolationAuthorityId::new("project").unwrap();
        let workspace_id = IsolationAuthorityId::new("workspace").unwrap();
        let request = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::V1,
            plan: IsolationPlan {
                target: IsolationTarget {
                    executable: target_id.clone(),
                    argv0: "tool".to_string(),
                    arguments: vec!["--flag".to_string(), "secret-value".to_string()],
                    cwd: IsolationPath::new("/workspace").unwrap(),
                },
                mounts: vec![
                    IsolationMount {
                        source: target_id.clone(),
                        destination: IsolationPath::new("/opt/bin/tool").unwrap(),
                        access: IsolationMountAccess::ReadOnly,
                        layer: 0,
                    },
                    IsolationMount {
                        source: project_id.clone(),
                        destination: IsolationPath::new("/project").unwrap(),
                        access: IsolationMountAccess::ReadOnly,
                        layer: 1,
                    },
                    IsolationMount {
                        source: workspace_id.clone(),
                        destination: IsolationPath::new("/workspace").unwrap(),
                        access: IsolationMountAccess::Writable,
                        layer: 2,
                    },
                ],
                environment: IsolationEnvironment {
                    values: BTreeMap::from([
                        ("API_TOKEN".to_string(), "secret-token".to_string()),
                        ("TMPDIR".to_string(), "/tmp".to_string()),
                    ]),
                },
                network: IsolationNetwork::Isolated,
                devices: IsolationDeviceSurface::Minimal,
                private_tmp: true,
                host_pid_namespace: true,
                shared_process_group: true,
            },
            authorities: vec![
                IsolationAuthority {
                    id: target_id,
                    inherited_fd: target.as_raw_fd() as u32,
                    purpose: IsolationAuthorityPurpose::Executable,
                },
                IsolationAuthority {
                    id: project_id,
                    inherited_fd: project.as_raw_fd() as u32,
                    purpose: IsolationAuthorityPurpose::ReadOnlyMount,
                },
                IsolationAuthority {
                    id: workspace_id,
                    inherited_fd: workspace.as_raw_fd() as u32,
                    purpose: IsolationAuthorityPurpose::WritableMount,
                },
            ],
            artifacts: BTreeMap::from([(
                IsolationArtifactRole::Launcher,
                launcher.as_raw_fd() as u32,
            )]),
            status_fd: status.as_raw_fd() as u32,
        };
        (request, vec![launcher, target, project, workspace, status])
    }

    #[test]
    fn inspection_identity_is_exact_and_target_bound() {
        let request = valid_inspection_request();
        validate_inspection_identity(&request).unwrap();

        let mut wrong_backend = request.clone();
        wrong_backend.backend_id = "another-backend".to_string();
        assert!(validate_inspection_identity(&wrong_backend)
            .unwrap_err()
            .contains("implements backend"));

        let mut wrong_target = request;
        wrong_target.target = match wrong_target.target {
            IsolationTargetTriple::X86_64UnknownLinuxGnu => {
                IsolationTargetTriple::Aarch64UnknownLinuxGnu
            }
            IsolationTargetTriple::Aarch64UnknownLinuxGnu => {
                IsolationTargetTriple::X86_64UnknownLinuxGnu
            }
        };
        assert!(validate_inspection_identity(&wrong_target)
            .unwrap_err()
            .contains("does not match"));
    }

    #[test]
    fn launcher_version_is_strict_and_minimum_bounded() {
        for accepted in ["0.11.0", "0.11.1", "1.0.0", "12.34.56"] {
            require_launcher_version(accepted).unwrap();
        }
        for refused in ["0.10.9", "0.11", "0.11.0.1", "v0.11.0", "0.11.x"] {
            assert!(require_launcher_version(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn inspection_executes_and_digests_the_exact_launcher_artifact() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let launcher_path = directory.path().join("bwrap");
        std::fs::write(
            &launcher_path,
            b"#!/bin/sh\ncase \"$1\" in\n  --version) printf 'bubblewrap 0.11.0\\n' ;;\n  --help) printf '%s\\n' '--args --argv0 --bind-fd --chdir --clearenv --dev --dir --json-status-fd --ro-bind-fd --setenv --tmpfs --unshare-ipc --unshare-net --unshare-user --unshare-uts' ;;\n  *) exit 2 ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&launcher_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let launcher = File::open(&launcher_path).unwrap();
        // Script interpreters reopen `/proc/self/fd/N`; unlike the production
        // ELF launcher, this fixture therefore needs the descriptor inherited.
        set_cloexec(launcher.as_raw_fd(), false).unwrap();
        let mut request = valid_inspection_request();
        request
            .artifacts
            .insert(IsolationArtifactRole::Launcher, launcher.as_raw_fd() as u32);
        let request_bytes = serde_json::to_vec(&request).unwrap();
        let request_file = create_sealed_memfd(c"adapter-inspection-test", &request_bytes).unwrap();

        let response = inspect(request_file.into_raw_fd()).unwrap();
        response.validate().unwrap();
        assert_eq!(
            response.artifacts[&IsolationArtifactRole::Launcher].version,
            "bubblewrap 0.11.0"
        );
        assert_eq!(
            response.artifacts[&IsolationArtifactRole::Launcher].digest,
            digest_fd(launcher.as_raw_fd()).unwrap()
        );
    }

    #[test]
    fn probe_capture_refuses_oversized_streams() {
        let error = read_probe_stream(
            std::io::Cursor::new(vec![b'x'; MAX_RESPONSE_BYTES + 1]),
            "stdout",
        )
        .unwrap_err();
        assert!(error.contains("exceeds"));
    }

    #[test]
    fn launch_compilation_preserves_order_and_all_plan_operations() {
        let (request, _handles) = valid_launch_request();
        let prepared = prepare_launch(&request).unwrap();
        assert_eq!(
            prepared.launcher_fd,
            request.artifacts[&IsolationArtifactRole::Launcher] as RawFd
        );
        assert!(prepared
            .arguments
            .windows(2)
            .any(|pair| pair == ["--tmpfs", "/"]));
        assert!(prepared
            .arguments
            .windows(2)
            .any(|pair| pair == ["--tmpfs", "/tmp"]));
        assert!(prepared
            .arguments
            .iter()
            .any(|value| value == "--unshare-net"));
        assert!(prepared
            .arguments
            .windows(3)
            .any(|values| { values == ["--setenv", "API_TOKEN", "secret-token"] }));
        assert!(prepared
            .arguments
            .windows(3)
            .any(|values| { values[0] == "--ro-bind-fd" && values[2] == "/opt/bin/tool" }));
        assert!(prepared
            .arguments
            .windows(3)
            .any(|values| { values[0] == "--bind-fd" && values[2] == "/workspace" }));
        assert_eq!(
            &prepared.arguments[prepared.arguments.len() - 3..],
            ["/opt/bin/tool", "--flag", "secret-value"]
        );
    }

    #[test]
    fn host_visible_launcher_command_contains_only_the_sealed_argument_descriptor() {
        let command = exact_launcher_command(41, 42);
        assert_eq!(command.get_program(), "/proc/self/fd/41");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("--args"), std::ffi::OsStr::new("42")]
        );
        let rendered = format!("{command:?}");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("API_TOKEN"));
    }

    #[test]
    fn launcher_boundary_closes_every_unreferenced_descriptor_on_exec() {
        let (request, _handles) = valid_launch_request();
        let prepared = prepare_launch(&request).unwrap();
        let ambient = File::open("/dev/null").unwrap();
        set_cloexec(ambient.as_raw_fd(), false).unwrap();

        seal_descriptor_boundary(prepared.launcher_fd, &prepared.inherited_fds).unwrap();

        let flags = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(flags(ambient.as_raw_fd()) & libc::FD_CLOEXEC, 0);
        assert_ne!(flags(prepared.launcher_fd) & libc::FD_CLOEXEC, 0);
        for fd in prepared
            .inherited_fds
            .iter()
            .filter(|fd| **fd != prepared.launcher_fd)
        {
            assert_eq!(flags(*fd) & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn host_network_omits_only_the_network_namespace_operation() {
        let (isolated_request, _handles) = valid_launch_request();
        let mut host_request = isolated_request.clone();
        host_request.plan.network = IsolationNetwork::Host;
        let host = prepare_launch(&host_request).unwrap();
        assert!(!host.arguments.iter().any(|value| value == "--unshare-net"));

        let isolated = prepare_launch(&isolated_request).unwrap();
        let mut expected = isolated.arguments;
        expected.retain(|value| value != "--unshare-net");
        assert_eq!(host.arguments, expected);
    }

    #[test]
    fn launch_refuses_descriptor_reuse_unknown_artifacts_and_invalid_strings() {
        let (mut duplicate, _duplicate_handles) = valid_launch_request();
        duplicate.status_fd = duplicate.artifacts[&IsolationArtifactRole::Launcher];
        assert!(prepare_launch(&duplicate)
            .unwrap_err()
            .contains("reused across"));

        let (mut extra_artifact, mut handles) = valid_launch_request();
        let loader = File::open("/dev/null").unwrap();
        extra_artifact
            .artifacts
            .insert(IsolationArtifactRole::Loader, loader.as_raw_fd() as u32);
        handles.push(loader);
        assert!(prepare_launch(&extra_artifact)
            .unwrap_err()
            .contains("does not support dynamic-loader"));

        let (mut invalid_argument, _invalid_handles) = valid_launch_request();
        invalid_argument.plan.target.arguments[0] = "bad\0argument".to_string();
        assert!(prepare_launch(&invalid_argument)
            .unwrap_err()
            .contains("interior NUL"));
    }

    #[test]
    fn request_reader_requires_sealed_strict_json() {
        let bytes = serde_json::to_vec(&valid_inspection_request()).unwrap();
        let sealed = create_sealed_memfd(c"adapter-test-request", &bytes).unwrap();
        let decoded: AdapterInspectionRequest = read_sealed_request(sealed.into_raw_fd()).unwrap();
        assert_eq!(decoded.backend_id, BACKEND_ID);

        let unsealed_fd =
            unsafe { libc::memfd_create(c"adapter-test-unsealed".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(unsealed_fd > libc::STDERR_FILENO);
        let mut unsealed = unsafe { File::from_raw_fd(unsealed_fd) };
        unsealed.write_all(&bytes).unwrap();
        unsealed.seek(std::io::SeekFrom::Start(0)).unwrap();
        assert!(
            read_sealed_request::<AdapterInspectionRequest>(unsealed.as_raw_fd())
                .unwrap_err()
                .contains("not sealed")
        );

        let duplicate = br#"{"protocol":"ryeos.isolation-adapter/v1","target":"x86_64-unknown-linux-gnu","backend_id":"linux-bubblewrap","backend_id":"linux-bubblewrap","artifacts":{"launcher":3}}"#;
        let sealed_duplicate = create_sealed_memfd(c"adapter-test-duplicate", duplicate).unwrap();
        assert!(
            read_sealed_request::<AdapterInspectionRequest>(sealed_duplicate.into_raw_fd())
                .unwrap_err()
                .contains("duplicate JSON object key")
        );
    }

    #[test]
    fn nul_argument_encoding_is_exact_and_rejects_ambiguity() {
        assert_eq!(
            encode_nul_arguments(&["one".to_string(), "two words".to_string()]).unwrap(),
            b"one\0two words\0"
        );
        assert!(encode_nul_arguments(&["bad\0argument".to_string()]).is_err());
    }
}
