//! The unified subprocess invocation boundary.
//!
//! Both the tool-style subprocess path and the runtime-style spawn path
//! build the SAME struct. The struct is then translated into a
//! `lillux::SubprocessRequest` at the lillux boundary.
//!
//! This struct is also the input to the node-owned `sandbox_wrap()` stage.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::protocol_vocabulary::{CallbackChannel, StdoutShape};
use crate::resolution::ResolutionOutput;

pub fn load_node_sandbox_policy(
    app_root: &std::path::Path,
) -> Result<NodeSandboxPolicy, EngineError> {
    let path = app_root.join(crate::AI_DIR).join("node/sandbox.yaml");
    let raw =
        std::fs::read_to_string(&path).map_err(|error| EngineError::SandboxPolicyRefused {
            reason: format!(
                "node sandbox policy is required at {}: {error}",
                path.display()
            ),
        })?;
    serde_yaml::from_str(&raw).map_err(|error| EngineError::SandboxPolicyRefused {
        reason: format!("invalid node sandbox policy {}: {error}", path.display()),
    })
}

/// The single executable-item boundary from a planned Lillux request to a
/// node-policy-wrapped request. Diagnostic probes intentionally do not use it.
pub fn sandbox_lillux_request(
    request: lillux::SubprocessRequest,
    app_root: &std::path::Path,
    project_path: &std::path::Path,
    item_ref: &str,
    thread_id: &str,
) -> Result<lillux::SubprocessRequest, EngineError> {
    let policy = load_node_sandbox_policy(app_root)?;
    if !request.timeout.is_finite() || request.timeout < 0.0 {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!("invalid subprocess timeout {}", request.timeout),
        });
    }
    let cwd = request
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| project_path.to_path_buf());
    let item_ref =
        CanonicalRef::parse(item_ref).map_err(|error| EngineError::SandboxPolicyRefused {
            reason: format!("invalid sandbox item reference `{item_ref}`: {error}"),
        })?;
    let spec = SubprocessSpec {
        cmd: PathBuf::from(request.cmd),
        args: request.args,
        cwd,
        env: request.envs,
        stdin: request.stdin_data.unwrap_or_default().into_bytes(),
        timeout: Duration::from_secs_f64(request.timeout),
        stdout_shape: StdoutShape::OpaqueBytes,
        callback_channel: CallbackChannel::None,
        item_ref,
        thread_id: thread_id.to_string(),
        project_path: project_path.to_path_buf(),
        sandbox: None,
    };
    let wrapped = sandbox_wrap(spec, &policy)?;
    Ok(lillux::SubprocessRequest {
        cmd: wrapped.cmd.to_string_lossy().into_owned(),
        args: wrapped.args,
        cwd: Some(wrapped.cwd.to_string_lossy().into_owned()),
        envs: wrapped.env,
        stdin_data: Some(String::from_utf8_lossy(&wrapped.stdin).into_owned()),
        timeout: wrapped.timeout.as_secs_f64(),
    })
}

/// The unified subprocess invocation boundary. Both the tool-style
/// subprocess path and the runtime-style spawn path build this SAME
/// struct. Translated into a `lillux::SubprocessRequest` at the
/// lillux boundary via `to_lillux_request`.
#[derive(Debug, Clone)]
pub struct SubprocessSpec {
    /// Absolute path to the binary to spawn.
    pub cmd: PathBuf,
    /// Argv excluding cmd[0]. Empty vec is allowed.
    pub args: Vec<String>,
    /// Working directory. Required, no default.
    pub cwd: PathBuf,
    /// Env vars to set on the child. Daemon scrubs first then sets.
    pub env: Vec<(String, String)>,
    /// Bytes written to child's stdin and then close-stdin.
    pub stdin: Vec<u8>,
    /// Hard timeout; child is killed on exceed.
    pub timeout: Duration,

    /// Stdout shape declared by the protocol descriptor; the lillux
    /// bridge consults this to choose buffered vs streaming decode.
    /// Default: `OpaqueBytes` (compatible with specs not yet
    /// routed through the builder).
    pub stdout_shape: StdoutShape,

