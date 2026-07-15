//! Project snapshot terminal client.
//!
//! All authoritative reads, CAS writes, trust decisions, and HEAD publication
//! happen in the daemon. This module only sends a two-proof runtime callback.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotStatusParams {
    #[serde(alias = "project")]
    pub project_path: PathBuf,
    #[serde(default)]
    pub include_unchanged: bool,
    #[serde(default)]
    pub time_budget_ms: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotLogParams {
    #[serde(alias = "project")]
    pub project_path: PathBuf,
    #[serde(default = "default_limit", deserialize_with = "deserialize_limit")]
    pub limit: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCreateParams {
    #[serde(alias = "project")]
    pub project_path: PathBuf,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub allow_empty: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotShowParams {
    pub snapshot_hash: String,
    #[serde(default, alias = "project")]
    pub project_path: Option<PathBuf>,
}

pub fn run_status(params: SnapshotStatusParams) -> Result<Value> {
    invoke("status", serde_json::to_value(params)?)
}

pub fn run_log(params: SnapshotLogParams) -> Result<Value> {
    invoke("log", serde_json::to_value(params)?)
}

pub fn run_create(params: SnapshotCreateParams) -> Result<Value> {
    invoke("create", serde_json::to_value(params)?)
}

pub fn run_show(params: SnapshotShowParams) -> Result<Value> {
    invoke("show", serde_json::to_value(params)?)
}

fn invoke(operation: &str, params: Value) -> Result<Value> {
    let thread_id = std::env::var("RYEOSD_THREAD_ID").context(
        "RYEOSD_THREAD_ID is not set — snapshot operations require a daemon-dispatched thread",
    )?;
    let client = ryeos_runtime::callback_uds::UdsRuntimeClient::from_env()
        .map_err(|error| anyhow::anyhow!("cannot build runtime callback client: {error}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build snapshot callback runtime")?;
    use ryeos_runtime::callback::RuntimeCallbackAPI;
    runtime
        .block_on(client.project_snapshot(
            &thread_id,
            serde_json::json!({"operation": operation, "params": params}),
        ))
        .map_err(|error| anyhow::anyhow!("runtime.project_snapshot failed: {error}"))
}

fn default_limit() -> usize {
    20
}

fn deserialize_limit<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Number(number) => number
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .ok_or_else(|| serde::de::Error::custom("limit must be a non-negative integer")),
        Value::String(value) => value
            .parse::<usize>()
            .map_err(|_| serde::de::Error::custom("limit must be a non-negative integer")),
        other => Err(serde::de::Error::custom(format!(
            "limit must be an integer or integer string, got {other}"
        ))),
    }
}
