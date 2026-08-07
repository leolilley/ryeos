//! Large-content ingest: stream operator-named bytes into the node's
//! large-object store through the daemon callback, and print the pin.
//!
//! The heavy lifting — streaming chunk-verified ingest, manifest
//! publication, authority checks — is all daemon-side; this action is the
//! thin thread-scoped envelope every state mutation uses.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentIngestParams {
    /// File or shard directory to ingest. Relativity is resolved against the
    /// caller's working directory before the daemon sees it.
    pub source_path: PathBuf,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub project_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ContentScrubParams {
    #[serde(default)]
    pub project_path: Option<PathBuf>,
}

pub fn run_content_scrub(_params: ContentScrubParams) -> Result<Value> {
    let thread_id = std::env::var("RYEOSD_THREAD_ID").context(
        "RYEOSD_THREAD_ID is not set — large-content scrub requires a daemon-dispatched thread",
    )?;
    let client = ryeos_runtime::callback_uds::UdsRuntimeClient::from_env()
        .map_err(|error| anyhow::anyhow!("cannot build runtime callback client: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build scrub callback runtime")?;
    use ryeos_runtime::callback::RuntimeCallbackAPI;
    runtime
        .block_on(client.scrub_large_content(&thread_id, serde_json::json!({})))
        .map_err(|error| anyhow::anyhow!("runtime.scrub_large_content failed: {error}"))
}

pub fn run_content_ingest(params: ContentIngestParams) -> Result<Value> {
    let source_path = if params.source_path.is_absolute() {
        params.source_path.clone()
    } else {
        std::env::current_dir()
            .context("resolving caller working directory")?
            .join(&params.source_path)
    };
    let thread_id = std::env::var("RYEOSD_THREAD_ID").context(
        "RYEOSD_THREAD_ID is not set — large-content ingest requires a daemon-dispatched thread",
    )?;
    let client = ryeos_runtime::callback_uds::UdsRuntimeClient::from_env()
        .map_err(|error| anyhow::anyhow!("cannot build runtime callback client: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build ingest callback runtime")?;
    use ryeos_runtime::callback::RuntimeCallbackAPI;
    runtime
        .block_on(client.ingest_large_content(
            &thread_id,
            serde_json::json!({
                "source_path": source_path,
                "expected_sha256": params.expected_sha256,
            }),
        ))
        .map_err(|error| anyhow::anyhow!("runtime.ingest_large_content failed: {error}"))
}
