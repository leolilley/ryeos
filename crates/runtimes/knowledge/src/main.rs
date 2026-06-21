//! Knowledge runtime — receives a BatchOpEnvelope on stdin, dispatches
//! the requested op, and writes a BatchOpResult to stdout.
//!
//! Spawned exclusively by `ryeosd` via `lillux::run`. Single mode:
//! always a thread, always wires CallbackClient lifecycle.

mod budget;
mod compose;
mod dispatch;
mod frontmatter;
mod graph;
mod ordering;
mod query;
mod render;
mod types;
mod validate;

use std::io::Read;

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::op_wire::{BatchOpEnvelope, BatchOpError, BatchOpResult};

use types::KnowledgeError;

/// Dispatch the envelope's op against the handler table, keyed strictly on
/// `envelope.op`. The op's typed payload is parsed inside the handler; a
/// non-object or wrong-shaped payload surfaces as `InvalidInput`.
fn dispatch_op(envelope: &BatchOpEnvelope) -> BatchOpResult {
    match dispatch::dispatch(&envelope.op, envelope.payload.clone()) {
        Ok(value) => BatchOpResult::success(envelope, value),
        Err(e) => BatchOpResult::failure(envelope, knowledge_to_batch_error(e)),
    }
}

fn main() -> anyhow::Result<()> {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_cli_tool());

    let mut stdin_data = Vec::new();
    std::io::stdin().read_to_end(&mut stdin_data)?;
    if stdin_data.is_empty() {
        eprintln!("ryeos-knowledge-runtime: empty stdin; BatchOpEnvelope required");
        std::process::exit(1);
    }

    let envelope: BatchOpEnvelope = serde_json::from_slice(&stdin_data)
        .map_err(|e| anyhow::anyhow!("invalid BatchOpEnvelope: {e}"))?;

    tracing::info!(
        kind = %envelope.kind,
        op = %envelope.op,
        thread_id = %envelope.thread_id,
        "knowledge runtime launch",
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(run_thread(&envelope));

    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

async fn run_thread(envelope: &BatchOpEnvelope) -> BatchOpResult {
    let thread_auth_token = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .expect("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon");
    let client = CallbackClient::new(
        &envelope.callback,
        &envelope.thread_id,
        envelope.project_root.to_str().unwrap_or(""),
        &thread_auth_token,
    );

    if let Err(e) = client.mark_running().await {
        return BatchOpResult::failure(
            envelope,
            BatchOpError::OpFailed {
                reason: format!("mark_running failed: {e}"),
            },
        );
    }

    // Library dispatch is sync; offload to a blocking task.
    let envelope_owned = envelope.clone();
    let thread_id = envelope.thread_id.clone();
    let kind = envelope.kind.clone();
    let op = envelope.op.clone();
    let result = tokio::task::spawn_blocking(move || dispatch_op(&envelope_owned))
        .await
        .unwrap_or_else(|e| {
            BatchOpResult::failure(
                &BatchOpEnvelope {
                    schema_version: 1,
                    kind,
                    op,
                    thread_id,
                    callback: envelope.callback.clone(),
                    project_root: envelope.project_root.clone(),
                    payload: serde_json::Value::Null,
                },
                BatchOpError::OpFailed {
                    reason: format!("dispatch panicked: {e}"),
                },
            )
        });

    let completion = if result.success {
        ryeos_runtime::TerminalCompletion {
            status: "completed".to_string(),
            outcome_code: Some("success".to_string()),
            result: result.output.clone(),
            error: None,
            cost: None,
        }
    } else {
        ryeos_runtime::TerminalCompletion {
            status: "failed".to_string(),
            outcome_code: Some("failed".to_string()),
            result: None,
            error: result
                .error
                .as_ref()
                .and_then(|e| serde_json::to_value(e).ok()),
            cost: None,
        }
    };
    if let Err(e) = client.finalize_thread(completion).await {
        tracing::error!(error = %e, "finalize_thread failed");
    }

    result
}

// Op-dispatch behavior (including the `inputs.roots` regression and the
// envelope-op-vs-payload-operation override) is covered by `dispatch`'s
// own test module, now that dispatch keys directly off `envelope.op`.

/// Map a `KnowledgeError` into a structured `BatchOpError`,
/// preserving the variant taxonomy where it overlaps.
fn knowledge_to_batch_error(err: KnowledgeError) -> BatchOpError {
    match err {
        KnowledgeError::InvalidInput { op, reason } => BatchOpError::InvalidInput {
            op,
            field: None,
            reason,
        },
        other => BatchOpError::OpFailed {
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(op: &str, payload: serde_json::Value) -> BatchOpEnvelope {
        BatchOpEnvelope {
            schema_version: 1,
            kind: "knowledge".into(),
            op: op.into(),
            thread_id: "T-test".into(),
            callback: ryeos_runtime::envelope::EnvelopeCallback {
                socket_path: std::path::PathBuf::from("/tmp/cb.sock"),
                token: "tat-test".into(),
            },
            project_root: std::path::PathBuf::from("/tmp/proj"),
            payload,
        }
    }

    #[test]
    fn error_mapping_preserves_taxonomy() {
        assert!(matches!(
            knowledge_to_batch_error(KnowledgeError::InvalidInput { op: "query".into(), reason: "x".into() }),
            BatchOpError::InvalidInput { field: None, .. }
        ));
        // Variants without a dedicated wire mapping collapse to OpFailed.
        assert!(matches!(
            knowledge_to_batch_error(KnowledgeError::Internal("boom".into())),
            BatchOpError::OpFailed { .. }
        ));
    }

    #[test]
    fn dispatch_op_maps_op_error_to_failure_result() {
        // An undeclared op surfaces as a structured failure result, not a
        // panic or a success — exercises BatchOpEnvelope -> BatchOpResult.
        let result = dispatch_op(&envelope("bogus", serde_json::json!({})));
        assert!(!result.success);
        assert!(matches!(result.error, Some(BatchOpError::InvalidInput { .. })));
    }

    #[test]
    fn dispatch_op_success_result() {
        let result = dispatch_op(&envelope(
            "graph",
            serde_json::json!({"items_by_ref": {}, "edges": [], "inputs": {}}),
        ));
        assert!(result.success, "error: {:?}", result.error);
        assert!(result.output.is_some());
    }
}
