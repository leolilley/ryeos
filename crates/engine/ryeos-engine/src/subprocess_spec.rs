//! The unified subprocess invocation boundary.
//!
//! Both the tool-style subprocess path and the runtime-style spawn path
//! build the SAME struct. The struct is then translated into a
//! `lillux::SubprocessRequest` at the lillux boundary.
//!
//! This struct is also the input to the node-owned `sandbox_wrap()` stage.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::protocol_vocabulary::{CallbackChannel, StdoutShape};
use crate::resolution::ResolutionOutput;

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
    pub allow_host_read: bool,
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
    if !policy.allow_host_read {
        return Err(EngineError::SandboxPolicyRefused {
            reason: "the bubblewrap backend currently requires explicit allow_host_read: true"
                .to_string(),
        });
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

    let resolve_path = |configured: &str| -> Result<PathBuf, EngineError> {
        let path = match configured {
            "{project}" => spec.project_path.clone(),
            "{cwd}" => spec.cwd.clone(),
            other => PathBuf::from(other),
        };
        if !path.is_absolute() {
            return Err(EngineError::SandboxPolicyRefused {
                reason: format!("sandbox path `{}` is not absolute", path.display()),
            });
        }
        Ok(path)
    };
    let mut writable_paths = policy
        .writable_paths
        .iter()
        .map(|path| resolve_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    writable_paths.sort();
    writable_paths.dedup();
    if !writable_paths.iter().any(|root| spec.cwd.starts_with(root)) {
        return Err(EngineError::SandboxPolicyRefused {
            reason: format!(
                "working directory {} is not writable by node policy",
                spec.cwd.display()
            ),
        });
    }

    let mut args = vec![
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
        "--unshare-all".to_string(),
    ];
    if policy.allow_network {
        args.push("--share-net".to_string());
    }
    args.extend(["--ro-bind".to_string(), "/".to_string(), "/".to_string()]);
    args.extend(["--dev".to_string(), "/dev".to_string()]);
    args.extend(["--proc".to_string(), "/proc".to_string()]);
    args.extend(["--tmpfs".to_string(), "/tmp".to_string()]);
    for path in &writable_paths {
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
    args.push(spec.cwd.to_string_lossy().into_owned());
    args.push("--".to_string());
    args.push(spec.cmd.to_string_lossy().into_owned());
    args.append(&mut spec.args);

    spec.cmd = policy.backend_path.clone();
    spec.args = args;
    spec.sandbox = Some(EffectiveSandbox {
        backend: "bubblewrap".to_string(),
        backend_path: policy.backend_path.clone(),
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
        SubprocessSpec {
            cmd: PathBuf::from("/usr/bin/example"),
            args: vec!["run".to_string()],
            cwd: PathBuf::from("/work/project"),
            env: vec![("PATH".to_string(), "/usr/bin".to_string())],
            stdin: Vec::new(),
            timeout: Duration::from_secs(30),
            stdout_shape: StdoutShape::OpaqueBytes,
            callback_channel: CallbackChannel::None,
            item_ref: CanonicalRef::parse("runtime:test").unwrap(),
            thread_id: "T-test".to_string(),
            project_path: PathBuf::from("/work/project"),
            sandbox: None,
        }
    }

    fn policy() -> NodeSandboxPolicy {
        NodeSandboxPolicy {
            version: 1,
            backend_path: PathBuf::from("/usr/bin/bwrap"),
            allow_network: false,
            allow_host_read: true,
            writable_paths: vec!["{project}".to_string()],
            allowed_env: vec!["PATH".to_string()],
            max_open_files: Some(128),
            max_processes: Some(32),
        }
    }

    #[test]
    fn wraps_command_with_effective_node_policy() {
        let wrapped = sandbox_wrap(spec(), &policy()).unwrap();
        assert_eq!(wrapped.cmd, PathBuf::from("/usr/bin/bwrap"));
        assert!(wrapped
            .args
            .windows(3)
            .any(|args| args == ["--bind", "/work/project", "/work/project"]));
        assert!(!wrapped.args.iter().any(|arg| arg == "--share-net"));
        assert_eq!(
            wrapped.sandbox.unwrap().writable_paths,
            vec![PathBuf::from("/work/project")]
        );
    }

    #[test]
    fn rejects_environment_not_granted_by_node() {
        let mut spec = spec();
        spec.env.push(("SECRET".to_string(), "value".to_string()));
        let error = sandbox_wrap(spec, &policy()).unwrap_err();
        assert!(error.to_string().contains("SECRET"));
    }
}
