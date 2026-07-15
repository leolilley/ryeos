//! Runtime-to-launch-preparer bindings and the dedicated launch-preparer
//! subprocess boundary.
//!
//! Launch preparers receive verified execution identities and may return
//! symbolic secret requirements, so they do not share the general
//! parser/composer runner. This module always uses the node-declared absolute
//! Bubblewrap backend with a fixed, networkless, host-filesystem-denying
//! profile and bounded streaming I/O.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use ryeos_handler_protocol::{
    HandlerRequest, HandlerResponse, LaunchPrepareRequest, LaunchPrepareResponse,
};

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::handlers::{HandlerRegistry, HandlerServes, VerifiedHandler};
use crate::resolution::TrustClass;
use crate::runtime_registry::{LaunchPreparationDecl, RuntimeRegistry};
use crate::sandbox::SandboxRuntime;

pub const LAUNCH_PREPARER_REQUEST_MAX_BYTES: usize = 10 * 1024 * 1024;
pub const LAUNCH_PREPARER_STDOUT_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const LAUNCH_PREPARER_STDERR_MAX_BYTES: usize = 64 * 1024;
pub const LAUNCH_PREPARER_TIMEOUT: Duration = Duration::from_secs(5);
const LAUNCH_PREPARER_CPU_SECONDS: u64 = 4;
const LAUNCH_PREPARER_ADDRESS_SPACE_BYTES: u64 = 256 * 1024 * 1024;
const LAUNCH_PREPARER_PROCESS_LIMIT: u64 = 16;
const LAUNCH_PREPARER_OPEN_FILE_LIMIT: u64 = 64;
const LAUNCH_PREPARER_RUNTIME_FILE_LIMIT: usize = 48;

#[derive(Debug, Clone)]
struct BoundLaunchPreparer {
    handler: Arc<VerifiedHandler>,
    handler_config: serde_json::Value,
}

/// Runtime-ref keyed registry built once after the runtime and handler
/// registries have both passed boot validation.
#[derive(Debug, Clone, Default)]
pub struct LaunchPreparerRegistry {
    by_runtime: HashMap<CanonicalRef, BoundLaunchPreparer>,
    runner: Option<LaunchPreparerRunner>,
}

impl LaunchPreparerRegistry {
    pub fn from_runtimes(
        runtimes: &RuntimeRegistry,
        handlers: &HandlerRegistry,
        runner: LaunchPreparerRunner,
    ) -> Result<Self, EngineError> {
        let mut by_runtime = HashMap::new();
        let mut sorted_runtimes: Vec<_> = runtimes.all().collect();
        sorted_runtimes.sort_by_key(|runtime| runtime.canonical_ref.to_string());

        for runtime in sorted_runtimes {
            let (handler_ref, handler_config) = match &runtime.yaml.launch_contract.preparation {
                LaunchPreparationDecl::None => continue,
                LaunchPreparationDecl::Handler { handler, config } => (handler, config),
            };
            let handler = handlers.get(handler_ref).ok_or_else(|| {
                EngineError::SchemaLoaderError {
                    reason: format!(
                        "runtime `{}` launch preparer `{handler_ref}` is not registered",
                        runtime.canonical_ref
                    ),
                }
            })?;
            if handler.descriptor().serves != HandlerServes::LaunchPreparer {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "runtime `{}` handler `{handler_ref}` does not serve `launch_preparer`",
                        runtime.canonical_ref
                    ),
                });
            }
            if handler.trust_class() != TrustClass::TrustedBundle {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "runtime `{}` launch preparer `{handler_ref}` is not trusted_bundle",
                        runtime.canonical_ref
                    ),
                });
            }
            if let VerifiedHandler::Unresolved { reason, .. } = handler {
                return Err(EngineError::SchemaLoaderError {
                    reason: format!(
                        "runtime `{}` launch preparer `{handler_ref}` is unresolved: {reason}",
                        runtime.canonical_ref
                    ),
                });
            }

            by_runtime.insert(
                runtime.canonical_ref.clone(),
                BoundLaunchPreparer {
                    handler: Arc::new(handler.clone()),
                    handler_config: handler_config.clone(),
                },
            );
        }

        Ok(Self {
            by_runtime,
            runner: Some(runner),
        })
    }

    pub fn contains(&self, runtime_ref: &CanonicalRef) -> bool {
        self.by_runtime.contains_key(runtime_ref)
    }

    pub fn len(&self) -> usize {
        self.by_runtime.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_runtime.is_empty()
    }

    pub fn handler_ref_for(&self, runtime_ref: &CanonicalRef) -> Option<&str> {
        self.by_runtime
            .get(runtime_ref)
            .map(|bound| bound.handler.canonical_ref())
    }

    /// Invoke the handler bound to `runtime_ref`. The registry overwrites the
    /// request's config with the signed runtime descriptor's config so caller
    /// data can never replace launch-preparer policy.
    pub fn prepare(
        &self,
        runtime_ref: &CanonicalRef,
        mut request: LaunchPrepareRequest,
    ) -> Result<LaunchPrepareResponse, EngineError> {
        let bound = self.by_runtime.get(runtime_ref).ok_or_else(|| {
            EngineError::Internal(format!(
                "no launch preparer bound for runtime `{runtime_ref}`"
            ))
        })?;
        let runner = self.runner.as_ref().ok_or_else(|| {
            EngineError::Internal("launch-preparer registry has no sandbox runner".to_owned())
        })?;
        request.handler_config = bound.handler_config.clone();
        let response = runner.run_launch_preparer_subprocess(
            &bound.handler,
            &HandlerRequest::LaunchPrepare(request),
        )?;
        match response {
            HandlerResponse::LaunchPrepare { response } => Ok(response),
            other => Err(EngineError::LaunchPreparerProtocolInvalid {
                handler: bound.handler.canonical_ref().to_owned(),
                detail: format!("unexpected launch-preparer response: {other:?}"),
            }),
        }
    }
}

