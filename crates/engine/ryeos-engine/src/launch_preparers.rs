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
        let (canonical_ref, binary_path) = match handler {
            VerifiedHandler::Resolved {
                canonical_ref,
                resolved_binary_path,
                ..
            } => (canonical_ref.clone(), resolved_binary_path.clone()),
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

        let args = fixed_bubblewrap_args(&binary_path).map_err(|error| {
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
            .args(args)
            .current_dir("/tmp")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
            unsafe {
                let bubblewrap_fd = self.bubblewrap.as_raw_fd();
                command.pre_exec(move || {
                    if libc::fcntl(bubblewrap_fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
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

fn fixed_bubblewrap_args(handler_binary: &Path) -> Result<Vec<String>, EngineError> {
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
        "--cap-drop".to_owned(),
        "ALL".to_owned(),
        "--tmpfs".to_owned(),
        "/".to_owned(),
    ];

    let library_mounts = exact_runtime_library_mounts(&command_path)?;
    let mut directories = BTreeSet::new();
    for destination in library_mounts
        .iter()
        .map(|(_, destination)| destination.as_path())
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
    for (source, destination) in library_mounts {
        args.extend([
            "--ro-bind".to_owned(),
            source.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ]);
    }
    let command = command_path.to_string_lossy().into_owned();
    args.extend(["--ro-bind".to_owned(), command.clone(), command]);

    args.extend([
        "--dev-bind".to_owned(),
        "/dev/null".to_owned(),
        "/dev/null".to_owned(),
    ]);
    args.extend(["--tmpfs".to_owned(), "/tmp".to_owned()]);
    args.extend(["--chdir".to_owned(), "/tmp".to_owned()]);
    args.push("--".to_owned());
    args.push(command_path.to_string_lossy().into_owned());
    Ok(args)
}

/// Resolve the verified preparer's exact dynamic-loader closure without
/// executing the preparer. The ELF interpreter's `--list` mode asks the loader
/// to resolve `DT_NEEDED` entries with an empty environment; only the returned
/// regular files are mounted into the private namespace. A static ELF has no
/// interpreter and therefore needs no runtime-library mounts.
#[cfg(target_os = "linux")]
fn exact_runtime_library_mounts(
    command_path: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>, EngineError> {
    let image = std::fs::read(command_path).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer binary {} cannot be inspected: {error}",
                command_path.display()
            ),
        }
    })?;
    let Some(interpreter) = elf_interpreter(&image)? else {
        return Ok(Vec::new());
    };
    if !interpreter.is_absolute() {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer ELF interpreter {} is not absolute",
                interpreter.display()
            ),
        });
    }
    let canonical_interpreter = canonical_runtime_file("ELF interpreter", &interpreter)?;
    let output = Command::new(&canonical_interpreter)
        .arg("--list")
        .arg(command_path)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|error| EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer ELF dependency inspection via {} failed: {error}",
                canonical_interpreter.display()
            ),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer ELF dependency inspection failed with status {}; stderr: {}",
                output.status,
                stderr.chars().take(512).collect::<String>()
            ),
        });
    }

    let listing = std::str::from_utf8(&output.stdout).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!("launch-preparer ELF dependency listing is not UTF-8: {error}"),
        }
    })?;
    let mut destinations = BTreeSet::new();
    destinations.insert(interpreter);
    for line in listing.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some((_, resolved)) = line.split_once("=>") {
            let resolved = resolved.trim();
            if resolved.starts_with("not found") {
                return Err(EngineError::SandboxPolicyRefused {
                    reason: format!("launch-preparer dependency is unavailable: {line}"),
                });
            }
            if let Some(path) = resolved.split_whitespace().next() {
                if path.starts_with('/') {
                    destinations.insert(PathBuf::from(path));
                }
            }
            continue;
        }
        if let Some(path) = line.split_whitespace().next() {
            if path.starts_with('/') {
                destinations.insert(PathBuf::from(path));
            }
        }
    }

    destinations
        .into_iter()
        .map(|destination| {
            let source = canonical_runtime_file("runtime library", &destination)?;
            Ok((source, destination))
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn exact_runtime_library_mounts(
    _command_path: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>, EngineError> {
    Err(EngineError::SandboxPolicyRefused {
        reason: "launch-preparer Bubblewrap isolation requires Linux".to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn canonical_runtime_file(label: &str, path: &Path) -> Result<PathBuf, EngineError> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!("launch-preparer {label} {} cannot be resolved: {error}", path.display()),
        }
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
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
    Ok(canonical)
}

#[cfg(target_os = "linux")]
fn elf_interpreter(image: &[u8]) -> Result<Option<PathBuf>, EngineError> {
    const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
    const PT_INTERP: u32 = 3;
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
    let (phoff, phentsize, phnum, offset_field, size_field) = match class {
        1 => (
            elf_integer(image, 28, 4, little_endian)?,
            elf_integer(image, 42, 2, little_endian)?,
            elf_integer(image, 44, 2, little_endian)?,
            4,
            16,
        ),
        2 => (
            elf_integer(image, 32, 8, little_endian)?,
            elf_integer(image, 54, 2, little_endian)?,
            elf_integer(image, 56, 2, little_endian)?,
            8,
            32,
        ),
        _ => return Err(invalid_elf()),
    };
    let phoff = usize::try_from(phoff).map_err(|_| invalid_elf())?;
    let phentsize = usize::try_from(phentsize).map_err(|_| invalid_elf())?;
    let phnum = usize::try_from(phnum).map_err(|_| invalid_elf())?;
    if phentsize == 0 || phnum == 0 {
        return Ok(None);
    }
    for index in 0..phnum {
        let header = phoff
            .checked_add(index.checked_mul(phentsize).ok_or_else(invalid_elf)?)
            .ok_or_else(invalid_elf)?;
        if elf_integer(image, header, 4, little_endian)? != u64::from(PT_INTERP) {
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
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        let value = std::str::from_utf8(bytes).map_err(|_| invalid_elf())?;
        if value.is_empty() || value.as_bytes().contains(&0) {
            return Err(invalid_elf());
        }
        return Ok(Some(PathBuf::from(value)));
    }
    Ok(None)
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