    /// Callback channel kind; the launcher consults this to know
    /// whether to register a callback token before spawn.
    /// Default: `None` (compatible).
    pub callback_channel: CallbackChannel,

    /// Provenance fields — used by tracing, callback wiring, and the
    /// sandbox-wrap stage. Not passed to lillux directly.
    pub item_ref: CanonicalRef,
    pub thread_id: String,
    pub project_path: PathBuf,
    /// Effective node-owned sandbox attached after planning.
    pub sandbox: Option<EffectiveSandbox>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSandboxPolicy {
    pub version: u32,
    pub backend_path: PathBuf,
    pub allow_network: bool,
    #[serde(default)]
    pub writable_paths: Vec<String>,
    #[serde(default)]
    pub allowed_env: Vec<String>,
    pub max_open_files: Option<u64>,
    pub max_processes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveSandbox {
    pub backend: String,
    pub backend_path: PathBuf,
    pub allow_network: bool,
    pub writable_paths: Vec<PathBuf>,
    pub max_open_files: Option<u64>,
    pub max_processes: Option<u64>,
}

/// All inputs needed by the vocabulary builders to produce a
/// `SubprocessSpec`. Carries every value any builder might need;
/// builders read only the fields their shape needs.
pub struct SubprocessBuildRequest {
    pub cmd: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub item_ref: CanonicalRef,
    pub thread_id: String,
    pub project_path: PathBuf,
    pub acting_principal: String,
    pub cas_root: PathBuf,
    pub callback_token: Option<String>,
    pub callback_socket_path: Option<String>,
    pub vault_handle: Option<String>,
    pub app_root: PathBuf,
    pub thread_auth_token: Option<String>,
    pub params: Value,
    pub resolution_output: Option<ResolutionOutput>,
}

/// Apply the node-owned sandbox policy without changing the dispatch seam.
pub fn sandbox_wrap(
    mut spec: SubprocessSpec,
    policy: &NodeSandboxPolicy,
) -> Result<SubprocessSpec, EngineError> {
    if policy.version != 1 {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!("unsupported node sandbox policy version {}", policy.version),
        });
    }
    if !policy.backend_path.is_absolute() {
        return Err(EngineError::SandboxPolicyRefused {
            reason: "backend_path must be absolute".to_string(),
        });
    }
    let backend_path = std::fs::canonicalize(&policy.backend_path).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!(
                "sandbox backend {} cannot be resolved: {error}",
                policy.backend_path.display()
            ),
        }
    })?;
    let backend_metadata =
        std::fs::metadata(&backend_path).map_err(|error| EngineError::SandboxPolicyRefused {
            reason: format!(
                "sandbox backend {} cannot be inspected: {error}",
                backend_path.display()
            ),
        })?;
    if !backend_metadata.is_file() {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!("sandbox backend {} is not a file", backend_path.display()),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if backend_metadata.permissions().mode() & 0o111 == 0 {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!(
                    "sandbox backend {} is not executable",
                    backend_path.display()
                ),
            });
        }
    }
    let env_allowed = |name: &str| {
        policy.allowed_env.iter().any(|allowed| {
            allowed == "*"
                || allowed == name
                || allowed
                    .strip_suffix('*')
                    .is_some_and(|prefix| name.starts_with(prefix))
        })
    };
    if let Some((name, _)) = spec.env.iter().find(|(name, _)| !env_allowed(name)) {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!("environment variable `{name}` is not allowed by node policy"),
        });
    }

    let canonical_project = std::fs::canonicalize(&spec.project_path).map_err(|error| {
        EngineError::SandboxPolicyRefused {
            reason: format!(
                "project path {} cannot be resolved: {error}",
                spec.project_path.display()
            ),
        }
    })?;
    let canonical_cwd =
        std::fs::canonicalize(&spec.cwd).map_err(|error| EngineError::SandboxPolicyRefused {
            reason: format!(
                "working directory {} cannot be resolved: {error}",
                spec.cwd.display()
            ),
        })?;
    let resolve_path = |configured: &str| -> Result<PathBuf, EngineError> {
        let path = match configured {
            "{project}" => canonical_project.clone(),
            "{cwd}" => canonical_cwd.clone(),
            other => PathBuf::from(other),
        };
        if !path.is_absolute() {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!("sandbox path `{}` is not absolute", path.display()),
            });
        }
        std::fs::canonicalize(&path).map_err(|error| EngineError::SandboxPolicyRefused {
            reason: format!(
                "sandbox path {} cannot be resolved: {error}",
                path.display()
            ),
        })
    };
    let mut writable_paths = policy
        .writable_paths
        .iter()
        .map(|path| resolve_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    writable_paths.sort();
    writable_paths.dedup();
    if !writable_paths
        .iter()
        .any(|root| canonical_cwd.starts_with(root))
    {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "working directory {} is not writable by node policy",
                spec.cwd.display()
            ),
        });
    }

    let command_path =
        std::fs::canonicalize(&spec.cmd).map_err(|error| EngineError::SandboxPolicyRefused {
            reason: format!("command {} cannot be resolved: {error}", spec.cmd.display()),
        })?;

    let mut args = vec![
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--unshare-all".to_string(),
    ];
    if policy.allow_network {
        args.push("--share-net".to_string());
    }
    args.extend(["--tmpfs".to_string(), "/".to_string()]);
    // Only the OS runtime surface and the exact verified executable are
    // visible. In particular, the app root (vault and signing keys) and the
    // operator's home are not mounted into the sandbox.
    for path in ["/usr", "/bin", "/lib", "/lib64"] {
        let path = PathBuf::from(path);
        if path.exists() {
            let source = std::fs::canonicalize(&path).unwrap_or(path.clone());
            let source = source.to_string_lossy().into_owned();
            args.extend([
                "--ro-bind".to_string(),
                source,
                path.to_string_lossy().into_owned(),
            ]);
        }
    }
    args.extend(["--dir".to_string(), "/etc".to_string()]);
    for path in [
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/resolv.conf",
        "/etc/ssl",
    ] {
        if Path::new(path).exists() {
            args.extend(["--ro-bind".to_string(), path.to_string(), path.to_string()]);
        }
    }
    let command_is_on_system_surface = ["/usr", "/bin", "/lib", "/lib64"]
        .iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .any(|root| command_path.starts_with(root));
    if !command_is_on_system_surface {
        let mut command_parents = command_path
            .ancestors()
            .skip(1)
            .filter(|path| *path != Path::new("/"))
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        command_parents.reverse();
        for parent in command_parents {
            args.extend(["--dir".to_string(), parent.to_string_lossy().into_owned()]);
        }
        let command = command_path.to_string_lossy().into_owned();
        args.extend(["--ro-bind".to_string(), command.clone(), command]);
    }
    args.extend(["--dev".to_string(), "/dev".to_string()]);
    args.extend(["--proc".to_string(), "/proc".to_string()]);
    args.extend(["--tmpfs".to_string(), "/tmp".to_string()]);
    for path in &writable_paths {
        let mut parents = path
            .ancestors()
            .skip(1)
            .filter(|parent| *parent != Path::new("/"))
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        parents.reverse();
        for parent in parents {
            args.extend(["--dir".to_string(), parent.to_string_lossy().into_owned()]);
        }
        let path = path.to_string_lossy().into_owned();
        args.extend(["--bind".to_string(), path.clone(), path]);
    }
    if let Some(limit) = policy.max_open_files {
        args.extend(["--rlimit-nofile".to_string(), limit.to_string()]);
    }
    if let Some(limit) = policy.max_processes {
        args.extend(["--rlimit-nproc".to_string(), limit.to_string()]);
    }
    args.push("--chdir".to_string());
    args.push(canonical_cwd.to_string_lossy().into_owned());
    args.push("--".to_string());
    args.push(command_path.to_string_lossy().into_owned());
    args.append(&mut spec.args);

    spec.cmd = backend_path.clone();
    spec.cwd = canonical_cwd;
    spec.project_path = canonical_project;
    spec.args = args;
    spec.sandbox = Some(EffectiveSandbox {
        backend: "bubblewrap".to_string(),
        backend_path,
        allow_network: policy.allow_network,
        writable_paths,
        max_open_files: policy.max_open_files,
        max_processes: policy.max_processes,
    });
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol_vocabulary::{CallbackChannel, StdoutShape};

    fn spec() -> SubprocessSpec {
        let project = tempfile::tempdir().unwrap().keep();
        SubprocessSpec {
            cmd: PathBuf::from("/bin/sh"),
            args: vec!["run".to_string()],
            cwd: project.clone(),
            env: vec![("PATH".to_string(), "/usr/bin".to_string())],
            stdin: Vec::new(),
            timeout: Duration::from_secs(30),
            stdout_shape: StdoutShape::OpaqueBytes,
            callback_channel: CallbackChannel::None,
            item_ref: CanonicalRef::parse("runtime:test").unwrap(),
            thread_id: "T-test".to_string(),
            project_path: project,
            sandbox: None,
        }
    }

    fn policy() -> NodeSandboxPolicy {
        NodeSandboxPolicy {
            version: 1,
            // The wrapper is only assembled in these unit tests. A ubiquitous
            // executable fixture keeps them independent of host bwrap setup.
            backend_path: PathBuf::from("/bin/sh"),
            allow_network: false,
            writable_paths: vec!["{project}".to_string()],
            allowed_env: vec!["PATH".to_string()],
            max_open_files: Some(128),
            max_processes: Some(32),
        }
    }

    #[test]
    fn wraps_command_with_effective_node_policy() {
        let wrapped = sandbox_wrap(spec(), &policy()).unwrap();
        assert_eq!(wrapped.cmd, std::fs::canonicalize("/bin/sh").unwrap());
        let project = wrapped.project_path.clone();
        let project_text = project.to_string_lossy();
        assert!(wrapped
            .args
            .windows(3)
            .any(|args| args == ["--bind", project_text.as_ref(), project_text.as_ref()]));
        assert!(!wrapped.args.iter().any(|arg| arg == "--share-net"));
        assert_eq!(wrapped.sandbox.unwrap().writable_paths, vec![project]);
    }

    #[test]
    fn rejects_environment_not_granted_by_node() {
        let mut spec = spec();
        spec.env.push(("SECRET".to_string(), "value".to_string()));
        let error = sandbox_wrap(spec, &policy()).unwrap_err();
        assert!(error.to_string().contains("SECRET"));
    }

    #[test]
    fn executable_boundary_requires_node_policy() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let request = lillux::SubprocessRequest {
            cmd: "/bin/sh".to_string(),
            args: vec![],
            cwd: Some(project.path().to_string_lossy().into_owned()),
            envs: vec![],
            stdin_data: None,
            timeout: 1.0,
        };
        let error = match sandbox_lillux_request(
            request,
            app_root.path(),
            project.path(),
            "tool:test/probe",
            "T-test",
        ) {
            Ok(_) => panic!("missing policy must refuse execution"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("policy is required"));
    }

    #[test]
    fn executable_boundary_routes_through_configured_backend() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(app_root.path().join(".ai/node")).unwrap();
        std::fs::write(
            app_root.path().join(".ai/node/sandbox.yaml"),
            "version: 1\nbackend_path: /bin/sh\nallow_network: false\nwritable_paths: [\"{project}\"]\nallowed_env: [\"*\"]\nmax_open_files: 128\nmax_processes: 32\n",
        )
        .unwrap();
        let request = lillux::SubprocessRequest {
            cmd: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "true".to_string()],
            cwd: Some(project.path().to_string_lossy().into_owned()),
            envs: vec![],
            stdin_data: None,
            timeout: 1.0,
        };
        let wrapped = sandbox_lillux_request(
            request,
            app_root.path(),
            project.path(),
            "tool:test/probe",
            "T-test",
        )
        .unwrap();
        assert_eq!(
            wrapped.cmd,
            std::fs::canonicalize("/bin/sh")
                .unwrap()
                .display()
                .to_string()
        );
        assert!(wrapped.args.iter().any(|arg| arg == "--unshare-all"));
    }
}