/// Fixed-isolation process runner shared by boot-time config validation and
/// live launch preparation.
#[derive(Debug, Clone)]
pub struct LaunchPreparerRunner {
    bubblewrap: Arc<std::fs::File>,
}

impl LaunchPreparerRunner {
    /// Capture the backend from the daemon's immutable node-sandbox snapshot.
    /// All widenable policy fields remain deliberately ignored because launch
    /// preparation always uses its own fixed profile.
    pub fn from_sandbox_runtime(sandbox: &SandboxRuntime) -> Result<Self, EngineError> {
        Ok(Self {
            bubblewrap: sandbox.capture_mandatory_bubblewrap_backend()?,
        })
    }

    pub(crate) fn run_launch_preparer_subprocess(
        &self,
        handler: &VerifiedHandler,
        request: &HandlerRequest,
    ) -> Result<HandlerResponse, EngineError> {
        let (canonical_ref, binary_path, binary_hash) = match handler {
            VerifiedHandler::Resolved {
                canonical_ref,
                resolved_binary_path,
                resolved_binary_hash,
                ..
            } => (
                canonical_ref.clone(),
                resolved_binary_path.clone(),
                resolved_binary_hash.clone(),
            ),
            VerifiedHandler::Unresolved {
                canonical_ref,
                reason,
                ..
            } => {
                return Err(EngineError::LaunchPreparerUnavailable {
                    handler: canonical_ref.clone(),
                    detail: format!("handler binary is unresolved: {reason}"),
                });
            }
        };

        let request_value = serde_json::to_value(request)
            .map_err(|error| EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("encode handler request: {error}"),
            })?;
        let request_json = lillux::canonical_json(&request_value)
            .map(String::into_bytes)
            .map_err(|error| EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("canonicalize handler request: {error}"),
            })?;
        if request_json.len() > LAUNCH_PREPARER_REQUEST_MAX_BYTES {
            return Err(EngineError::LaunchPreparerLimitExceeded {
                handler: canonical_ref,
                detail: format!(
                    "launch-preparer request is {} bytes; limit is {}",
                    request_json.len(),
                    LAUNCH_PREPARER_REQUEST_MAX_BYTES
                ),
            });
        }
        ryeos_handler_protocol::from_json_slice_strict::<serde_json::Value>(&request_json)
            .map_err(|error| EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("invalid launch-preparer request JSON: {error}"),
            })?;

        let invocation = fixed_bubblewrap_args(&binary_path, &binary_hash).map_err(|error| {
            EngineError::LaunchPreparerUnavailable {
                handler: canonical_ref.clone(),
                detail: error.to_string(),
            }
        })?;
        #[cfg(unix)]
        let bubblewrap_command = {
            use std::os::fd::AsRawFd as _;
            format!("/proc/self/fd/{}", self.bubblewrap.as_raw_fd())
        };
        #[cfg(not(unix))]
        let bubblewrap_command = String::new();
        let mut command = Command::new(bubblewrap_command);
        command
            .args(&invocation.args)
            .current_dir("/tmp")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
            let inherited_fds = invocation
                .inherited_files
                .iter()
                .map(|file| file.as_raw_fd())
                .collect::<Vec<_>>();
            unsafe {
                let bubblewrap_fd = self.bubblewrap.as_raw_fd();
                command.pre_exec(move || {
                    for fd in std::iter::once(&bubblewrap_fd).chain(inherited_fds.iter()) {
                        let flags = libc::fcntl(*fd, libc::F_GETFD);
                        if flags == -1
                            || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    apply_launch_preparer_rlimits()
                });
            }
        }
        #[cfg(not(unix))]
        {
            return Err(EngineError::LaunchPreparerUnavailable {
                handler: canonical_ref,
                detail: "launch preparers require a Unix process-group boundary".to_owned(),
            });
        }

        let started_at = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|error| EngineError::LaunchPreparerUnavailable {
                handler: canonical_ref.clone(),
                detail: format!("spawn fixed Bubblewrap profile: {error}"),
            })?;
        let process_group = child.id() as i32;
        let Some(stdin) = child.stdin.take() else {
            kill_process_group(process_group, &mut child);
            let _ = child.wait();
            return Err(EngineError::LaunchPreparerUnavailable {
                handler: canonical_ref,
                detail: "launch-preparer stdin pipe unavailable".to_owned(),
            });
        };
        let Some(stdout) = child.stdout.take() else {
            kill_process_group(process_group, &mut child);
            let _ = child.wait();
            return Err(EngineError::LaunchPreparerUnavailable {
                handler: canonical_ref,
                detail: "launch-preparer stdout pipe unavailable".to_owned(),
            });
        };
        let Some(stderr) = child.stderr.take() else {
            kill_process_group(process_group, &mut child);
            let _ = child.wait();
            return Err(EngineError::LaunchPreparerUnavailable {
                handler: canonical_ref,
                detail: "launch-preparer stderr pipe unavailable".to_owned(),
            });
        };

        let (event_tx, event_rx) = mpsc::channel();
        let stdin_events = event_tx.clone();
        let stdin_thread = thread::spawn(move || {
            let mut stdin = stdin;
            let result = stdin.write_all(&request_json).and_then(|_| stdin.flush());
            if let Err(error) = &result {
                let _ = stdin_events.send(StreamEvent::IoFailure {
                    stream: "stdin",
                    detail: error.to_string(),
                });
            }
            result.map_err(|error| error.to_string())
        });
        let stdout_thread = spawn_bounded_reader(
            stdout,
            "stdout",
            LAUNCH_PREPARER_STDOUT_MAX_BYTES,
            event_tx.clone(),
        );
        let stderr_thread = spawn_bounded_reader(
            stderr,
            "stderr",
            LAUNCH_PREPARER_STDERR_MAX_BYTES,
            event_tx.clone(),
        );
        drop(event_tx);

        let deadline = started_at + LAUNCH_PREPARER_TIMEOUT;
        let mut violation = None;
        let status = loop {
            if let Ok(event) = event_rx.try_recv() {
                violation = Some(event);
                kill_process_group(process_group, &mut child);
                break child.wait().ok();
            }
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    violation = Some(StreamEvent::IoFailure {
                        stream: "process",
                        detail: error.to_string(),
                    });
                    kill_process_group(process_group, &mut child);
                    break child.wait().ok();
                }
            }
            if Instant::now() >= deadline {
                violation = Some(StreamEvent::Timeout);
                kill_process_group(process_group, &mut child);
                break child.wait().ok();
            }
            thread::sleep(Duration::from_millis(2));
        };

        let stdin_result = stdin_thread.join().map_err(|_| {
            EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: "launch-preparer stdin writer panicked".to_owned(),
            }
        })?;
        let stdout_capture = join_bounded_reader(stdout_thread, &canonical_ref, "stdout")?;
        let stderr_capture = join_bounded_reader(stderr_thread, &canonical_ref, "stderr")?;

        if violation.is_none() {
            violation = stdout_capture.violation.clone().or(stderr_capture.violation.clone());
        }
        if let Some(violation) = violation {
            return Err(stream_violation_error(&canonical_ref, violation));
        }
        if let Err(detail) = stdin_result {
            return Err(EngineError::LaunchPreparerUnavailable {
                handler: canonical_ref,
                detail: format!("launch-preparer stdin failure: {detail}"),
            });
        }

        let status = status.ok_or_else(|| EngineError::LaunchPreparerUnavailable {
            handler: canonical_ref.clone(),
            detail: "launch-preparer exit status unavailable".to_owned(),
        })?;
        if !status.success() {
            return Err(nonzero_error(
                canonical_ref,
                status,
                stderr_capture.bytes,
            ));
        }

        let response: HandlerResponse =
            ryeos_handler_protocol::from_json_slice_strict(&stdout_capture.bytes).map_err(
                |error| EngineError::LaunchPreparerProtocolInvalid {
                    handler: canonical_ref.clone(),
                    detail: error.to_string(),
                },
            )?;
        let response_value = serde_json::to_value(&response).map_err(|error| {
            EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref.clone(),
                detail: format!("encode launch-preparer response: {error}"),
            }
        })?;
        let canonical_response = lillux::canonical_json(&response_value).map_err(|error| {
            EngineError::LaunchPreparerProtocolInvalid {
                handler: canonical_ref,
                detail: format!("canonicalize launch-preparer response: {error}"),
            }
        })?;
        if canonical_response.len() > LAUNCH_PREPARER_STDOUT_MAX_BYTES {
            return Err(EngineError::LaunchPreparerLimitExceeded {
                handler: handler.canonical_ref().to_owned(),
                detail: format!(
                    "canonical launch-preparer response is {} bytes; limit is {}",
                    canonical_response.len(),
                    LAUNCH_PREPARER_STDOUT_MAX_BYTES
                ),
            });
        }
        Ok(response)
    }
}

