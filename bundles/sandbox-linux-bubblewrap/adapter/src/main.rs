use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ryeos_isolation_protocol::{
    AdapterInspectionRequest, AdapterInspectionResponse, AdapterLaunchLifecycle,
    AdapterLaunchRequest, AdapterWorkspaceRequest, AdapterWorkspaceResponse, InspectedArtifact,
    IsolationAdapterProtocolVersion, IsolationArtifactRole, IsolationAuthorityPurpose,
    IsolationCapability, IsolationDiagnostic, IsolationDiagnosticCode, IsolationMountAccess,
    IsolationNetwork, IsolationTargetTriple, LauncherRefusalDocument, MAX_REQUEST_BYTES,
    MAX_RESPONSE_BYTES, MAX_WORKSPACE_MUTATIONS, MAX_WORKSPACE_RESPONSE_BYTES,
    WorkspaceLifecycleOperation, WorkspaceMutation, WorkspaceMutationKind, from_json_slice_strict,
};
use sha2::{Digest as _, Sha256};

const ADAPTER_BUILD: &str = env!("CARGO_PKG_VERSION");
const BACKEND_ID: &str = "linux-bubblewrap";
const LAUNCHER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const TARGET_ARGV_BRIDGE_PATH: &str = "/run/ryeos/argv-bridge";
const MAX_ADAPTER_BRIDGE_BYTES: usize = 64 * 1024 * 1024;

fn main() {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_default();
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
        Some("workspace") => {
            let result = workspace(request_fd);
            match result {
                Ok(response) => write_workspace_response(&response),
                Err(error) => fail_process(&error),
            }
        }
        Some("exec-sealed-argv") => execute_sealed_argv(request_fd, program),
        _ => fail_process("unsupported adapter mode"),
    }
}

/// Internal sandbox-side bridge for the exact target argv.
///
/// Bubblewrap's `--args FD` deliberately parses setup options only; a command
/// found in that nested file is not propagated to its outer argv. Keep target
/// arguments out of the host-visible process list by executing this already
/// admitted static adapter from its retained descriptor, then reading the
/// exact target vector from a second sealed descriptor inside the sandbox.
fn execute_sealed_argv(argv_fd: RawFd, target_argv0: OsString) -> ! {
    let arguments = read_sealed_argv(argv_fd).unwrap_or_else(|error| fail_process(&error));
    let (program, arguments) = arguments
        .split_first()
        .unwrap_or_else(|| fail_process("sealed target argv has no executable"));
    if target_argv0.is_empty() {
        fail_process("sealed target argv0 is empty");
    }
    if let Err(error) = seal_target_descriptor_boundary() {
        fail_process(&error);
    }
    let error = Command::new(program)
        .arg0(target_argv0)
        .args(arguments)
        .exec();
    fail_process(&format!("exec sealed target argv: {error}"));
}

fn workspace(request_fd: RawFd) -> Result<AdapterWorkspaceResponse, String> {
    let request: AdapterWorkspaceRequest = read_sealed_request(request_fd)?;
    request
        .validate()
        .map_err(|error| format!("invalid workspace request: {error}"))?;
    let fd_for = |purpose| {
        request
            .authorities
            .iter()
            .find(|authority| authority.purpose == purpose)
            .map(|authority| authority.inherited_fd as RawFd)
            .ok_or_else(|| "workspace request is missing an authority".to_string())
    };
    let project_fd = fd_for(IsolationAuthorityPurpose::WorkspaceProject)?;
    let backend_state_fd = fd_for(IsolationAuthorityPurpose::WorkspaceBackendState)?;
    let _project = pin_directory_fd(project_fd, "project")?;
    let backend_state = pin_directory_fd(backend_state_fd, "backend state")?;
    let upper = match request.operation {
        WorkspaceLifecycleOperation::Create => backend_state
            .open_or_create_child(std::ffi::OsStr::new("upper"), 0o700)
            .map_err(|error| format!("create backend-private upper directory: {error}"))?,
        WorkspaceLifecycleOperation::FreezeAndDiff | WorkspaceLifecycleOperation::Destroy => {
            backend_state
                .open_child_directory(std::ffi::OsStr::new("upper"))
                .map_err(|error| format!("open backend-private upper directory: {error}"))?
                .ok_or_else(|| "backend-private upper directory is missing".to_string())?
        }
    };
    let _work = match request.operation {
        WorkspaceLifecycleOperation::Create => backend_state
            .open_or_create_child(std::ffi::OsStr::new("work"), 0o700)
            .map_err(|error| format!("create backend-private work directory: {error}"))?,
        WorkspaceLifecycleOperation::FreezeAndDiff | WorkspaceLifecycleOperation::Destroy => {
            backend_state
                .open_child_directory(std::ffi::OsStr::new("work"))
                .map_err(|error| format!("open backend-private work directory: {error}"))?
                .ok_or_else(|| "backend-private work directory is missing".to_string())?
        }
    };
    let entries = backend_state
        .entries_no_follow_bounded(3)
        .map_err(|error| format!("inventory backend-private workspace state: {error}"))?;
    if entries.len() != 2
        || entries.iter().any(|entry| {
            entry.entry_type != lillux::PinnedEntryType::Directory
                || !matches!(entry.name.to_str(), Some("upper" | "work"))
        })
    {
        return Err("backend-private workspace state has an invalid layout".to_string());
    }
    let pinned_root_identities = BTreeMap::from([
        ("project".to_string(), directory_identity(project_fd)?),
        (
            "backend_state".to_string(),
            directory_identity(backend_state_fd)?,
        ),
    ]);
    let mount_identity = format!(
        "native-overlay:{}:{}",
        pinned_root_identities["project"], pinned_root_identities["backend_state"]
    );
    let mutations = match request.operation {
        WorkspaceLifecycleOperation::Create | WorkspaceLifecycleOperation::Destroy => Vec::new(),
        WorkspaceLifecycleOperation::FreezeAndDiff => {
            let upper = upper
                .try_clone_descriptor()
                .map_err(|error| format!("clone backend-private upper directory: {error}"))?;
            scan_workspace_upper(upper.as_raw_fd())?
        }
    };
    let response = AdapterWorkspaceResponse {
        protocol: IsolationAdapterProtocolVersion::Current,
        operation: request.operation,
        workspace_id: request.workspace_id.clone(),
        launch_owner: request.launch_owner.clone(),
        backend_id: BACKEND_ID.to_string(),
        backend_version: ADAPTER_BUILD.to_string(),
        pinned_root_identities,
        mount_identity,
        mutation_content_root: (request.operation == WorkspaceLifecycleOperation::FreezeAndDiff)
            .then(|| "upper".to_string()),
        mutations,
        destroyed: request.operation == WorkspaceLifecycleOperation::Destroy,
    };
    response
        .validate_for(&request)
        .map_err(|error| format!("invalid workspace response: {error}"))?;
    Ok(response)
}

fn pin_directory_fd(fd: RawFd, label: &str) -> Result<lillux::PinnedDirectory, String> {
    validate_directory_fd(fd, label)?;
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(format!(
            "duplicate workspace {label} authority: {}",
            std::io::Error::last_os_error()
        ));
    }
    let file = unsafe { File::from_raw_fd(duplicate) };
    lillux::PinnedDirectory::from_open_directory(
        std::path::PathBuf::from(format!("<workspace-{label}>")),
        file,
    )
    .map_err(|error| format!("pin workspace {label} authority: {error}"))
}

