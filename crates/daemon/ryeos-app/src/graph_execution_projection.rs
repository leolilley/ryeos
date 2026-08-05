//! Internal graph execution trace projection.
//!
//! This is deliberately not a public portable-execution-graph API. It is a
//! small read-model primitive over already-persisted thread events and
//! `graph_node_receipt` artifacts so callers/tests can reason about the bridge
//! from authored graph definition identity to realized node consequences.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::state_store::{PersistedEventRecord, ThreadArtifactRecord};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphExecutionTrace {
    pub thread_id: String,
    pub definition_ref: Option<String>,
    pub effective_definition_digest: Option<String>,
    pub graph_run_id: Option<String>,
    pub nodes: Vec<GraphExecutionNodeTrace>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphExecutionNodeTrace {
    pub node_ref: String,
    pub node: String,
    pub step: Option<u32>,
    pub status: Option<String>,
    pub events: Vec<GraphExecutionEventTrace>,
    pub receipt: Option<GraphNodeReceiptTrace>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphExecutionEventTrace {
    pub event_type: String,
    pub thread_seq: i64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphNodeReceiptTrace {
    pub artifact_id: i64,
    pub uri: String,
    pub step: Option<u32>,
    pub node_result_hash: Option<String>,
    pub error: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Default)]
struct NodeAccumulator {
    node_ref: String,
    node: String,
    step: Option<u32>,
    status: Option<String>,
    events: Vec<GraphExecutionEventTrace>,
    receipt: Option<GraphNodeReceiptTrace>,
}

/// Build an internal trace projection from persisted runtime events and
/// `graph_node_receipt` artifacts for a single graph execution thread.
///
/// The projection is intentionally lossy only in organization, not in payload:
/// raw event and receipt payloads remain attached for future consumers while
/// common identity/status fields are lifted into a stable shape.
pub fn build_graph_execution_trace(
    thread_id: impl Into<String>,
    events: &[PersistedEventRecord],
    artifacts: &[ThreadArtifactRecord],
) -> anyhow::Result<GraphExecutionTrace> {
    let thread_id = thread_id.into();
    let mut definition_ref = None;
    let mut effective_definition_digest = None;
    let mut graph_run_id = None;
    let mut nodes: BTreeMap<String, NodeAccumulator> = BTreeMap::new();

    for event in events {
        capture_graph_identity(
            &event.payload,
            &mut definition_ref,
            &mut effective_definition_digest,
            &mut graph_run_id,
        )?;

        let Some((node_ref, node)) =
            node_identity_from_payload(&event.payload, definition_ref.as_deref())
        else {
            continue;
        };

        let entry = nodes
            .entry(node_ref.clone())
            .or_insert_with(|| NodeAccumulator {
                node_ref,
                node,
                step: None,
                status: None,
                events: Vec::new(),
                receipt: None,
            });
        if entry.step.is_none() {
            entry.step = event
                .payload
                .get("step")
                .and_then(Value::as_u64)
                .map(|n| n as u32);
        }
        if let Some(status) = event.payload.get("status").and_then(Value::as_str) {
            entry.status = Some(status.to_string());
        }
        entry.events.push(GraphExecutionEventTrace {
            event_type: event.event_type.clone(),
            thread_seq: event.thread_seq,
            payload: event.payload.clone(),
        });
    }

    for artifact in artifacts {
        if artifact.artifact_type != "graph_node_receipt" {
            continue;
        }
        let Some(metadata) = artifact.metadata.as_ref() else {
            continue;
        };

        capture_graph_identity(
            metadata,
            &mut definition_ref,
            &mut effective_definition_digest,
            &mut graph_run_id,
        )?;

        let Some((node_ref, node)) =
            node_identity_from_payload(metadata, definition_ref.as_deref())
        else {
            continue;
        };
        let step = metadata
            .get("step")
            .and_then(Value::as_u64)
            .map(|n| n as u32);
        let error = metadata
            .get("error")
            .and_then(Value::as_str)
            .map(String::from);
        let node_result_hash = metadata
            .get("node_result_hash")
            .and_then(Value::as_str)
            .map(String::from);

        let entry = nodes
            .entry(node_ref.clone())
            .or_insert_with(|| NodeAccumulator {
                node_ref,
                node,
                step,
                status: None,
                events: Vec::new(),
                receipt: None,
            });
        if entry.step.is_none() {
            entry.step = step;
        }
        if error.is_some() {
            entry.status = Some("error".to_string());
        }
        entry.receipt = Some(GraphNodeReceiptTrace {
            artifact_id: artifact.artifact_id,
            uri: artifact.uri.clone(),
            step,
            node_result_hash,
            error,
            payload: metadata.clone(),
        });
    }

    Ok(GraphExecutionTrace {
        thread_id,
        definition_ref,
        effective_definition_digest,
        graph_run_id,
        nodes: nodes.into_values().map(NodeAccumulator::finish).collect(),
    })
}

impl NodeAccumulator {
    fn finish(self) -> GraphExecutionNodeTrace {
        GraphExecutionNodeTrace {
            node_ref: self.node_ref,
            node: self.node,
            step: self.step,
            status: self.status,
            events: self.events,
            receipt: self.receipt,
        }
    }
}

fn capture_graph_identity(
    payload: &Value,
    definition_ref: &mut Option<String>,
    effective_definition_digest: &mut Option<String>,
    graph_run_id: &mut Option<String>,
) -> anyhow::Result<()> {
    capture_exact(
        definition_ref,
        payload.get("definition_ref"),
        "definition_ref",
    )?;
    capture_exact(
        effective_definition_digest,
        payload.get("effective_definition_digest"),
        "effective_definition_digest",
    )?;
    capture_exact(graph_run_id, payload.get("graph_run_id"), "graph_run_id")?;
    Ok(())
}

fn capture_exact(
    slot: &mut Option<String>,
    value: Option<&Value>,
    field: &str,
) -> anyhow::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("graph execution `{field}` must be a string"))?;
    match field {
        "definition_ref" => {
            let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(value)
                .map_err(|error| anyhow::anyhow!("invalid graph definition_ref: {error}"))?;
            if canonical.kind != "graph" {
                anyhow::bail!("graph execution definition_ref must identify a graph");
            }
        }
        "effective_definition_digest" => {
            ryeos_engine::resolution::EffectiveDefinitionDigest::parse(value.to_string())
                .map_err(|error| anyhow::anyhow!("invalid graph effective digest: {error}"))?;
        }
        "graph_run_id" if value.is_empty() || value.trim() != value || value.len() > 256 => {
            anyhow::bail!("graph execution graph_run_id must be non-empty, trimmed, and bounded");
        }
        _ => {}
    }
    match slot {
        Some(existing) if existing != value => anyhow::bail!(
            "graph execution identity divergence for `{field}`: `{existing}` != `{value}`"
        ),
        Some(_) => {}
        None => *slot = Some(value.to_string()),
    }
    Ok(())
}