#[cfg(target_os = "linux")]
fn apply_launch_preparer_rlimits() -> std::io::Result<()> {
    set_fixed_rlimit(libc::RLIMIT_CPU, LAUNCH_PREPARER_CPU_SECONDS)?;
    set_fixed_rlimit(libc::RLIMIT_AS, LAUNCH_PREPARER_ADDRESS_SPACE_BYTES)?;
    set_fixed_rlimit(libc::RLIMIT_NPROC, LAUNCH_PREPARER_PROCESS_LIMIT)?;
    set_fixed_rlimit(libc::RLIMIT_NOFILE, LAUNCH_PREPARER_OPEN_FILE_LIMIT)?;
    set_fixed_rlimit(libc::RLIMIT_CORE, 0)
}

#[cfg(target_os = "linux")]
fn set_fixed_rlimit(resource: libc::__rlimit_resource_t, value: u64) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    if unsafe { libc::setrlimit(resource, &limit) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn apply_launch_preparer_rlimits() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "fixed launch-preparer resource limits require Linux",
    ))
}

struct FixedBubblewrapInvocation {
    args: Vec<String>,
    /// Retained until Bubblewrap has consumed each `--ro-bind-fd` source.
    inherited_files: Vec<Arc<std::fs::File>>,
}

#[derive(Debug)]
struct PinnedRuntimeFile {
    file: Arc<std::fs::File>,
    destination: PathBuf,
}

