use serde_json::json;

use super::Walker;
use crate::model::NodeReceipt;
use crate::persistence;
use ryeos_runtime::events::RuntimeEventType;

pub(super) fn node_ref(definition_ref: &str, node: &str) -> String {
    format!("{definition_ref}#node:{node}")
}

fn graph_call_id(graph_run_id: &str, step: u32, node: &str) -> String {
    format!("{graph_run_id}:{step}:{node}")
}

impl Walker {
    pub(super) async fn write_node_receipt_or_warn(
        &self,
        graph_run_id: &str,
        receipt: &NodeReceipt,
    ) {
        let r = persistence::write_node_receipt(&self.client, graph_run_id, receipt).await;
        self.record_callback_warning("write_node_receipt", r.map(|_| ()))
    }

    pub(super) async fn emit_graph_step_started(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
    ) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::GraphStepStarted,
                json!({
                    "graph_run_id": graph_run_id,
                    "definition_ref": &self.graph.definition_ref,
                    "definition_hash": &self.graph.definition_hash,
                    "node": current,
                    "node_ref": node_ref(&self.graph.definition_ref, current),
                    "step": step,
                }),
            )
            .await;
        self.record_callback_warning("graph_step_started", r);
    }

    pub(super) async fn emit_graph_follow_suspended(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        item_id: &str,
        expected: Option<usize>,
    ) {
        let mut payload = json!({
            "graph_run_id": graph_run_id,
            "definition_ref": &self.graph.definition_ref,
            "definition_hash": &self.graph.definition_hash,
            "node": current,
            "node_ref": node_ref(&self.graph.definition_ref, current),
            "step": step,
            "item_id": item_id,
        });
        if let Some(expected) = expected {
            payload["expected"] = json!(expected);
        }
        let r = self
            .client
            .append_runtime_event(RuntimeEventType::GraphFollowSuspended, payload)
            .await;
        self.record_callback_warning("graph_follow_suspended", r);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn emit_graph_node_retry(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        item_id: &str,
        failed_attempt: u32,
        total_attempts: u32,
        delay_ms: u64,
        error: &str,
    ) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::GraphNodeRetry,
                json!({
                    "graph_run_id": graph_run_id,
                    "definition_ref": &self.graph.definition_ref,
                    "definition_hash": &self.graph.definition_hash,
                    "node": current,
                    "node_ref": node_ref(&self.graph.definition_ref, current),
                    "step": step,
                    "item_id": item_id,
                    "attempt": failed_attempt,
                    "attempts": total_attempts,
                    "delay_ms": delay_ms,
                    "error": error,
                }),
            )
            .await;
        self.record_callback_warning("graph_node_retry", r);
    }

    pub(super) async fn emit_tool_call_start(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        item_id: &str,
    ) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::ToolCallStart,
                json!({
                    "tool": item_id,
                    "call_id": graph_call_id(graph_run_id, step, current),
                    "graph_run_id": graph_run_id,
                    "definition_ref": &self.graph.definition_ref,
                    "definition_hash": &self.graph.definition_hash,
                    "node": current,
                    "node_ref": node_ref(&self.graph.definition_ref, current),
                    "step": step,
                    "item_id": item_id,
                }),
            )
            .await;
        self.record_callback_warning("tool_call_start", r);
    }

    pub(super) async fn emit_tool_call_result(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        item_id: &str,
        status: &str,
    ) {
        let r = self
            .client
            .append_runtime_event(
                RuntimeEventType::ToolCallResult,
                json!({
                    "tool": item_id,
                    "call_id": graph_call_id(graph_run_id, step, current),
                    "graph_run_id": graph_run_id,
                    "definition_ref": &self.graph.definition_ref,
                    "definition_hash": &self.graph.definition_hash,
                    "node": current,
                    "node_ref": node_ref(&self.graph.definition_ref, current),
                    "step": step,
                    "item_id": item_id,
                    "status": status,
                }),
            )
            .await;
        self.record_callback_warning("tool_call_result", r);
    }

    pub(super) async fn emit_graph_step_completed(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        status: &str,
        error: Option<&str>,
    ) {
        let mut payload = json!({
            "graph_run_id": graph_run_id,
            "definition_ref": &self.graph.definition_ref,
            "definition_hash": &self.graph.definition_hash,
            "node": current,
            "node_ref": node_ref(&self.graph.definition_ref, current),
            "step": step,
            "status": status,
        });
        if let Some(err) = error {
            payload["error"] = json!(err);
        }
        let r = self
            .client
            .append_runtime_event(RuntimeEventType::GraphStepCompleted, payload)
            .await;
        self.record_callback_warning("graph_step_completed", r);
    }

    pub(super) async fn emit_graph_branch_taken(
        &self,
        graph_run_id: &str,
        step: u32,
        current: &str,
        target: Option<&str>,
    ) {
        if let Some(t) = target {
            let r = self
                .client
                .append_runtime_event(
                    RuntimeEventType::GraphBranchTaken,
                    json!({
                        "graph_run_id": graph_run_id,
                        "definition_ref": &self.graph.definition_ref,
                        "definition_hash": &self.graph.definition_hash,
                        "node": current,
                        "node_ref": node_ref(&self.graph.definition_ref, current),
                        "step": step,
                        "target": t,
                        "target_node_ref": node_ref(&self.graph.definition_ref, t),
                    }),
                )
                .await;
            self.record_callback_warning("graph_branch_taken", r);
        }
    }
}
