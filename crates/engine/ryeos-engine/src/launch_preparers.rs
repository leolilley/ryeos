//! Runtime-to-launch-preparer bindings and the dedicated launch-preparer
//! subprocess boundary.
//!
//! Launch preparers receive verified execution identities and may return
//! symbolic secret requirements, so they do not share the general
//! parser/composer runner. This module always uses the node-declared absolute
//! Bubblewrap backend with a fixed, networkless, host-filesystem-denying
//! profile and bounded streaming I/O.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
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
use crate::subprocess_spec::load_node_sandbox_policy;

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
    bubblewrap_path: PathBuf,
}

impl LaunchPreparerRunner {
    /// Load only the node-owned absolute backend path. All other policy fields
    /// are deliberately ignored because launch preparation has a fixed profile
    /// that descriptors and operator sandbox policy cannot widen.
    pub fn from_node_policy(app_root: &Path) -> Result<Self, EngineError> {
        let policy = load_node_sandbox_policy(app_root)?;
        let bubblewrap_path = validate_bubblewrap_backend(&policy.backend_path)?;
        Ok(Self { bubblewrap_path })
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
        let mut command = Command::new(&self.bubblewrap_path);
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
                command.pre_exec(apply_launch_preparer_rlimits);
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

fn validate_bubblewrap_backend(configured: &Path) -> Result<PathBuf, EngineError> {
    if !configured.is_absolute() {
        return Err(EngineError::SandboxPolicyRefused {
            reason: "launch-preparer Bubblewrap backend must be absolute".to_owned(),
        });
    }
    let canonical = std::fs::canonicalize(configured).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer Bubblewrap backend {} cannot be resolved: {error}",
                configured.display()
            ),
        }
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer Bubblewrap backend {} cannot be inspected: {error}",
                canonical.display()
            ),
        }
    })?;
    if !metadata.is_file() {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer Bubblewrap backend {} is not a file",
                canonical.display()
            ),
        });
    }
    let name = canonical.file_name().and_then(OsStr::to_str).unwrap_or("");
    if !matches!(name, "bwrap" | "bubblewrap") {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "launch-preparer backend {} is not Bubblewrap",
                canonical.display()
            ),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!(
                    "launch-preparer Bubblewrap backend {} is not executable",
                    canonical.display()
                ),
            });
        }
    }
    Ok(canonical)
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

    let library_mounts = ["/lib", "/lib64", "/usr/lib", "/usr/lib64"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .map(|destination| {
            let source = std::fs::canonicalize(&destination).unwrap_or(destination.clone());
            (source, destination)
        })
        .collect::<Vec<_>>();
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