fn node_identity_from_payload(
    payload: &Value,
    definition_ref: Option<&str>,
) -> Option<(String, String)> {
    let node = payload.get("node")?.as_str()?.to_string();
    let node_ref = payload
        .get("node_ref")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| definition_ref.map(|def| format!("{def}#node:{node}")))?;
    Some((node_ref, node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(thread_seq: i64, event_type: &str, payload: Value) -> PersistedEventRecord {
        PersistedEventRecord {
            event_id: thread_seq,
            event_hash: None,
            chain_root_id: "T-test".to_string(),
            chain_seq: thread_seq,
            thread_id: "T-test".to_string(),
            thread_seq,
            event_type: event_type.to_string(),
            storage_class: "durable".to_string(),
            ts: "2026-06-08T00:00:00Z".to_string(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload,
        }
    }

    fn receipt(artifact_id: i64, metadata: Value) -> ThreadArtifactRecord {
        ThreadArtifactRecord {
            artifact_id,
            artifact_type: "graph_node_receipt".to_string(),
            uri: format!("graph://runs/gr-1/node-receipts/{artifact_id}"),
            content_hash: None,
            metadata: Some(metadata),
        }
    }

    #[test]
    fn groups_events_and_receipts_by_node_ref() {
        let events = vec![
            event(
                1,
                "graph_started",
                json!({
                    "definition_ref": "graph:flow",
                    "effective_definition_digest": "d".repeat(64),
                    "graph_run_id": "gr-1",
                }),
            ),
            event(
                2,
                "graph_step_started",
                json!({
                    "definition_ref": "graph:flow",
                    "effective_definition_digest": "d".repeat(64),
                    "graph_run_id": "gr-1",
                    "node": "greet",
                    "node_ref": "graph:flow#node:greet",
                    "step": 0,
                }),
            ),
            event(
                3,
                "tool_call_result",
                json!({
                    "definition_ref": "graph:flow",
                    "effective_definition_digest": "d".repeat(64),
                    "graph_run_id": "gr-1",
                    "node": "greet",
                    "node_ref": "graph:flow#node:greet",
                    "step": 0,
                    "status": "ok",
                }),
            ),
        ];
        let artifacts = vec![receipt(
            10,
            json!({
                "definition_ref": "graph:flow",
                "effective_definition_digest": "d".repeat(64),
                "graph_run_id": "gr-1",
                "node": "greet",
                "step": 0,
                "node_result_hash": "resulthash",
            }),
        )];

        let trace = build_graph_execution_trace("T-test", &events, &artifacts).unwrap();

        assert_eq!(trace.definition_ref.as_deref(), Some("graph:flow"));
        assert_eq!(
            trace.effective_definition_digest.as_deref(),
            Some("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd")
        );
        assert_eq!(trace.graph_run_id.as_deref(), Some("gr-1"));
        assert_eq!(trace.nodes.len(), 1);
        let node = &trace.nodes[0];
        assert_eq!(node.node_ref, "graph:flow#node:greet");
        assert_eq!(node.node, "greet");
        assert_eq!(node.step, Some(0));
        assert_eq!(node.status.as_deref(), Some("ok"));
        assert_eq!(node.events.len(), 2);
        assert_eq!(
            node.receipt
                .as_ref()
                .and_then(|r| r.node_result_hash.as_deref()),
            Some("resulthash")
        );
    }

    #[test]
    fn error_receipt_marks_node_error_without_result_hash() {
        let artifacts = vec![receipt(
            1,
            json!({
                "definition_ref": "graph:denied",
                "effective_definition_digest": "e".repeat(64),
                "graph_run_id": "gr-denied",
                "node": "greet",
                "step": 0,
                "node_result_hash": null,
                "error": "dispatch failed",
            }),
        )];

        let trace = build_graph_execution_trace("T-denied", &[], &artifacts).unwrap();

        assert_eq!(trace.definition_ref.as_deref(), Some("graph:denied"));
        assert_eq!(trace.nodes.len(), 1);
        let node = &trace.nodes[0];
        assert_eq!(node.node_ref, "graph:denied#node:greet");
        assert_eq!(node.status.as_deref(), Some("error"));
        let receipt = node.receipt.as_ref().expect("receipt");
        assert_eq!(receipt.node_result_hash, None);
        assert_eq!(receipt.error.as_deref(), Some("dispatch failed"));
    }

    #[test]
    fn ignores_non_graph_receipt_artifacts() {
        let artifacts = vec![ThreadArtifactRecord {
            artifact_id: 1,
            artifact_type: "graph_transcript".to_string(),
            uri: "graph://flow/runs/gr-1".to_string(),
            content_hash: None,
            metadata: None,
        }];

        let trace = build_graph_execution_trace("T-test", &[], &artifacts).unwrap();

        assert!(trace.nodes.is_empty());
        assert_eq!(trace.definition_ref, None);
    }

    #[test]
    fn refuses_effective_definition_digest_divergence() {
        let events = vec![
            event(
                1,
                "graph_started",
                json!({
                    "definition_ref": "graph:flow",
                    "effective_definition_digest": "a".repeat(64),
                    "graph_run_id": "gr-1",
                }),
            ),
            event(
                2,
                "graph_step_started",
                json!({
                    "definition_ref": "graph:flow",
                    "effective_definition_digest": "b".repeat(64),
                    "graph_run_id": "gr-1",
                    "node": "greet",
                    "step": 0,
                }),
            ),
        ];

        let error = build_graph_execution_trace("T-test", &events, &[]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("identity divergence for `effective_definition_digest`")
        );
    }
}
