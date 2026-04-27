//! Minimal native knowledge runtime stub.
//!
//! Reads a `LaunchEnvelope` from stdin, strips YAML frontmatter from the
//! daemon-verified `resolution.root.raw_content`, and returns the body
//! as the `RuntimeResult.result`. Mirrors what the legacy Python
//! `knowledge.py` executor does today; the future graph-aware composer
//! described in `docs/future/knowledge-runtime.md` will replace this
//! body without altering the ABI.

use std::io::Read;

use serde_json::json;

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::{LaunchEnvelope, RuntimeResult};

/// Record a callback failure as a non-fatal warning rather than
/// silently dropping it (mirrors `ryeos-directive-runtime`'s
/// `record_callback_warning`).
fn record_callback_warning(
    warnings: &mut Vec<String>,
    label: &str,
    result: anyhow::Result<()>,
) {
    if let Err(e) = result {
        warnings.push(format!("callback {label} failed: {e}"));
    }
}

/// Strip a leading YAML frontmatter block (delimited by lines that
/// contain only `---`) from `body`. If no frontmatter is present, the
/// input is returned unchanged. The leading newline after the closing
/// delimiter is consumed so the returned body starts at content.
fn strip_frontmatter(body: &str) -> &str {
    let rest = match body.strip_prefix("---\n") {
        Some(r) => r,
        None => match body.strip_prefix("---\r\n") {
            Some(r) => r,
            None => return body,
        },
    };
    // Find the closing `---` line.
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return &rest[offset + line.len()..];
        }
        offset += line.len();
    }
    // Unterminated frontmatter — return original body unchanged.
    body
}

fn main() -> anyhow::Result<()> {
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_cli_tool());

    let mut stdin_data = Vec::new();
    std::io::stdin().read_to_end(&mut stdin_data)?;
    if stdin_data.is_empty() {
        eprintln!("ryeos-knowledge-runtime: empty stdin; envelope required");
        std::process::exit(1);
    }

    let envelope: LaunchEnvelope = serde_json::from_slice(&stdin_data)
        .map_err(|e| anyhow::anyhow!("invalid envelope: {e}"))?;

    let thread_id = envelope.thread_id.clone();
    let project_root = envelope.roots.project_root.clone();

    tracing::info!(
        thread_id = %thread_id,
        invocation_id = %envelope.invocation_id,
        item_id = %envelope.resolution.root.requested_id,
        "knowledge runtime launch"
    );

    let callback = CallbackClient::new(
        &envelope.callback,
        &thread_id,
        project_root.to_str().unwrap_or(""),
    );

    let body = strip_frontmatter(&envelope.resolution.root.raw_content).to_string();

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async move {
        let mut warnings: Vec<String> = Vec::new();
        record_callback_warning(&mut warnings, "mark_running", callback.mark_running().await);
        record_callback_warning(
            &mut warnings,
            "finalize_thread(completed)",
            callback.finalize_thread("completed").await,
        );
        RuntimeResult {
            success: true,
            status: "completed".to_string(),
            thread_id: thread_id.clone(),
            result: Some(json!(body)),
            outputs: json!({}),
            cost: None,
            warnings,
        }
    });

    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