fn fixed_bubblewrap_args(
    handler_binary: &Path,
    expected_handler_hash: &str,
) -> Result<FixedBubblewrapInvocation, EngineError> {
    let command_path = std::fs::canonicalize(handler_binary).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer binary {} cannot be resolved: {error}",
                handler_binary.display()
            ),
        }
    })?;
    let mut args = vec![
        "--die-with-parent".to_owned(),
        "--new-session".to_owned(),
        "--unshare-all".to_owned(),
        "--clearenv".to_owned(),
        "--cap-drop".to_owned(),
        "ALL".to_owned(),
        "--tmpfs".to_owned(),
        "/".to_owned(),
    ];

    validate_runtime_destination("launch-preparer binary", &command_path)?;
    let (handler, handler_image) = pin_runtime_file(
        "verified launch-preparer binary",
        &command_path,
        Some(expected_handler_hash),
        true,
    )?;
    let library_mounts = exact_runtime_library_mounts(&handler_image, &handler.file)?;
    let mut directories = BTreeSet::new();
    for destination in library_mounts
        .iter()
        .map(|mount| mount.destination.as_path())
        .chain(std::iter::once(command_path.as_path()))
        .chain(std::iter::once(Path::new("/dev")))
    {
        directories.extend(
            destination
                .ancestors()
                .skip(1)
                .filter(|path| *path != Path::new("/"))
                .map(Path::to_path_buf),
        );
    }
    for directory in directories {
        args.extend(["--dir".to_owned(), directory.to_string_lossy().into_owned()]);
    }
    let mut inherited_files = Vec::with_capacity(library_mounts.len() + 1);
    for mount in library_mounts {
        let fd = runtime_file_fd(&mount.file);
        args.extend([
            "--ro-bind-fd".to_owned(),
            fd.to_string(),
            mount.destination.to_string_lossy().into_owned(),
        ]);
        inherited_files.push(mount.file);
    }
    let command = command_path.to_string_lossy().into_owned();
    args.extend([
        "--ro-bind-fd".to_owned(),
        runtime_file_fd(&handler.file).to_string(),
        command.clone(),
    ]);
    inherited_files.push(handler.file);

    args.extend([
        "--dev-bind".to_owned(),
        "/dev/null".to_owned(),
        "/dev/null".to_owned(),
    ]);
    args.extend(["--tmpfs".to_owned(), "/tmp".to_owned()]);
    args.extend(["--chdir".to_owned(), "/tmp".to_owned()]);
    args.push("--".to_owned());
    args.push(command_path.to_string_lossy().into_owned());
    Ok(FixedBubblewrapInvocation {
        args,
        inherited_files,
    })
}

