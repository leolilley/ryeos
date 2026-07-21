//! Shared subprocess runner for handler binaries (parsers + composers).
//!
//! Both `ParserDispatcher` and `ComposerRegistry` spawn handler
//! binaries the same way: serialize the request as JSON, run the
//! binary through the immutable node [`IsolationRuntime`] (which also scrubs the
//! environment), parse
//! the stdout JSON envelope, and turn timeouts / non-zero exits /
//! malformed envelopes into structured engine errors.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ryeos_handler_protocol::{HandlerRequest, HandlerResponse};

use crate::error::EngineError;
use crate::handlers::VerifiedHandler;
use crate::isolation::{
    IsolationLaunchContext, IsolationProjectAuthority, IsolationRuntime, IsolationVerifiedCode,
};

/// Immutable launch authority shared by every handler in one verified
/// registry generation. Handler binaries are pure node infrastructure: their
/// installed roots are visible read-only and no host writable mount is
/// granted.
#[derive(Debug)]
pub(crate) struct HandlerLaunchRuntime {
    isolation: Arc<IsolationRuntime>,
    bundle_roots: Vec<PathBuf>,
}

impl HandlerLaunchRuntime {
    pub(crate) fn new(isolation: Arc<IsolationRuntime>, bundle_roots: Vec<PathBuf>) -> Self {
        Self {
            isolation,
            bundle_roots,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self::new(Arc::new(IsolationRuntime::default()), Vec::new())
    }
}

/// Run a handler subprocess and decode its envelope.
///
/// The registry's immutable isolation snapshot is applied before Lillux sees
/// the request. In enforce mode the exact manifest-verified binary is captured
/// into the node's verified-code store, bundle roots are mounted read-only,
/// and no host writable mount is granted. Disabled mode still applies the
/// node-owned output retention limits.
///
/// Returns [`EngineError::HandlerBinaryMissing`] if the handler was
/// registered but its binary could not be resolved (user-tier handler
/// pushed from a remote without the corresponding bundle installed).
pub(crate) fn run_handler_subprocess(
    handler: &VerifiedHandler,
    request: &HandlerRequest,
    timeout: Duration,
    launch: &HandlerLaunchRuntime,
) -> Result<HandlerResponse, EngineError> {
    let (canonical_ref, binary_path, binary_hash, bundle_root) = match handler {
        VerifiedHandler::Resolved {
            canonical_ref,
            resolved_binary_path,
            resolved_binary_hash,
            bundle_root,
            ..
        } => (
            canonical_ref.clone(),
            resolved_binary_path.clone(),
            resolved_binary_hash.clone(),
            bundle_root.clone(),
        ),
        VerifiedHandler::Unresolved {
            canonical_ref,
            reason,
            descriptor,
            ..
        } => {
            return Err(EngineError::HandlerBinaryMissing {
                handler: canonical_ref.clone(),
                binary_ref: descriptor.binary_ref.clone(),
                reason: reason.clone(),
                remediation: format!(
                    "binary '{}' not installed on this node — install the \
                     bundle containing it or push it as a project-tier item",
                    descriptor.binary_ref
                ),
            });
        }
    };

    let request_json = serde_json::to_string(request)
        .map_err(|e| EngineError::Internal(format!("encode handler request: {e}")))?;

    let req = lillux::exec::SubprocessRequest {
        cmd: binary_path.display().to_string(),
        args: vec![],
        cwd: Some(bundle_root.display().to_string()),
        envs: vec![],
        stdin_data: Some(request_json),
        timeout: timeout.as_secs_f64(),
        limits: None,
        inherited_fds: Vec::new(),
        supervised_status: None,
    };

    let verified_code = [IsolationVerifiedCode {
        source_path: binary_path,
        content_hash: binary_hash,
    }];
    let req = launch.isolation.apply(
        req,
        IsolationLaunchContext {
            project_path: &bundle_root,
            project_authority: IsolationProjectAuthority::ReadOnly,
            live_access: None,
            state_root: None,
            checkpoint_dir: None,
            daemon_socket_path: None,
            bundle_roots: &launch.bundle_roots,
            node_trusted_keys_dir: None,
            verified_code: &verified_code,
            item_ref: &canonical_ref,
            thread_id: "handler",
        },
    )?;

    let output = lillux::exec::lib_run(req);
    if !output.success {
        if output.timed_out {
            return Err(EngineError::HandlerSpawnFailed {
                handler: canonical_ref.clone(),
                detail: format!("timed out after {}s", output.duration_ms / 1000.0),
            });
        }
        return Err(EngineError::HandlerExitNonZero {
            handler: canonical_ref.clone(),
            exit_code: output.exit_code,
            stderr: output.stderr,
        });
    }

    ryeos_handler_protocol::from_json_str_strict(&output.stdout).map_err(|e| {
        EngineError::HandlerProtocolViolation {
            handler: canonical_ref,
            detail: e.to_string(),
        }
    })
}
