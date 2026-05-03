//! Knowledge runtime — receives a BatchOpEnvelope on stdin, dispatches
//! the requested op, and writes a BatchOpResult to stdout.
//!
//! Spawned exclusively by `ryeosd` via `lillux::run`. Single mode:
//! always a thread, always wires CallbackClient lifecycle.

mod budget;
mod compose;
mod dispatch;
mod frontmatter;
mod ordering;
mod render;
mod types;

use std::io::Read;

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::op_wire::{BatchOpEnvelope, BatchOpError, BatchOpResult};

use types::{KnowledgeError, KnowledgeRequest};

fn parse_request(envelope: &BatchOpEnvelope) -> Result<KnowledgeRequest, KnowledgeError> {
    let mut tagged = serde_json::Map::new();
    tagged.insert("operation".into(), serde_json::Value::String(envelope.op.clone()));
    if let Some(obj) = envelope.payload.as_object() {
        for (k, v) in obj {
            tagged.insert(k.clone(), v.clone());
        }
    } else {
        return Err(KnowledgeError::MalformedEnvelope(
            "BatchOpEnvelope.payload must be an object".into(),
        ));
    }
    serde_json::from_value(serde_json::Value::Object(tagged)).map_err(|e| {
        KnowledgeError::InvalidInput {
            op: envelope.op.clone(),
            reason: e.to_string(),
        }
    })
}

fn dispatch_op(envelope: &BatchOpEnvelope) -> BatchOpResult {
    match parse_request(envelope).and_then(|req| dispatch::dispatch(&req)) {
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

    if let Err(e) = client.finalize_thread("completed").await {
        tracing::error!(error = %e, "finalize_thread failed");
    }

    result
}

/// Map a `KnowledgeError` into a structured `BatchOpError`,
/// preserving the variant taxonomy where it overlaps.
fn knowledge_to_batch_error(err: KnowledgeError) -> BatchOpError {
    match err {
        KnowledgeError::NotImplemented { op, phase } => BatchOpError::NotImplemented { op, phase },
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