/// Resolve the verified preparer's exact dynamic-loader closure without
/// executing the preparer. The ELF interpreter's `--list` mode asks the loader
/// to resolve `DT_NEEDED` entries with an empty environment; only the returned
/// regular files are mounted into the private namespace. A static ELF has no
/// interpreter and therefore needs no runtime-library mounts.
#[cfg(target_os = "linux")]
fn exact_runtime_library_mounts(
    image: &[u8],
    command_file: &Arc<std::fs::File>,
) -> Result<Vec<PinnedRuntimeFile>, EngineError> {
    let Some(interpreter) = elf_interpreter(image)? else {
        return Ok(Vec::new());
    };
    validate_runtime_destination("ELF interpreter", &interpreter)?;
    let (pinned_interpreter, _) =
        pin_runtime_file("ELF interpreter", &interpreter, None, true)?;
    validate_node_dynamic_loader(&pinned_interpreter.destination)?;
    let interpreter_fd = runtime_file_fd(&pinned_interpreter.file);
    let command_fd = runtime_file_fd(command_file);
    let output = lillux::run(lillux::SubprocessRequest {
        cmd: format!("/proc/self/fd/{interpreter_fd}"),
        args: vec!["--list".to_owned(), format!("/proc/self/fd/{command_fd}")],
        cwd: Some("/".to_owned()),
        envs: Vec::new(),
        stdin_data: None,
        timeout: LAUNCH_PREPARER_TIMEOUT.as_secs_f64(),
        limits: Some(lillux::SubprocessLimits {
            max_open_files: Some(LAUNCH_PREPARER_OPEN_FILE_LIMIT),
            max_stdout_bytes: Some(LAUNCH_PREPARER_STDOUT_MAX_BYTES as u64),
            max_stderr_bytes: Some(LAUNCH_PREPARER_STDERR_MAX_BYTES as u64),
        }),
        inherited_fds: vec![pinned_interpreter.file.clone(), command_file.clone()],
        supervised_status: None,
    });
    if !output.success {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer ELF dependency inspection failed{}: {}",
                if output.timed_out { " (timeout)" } else { "" },
                output.stderr.chars().take(512).collect::<String>()
            ),
        });
    }

    let listing = output.stdout;
    let mut destinations = BTreeSet::new();
    destinations.insert(interpreter.clone());
    for line in listing.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some((dependency, resolved)) = line.split_once("=>") {
            if resolved.contains("=>") || !valid_loader_name(dependency.trim()) {
                return Err(EngineError::SandboxPolicyRefused {
                    reason: format!("launch-preparer dependency listing is malformed: {line}"),
                });
            }
            let resolved = resolved.trim();
            if resolved.starts_with("not found") {
                return Err(EngineError::SandboxPolicyRefused {
                    reason: format!("launch-preparer dependency is unavailable: {line}"),
                });
            }
            let mut fields = resolved.split_whitespace();
            let path = fields.next().ok_or_else(|| {
                EngineError::SandboxPolicyRefused {
                    reason: format!("launch-preparer dependency listing is malformed: {line}"),
                }
            })?;
            let address = fields.next().ok_or_else(|| {
                EngineError::SandboxPolicyRefused {
                    reason: format!("launch-preparer dependency listing is malformed: {line}"),
                }
            })?;
            if fields.next().is_some()
                || !valid_loader_address(address)
                || !path.starts_with('/')
            {
                return Err(EngineError::SandboxPolicyRefused {
                    reason: format!("launch-preparer dependency did not resolve absolutely: {line}"),
                });
            }
            destinations.insert(PathBuf::from(path));
            continue;
        }
        let mut fields = line.split_whitespace();
        let name = fields.next().unwrap_or_default();
        let address = fields.next().unwrap_or_default();
        if fields.next().is_some() || !valid_loader_address(address) {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!("launch-preparer dependency listing is unrecognized: {line}"),
            });
        }
        let loader_fd_path = format!("/proc/self/fd/{interpreter_fd}");
        if name == interpreter.to_string_lossy().as_ref()
            || name == pinned_interpreter.destination.to_string_lossy().as_ref()
            || name == loader_fd_path
        {
            continue;
        }
        if !name.starts_with("linux-vdso.") || !valid_loader_name(name) {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!("launch-preparer dependency listing is unrecognized: {line}"),
            });
        }
    }
    if destinations.len() > LAUNCH_PREPARER_RUNTIME_FILE_LIMIT {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer ELF closure contains {} runtime files; limit is {}",
                destinations.len(),
                LAUNCH_PREPARER_RUNTIME_FILE_LIMIT
            ),
        });
    }

    let mut mounts = Vec::with_capacity(destinations.len());
    for destination in destinations {
        validate_runtime_destination("runtime library", &destination)?;
        let mut pinned = if destination == interpreter {
            PinnedRuntimeFile {
                file: pinned_interpreter.file.clone(),
                destination: pinned_interpreter.destination.clone(),
            }
        } else {
            pin_runtime_file("runtime library", &destination, None, false)?.0
        };
        validate_node_owned_immutable_path("runtime library", &pinned.destination)?;
        pinned.destination = destination;
        mounts.push(pinned);
    }
    Ok(mounts)
}

#[cfg(target_os = "linux")]
fn valid_loader_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-')
        })
}

#[cfg(target_os = "linux")]
fn valid_loader_address(value: &str) -> bool {
    value
        .strip_prefix("(0x")
        .and_then(|value| value.strip_suffix(')'))
        .is_some_and(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[cfg(target_os = "linux")]
fn validate_node_dynamic_loader(path: &Path) -> Result<(), EngineError> {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let recognized_loader_name = (name.starts_with("ld-linux") && name.contains(".so."))
        || (name.starts_with("ld-musl-") && name.ends_with(".so.1"))
        || matches!(name, "ld.so.1" | "ld64.so.1" | "ld64.so.2");
    if !recognized_loader_name {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer ELF interpreter {} is not a recognized node dynamic loader",
                path.display()
            ),
        });
    }
    validate_node_owned_immutable_path("ELF interpreter", path)
}

