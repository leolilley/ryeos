//! Dispatch: match KnowledgeRequest → call into the right module.

use serde_json::Value;

use crate::compose;
use crate::types::{KnowledgeError, KnowledgeRequest};

/// The single dispatch entry point. Called by main.rs after parsing
/// the envelope.
pub fn dispatch(request: &KnowledgeRequest) -> Result<Value, KnowledgeError> {
    match request {
        KnowledgeRequest::Compose(payload) => {
            let out = compose::compose(payload)?;
            serde_json::to_value(out).map_err(|e| KnowledgeError::Internal(e.to_string()))
        }
        KnowledgeRequest::ComposePositions(payload) => {
            let out = compose::compose_positions(payload)?;
            serde_json::to_value(out).map_err(|e| KnowledgeError::Internal(e.to_string()))
        }
        KnowledgeRequest::Query(_) => Err(KnowledgeError::NotImplemented { op: "query".into(), phase: 3 }),
        KnowledgeRequest::Graph(_) => Err(KnowledgeError::NotImplemented { op: "graph".into(), phase: 3 }),
        KnowledgeRequest::Validate(_) => Err(KnowledgeError::NotImplemented { op: "validate".into(), phase: 4 }),
        KnowledgeRequest::Snapshot(_) => Err(KnowledgeError::NotImplemented { op: "snapshot".into(), phase: 5 }),
        KnowledgeRequest::Index(_) => Err(KnowledgeError::NotImplemented { op: "index".into(), phase: 6 }),
    }
}
