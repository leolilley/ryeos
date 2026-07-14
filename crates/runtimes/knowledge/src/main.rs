//! Knowledge runtime — receives a MethodCallEnvelope on stdin, dispatches
//! the requested method, and writes a MethodCallResult to stdout.
//!
//! Spawned exclusively by `ryeosd` via `lillux::run`. Single mode:
//! always a thread, always attaches and marks running through CallbackClient.
//! The daemon validates terminal stdout and owns terminal state publication.

mod budget;
mod compose;
mod dispatch;
mod frontmatter;
mod graph;
mod ordering;
mod query;
mod render;
mod token_estimation;
mod types;
mod validate;

use std::io::Read;

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::method_wire::{
    MethodCallEnvelope, MethodCallError, MethodCallResult, METHOD_CALL_SCHEMA_VERSION,
};

use types::KnowledgeError;

/// Dispatch the envelope's method against the handler table, keyed strictly
/// on `envelope.method`. The method's typed payload is parsed inside the
/// handler; a non-object or wrong-shaped payload surfaces as `InvalidArg`.
fn dispatch_method(envelope: &MethodCallEnvelope) -> MethodCallResult {
    match dispatch::dispatch(
        &envelope.method,
        envelope.payload.clone(),
        &envelope.runtime_config,
    ) {
        Ok(value) => MethodCallResult::success(envelope, value),
        Err(e) => MethodCallResult::failure(envelope, knowledge_to_batch_error(e)),
    }
}

fn parse_method_call_envelope(input: &[u8]) -> anyhow::Result<MethodCallEnvelope> {
    let envelope: MethodCallEnvelope = serde_json::from_slice(input)
        .map_err(|e| anyhow::anyhow!("invalid MethodCallEnvelope: {e}"))?;
    envelope
        .validate_schema_version()
        .map_err(anyhow::Error::msg)?;
    Ok(envelope)
}

fn main() -> anyhow::Result<()> {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_cli_tool());

    let mut stdin_data = Vec::new();
    std::io::stdin().read_to_end(&mut stdin_data)?;
    if stdin_data.is_empty() {
        eprintln!("ryeos-knowledge-runtime: empty stdin; MethodCallEnvelope required");
        std::process::exit(1);
    }

    let envelope = parse_method_call_envelope(&stdin_data)?;

    tracing::info!(
        kind = %envelope.kind,
        method = %envelope.method,
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

async fn run_thread(envelope: &MethodCallEnvelope) -> MethodCallResult {
    let thread_auth_token = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .expect("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon");
    let client = CallbackClient::new(
        &envelope.callback,
        &envelope.thread_id,
        envelope.callback_project_path.to_str().unwrap_or(""),
        &thread_auth_token,
    );

    // Register this process's pgid before marking running so the daemon can
    // tell a live runtime from a crashed one on restart (else it resumes a
    // duplicate). Resume-critical.
    if let Err(e) = client.attach_current_process().await {
        return MethodCallResult::failure(
            envelope,
            MethodCallError::MethodFailed {
                reason: format!("attach_process failed: {e}"),
            },
        );
    }

    if let Err(e) = client.mark_running().await {
        return MethodCallResult::failure(
            envelope,
            MethodCallError::MethodFailed {
                reason: format!("mark_running failed: {e}"),
            },
        );
    }

    // Library dispatch is sync; offload to a blocking task.
    let envelope_owned = envelope.clone();
    let thread_id = envelope.thread_id.clone();
    let kind = envelope.kind.clone();
    let method = envelope.method.clone();
    let result = tokio::task::spawn_blocking(move || dispatch_method(&envelope_owned))
        .await
        .unwrap_or_else(|e| {
            MethodCallResult::failure(
                &MethodCallEnvelope {
                    schema_version: METHOD_CALL_SCHEMA_VERSION,
                    kind,
                    method,
                    thread_id,
                    callback: envelope.callback.clone(),
                    callback_project_path: envelope.callback_project_path.clone(),
                    project_root: envelope.project_root.clone(),
                    runtime_config: envelope.runtime_config.clone(),
                    payload: serde_json::Value::Null,
                },
                MethodCallError::MethodFailed {
                    reason: format!("dispatch panicked: {e}"),
                },
            )
        });

    // Do not publish terminal state here. The daemon must first validate the
    // process outcome, method-result wire semantics, kind/method echo, and any
    // launch-augmentation projection derived from the output.
    result
}

// Method-dispatch behavior (including the `args.roots` regression and the
// envelope-method-vs-payload-method override) is covered by `dispatch`'s
// own test module, now that dispatch keys directly off `envelope.method`.

/// Map a `KnowledgeError` into a structured `MethodCallError`,
/// preserving the variant taxonomy where it overlaps.
fn knowledge_to_batch_error(err: KnowledgeError) -> MethodCallError {
    match err {
        KnowledgeError::InvalidArg { method, reason } => MethodCallError::InvalidArg {
            method,
            field: None,
            reason,
        },
        other => MethodCallError::MethodFailed {
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(method: &str, payload: serde_json::Value) -> MethodCallEnvelope {
        MethodCallEnvelope {
            schema_version: METHOD_CALL_SCHEMA_VERSION,
            kind: "knowledge".into(),
            method: method.into(),
            thread_id: "T-test".into(),
            callback: ryeos_runtime::envelope::EnvelopeCallback {
                socket_path: std::path::PathBuf::from("/tmp/cb.sock"),
                token: "tat-test".into(),
            },
            callback_project_path: std::path::PathBuf::from("/tmp/proj-state"),
            project_root: std::path::PathBuf::from("/tmp/proj"),
            runtime_config: std::collections::BTreeMap::new(),
            payload,
        }
    }

    #[test]
    fn error_mapping_preserves_taxonomy() {
        assert!(matches!(
            knowledge_to_batch_error(KnowledgeError::InvalidArg {
                method: "query".into(),
                reason: "x".into()
            }),
            MethodCallError::InvalidArg { field: None, .. }
        ));
        // Variants without a dedicated wire mapping collapse to MethodFailed.
        assert!(matches!(
            knowledge_to_batch_error(KnowledgeError::Internal("boom".into())),
            MethodCallError::MethodFailed { .. }
        ));
    }

    #[test]
    fn rejects_unknown_method_envelope_schema_version() {
        let mut envelope = envelope("compose", serde_json::json!({}));
        envelope.schema_version = METHOD_CALL_SCHEMA_VERSION + 1;
        let bytes = serde_json::to_vec(&envelope).unwrap();
        assert!(parse_method_call_envelope(&bytes).is_err());
    }

    #[test]
    fn dispatch_method_maps_method_error_to_failure_result() {
        // An undeclared method surfaces as a structured failure result, not a
        // panic or a success — exercises MethodCallEnvelope -> MethodCallResult.
        let result = dispatch_method(&envelope("bogus", serde_json::json!({})));
        assert!(!result.success);
        assert!(matches!(
            result.error,
            Some(MethodCallError::InvalidArg { .. })
        ));
    }

    #[test]
    fn dispatch_method_success_result() {
        let result = dispatch_method(&envelope(
            "graph",
            serde_json::json!({"items_by_ref": {}, "edges": [], "args": {}}),
        ));
        assert!(result.success, "error: {:?}", result.error);
        assert!(result.output.is_some());
    }
}