#[cfg(target_os = "linux")]
fn validate_node_owned_immutable_path(label: &str, path: &Path) -> Result<(), EngineError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    // The canonical runtime file and its entire path must be owned by the node
    // administrator and immune to group/world replacement. This admits
    // conventional system layouts and immutable stores without trusting any
    // handler-selected writable directory.
    let mut current = Some(path);
    while let Some(component) = current {
        let metadata = std::fs::symlink_metadata(component).map_err(|error| {
            EngineError::SandboxPolicyRefused {
                reason: format!(
                    "launch-preparer {label} component {} cannot be inspected: {error}",
                    component.display()
                ),
            }
        })?;
        if metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!(
                    "launch-preparer {label} component {} is not node-owned and immutable",
                    component.display()
                ),
            });
        }
        current = component.parent();
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn exact_runtime_library_mounts(
    _image: &[u8],
    _command_file: &Arc<std::fs::File>,
) -> Result<Vec<PinnedRuntimeFile>, EngineError> {
    Err(EngineError::SandboxPolicyRefused {
        reason: "launch-preparer Bubblewrap isolation requires Linux".to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn pin_runtime_file(
    label: &str,
    path: &Path,
    expected_hash: Option<&str>,
    require_executable: bool,
) -> Result<(PinnedRuntimeFile, Vec<u8>), EngineError> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::fs::{FileExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let canonical = std::fs::canonicalize(path).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!("launch-preparer {label} {} cannot be resolved: {error}", path.display()),
        }
    })?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&canonical)
        .map_err(|error| EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} cannot be pinned: {error}",
                canonical.display()
            ),
        })?;
    if file.as_raw_fd() <= libc::STDERR_FILENO {
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!(
                    "launch-preparer {label} {} descriptor cannot be moved above stdio: {}",
                    canonical.display(),
                    std::io::Error::last_os_error()
                ),
            });
        }
        file = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    let descriptor_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if descriptor_flags < 0 || descriptor_flags & libc::FD_CLOEXEC == 0 {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} descriptor is not protected by close-on-exec",
                canonical.display()
            ),
        });
    }
    let metadata = file.metadata().map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} cannot be inspected: {error}",
                canonical.display()
            ),
        }
    })?;
    if !metadata.is_file() {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} is not a regular file",
                canonical.display()
            ),
        });
    }
    if metadata.permissions().mode() & 0o6000 != 0 {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} must not be setuid or setgid",
                canonical.display()
            ),
        });
    }
    if require_executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} is not executable",
                canonical.display()
            ),
        });
    }
    reject_pinned_file_capabilities(label, file.as_raw_fd(), &canonical)?;
    let mut magic = [0_u8; 4];
    if file.read_at(&mut magic, 0).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} cannot be inspected as ELF: {error}",
                canonical.display()
            ),
        }
    })? != magic.len()
        || magic != *b"\x7fELF"
    {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} is not an ELF image",
                canonical.display()
            ),
        });
    }
    let mut image = Vec::new();
    if let Some(expected_hash) = expected_hash {
        file.read_to_end(&mut image).map_err(|error| {
            EngineError::SandboxPolicyRefused {
                reason: format!(
                    "launch-preparer {label} {} cannot be read from its pinned descriptor: {error}",
                    canonical.display()
                ),
            }
        })?;
        let actual_hash = lillux::sha256_hex(&image);
        if actual_hash != expected_hash {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!(
                    "launch-preparer {label} {} changed after bundle admission (expected {expected_hash}, got {actual_hash})",
                    canonical.display()
                ),
            });
        }
    }
    Ok((
        PinnedRuntimeFile {
            file: Arc::new(file),
            destination: canonical,
        },
        image,
    ))
}

