//! Dispatch: match KnowledgeRequest → call into the right module.

use serde_json::Value;

use crate::compose;
use crate::graph;
use crate::query;
use crate::types::{KnowledgeError, KnowledgeRequest};
use crate::validate;

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
        KnowledgeRequest::Query(payload) => {
            let out = query::query(payload)?;
            serde_json::to_value(out).map_err(|e| KnowledgeError::Internal(e.to_string()))
        }
        KnowledgeRequest::Graph(payload) => {
            let out = graph::graph(payload)?;
            serde_json::to_value(out).map_err(|e| KnowledgeError::Internal(e.to_string()))
        }
        KnowledgeRequest::Validate(payload) => {
            let out = validate::validate(payload)?;
            serde_json::to_value(out).map_err(|e| KnowledgeError::Internal(e.to_string()))
        }
        KnowledgeRequest::Snapshot(_) => Err(KnowledgeError::NotImplemented {
            op: "snapshot".into(),
            phase: 5,
        }),
        KnowledgeRequest::Index(_) => Err(KnowledgeError::NotImplemented {
            op: "index".into(),
            phase: 6,
        }),
    }
}
