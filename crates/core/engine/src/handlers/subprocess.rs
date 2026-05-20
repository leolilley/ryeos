//! Shared subprocess runner for handler binaries (parsers + composers).
//!
//! Both `ParserDispatcher` and `ComposerRegistry` spawn handler
//! binaries the same way: serialize the request as JSON, run the
//! binary with `lillux::exec::lib_run` (which scrubs the env via
//! `env_clear()` — mandatory for hermetic handler execution), parse
//! the stdout JSON envelope, and turn timeouts / non-zero exits /
//! malformed envelopes into structured engine errors.

use std::time::Duration;

use ryeos_handler_protocol::{HandlerRequest, HandlerResponse};

use crate::error::EngineError;
use crate::handlers::VerifiedHandler;

/// Run a handler subprocess and decode its envelope.
///
/// `env_clear()` is enforced via `lillux::exec::lib_run`; handler
/// binaries always run with a scrubbed env so behaviour is hermetic.
///
/// Returns [`EngineError::HandlerBinaryMissing`] if the handler was
/// registered but its binary could not be resolved (user-tier handler
/// pushed from a remote without the corresponding bundle installed).
pub(crate) fn run_handler_subprocess(
    handler: &VerifiedHandler,
    request: &HandlerRequest,
    timeout: Duration,
) -> Result<HandlerResponse, EngineError> {
    let (canonical_ref, binary_path) = match handler {
        VerifiedHandler::Resolved { canonical_ref, resolved_binary_path, .. } => {
            (canonical_ref.clone(), resolved_binary_path.clone())
        }
        VerifiedHandler::Unresolved { canonical_ref, reason, descriptor, .. } => {
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
        cwd: None,
        envs: vec![],
        stdin_data: Some(request_json),
        timeout: timeout.as_secs_f64(),
    };

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

    serde_json::from_str(&output.stdout).map_err(|e| EngineError::HandlerProtocolViolation {
        handler: canonical_ref,
        detail: e.to_string(),
    })
}