#[cfg(not(target_os = "linux"))]
fn pin_runtime_file(
    _label: &str,
    _path: &Path,
    _expected_hash: Option<&str>,
    _require_executable: bool,
) -> Result<(PinnedRuntimeFile, Vec<u8>), EngineError> {
    Err(EngineError::SandboxPolicyRefused {
        reason: "launch-preparer Bubblewrap isolation requires Linux".to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn reject_pinned_file_capabilities(
    label: &str,
    fd: i32,
    path: &Path,
) -> Result<(), EngineError> {
    let result = unsafe {
        libc::fgetxattr(
            fd,
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if result >= 0 {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} must not carry Linux file capabilities",
                path.display()
            ),
        });
    }
    let error = std::io::Error::last_os_error();
    let code = error.raw_os_error();
    if code != Some(libc::ENODATA)
        && code != Some(libc::ENOTSUP)
        && code != Some(libc::EOPNOTSUPP)
    {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} capabilities cannot be inspected: {error}",
                path.display()
            ),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn runtime_file_fd(file: &Arc<std::fs::File>) -> i32 {
    use std::os::fd::AsRawFd as _;
    file.as_raw_fd()
}

#[cfg(not(target_os = "linux"))]
fn runtime_file_fd(_file: &Arc<std::fs::File>) -> i32 {
    -1
}

fn validate_runtime_destination(label: &str, path: &Path) -> Result<(), EngineError> {
    use std::path::Component;

    if !path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, Component::RootDir | Component::Normal(_))
        })
    {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer {label} {} is not a normalized absolute path",
                path.display()
            ),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn elf_interpreter(image: &[u8]) -> Result<Option<PathBuf>, EngineError> {
    const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
    const PT_DYNAMIC: u32 = 2;
    const PT_INTERP: u32 = 3;
    const PN_XNUM: u64 = 0xffff;
    if image.get(..4) != Some(ELF_MAGIC.as_slice()) {
        return Err(EngineError::SandboxPolicyRefused {
            reason: "launch-preparer executable is not an ELF image".to_owned(),
        });
    }
    let class = *image.get(4).ok_or_else(invalid_elf)?;
    let little_endian = match image.get(5).copied() {
        Some(1) => true,
        Some(2) => false,
        _ => return Err(invalid_elf()),
    };
    if image.get(6).copied() != Some(1) {
        return Err(invalid_elf());
    }
    let (phoff, phentsize, phnum, offset_field, size_field, expected_ehsize, expected_phentsize) = match class {
        1 => (
            elf_integer(image, 28, 4, little_endian)?,
            elf_integer(image, 42, 2, little_endian)?,
            elf_integer(image, 44, 2, little_endian)?,
            4,
            16,
            52,
            32,
        ),
        2 => (
            elf_integer(image, 32, 8, little_endian)?,
            elf_integer(image, 54, 2, little_endian)?,
            elf_integer(image, 56, 2, little_endian)?,
            8,
            32,
            64,
            56,
        ),
        _ => return Err(invalid_elf()),
    };
    if !matches!(elf_integer(image, 16, 2, little_endian)?, 2 | 3) {
        return Err(invalid_elf());
    }
    let ehsize_offset = if class == 1 { 40 } else { 52 };
    if elf_integer(image, ehsize_offset, 2, little_endian)? != expected_ehsize {
        return Err(invalid_elf());
    }
    if phnum == PN_XNUM {
        // Extended program-header numbering requires interpreting the section
        // table. Launch preparers deliberately reject that unnecessary parser
        // surface instead of guessing at the header count.
        return Err(invalid_elf());
    }
    let phoff = usize::try_from(phoff).map_err(|_| invalid_elf())?;
    let phentsize = usize::try_from(phentsize).map_err(|_| invalid_elf())?;
    let phnum = usize::try_from(phnum).map_err(|_| invalid_elf())?;
    if phentsize != expected_phentsize || phnum == 0 || phoff < expected_ehsize as usize {
        return Err(invalid_elf());
    }
    let table_end = phoff
        .checked_add(phnum.checked_mul(phentsize).ok_or_else(invalid_elf)?)
        .ok_or_else(invalid_elf)?;
    if table_end > image.len() {
        return Err(invalid_elf());
    }
    let mut interpreter = None;
    let mut dynamic_segment = None;
    for index in 0..phnum {
        let header = phoff
            .checked_add(index.checked_mul(phentsize).ok_or_else(invalid_elf)?)
            .ok_or_else(invalid_elf)?;
        let segment_type = elf_integer(image, header, 4, little_endian)?;
        if segment_type == u64::from(PT_DYNAMIC) {
            let offset = usize::try_from(elf_integer(
                image,
                header.checked_add(offset_field).ok_or_else(invalid_elf)?,
                if class == 1 { 4 } else { 8 },
                little_endian,
            )?)
            .map_err(|_| invalid_elf())?;
            let size = usize::try_from(elf_integer(
                image,
                header.checked_add(size_field).ok_or_else(invalid_elf)?,
                if class == 1 { 4 } else { 8 },
                little_endian,
            )?)
            .map_err(|_| invalid_elf())?;
            if dynamic_segment.replace((offset, size)).is_some() {
                return Err(invalid_elf());
            }
        }
        if segment_type != u64::from(PT_INTERP) {
            continue;
        }
        let offset = usize::try_from(elf_integer(
            image,
            header.checked_add(offset_field).ok_or_else(invalid_elf)?,
            if class == 1 { 4 } else { 8 },
            little_endian,
        )?)
        .map_err(|_| invalid_elf())?;
        let size = usize::try_from(elf_integer(
            image,
            header.checked_add(size_field).ok_or_else(invalid_elf)?,
            if class == 1 { 4 } else { 8 },
            little_endian,
        )?)
        .map_err(|_| invalid_elf())?;
        if size == 0 || size > 4096 {
            return Err(invalid_elf());
        }
        let bytes = image
            .get(offset..offset.checked_add(size).ok_or_else(invalid_elf)?)
            .ok_or_else(invalid_elf)?;
        let bytes = bytes.strip_suffix(&[0]).ok_or_else(invalid_elf)?;
        let value = std::str::from_utf8(bytes).map_err(|_| invalid_elf())?;
        if value.is_empty() || value.as_bytes().contains(&0) {
            return Err(invalid_elf());
        }
        if interpreter.replace(PathBuf::from(value)).is_some() {
            return Err(invalid_elf());
        }
    }
    if let Some((offset, size)) = dynamic_segment {
        reject_elf_runtime_search_paths(image, class, little_endian, offset, size)?;
    }
    Ok(interpreter)
}

