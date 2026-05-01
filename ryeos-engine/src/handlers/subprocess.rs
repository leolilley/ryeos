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
pub(crate) fn run_handler_subprocess(
    handler: &VerifiedHandler,
    request: &HandlerRequest,
    timeout: Duration,
) -> Result<HandlerResponse, EngineError> {
    let request_json = serde_json::to_string(request)
        .map_err(|e| EngineError::Internal(format!("encode handler request: {e}")))?;

    let req = lillux::exec::SubprocessRequest {
        cmd: handler.resolved_binary_path.display().to_string(),
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
                handler: handler.canonical_ref.clone(),
                detail: format!("timed out after {}s", output.duration_ms / 1000.0),
            });
        }
        return Err(EngineError::HandlerExitNonZero {
            handler: handler.canonical_ref.clone(),
            exit_code: output.exit_code,
            stderr: output.stderr,
        });
    }

    serde_json::from_str(&output.stdout).map_err(|e| EngineError::HandlerProtocolViolation {
        handler: handler.canonical_ref.clone(),
        detail: e.to_string(),
    })
}
