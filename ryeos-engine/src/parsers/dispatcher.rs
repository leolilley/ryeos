//! `ParserDispatcher` — engine-local synchronous parser invocation.
//!
//! Resolves a parser ref (canonical `tool:` ref) → descriptor →
//! native handler. Handles signature stripping, then hands the cleaned
//! content to the handler. Never goes through the launcher / runtime.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::contracts::SignatureEnvelope;
use crate::error::EngineError;

use super::handlers::{NativeParserHandlerRegistry, ParseInput};
use super::registry::ParserRegistry;

/// Dispatcher cloning is cheap: `parser_tools` is a `HashMap` of
/// owned descriptors (Clone), and the handler registry is held behind
/// an `Arc` so per-request overlays don't have to rebuild handler
/// state. Each request can fork an effective dispatcher with the
/// project overlay applied to `parser_tools` while sharing the same
/// handler set.
#[derive(Debug, Clone)]
pub struct ParserDispatcher {
    pub parser_tools: ParserRegistry,
    pub native_handlers: Arc<NativeParserHandlerRegistry>,
}

impl ParserDispatcher {
    pub fn new(
        parser_tools: ParserRegistry,
        native_handlers: NativeParserHandlerRegistry,
    ) -> Self {
        Self {
            parser_tools,
            native_handlers: Arc::new(native_handlers),
        }
    }

    /// Build a dispatcher that shares this dispatcher's handler set
    /// but uses a different `ParserRegistry`. Used by
    /// `Engine::effective_parser_dispatcher` to apply a per-request
    /// project overlay without reconstructing handlers.
    pub fn with_parser_tools(&self, parser_tools: ParserRegistry) -> Self {
        Self {
            parser_tools,
            native_handlers: Arc::clone(&self.native_handlers),
        }
    }

    /// Test-only convenience: build a dispatcher from a list of
    /// `(canonical_ref, descriptor)` pairs and the built-in native
    /// handlers. Production code loads `ParserRegistry` from disk via
    /// `ParserRegistry::load_base`.
    pub fn from_descriptors(
        entries: Vec<(String, super::descriptor::ParserDescriptor)>,
    ) -> Self {
        Self::new(
            ParserRegistry::from_entries(entries),
            NativeParserHandlerRegistry::with_builtins(),
        )
    }

    /// Resolve `parser_ref` → descriptor → native handler, then run it.
    ///
    /// Signature stripping uses the kind-specific
    /// `signature_envelope` so a `# rye:signed:...` line in the body of
    /// a markdown file (whose envelope is `<!-- ... -->`) is never
    /// mistakenly stripped as if it were the bootstrap signature.
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

        let handler_name = descriptor
            .executor_id
            .strip_prefix("native:")
            .ok_or_else(|| EngineError::Internal(format!(
                "parser `{parser_ref}` has non-native executor `{}`; \
                 only `native:` parsers are supported in v1",
                descriptor.executor_id
            )))?;

        let handler = self
            .native_handlers
            .get(handler_name)
            .ok_or_else(|| EngineError::Internal(format!(
                "parser `{parser_ref}` references unknown native handler `{handler_name}`"
            )))?;

        // Strip ONLY signatures wrapped in this kind's envelope. A
        // `# rye:signed:...` line that lives in the body of a markdown
        // document (envelope `<!-- ... -->`) is not part of the
        // bootstrap signature layer and must reach the parser intact.
        let stripped = lillux::signature::strip_signature_lines_with_envelope(
            content,
            &signature_envelope.prefix,
            signature_envelope.suffix.as_deref(),
        );

        handler.parse(
            &descriptor.parser_config,
            ParseInput {
                content: &stripped,
                path,
            },
        )
    }
}