#[cfg(target_os = "linux")]
fn reject_elf_runtime_search_paths(
    image: &[u8],
    class: u8,
    little_endian: bool,
    offset: usize,
    size: usize,
) -> Result<(), EngineError> {
    const DT_NULL: u64 = 0;
    const DT_RPATH: u64 = 15;
    const DT_RUNPATH: u64 = 29;

    let (entry_size, integer_width) = match class {
        1 => (8, 4),
        2 => (16, 8),
        _ => return Err(invalid_elf()),
    };
    if size == 0 || size % entry_size != 0 {
        return Err(invalid_elf());
    }
    let dynamic = image
        .get(offset..offset.checked_add(size).ok_or_else(invalid_elf)?)
        .ok_or_else(invalid_elf)?;
    let mut terminated = false;
    for entry in dynamic.chunks_exact(entry_size) {
        match elf_integer(entry, 0, integer_width, little_endian)? {
            DT_NULL => {
                terminated = true;
                break;
            }
            DT_RPATH | DT_RUNPATH => {
                return Err(EngineError::SandboxPolicyRefused {
                    reason: "launch-preparer executable must not declare DT_RPATH or DT_RUNPATH"
                        .to_owned(),
                });
            }
            _ => {}
        }
    }
    if !terminated {
        return Err(invalid_elf());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn elf_integer(
    image: &[u8],
    offset: usize,
    width: usize,
    little_endian: bool,
) -> Result<u64, EngineError> {
    let bytes = image
        .get(offset..offset.checked_add(width).ok_or_else(invalid_elf)?)
        .ok_or_else(invalid_elf)?;
    let mut value = 0_u64;
    if little_endian {
        for (shift, byte) in bytes.iter().enumerate() {
            value |= u64::from(*byte) << (shift * 8);
        }
    } else {
        for byte in bytes {
            value = (value << 8) | u64::from(*byte);
        }
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
fn invalid_elf() -> EngineError {
    EngineError::SandboxPolicyRefused {
        reason: "launch-preparer executable has an invalid ELF program-header table".to_owned(),
    }
}

#[derive(Debug, Clone)]
enum StreamEvent {
    LimitExceeded { stream: &'static str, cap: usize },
    IoFailure { stream: &'static str, detail: String },
    Timeout,
}

#[derive(Debug)]
struct BoundedCapture {
    bytes: Vec<u8>,
    violation: Option<StreamEvent>,
}

fn spawn_bounded_reader<R>(
    mut reader: R,
    stream: &'static str,
    cap: usize,
    events: mpsc::Sender<StreamEvent>,
) -> thread::JoinHandle<BoundedCapture>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(cap.min(64 * 1024));
        let mut buffer = [0_u8; 8192];
        loop {
            let remaining_with_probe = cap.saturating_sub(bytes.len()).saturating_add(1);
            let read_len = remaining_with_probe.min(buffer.len());
            match reader.read(&mut buffer[..read_len]) {
                Ok(0) => {
                    return BoundedCapture {
                        bytes,
                        violation: None,
                    };
                }
                Ok(count) => {
                    let remaining = cap.saturating_sub(bytes.len());
                    bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                    if count > remaining {
                        let violation = StreamEvent::LimitExceeded { stream, cap };
                        let _ = events.send(violation.clone());
                        return BoundedCapture {
                            bytes,
                            violation: Some(violation),
                        };
                    }
                }
                Err(error) => {
                    let violation = StreamEvent::IoFailure {
                        stream,
                        detail: error.to_string(),
                    };
                    let _ = events.send(violation.clone());
                    return BoundedCapture {
                        bytes,
                        violation: Some(violation),
                    };
                }
            }
        }
    })
}

fn join_bounded_reader(
    thread: thread::JoinHandle<BoundedCapture>,
    handler: &str,
    stream: &'static str,
) -> Result<BoundedCapture, EngineError> {
    thread.join().map_err(|_| EngineError::LaunchPreparerProtocolInvalid {
        handler: handler.to_owned(),
        detail: format!("launch-preparer {stream} reader panicked"),
    })
}

fn stream_violation_error(handler: &str, violation: StreamEvent) -> EngineError {
    match violation {
        StreamEvent::Timeout => EngineError::LaunchPreparerUnavailable {
            handler: handler.to_owned(),
            detail: format!(
                "launch-preparer timed out after {}s",
                LAUNCH_PREPARER_TIMEOUT.as_secs()
            ),
        },
        StreamEvent::LimitExceeded { stream, cap } => EngineError::LaunchPreparerLimitExceeded {
            handler: handler.to_owned(),
            detail: format!("launch-preparer {stream} exceeded {cap} bytes"),
        },
        StreamEvent::IoFailure { stream, detail } => EngineError::LaunchPreparerUnavailable {
            handler: handler.to_owned(),
            detail: format!("launch-preparer {stream} I/O failure: {detail}"),
        },
    }
}

fn nonzero_error(handler: String, status: ExitStatus, stderr: Vec<u8>) -> EngineError {
    EngineError::LaunchPreparerUnavailable {
        handler,
        detail: format!(
            "handler exited with code {}; stderr: {}",
            status.code().unwrap_or(-1),
            String::from_utf8_lossy(&stderr)
        ),
    }
}

fn kill_process_group(process_group: i32, child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        let _ = kill(-process_group, SIGKILL);
    }
    let _ = child.kill();
}
