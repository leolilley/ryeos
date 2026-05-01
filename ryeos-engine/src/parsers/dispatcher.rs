//! `ParserDispatcher` — subprocess-based parser dispatch via HandlerRegistry.
//!
//! Resolves a parser ref (canonical `parser:` ref) → descriptor → handler
//! binary. Spawns the handler subprocess with a `HandlerRequest::Parse`,
//! parses the JSON response. Never calls native handlers in-process.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ryeos_handler_protocol::{
    HandlerRequest, HandlerResponse, ParseRequest, ValidateParserConfigRequest,
};
use serde_json::Value;

use crate::contracts::SignatureEnvelope;
use crate::error::EngineError;
use crate::handlers::{HandlerRegistry, HandlerServes, VerifiedHandler};

use super::registry::ParserRegistry;

const PARSE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct ParserDispatcher {
    pub parser_tools: ParserRegistry,
    handlers: Arc<HandlerRegistry>,
}

impl ParserDispatcher {
    pub fn new(parser_tools: ParserRegistry, handlers: Arc<HandlerRegistry>) -> Self {
        Self {
            parser_tools,
            handlers,
        }
    }

    pub fn with_parser_tools(&self, parser_tools: ParserRegistry) -> Self {
        Self {
            parser_tools,
            handlers: Arc::clone(&self.handlers),
        }
    }

    pub fn dispatch(
        &self,
        parser_ref: &str,
        content: &str,
        path: Option<&Path>,
        signature_envelope: &SignatureEnvelope,
    ) -> Result<Value, EngineError> {
        tracing::trace!(
            parser_ref = parser_ref,
            sig_prefix = %signature_envelope.prefix,
            "parser dispatch"
        );

        let descriptor = self.parser_tools.get(parser_ref).ok_or_else(|| {
            EngineError::ParserNotRegistered {
                parser_id: parser_ref.to_string(),
            }
        })?;

        let handler = self
            .handlers
            .ensure_serves(&descriptor.handler, HandlerServes::Parser)
            .map_err(EngineError::Handler)?;

        let stripped = lillux::signature::strip_signature_lines_with_envelope(
            content,
            &signature_envelope.prefix,
            signature_envelope.suffix.as_deref(),
        );

        let request = HandlerRequest::Parse(ParseRequest {
            parser_config: descriptor.parser_config.clone(),
            content: stripped,
            source_path: path.map(|p| p.display().to_string()),
        });

        let resp = run_handler_subprocess(handler, &request, PARSE_TIMEOUT)?;

        match resp {
            HandlerResponse::ParseOk { value } => Ok(value),
            HandlerResponse::ParseErr { kind, message } => {
                Err(EngineError::ParserFailed {
                    parser_id: parser_ref.into(),
                    kind: crate::error::ParseErrKind::from_wire(kind),
                    message,
                })
            }
            other => Err(EngineError::Internal(format!(
                "parser handler returned unexpected response: {other:?}"
            ))),
        }
    }

    pub fn validate_config(&self, parser_ref: &str) -> Result<(), EngineError> {
        let descriptor = self.parser_tools.get(parser_ref).ok_or_else(|| {
            EngineError::ParserNotRegistered {
                parser_id: parser_ref.to_string(),
            }
        })?;

        let handler = self
            .handlers
            .ensure_serves(&descriptor.handler, HandlerServes::Parser)
            .map_err(EngineError::Handler)?;

        let request =
            HandlerRequest::ValidateParserConfig(ValidateParserConfigRequest {
                parser_config: descriptor.parser_config.clone(),
            });

        let resp = run_handler_subprocess(handler, &request, PARSE_TIMEOUT)?;

        match resp {
            HandlerResponse::ValidateOk => Ok(()),
            HandlerResponse::ValidateErr { message } => Err(EngineError::ParserFailed {
                parser_id: parser_ref.into(),
                kind: crate::error::ParseErrKind::Internal,
                message,
            }),
            other => Err(EngineError::Internal(format!(
                "parser handler returned unexpected response to validate_config: {other:?}"
            ))),
        }
    }
}

fn run_handler_subprocess(
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