fn validate_directory_fd(fd: RawFd, label: &str) -> Result<(), String> {
    validate_inherited_fd(fd, label)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(format!(
            "inspect workspace {label}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(format!("workspace {label} authority is not a directory"));
    }
    Ok(())
}

fn directory_identity(fd: RawFd) -> Result<String, String> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(format!(
            "inspect workspace directory identity: {}",
            std::io::Error::last_os_error()
        ));
    }
    let stat = unsafe { stat.assume_init() };
    Ok(format!("dev{}-ino{}", stat.st_dev, stat.st_ino))
}

fn scan_workspace_upper(root_fd: RawFd) -> Result<Vec<WorkspaceMutation>, String> {
    let mut mutations = Vec::new();
    scan_workspace_directory(root_fd, "", &mut mutations)?;
    mutations.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| format!("{:?}", left.kind).cmp(&format!("{:?}", right.kind)))
    });
    if mutations.len() > MAX_WORKSPACE_MUTATIONS {
        return Err(format!(
            "workspace delta exceeds {MAX_WORKSPACE_MUTATIONS} mutations"
        ));
    }
    Ok(mutations)
}

fn scan_workspace_directory(
    directory_fd: RawFd,
    prefix: &str,
    mutations: &mut Vec<WorkspaceMutation>,
) -> Result<(), String> {
    let directory_path = format!("/proc/self/fd/{directory_fd}");
    let mut entries = std::fs::read_dir(&directory_path)
        .map_err(|error| format!("read workspace upper directory: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read workspace upper entry: {error}"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "workspace delta path is not UTF-8".to_string())?;
        if matches!(name.as_str(), "." | "..") || name.contains('/') {
            return Err("workspace delta contains an invalid path component".to_string());
        }
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let c_name = std::ffi::CString::new(name)
            .map_err(|_| "workspace path contains an interior NUL".to_string())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory_fd,
                c_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(format!(
                "inspect workspace mutation {relative}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let stat = unsafe { stat.assume_init() };
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                let (size, content_hash) =
                    hash_regular_at(directory_fd, &c_name, &stat, &relative)?;
                mutations.push(WorkspaceMutation {
                    path: relative,
                    kind: WorkspaceMutationKind::UpsertRegular,
                    normalized_mode: Some(if stat.st_mode & 0o111 != 0 {
                        0o755
                    } else {
                        0o644
                    }),
                    size: Some(size),
                    content_hash: Some(content_hash),
                });
            }
            libc::S_IFDIR => {
                let child = unsafe {
                    libc::openat(
                        directory_fd,
                        c_name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if child < 0 {
                    return Err(format!(
                        "open workspace mutation directory {relative}: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let opaque = directory_is_opaque_fd(child)?;
                mutations.push(WorkspaceMutation {
                    path: relative.clone(),
                    kind: if opaque {
                        WorkspaceMutationKind::OpaqueDirectory
                    } else {
                        WorkspaceMutationKind::EnsureDirectory
                    },
                    normalized_mode: None,
                    size: None,
                    content_hash: None,
                });
                let result = scan_workspace_directory(child, &relative, mutations);
                close_fd(child);
                result?;
            }
            libc::S_IFCHR if stat.st_rdev == 0 => mutations.push(WorkspaceMutation {
                path: relative,
                kind: WorkspaceMutationKind::DeletePath,
                normalized_mode: None,
                size: None,
                content_hash: None,
            }),
            _ => {
                return Err(format!(
                    "workspace delta contains unsupported entry type at {relative}"
                ));
            }
        }
    }
    Ok(())
}

fn hash_regular_at(
    directory_fd: RawFd,
    name: &std::ffi::CStr,
    expected: &libc::stat,
    relative: &str,
) -> Result<(u64, String), String> {
    let fd = unsafe {
        libc::openat(
            directory_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "open workspace mutation {relative}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let verify_identity = |file: &File| -> Result<libc::stat, String> {
        let mut observed = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(file.as_raw_fd(), observed.as_mut_ptr()) } != 0 {
            return Err(format!(
                "inspect opened workspace mutation {relative}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let observed = unsafe { observed.assume_init() };
        if observed.st_dev != expected.st_dev
            || observed.st_ino != expected.st_ino
            || observed.st_size != expected.st_size
            || observed.st_mode & libc::S_IFMT != libc::S_IFREG
        {
            return Err(format!(
                "workspace mutation changed identity during freeze: {relative}"
            ));
        }
        Ok(observed)
    };
    verify_identity(&file)?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read workspace mutation {relative}: {error}"))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(read).map_err(|_| "workspace read size overflow")?)
            .ok_or_else(|| "workspace read size overflow".to_string())?;
        digest.update(&buffer[..read]);
    }
    let after = verify_identity(&file)?;
    if total != u64::try_from(after.st_size).map_err(|_| "negative workspace file size")? {
        return Err(format!(
            "workspace mutation changed size during freeze: {relative}"
        ));
    }
    Ok((total, format!("{:x}", digest.finalize())))
}

fn directory_is_opaque_fd(fd: RawFd) -> Result<bool, String> {
    for name in [c"trusted.overlay.opaque", c"user.overlay.opaque"] {
        let mut value = [0u8; 16];
        let read =
            unsafe { libc::fgetxattr(fd, name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
        if read > 0 && matches!(value[0], b'y' | b'Y') {
            return Ok(true);
        }
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(code) if code == libc::ENODATA || code == libc::ENOTSUP)
            {
                return Err(format!("read workspace opaque marker: {error}"));
            }
        }
    }
    Ok(false)
}

fn inspect(request_fd: RawFd) -> Result<AdapterInspectionResponse, String> {
    let request: AdapterInspectionRequest = read_sealed_request(request_fd)?;
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
        "--block-fd",
        "--chdir",
        "--clearenv",
        "--dev",
        "--die-with-parent",
        "--dir",
        "--json-status-fd",
        "--overlay",
        "--overlay-src",
        "--ro-bind-fd",
        "--seccomp",
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
        protocol: IsolationAdapterProtocolVersion::Current,
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
    lifecycle: PreparedLaunchLifecycle,
    target_channel_source: Option<RawFd>,
    inherited_fds: BTreeSet<RawFd>,
    arguments: Vec<String>,
    target_command: Vec<String>,
    retained_files: Vec<File>,
    next_internal_fd: RawFd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedLaunchLifecycle {
    Run,
    AwaitAttachment { release_keepalive_fd: RawFd },
}

fn prepare_launch(request: &AdapterLaunchRequest) -> Result<PreparedLaunch, String> {
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
    validate_current_adapter(request.adapter_fd as RawFd)?;
    validate_inherited_fd(request.status_fd as RawFd, "status writer")?;
    if let AdapterLaunchLifecycle::AwaitAttachment {
        release_fd,
        release_keepalive_fd,
    } = request.lifecycle
    {
        validate_inherited_fd(release_fd as RawFd, "attachment release reader")?;
        validate_inherited_fd(
            release_keepalive_fd as RawFd,
            "attachment release keepalive writer",
        )?;
    }
    for authority in &request.authorities {
        validate_inherited_fd(authority.inherited_fd as RawFd, "plan authority")?;
        if authority.purpose != IsolationAuthorityPurpose::TargetDuplexChannel {
            validate_bubblewrap_source_fd(
                authority.inherited_fd as RawFd,
                &format!("plan authority {}", authority.id.as_str()),
            )?;
        }
    }
    let target_channel_source = request
        .plan
        .target_channel
        .as_ref()
        .map(|channel| {
            let authority = request
                .authorities
                .iter()
                .find(|authority| authority.id == channel.source)
                .ok_or_else(|| "target channel source disappeared after validation".to_owned())?;
            let fd = authority.inherited_fd as RawFd;
            validate_connected_unix_stream(fd)?;
            Ok::<RawFd, String>(fd)
        })
        .transpose()?;
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

    let mut retained_files = Vec::new();
    let mut arguments = vec![
        "--json-status-fd".to_string(),
        request.status_fd.to_string(),
        "--die-with-parent".to_string(),
        "--clearenv".to_string(),
        "--unshare-user".to_string(),
        "--unshare-ipc".to_string(),
        "--unshare-uts".to_string(),
    ];
    let lifecycle = match request.lifecycle {
        AdapterLaunchLifecycle::Run => PreparedLaunchLifecycle::Run,
        AdapterLaunchLifecycle::AwaitAttachment {
            release_fd,
            release_keepalive_fd,
        } => {
            arguments.splice(2..2, ["--block-fd".to_string(), release_fd.to_string()]);
            PreparedLaunchLifecycle::AwaitAttachment {
                release_keepalive_fd: release_keepalive_fd as RawFd,
            }
        }
    };
    if request.plan.network == IsolationNetwork::Isolated {
        arguments.push("--unshare-net".to_string());
    }
    if request.plan.shared_process_group {
        // Durable workspace quiescence is group-scoped. Prevent the sandboxed
        // process tree from escaping that exact group with setsid/setpgid;
        // every descendant inherits this seccomp filter. The outer Lillux
        // launcher establishes the group before Bubblewrap installs it.
        let filter = create_process_group_containment_filter()?;
        arguments.extend(["--seccomp".to_string(), filter.as_raw_fd().to_string()]);
        retained_files.push(filter);
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
    if let Some(workspace) = &request.plan.project_workspace {
        append_parent_directories(
            &mut arguments,
            workspace.destination.as_str(),
            &mut created_directories,
        );
    }
    if let Some(workspace) = &request.plan.project_workspace {
        let project = authority_by_id.get(&workspace.project).ok_or_else(|| {
            "workspace project authority disappeared after validation".to_string()
        })?;
        let backend_state_authority =
            authority_by_id
                .get(&workspace.backend_state)
                .ok_or_else(|| {
                    "workspace backend-state authority disappeared after validation".to_string()
                })?;
        let backend_state = pin_directory_fd(
            backend_state_authority.inherited_fd as RawFd,
            "backend state",
        )?;
        let upper = backend_state
            .open_child_directory(std::ffi::OsStr::new("upper"))
            .map_err(|error| format!("open backend-private upper directory: {error}"))?
            .ok_or_else(|| "backend-private upper directory is missing".to_string())?;
        let work = backend_state
            .open_child_directory(std::ffi::OsStr::new("work"))
            .map_err(|error| format!("open backend-private work directory: {error}"))?
            .ok_or_else(|| "backend-private work directory is missing".to_string())?;
        let upper = upper
            .try_clone_descriptor()
            .map_err(|error| format!("clone backend-private upper directory: {error}"))?;
        let work = work
            .try_clone_descriptor()
            .map_err(|error| format!("clone backend-private work directory: {error}"))?;
        arguments.extend([
            "--overlay-src".to_string(),
            format!("/proc/self/fd/{}", project.inherited_fd),
            "--overlay".to_string(),
            format!("/proc/self/fd/{}", upper.as_raw_fd()),
            format!("/proc/self/fd/{}", work.as_raw_fd()),
            workspace.destination.as_str().to_string(),
        ]);
        retained_files.extend([upper, work]);
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
    ]);

    let target_argv = std::iter::once(target_mount.destination.as_str().to_string())
        .chain(request.plan.target.arguments.iter().cloned())
        .collect::<Vec<_>>();
    let mut next_internal_fd = first_internal_descriptor(request)?;
    let target_argv_file = relocate_internal_file(
        create_sealed_memfd(c"ryeos-target-argv", &encode_nul_arguments(&target_argv)?)?,
        &mut next_internal_fd,
        "target argv",
    )?;
    let target_argv_fd = target_argv_file.as_raw_fd();
    retained_files.push(target_argv_file);
    let bridge =
        capture_current_adapter_bridge(request.adapter_fd as RawFd, &mut next_internal_fd)?;
    validate_inherited_fd(bridge.as_raw_fd(), "target argv bridge")?;
    let bridge_fd = bridge.as_raw_fd();
    retained_files.push(bridge);
    append_parent_directories(
        &mut arguments,
        TARGET_ARGV_BRIDGE_PATH,
        &mut created_directories,
    );
    arguments.extend([
        "--perms".to_owned(),
        "0500".to_owned(),
        "--ro-bind-data".to_owned(),
        bridge_fd.to_string(),
        TARGET_ARGV_BRIDGE_PATH.to_owned(),
    ]);
    let target_command = vec![
        TARGET_ARGV_BRIDGE_PATH.to_owned(),
        "exec-sealed-argv".to_owned(),
        target_argv_fd.to_string(),
    ];

    let mut inherited_fds: BTreeSet<_> = request
        .authorities
        .iter()
        .map(|authority| authority.inherited_fd as RawFd)
        .chain([launcher_fd, request.status_fd as RawFd])
        .chain(retained_files.iter().map(|file| file.as_raw_fd()))
        .collect();
    if let Some(workspace) = &request.plan.project_workspace {
        let backend_state = authority_by_id
            .get(&workspace.backend_state)
            .ok_or_else(|| {
                "workspace backend-state authority disappeared after validation".to_string()
            })?;
        inherited_fds.remove(&(backend_state.inherited_fd as RawFd));
    }
    if let AdapterLaunchLifecycle::AwaitAttachment {
        release_fd,
        release_keepalive_fd,
    } = request.lifecycle
    {
        inherited_fds.extend([release_fd as RawFd, release_keepalive_fd as RawFd]);
    }
    Ok(PreparedLaunch {
        launcher_fd,
        lifecycle,
        target_channel_source,
        inherited_fds,
        arguments,
        target_command,
        retained_files,
        next_internal_fd,
    })
}

fn exec_launcher(mut prepared: PreparedLaunch) -> Result<std::convert::Infallible, String> {
    let bytes = encode_nul_arguments(&prepared.arguments)?;
    let argument_file = relocate_internal_file(
        create_sealed_memfd(c"ryeos-bwrap-args", &bytes)?,
        &mut prepared.next_internal_fd,
        "Bubblewrap argument vector",
    )?;
    let argument_fd = argument_file.as_raw_fd();
    prepared.inherited_fds.insert(argument_fd);
    // Keep auxiliary descriptors owned until exec. Their fd numbers are
    // embedded in the sealed Bubblewrap argument vector.
    let retained_files = std::mem::take(&mut prepared.retained_files);

    if let PreparedLaunchLifecycle::AwaitAttachment {
        release_keepalive_fd,
    } = prepared.lifecycle
    {
        spawn_attachment_release_keepalive(release_keepalive_fd)?;
        prepared.inherited_fds.remove(&release_keepalive_fd);
        close_fd(release_keepalive_fd);
    }
    relocate_target_channel(&mut prepared)?;
    seal_descriptor_boundary(prepared.launcher_fd, &prepared.inherited_fds)?;

    let error =
        exact_launcher_command(prepared.launcher_fd, argument_fd, &prepared.target_command).exec();
    // `CommandExt::exec` borrows no descriptor owners. Make their lifetime
    // explicit across that call: Bubblewrap resolves every fd-backed mount
    // (including the sandbox-side argv bridge) after the exec boundary.
    // On success this process image is replaced; on failure these drops keep
    // the adapter's error path leak-free.
    drop(retained_files);
    drop(argument_file);
    Err(format!("exec exact Bubblewrap launcher: {error}"))
}

fn relocate_target_channel(prepared: &mut PreparedLaunch) -> Result<(), String> {
    if let Some(source_fd) = prepared.target_channel_source.take() {
        if unsafe { libc::dup2(source_fd, libc::STDIN_FILENO) } < 0 {
            return Err(format!(
                "relocate target channel onto stdin: {}",
                std::io::Error::last_os_error()
            ));
        }
        prepared.inherited_fds.remove(&source_fd);
        close_fd(source_fd);
    }
    Ok(())
}

/// Retain a child-side attachment-release writer until Bubblewrap exits.
///
/// Bubblewrap deliberately treats EOF on `--block-fd` as a completed wait. A
/// daemon crash must therefore not close the final writer and accidentally
/// run an unattached target. This tiny same-group keeper owns the duplicate,
/// dies with the adapter/Bubblewrap parent, and never crosses into the target
/// process. The backend's `--die-with-parent` independently kills the blocked
/// sandbox child when the outer launcher dies.
fn spawn_attachment_release_keepalive(writer_fd: RawFd) -> Result<(), String> {
    let parent_pid = unsafe { libc::getpid() };
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(format!(
            "fork attachment-release keepalive: {}",
            std::io::Error::last_os_error()
        ));
    }
    if child == 0 {
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0
            || unsafe { libc::getppid() } != parent_pid
        {
            unsafe { libc::_exit(125) };
        }
        let close_before = writer_fd > 3
            && unsafe {
                libc::syscall(libc::SYS_close_range, 3_u32, (writer_fd - 1) as u32, 0_u32)
            } != 0;
        let close_after = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                (writer_fd + 1) as u32,
                u32::MAX,
                0_u32,
            )
        } != 0;
        if close_before || close_after {
            unsafe { libc::_exit(125) };
        }
        loop {
            unsafe { libc::pause() };
        }
    }
    Ok(())
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn create_process_group_containment_filter() -> Result<File, String> {
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_JMP_JSET_K: u16 = 0x45;
    const BPF_RET_K: u16 = 0x06;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;

    let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let filters = [
        instruction(BPF_LD_W_ABS, 0, 0, 4),
        instruction(BPF_JMP_JEQ_K, 1, 0, AUDIT_ARCH),
        instruction(BPF_RET_K, 0, 0, SECCOMP_RET_KILL_PROCESS),
        instruction(BPF_LD_W_ABS, 0, 0, 0),
        // x86_64's x32 ABI shares AUDIT_ARCH_X86_64 and distinguishes its
        // syscall table with bit 30. Deny that range before native-number
        // comparisons so the containment rules cannot be bypassed.
        instruction(BPF_JMP_JSET_K, 0, 1, 0x4000_0000),
        instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32),
        instruction(BPF_JMP_JEQ_K, 0, 1, libc::SYS_setsid as u32),
        instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32),
        instruction(BPF_JMP_JEQ_K, 0, 1, libc::SYS_setpgid as u32),
        instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32),
        instruction(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
    ];
    // sock_filter is a kernel ABI POD structure; the memfd carries the exact
    // raw BPF instruction array consumed by Bubblewrap's --seccomp option.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            filters.as_ptr().cast::<u8>(),
            std::mem::size_of_val(&filters),
        )
    };
    create_sealed_memfd(c"ryeos-process-group-seccomp", bytes)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn create_process_group_containment_filter() -> Result<File, String> {
    Err("process-group containment seccomp is unsupported on this architecture".to_string())
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

fn exact_launcher_command(
    launcher_fd: RawFd,
    argument_fd: RawFd,
    target_command: &[String],
) -> Command {
    let mut command = Command::new(format!("/proc/self/fd/{launcher_fd}"));
    command
        .args(["--args", &argument_fd.to_string(), "--"])
        .args(target_command)
        .env_clear();
    command
}

fn read_sealed_argv(fd: RawFd) -> Result<Vec<OsString>, String> {
    validate_inherited_fd(fd, "target argv")?;
    require_seals(fd)?;
    // SAFETY: the sandbox bridge owns this inherited descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let length = file
        .metadata()
        .map_err(|error| format!("inspect target argv descriptor: {error}"))?
        .len() as usize;
    if length == 0 || length > MAX_REQUEST_BYTES {
        return Err(format!(
            "target argv descriptor must contain 1..={MAX_REQUEST_BYTES} bytes"
        ));
    }
    let mut bytes = Vec::with_capacity(length);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read target argv descriptor: {error}"))?;
    if bytes.last() != Some(&0) {
        return Err("target argv descriptor is not NUL terminated".to_owned());
    }
    let mut arguments = Vec::new();
    for argument in bytes[..bytes.len() - 1].split(|byte| *byte == 0) {
        if argument.is_empty() {
            return Err("target argv contains an empty argument".to_owned());
        }
        let argument = std::str::from_utf8(argument)
            .map_err(|_| "target argv contains non-UTF-8 bytes".to_owned())?;
        arguments.push(OsString::from(argument));
    }
    if arguments.is_empty() || arguments.len() > 9000 {
        return Err("target argv has an invalid argument count".to_owned());
    }
    Ok(arguments)
}

fn first_internal_descriptor(request: &AdapterLaunchRequest) -> Result<RawFd, String> {
    let mut maximum = request.status_fd.max(request.adapter_fd);
    maximum = maximum.max(request.artifacts.values().copied().max().unwrap_or(0));
    maximum = maximum.max(
        request
            .authorities
            .iter()
            .map(|authority| authority.inherited_fd)
            .max()
            .unwrap_or(0),
    );
    if let AdapterLaunchLifecycle::AwaitAttachment {
        release_fd,
        release_keepalive_fd,
    } = request.lifecycle
    {
        maximum = maximum.max(release_fd).max(release_keepalive_fd);
    }
    let minimum = maximum
        .checked_add(1)
        .ok_or_else(|| "request descriptors leave no internal descriptor range".to_owned())?;
    RawFd::try_from(minimum.max(3))
        .map_err(|_| "request descriptors exceed the adapter descriptor range".to_owned())
}

fn relocate_internal_file(file: File, next: &mut RawFd, label: &str) -> Result<File, String> {
    let duplicate = duplicate_internal_fd(file.as_raw_fd(), next, label)?;
    drop(file);
    Ok(duplicate)
}

fn duplicate_internal_fd(source_fd: RawFd, next: &mut RawFd, label: &str) -> Result<File, String> {
    let fd = unsafe { libc::fcntl(source_fd, libc::F_DUPFD_CLOEXEC, *next) };
    if fd <= libc::STDERR_FILENO {
        if fd >= 0 {
            close_fd(fd);
        }
        return Err(format!(
            "relocate {label} above admitted descriptors: {}",
            std::io::Error::last_os_error()
        ));
    }
    *next = fd
        .checked_add(1)
        .ok_or_else(|| format!("{label} exhausted the descriptor range"))?;
    // SAFETY: F_DUPFD_CLOEXEC returned a unique owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn validate_current_adapter(fd: RawFd) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;

    validate_inherited_fd(fd, "adapter bridge")?;
    let mut retained: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut retained) } != 0 {
        return Err(format!(
            "inspect retained adapter bridge: {}",
            std::io::Error::last_os_error()
        ));
    }
    let current = std::fs::metadata("/proc/self/exe")
        .map_err(|error| format!("inspect current adapter executable: {error}"))?;
    if retained.st_dev != current.dev() || retained.st_ino != current.ino() {
        return Err(
            "retained adapter bridge does not identify the current adapter image".to_owned(),
        );
    }
    Ok(())
}

fn capture_current_adapter_bridge(
    source_fd: RawFd,
    next_internal_fd: &mut RawFd,
) -> Result<File, String> {
    use std::os::unix::fs::FileExt as _;

    let duplicate = unsafe { libc::fcntl(source_fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate <= libc::STDERR_FILENO {
        if duplicate >= 0 {
            close_fd(duplicate);
        }
        return Err(format!(
            "duplicate admitted adapter for bridge capture: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: F_DUPFD_CLOEXEC returned a unique owned descriptor.
    let source = unsafe { File::from_raw_fd(duplicate) };
    let length = usize::try_from(
        source
            .metadata()
            .map_err(|error| format!("inspect admitted adapter bridge bytes: {error}"))?
            .len(),
    )
    .map_err(|_| "admitted adapter bridge size cannot be represented".to_owned())?;
    if length == 0 || length > MAX_ADAPTER_BRIDGE_BYTES {
        return Err(format!(
            "admitted adapter bridge must contain 1..={MAX_ADAPTER_BRIDGE_BYTES} bytes"
        ));
    }
    let mut bytes = vec![0_u8; length];
    source
        .read_exact_at(&mut bytes, 0)
        .map_err(|error| format!("capture admitted adapter bridge bytes: {error}"))?;
    relocate_internal_file(
        create_sealed_memfd(c"ryeos-target-argv-bridge", &bytes)?,
        next_internal_fd,
        "target argv bridge",
    )
}

/// Bubblewrap 0.11 resolves fd-backed mount sources through
/// `/proc/self/fd/<n>` before setting up the new root. Validate that exact
/// backend precondition while the adapter still has authority labels, so a
/// deleted or otherwise unresolvable inode fails closed with its protocol
/// role rather than becoming an anonymous launcher error.
fn validate_bubblewrap_source_fd(fd: RawFd, label: &str) -> Result<(), String> {
    let source = format!("/proc/self/fd/{fd}");
    std::fs::canonicalize(&source)
        .map(|_| ())
        .map_err(|error| format!("{label} is not a resolvable Bubblewrap mount source: {error}"))
}

fn seal_target_descriptor_boundary() -> Result<(), String> {
    // The sandbox deliberately contains no ambient procfs. The bridge has
    // already consumed its sealed argv descriptor, and the only target-side
    // channel is fixed onto stdin, so close the entire auxiliary descriptor
    // range rather than discovering it through `/proc/self/fd`.
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            (libc::STDERR_FILENO + 1) as u32,
            u32::MAX,
            0_u32,
        )
    } != 0
    {
        return Err(format!(
            "close target bridge descriptor range: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
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
        IsolationCapability::FilesystemProjectWorkspaceCow,
        IsolationCapability::FilesystemWorkspaceDelta,
        IsolationCapability::FilesystemPrivateTmp,
        IsolationCapability::DevicesMinimal,
        IsolationCapability::EnvironmentExact,
        IsolationCapability::NetworkHost,
        IsolationCapability::NetworkIsolated,
        IsolationCapability::ProcessHostPidNamespace,
        IsolationCapability::ProcessTargetPidReporting,
        IsolationCapability::LifecycleSharedProcessGroup,
        IsolationCapability::IpcTargetUnixStream,
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

fn write_workspace_response(response: &AdapterWorkspaceResponse) -> ! {
    let bytes = serde_json::to_vec(response)
        .unwrap_or_else(|error| fail_process(&format!("serialize workspace response: {error}")));
    if bytes.len() > MAX_WORKSPACE_RESPONSE_BYTES {
        fail_process("workspace response exceeds protocol limit");
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

fn validate_connected_unix_stream(fd: RawFd) -> Result<(), String> {
    let mut socket_type: libc::c_int = 0;
    let mut socket_type_len = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut socket_type_len,
        )
    } != 0
    {
        return Err(format!(
            "target channel is not a socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    if socket_type != libc::SOCK_STREAM {
        return Err("target channel is not a SOCK_STREAM socket".to_owned());
    }

    let mut peer: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut peer_len = std::mem::size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getpeername(
            fd,
            (&mut peer as *mut libc::sockaddr_storage).cast(),
            &mut peer_len,
        )
    } != 0
    {
        return Err(format!(
            "target channel is not connected: {}",
            std::io::Error::last_os_error()
        ));
    }
    if peer.ss_family as libc::c_int != libc::AF_UNIX {
        return Err("target channel is not an AF_UNIX socket".to_owned());
    }
    Ok(())
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
        IsolationProjectWorkspace, IsolationTarget, IsolationTargetChannel,
    };

    fn valid_inspection_request() -> AdapterInspectionRequest {
        AdapterInspectionRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            target: host_target().expect("adapter tests require a supported Linux GNU target"),
            backend_id: BACKEND_ID.to_string(),
            artifacts: BTreeMap::from([(IsolationArtifactRole::Launcher, 3)]),
        }
    }

    #[test]
    fn workspace_backend_owns_its_overlay_layout_and_declares_diff_content() {
        let root = tempfile::tempdir().unwrap();
        let project_path = root.path().join("project");
        let backend_state_path = root.path().join("backend-state");
        std::fs::create_dir(&project_path).unwrap();
        std::fs::create_dir(&backend_state_path).unwrap();
        let project = File::open(&project_path).unwrap();
        let backend_state = File::open(&backend_state_path).unwrap();
        let invoke = |operation| {
            let request = AdapterWorkspaceRequest {
                protocol: IsolationAdapterProtocolVersion::Current,
                operation,
                workspace_id: "workspace-one".to_string(),
                launch_owner: "{\"attempt\":1}".to_string(),
                base_snapshot: "a".repeat(64),
                authorities: vec![
                    IsolationAuthority {
                        id: IsolationAuthorityId::new("workspace-project").unwrap(),
                        inherited_fd: project.as_raw_fd() as u32,
                        purpose: IsolationAuthorityPurpose::WorkspaceProject,
                    },
                    IsolationAuthority {
                        id: IsolationAuthorityId::new("workspace-backend-state").unwrap(),
                        inherited_fd: backend_state.as_raw_fd() as u32,
                        purpose: IsolationAuthorityPurpose::WorkspaceBackendState,
                    },
                ],
            };
            let request = create_sealed_memfd(
                c"adapter-workspace-test",
                &serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();
            workspace(request.into_raw_fd()).unwrap()
        };

        let created = invoke(WorkspaceLifecycleOperation::Create);
        assert_eq!(
            created
                .pinned_root_identities
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["backend_state".to_string(), "project".to_string()]
        );
        assert!(created.mutation_content_root.is_none());
        assert!(!root.path().join("upper").exists());
        assert!(!root.path().join("work").exists());
        assert!(backend_state_path.join("upper").is_dir());
        assert!(backend_state_path.join("work").is_dir());

        std::fs::write(backend_state_path.join("upper/change.txt"), b"changed").unwrap();
        let frozen = invoke(WorkspaceLifecycleOperation::FreezeAndDiff);
        assert_eq!(frozen.mutation_content_root.as_deref(), Some("upper"));
        assert_eq!(frozen.mutations.len(), 1);
        assert_eq!(frozen.mutations[0].path, "change.txt");
        assert_eq!(
            frozen.pinned_root_identities,
            created.pinned_root_identities
        );

        let destroyed = invoke(WorkspaceLifecycleOperation::Destroy);
        assert!(destroyed.destroyed);
        assert!(destroyed.mutation_content_root.is_none());
    }

    fn valid_launch_request() -> (AdapterLaunchRequest, Vec<File>) {
        let adapter = File::open("/proc/self/exe").unwrap();
        let launcher = File::open("/dev/null").unwrap();
        let target = File::open("/dev/null").unwrap();
        let project = File::open("/").unwrap();
        let workspace = File::open("/tmp").unwrap();
        let status = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();
        let release = File::open("/dev/null").unwrap();
        let release_keepalive = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap();

        let target_id = IsolationAuthorityId::new("target").unwrap();
        let project_id = IsolationAuthorityId::new("project").unwrap();
        let workspace_id = IsolationAuthorityId::new("workspace").unwrap();
        let request = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
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
                project_workspace: None,
                target_channel: None,
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
            adapter_fd: adapter.as_raw_fd() as u32,
            status_fd: status.as_raw_fd() as u32,
            lifecycle: AdapterLaunchLifecycle::AwaitAttachment {
                release_fd: release.as_raw_fd() as u32,
                release_keepalive_fd: release_keepalive.as_raw_fd() as u32,
            },
        };
        (
            request,
            vec![
                adapter,
                launcher,
                target,
                project,
                workspace,
                status,
                release,
                release_keepalive,
            ],
        )
    }

    #[test]
    fn inspection_identity_is_exact_and_target_bound() {
        let request = valid_inspection_request();
        validate_inspection_identity(&request).unwrap();

        let mut wrong_backend = request.clone();
        wrong_backend.backend_id = "another-backend".to_string();
        assert!(
            validate_inspection_identity(&wrong_backend)
                .unwrap_err()
                .contains("implements backend")
        );

        let mut wrong_target = request;
        wrong_target.target = match wrong_target.target {
            IsolationTargetTriple::X86_64UnknownLinuxGnu => {
                IsolationTargetTriple::Aarch64UnknownLinuxGnu
            }
            IsolationTargetTriple::Aarch64UnknownLinuxGnu => {
                IsolationTargetTriple::X86_64UnknownLinuxGnu
            }
        };
        assert!(
            validate_inspection_identity(&wrong_target)
                .unwrap_err()
                .contains("does not match")
        );
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
            b"#!/bin/sh\ncase \"$1\" in\n  --version) printf 'bubblewrap 0.11.0\\n' ;;\n  --help) printf '%s\\n' '--args --argv0 --bind-fd --block-fd --chdir --clearenv --dev --die-with-parent --dir --json-status-fd --overlay --overlay-src --ro-bind-fd --seccomp --setenv --sync-fd --tmpfs --unshare-ipc --unshare-net --unshare-user --unshare-uts' ;;\n  *) exit 2 ;;\nesac\n",
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
        assert!(
            prepared
                .arguments
                .windows(2)
                .any(|pair| pair == ["--tmpfs", "/"])
        );
        assert!(
            prepared
                .arguments
                .windows(2)
                .any(|pair| pair == ["--tmpfs", "/tmp"])
        );
        let release_fd = match request.lifecycle {
            AdapterLaunchLifecycle::AwaitAttachment { release_fd, .. } => release_fd,
            AdapterLaunchLifecycle::Run => panic!("fixture must await attachment"),
        };
        assert!(
            prepared
                .arguments
                .windows(2)
                .any(|pair| { pair[0] == "--block-fd" && pair[1] == release_fd.to_string() })
        );
        assert!(
            prepared
                .arguments
                .iter()
                .any(|argument| argument == "--die-with-parent")
        );
        assert!(
            prepared
                .arguments
                .iter()
                .any(|value| value == "--unshare-net")
        );
        assert!(
            prepared
                .arguments
                .windows(3)
                .any(|values| { values == ["--setenv", "API_TOKEN", "secret-token"] })
        );
        assert!(
            prepared
                .arguments
                .windows(3)
                .any(|values| { values[0] == "--ro-bind-fd" && values[2] == "/opt/bin/tool" })
        );
        assert!(
            prepared
                .arguments
                .windows(3)
                .any(|values| { values[0] == "--bind-fd" && values[2] == "/workspace" })
        );
        assert!(!prepared.arguments.iter().any(|value| value == "--"));
        assert_eq!(prepared.target_command.len(), 3);
        assert_eq!(prepared.target_command[0], TARGET_ARGV_BRIDGE_PATH);
        assert_eq!(prepared.target_command[1], "exec-sealed-argv");
        let target_argv_fd = prepared.target_command[2].parse::<RawFd>().unwrap();
        let request_maximum = first_internal_descriptor(&request).unwrap() - 1;
        assert!(target_argv_fd > request_maximum);
        let bridge_fd = prepared
            .arguments
            .windows(3)
            .find(|values| values[0] == "--ro-bind-data" && values[2] == TARGET_ARGV_BRIDGE_PATH)
            .unwrap()[1]
            .parse::<RawFd>()
            .unwrap();
        assert!(bridge_fd > request_maximum);
        assert!(
            !prepared
                .target_command
                .iter()
                .any(|value| value == "secret-value")
        );
    }

    #[test]
    fn normal_launch_omits_attachment_hold_and_keeper() {
        let (mut request, _handles) = valid_launch_request();
        let attachment_descriptors = match request.lifecycle {
            AdapterLaunchLifecycle::AwaitAttachment {
                release_fd,
                release_keepalive_fd,
            } => [release_fd as RawFd, release_keepalive_fd as RawFd],
            AdapterLaunchLifecycle::Run => panic!("fixture must await attachment"),
        };
        request.lifecycle = AdapterLaunchLifecycle::Run;

        let prepared = prepare_launch(&request).unwrap();
        assert_eq!(prepared.lifecycle, PreparedLaunchLifecycle::Run);
        assert!(!prepared.arguments.iter().any(|value| value == "--block-fd"));
        assert!(
            attachment_descriptors
                .into_iter()
                .all(|descriptor| !prepared.inherited_fds.contains(&descriptor))
        );
    }

    #[test]
    fn target_channel_requires_a_connected_stream_and_relocates_to_fd_zero() {
        use std::os::unix::net::UnixStream;

        let (mut request, mut handles) = valid_launch_request();
        request.lifecycle = AdapterLaunchLifecycle::Run;
        let (worker, daemon) = UnixStream::pair().unwrap();
        let source = IsolationAuthorityId::new("session-channel").unwrap();
        request.plan.target_channel = Some(IsolationTargetChannel {
            source: source.clone(),
            target_fd: 0,
            env_name: "RYEOS_SESSION_FD".to_owned(),
        });
        request
            .plan
            .environment
            .values
            .insert("RYEOS_SESSION_FD".to_owned(), "0".to_owned());
        request.authorities.push(IsolationAuthority {
            id: source,
            inherited_fd: worker.as_raw_fd() as u32,
            purpose: IsolationAuthorityPurpose::TargetDuplexChannel,
        });
        let prepared = prepare_launch(&request).unwrap();
        assert_eq!(prepared.target_channel_source, Some(worker.as_raw_fd()));

        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            let mut prepared = prepared;
            let source_fd = worker.as_raw_fd();
            let mut source_stat: libc::stat = unsafe { std::mem::zeroed() };
            let source_ok = unsafe { libc::fstat(source_fd, &mut source_stat) } == 0;
            let ok = relocate_target_channel(&mut prepared).is_ok()
                && prepared.target_channel_source.is_none()
                && !prepared.inherited_fds.contains(&source_fd);
            let mut target_stat: libc::stat = unsafe { std::mem::zeroed() };
            let target_ok = unsafe { libc::fstat(0, &mut target_stat) } == 0
                && source_stat.st_dev == target_stat.st_dev
                && source_stat.st_ino == target_stat.st_ino;
            let source_closed = unsafe { libc::fcntl(source_fd, libc::F_GETFD) } < 0;
            unsafe { libc::_exit(i32::from(!(source_ok && ok && target_ok && source_closed))) };
        }
        handles.clear();
        drop(worker);
        drop(daemon);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);

        let (mut regular_request, _regular_handles) = valid_launch_request();
        regular_request.lifecycle = AdapterLaunchLifecycle::Run;
        let regular = File::open("/dev/null").unwrap();
        let source = IsolationAuthorityId::new("session-channel").unwrap();
        regular_request.plan.target_channel = Some(IsolationTargetChannel {
            source: source.clone(),
            target_fd: 0,
            env_name: "RYEOS_SESSION_FD".to_owned(),
        });
        regular_request
            .plan
            .environment
            .values
            .insert("RYEOS_SESSION_FD".to_owned(), "0".to_owned());
        regular_request.authorities.push(IsolationAuthority {
            id: source,
            inherited_fd: regular.as_raw_fd() as u32,
            purpose: IsolationAuthorityPurpose::TargetDuplexChannel,
        });
        let error = prepare_launch(&regular_request).unwrap_err();
        assert!(error.contains("target channel") && error.contains("socket"));
    }

    #[test]
    fn host_visible_launcher_command_contains_only_the_sealed_argument_descriptor() {
        let target_command = vec![
            TARGET_ARGV_BRIDGE_PATH.to_owned(),
            "exec-sealed-argv".to_owned(),
            "44".to_owned(),
        ];
        let command = exact_launcher_command(41, 42, &target_command);
        assert_eq!(command.get_program(), "/proc/self/fd/41");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("--args"),
                std::ffi::OsStr::new("42"),
                std::ffi::OsStr::new("--"),
                std::ffi::OsStr::new(TARGET_ARGV_BRIDGE_PATH),
                std::ffi::OsStr::new("exec-sealed-argv"),
                std::ffi::OsStr::new("44"),
            ]
        );
        let rendered = format!("{command:?}");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("API_TOKEN"));
    }

    #[test]
    fn target_argv_round_trips_only_through_a_sealed_descriptor() {
        let expected = vec![
            "/opt/bin/tool".to_owned(),
            "--flag".to_owned(),
            "secret-value".to_owned(),
        ];
        let file = create_sealed_memfd(
            c"adapter-target-argv-test",
            &encode_nul_arguments(&expected).unwrap(),
        )
        .unwrap();
        let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        assert!(duplicate > libc::STDERR_FILENO);
        let observed = read_sealed_argv(duplicate).unwrap();
        assert_eq!(
            observed,
            expected.into_iter().map(OsString::from).collect::<Vec<_>>()
        );
    }

    #[test]
    fn adapter_bridge_capture_is_offset_independent() {
        let mut adapter = File::open("/proc/self/exe").unwrap();
        adapter.seek(std::io::SeekFrom::Start(7)).unwrap();
        let original_offset = adapter.stream_position().unwrap();
        let expected_length = adapter.metadata().unwrap().len();
        let mut next_internal_fd = 64;

        let bridge =
            capture_current_adapter_bridge(adapter.as_raw_fd(), &mut next_internal_fd).unwrap();

        assert_eq!(adapter.stream_position().unwrap(), original_offset);
        assert_eq!(bridge.metadata().unwrap().len(), expected_length);
        assert!(bridge.as_raw_fd() >= 64);
        require_seals(bridge.as_raw_fd()).unwrap();
    }

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[ignore = "requires the built static bundle payload and Linux user namespaces"]
    fn production_payload_reaches_a_descriptor_bound_target_through_the_argv_bridge() {
        let payload = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../.ai/bin/x86_64-unknown-linux-gnu");
        let adapter_path = payload.join("ryeos-bubblewrap-adapter");
        let adapter_bridge = File::open(&adapter_path).unwrap();
        let launcher = File::open(payload.join("bwrap")).unwrap();
        let target_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../core/.ai/bin/x86_64-unknown-linux-gnu/rye-parser-yaml-header-document");
        let target = File::open(&target_path).unwrap();
        let project = File::open("/").unwrap();
        let workspace = File::open("/tmp").unwrap();
        let system_lib = File::open("/lib").unwrap();
        let system_lib64 = File::open("/lib64").unwrap();
        let system_usr_lib = File::open("/usr/lib").unwrap();
        let overlay_root = std::env::temp_dir().join(format!(
            "ryeos-adapter-overlay-fixture-{}",
            std::process::id()
        ));
        let workspace_project_path = overlay_root.join("project");
        let backend_state_path = overlay_root.join("backend-state");
        let upper_path = backend_state_path.join("upper");
        let work_path = backend_state_path.join("work");
        std::fs::create_dir_all(&workspace_project_path).unwrap();
        std::fs::create_dir_all(&upper_path).unwrap();
        std::fs::create_dir_all(&work_path).unwrap();
        let workspace_project = File::open(&workspace_project_path).unwrap();
        let backend_state = File::open(&backend_state_path).unwrap();
        let status = lillux::supervised_launcher_status_pipe().unwrap();

        let target_id = IsolationAuthorityId::new("target").unwrap();
        let project_id = IsolationAuthorityId::new("project").unwrap();
        let workspace_id = IsolationAuthorityId::new("workspace").unwrap();
        let system_lib_id = IsolationAuthorityId::new("system-lib").unwrap();
        let system_lib64_id = IsolationAuthorityId::new("system-lib64").unwrap();
        let system_usr_lib_id = IsolationAuthorityId::new("system-usr-lib").unwrap();
        let workspace_project_id = IsolationAuthorityId::new("workspace-project").unwrap();
        let backend_state_id = IsolationAuthorityId::new("workspace-backend-state").unwrap();
        let request = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            plan: IsolationPlan {
                target: IsolationTarget {
                    executable: target_id.clone(),
                    argv0: "rye-parser-yaml-header-document".to_owned(),
                    arguments: Vec::new(),
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
                        destination: IsolationPath::new("/scratch").unwrap(),
                        access: IsolationMountAccess::Writable,
                        layer: 2,
                    },
                    IsolationMount {
                        source: system_lib_id.clone(),
                        destination: IsolationPath::new("/lib").unwrap(),
                        access: IsolationMountAccess::ReadOnly,
                        layer: 3,
                    },
                    IsolationMount {
                        source: system_lib64_id.clone(),
                        destination: IsolationPath::new("/lib64").unwrap(),
                        access: IsolationMountAccess::ReadOnly,
                        layer: 4,
                    },
                    IsolationMount {
                        source: system_usr_lib_id.clone(),
                        destination: IsolationPath::new("/usr/lib").unwrap(),
                        access: IsolationMountAccess::ReadOnly,
                        layer: 5,
                    },
                ],
                project_workspace: Some(IsolationProjectWorkspace {
                    workspace_id: "fixture-workspace".to_owned(),
                    project: workspace_project_id.clone(),
                    backend_state: backend_state_id.clone(),
                    destination: IsolationPath::new("/workspace").unwrap(),
                }),
                target_channel: None,
                environment: IsolationEnvironment {
                    values: BTreeMap::new(),
                },
                network: IsolationNetwork::Host,
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
                IsolationAuthority {
                    id: workspace_project_id,
                    inherited_fd: workspace_project.as_raw_fd() as u32,
                    purpose: IsolationAuthorityPurpose::WorkspaceProject,
                },
                IsolationAuthority {
                    id: backend_state_id,
                    inherited_fd: backend_state.as_raw_fd() as u32,
                    purpose: IsolationAuthorityPurpose::WorkspaceBackendState,
                },
                IsolationAuthority {
                    id: system_lib_id,
                    inherited_fd: system_lib.as_raw_fd() as u32,
                    purpose: IsolationAuthorityPurpose::ReadOnlyMount,
                },
                IsolationAuthority {
                    id: system_lib64_id,
                    inherited_fd: system_lib64.as_raw_fd() as u32,
                    purpose: IsolationAuthorityPurpose::ReadOnlyMount,
                },
                IsolationAuthority {
                    id: system_usr_lib_id,
                    inherited_fd: system_usr_lib.as_raw_fd() as u32,
                    purpose: IsolationAuthorityPurpose::ReadOnlyMount,
                },
            ],
            artifacts: BTreeMap::from([(
                IsolationArtifactRole::Launcher,
                launcher.as_raw_fd() as u32,
            )]),
            adapter_fd: adapter_bridge.as_raw_fd() as u32,
            status_fd: status.writer_fd() as u32,
            lifecycle: AdapterLaunchLifecycle::Run,
        };
        request.validate().unwrap();
        let request_handle = create_sealed_memfd(
            c"adapter-production-launch-test",
            &serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        let request_fd = request_handle.as_raw_fd().to_string();
        let output = lillux::run(lillux::SubprocessRequest {
            cmd: adapter_path.to_string_lossy().into_owned(),
            argv0: None,
            args: vec!["launch".to_owned(), request_fd],
            cwd: Some("/".to_owned()),
            envs: Vec::new(),
            stdin_data: Some(
                r##"{"schema_version":2,"request":{"command":"validate_parser_config","parser_config":{"require_header":true,"forms":[{"kind":"comment_marker","marker":"ryeos-tool","comment_prefix":"#","allow_after_shebang":true}]}}}"##
                    .to_owned(),
            ),
            timeout: 5.0,
            limits: Some(lillux::SubprocessLimits {
                max_stdout_bytes: Some(64 * 1024),
                max_stderr_bytes: Some(64 * 1024),
                ..lillux::SubprocessLimits::default()
            }),
            inherited_fds: vec![
                std::sync::Arc::new(adapter_bridge),
                std::sync::Arc::new(launcher),
                std::sync::Arc::new(target),
                std::sync::Arc::new(project),
                std::sync::Arc::new(workspace),
                std::sync::Arc::new(system_lib),
                std::sync::Arc::new(system_lib64),
                std::sync::Arc::new(system_usr_lib),
                std::sync::Arc::new(workspace_project),
                std::sync::Arc::new(backend_state),
                status.writer,
                std::sync::Arc::new(request_handle),
            ],
            supervised_status: Some(status.reader),
        });
        assert!(output.success, "stderr={}", output.stderr);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output.stdout).unwrap(),
            serde_json::json!({
                "schema_version": 2,
                "response": { "result": "validate_ok" }
            })
        );
        let overlay_work = work_path.join("work");
        if overlay_work.exists() {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&overlay_work, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        std::fs::remove_dir_all(overlay_root).unwrap();
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
        let normalize_seccomp_fd = |mut arguments: Vec<String>| {
            let index = arguments
                .iter()
                .position(|value| value == "--seccomp")
                .expect("compiled launch has seccomp authority");
            arguments[index + 1] = "<seccomp-fd>".to_string();
            let bridge = arguments
                .windows(3)
                .position(|values| {
                    values[0] == "--ro-bind-data" && values[2] == TARGET_ARGV_BRIDGE_PATH
                })
                .expect("compiled launch has target argv bridge authority");
            arguments[bridge + 1] = "<bridge-fd>".to_string();
            arguments
        };
        let host = normalize_seccomp_fd(host.arguments);
        let mut expected = normalize_seccomp_fd(isolated.arguments);
        expected.retain(|value| value != "--unshare-net");
        assert_eq!(host, expected);
    }

    #[test]
    fn launch_refuses_descriptor_reuse_unknown_artifacts_and_invalid_strings() {
        let (mut duplicate, _duplicate_handles) = valid_launch_request();
        duplicate.status_fd = duplicate.artifacts[&IsolationArtifactRole::Launcher];
        assert!(
            prepare_launch(&duplicate)
                .unwrap_err()
                .contains("reused across")
        );

        let (mut extra_artifact, mut handles) = valid_launch_request();
        let loader = File::open("/dev/null").unwrap();
        extra_artifact
            .artifacts
            .insert(IsolationArtifactRole::Loader, loader.as_raw_fd() as u32);
        handles.push(loader);
        assert!(
            prepare_launch(&extra_artifact)
                .unwrap_err()
                .contains("does not support dynamic-loader")
        );

        let (mut invalid_argument, _invalid_handles) = valid_launch_request();
        invalid_argument.plan.target.arguments[0] = "bad\0argument".to_string();
        assert!(
            prepare_launch(&invalid_argument)
                .unwrap_err()
                .contains("interior NUL")
        );
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

        let duplicate = br#"{"protocol":"ryeos.isolation-adapter/v3","target":"x86_64-unknown-linux-gnu","backend_id":"linux-bubblewrap","backend_id":"linux-bubblewrap","artifacts":{"launcher":3}}"#;
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
