//! Wire protocol for handler binaries (parsers + composers).
//!
//! Handler binaries read a single `HandlerRequest` from stdin as a
//! single JSON object (one-shot, pipe closed), do their work, and
//! write a single `HandlerResponse` to stdout (one-shot, then
//! exit). Tracing / logging goes to STDERR ONLY. Stdout is reserved
//! for the response envelope.
//!
//! Exit code:
//!   - 0 on a well-formed response (whether `Ok` or `Err`).
//!   - non-zero ONLY for unrecoverable failures that prevent
//!     producing any response (panic, OOM, malformed stdin).
//!
//! Timeout enforced by the engine; binaries do not need to set their
//! own.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// ── Request / Response envelope ──────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum HandlerRequest {
    Parse(ParseRequest),
    ValidateParserConfig(ValidateParserConfigRequest),
    Compose(ComposeRequest),
    ValidateComposerConfig(ValidateComposerConfigRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum HandlerResponse {
    ParseOk { value: Value },
    ParseErr { kind: ParseErrKind, message: String },
    ValidateOk,
    ValidateErr { message: String },
    ComposeOk(ComposeSuccess),
    ComposeErr { step: ResolutionStepNameWire, reason: String },
}

// ── Parser ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseRequest {
    pub parser_config: Value,
    pub content: String,
    /// Optional file path for diagnostics. Wire format only — handlers
    /// must not assume the file exists at this path on their fs.
    #[serde(default)]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateParserConfigRequest {
    pub parser_config: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ParseErrKind {
    Syntax,
    Schema,
    Internal,
}

// ── Composer ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeRequest {
    pub composer_config: Value,
    pub root: ComposeInput,
    /// Deepest ancestor first. Same semantic order as the engine
    /// in-process compose call site uses today.
    #[serde(default)]
    pub ancestors: Vec<ComposeInput>,
}

/// Slim ancestor payload. Strips raw_content / raw_content_digest /
/// alias_resolution / added_by / source_path from ResolvedAncestor —
/// composers only need identity + trust + parsed value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeInput {
    pub item: ComposeItemContext,
    pub parsed: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeItemContext {
    pub requested_id: String,
    pub resolved_ref: String,
    pub trust_class: TrustClassWire,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TrustClassWire {
    TrustedSystem,
    TrustedUser,
    UntrustedUserSpace,
    Unsigned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateComposerConfigRequest {
    pub composer_config: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeSuccess {
    pub composed: Value,
    #[serde(default)]
    pub derived: HashMap<String, Value>,
    #[serde(default)]
    pub policy_facts: HashMap<String, Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionStepNameWire {
    PipelineInit,
    ResolveExtendsChain,
    ResolveReferences,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_round_trips() {
        let req = HandlerRequest::Parse(ParseRequest {
            parser_config: serde_json::json!({"strict": true}),
            content: "key: value\n".into(),
            source_path: Some("/tmp/x.yaml".into()),
        });
        let s = serde_json::to_string(&req).unwrap();
        let back: HandlerRequest = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, HandlerRequest::Parse(_)));
    }

    #[test]
    fn compose_request_strips_to_slim_input() {
        let req = HandlerRequest::Compose(ComposeRequest {
            composer_config: serde_json::json!({}),
            root: ComposeInput {
                item: ComposeItemContext {
                    requested_id: "@alias".into(),
                    resolved_ref: "directive:foo".into(),
                    trust_class: TrustClassWire::TrustedSystem,
                },
                parsed: serde_json::json!({"body": "hi"}),
            },
            ancestors: vec![],
        });
        let s = serde_json::to_string(&req).unwrap();
        // Must NOT contain raw_content / source_path / added_by /
        // alias_resolution — confirm the slim shape.
        assert!(!s.contains("raw_content"));
        assert!(!s.contains("source_path"));
        assert!(!s.contains("added_by"));
        assert!(!s.contains("alias_resolution"));
    }

    #[test]
    fn handler_response_variants_serialize_with_result_tag() {
        let r = HandlerResponse::ParseOk { value: serde_json::json!(42) };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""result":"parse_ok""#));
    }
}
