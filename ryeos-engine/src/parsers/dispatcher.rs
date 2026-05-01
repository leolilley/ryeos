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
use crate::handlers::subprocess::run_handler_subprocess;
use crate::handlers::{HandlerRegistry, HandlerServes};

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


