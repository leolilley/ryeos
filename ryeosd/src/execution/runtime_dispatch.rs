use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct DispatchActionParams {
    thread_id: String,
    project_path: String,
    action: ActionPayload,
}

#[derive(Debug, Deserialize)]
struct ActionPayload {
    primary: String,
    item_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    kind: Option<String>,
    #[serde(default)]
    params: Value,
    #[serde(default = "default_thread")]
    thread: String,
}

fn default_thread() -> String {
    "inline".to_string()
}

pub fn handle(params: &Value, state: &AppState) -> Result<Value> {
    let params: DispatchActionParams =
        serde_json::from_value(params.clone()).context("invalid runtime.dispatch_action params")?;

    match params.action.primary.as_str() {
        "execute" => handle_execute(params, state),
        other => anyhow::bail!("unsupported action primary: {other}"),
    }
}

fn handle_execute(params: DispatchActionParams, state: &AppState) -> Result<Value> {
    let site_id = state.threads.site_id();
    let project_path =
        crate::execution::project_source::normalize_project_path(&params.project_path);

    let caller_principal_id = state
        .threads
        .get_thread(&params.thread_id)
        .ok()
        .flatten()
        .and_then(|t| t.requested_by)
        .unwrap_or_else(|| state.identity.principal_id());

    let caller_scopes = vec!["execute".to_string()];

    let resolved = crate::services::thread_lifecycle::resolve_root_execution(
        &state.engine,
        site_id,
        &project_path,
        &params.action.item_id,
        &params.action.thread,
        params.action.params.clone(),
        Some(caller_principal_id.clone()),
        caller_scopes,
        false,
    )?;

    let required_secrets = &resolved.resolved_item.metadata.required_secrets;
    let vault_bindings = if !required_secrets.is_empty() {
        state
            .vault_store()
            .resolve_vault_env(&caller_principal_id, required_secrets)?
    } else {
        HashMap::new()
    };

    let exec_params = crate::execution::runner::ExecutionParams {
        resolved,
        acting_principal: caller_principal_id,
        project_path: project_path.clone(),
        vault_bindings,
        snapshot_hash: None,
        item_ref: params.action.item_id,
        parameters: params.action.params,
        temp_dir: None,
    };

    let rt = tokio::runtime::Handle::current();
    let result = rt.block_on(crate::execution::runner::run_inline(
        state.clone(),
        exec_params,
    ))?;

    Ok(json!({
        "thread": result.finalized_thread,
        "result": result.result,
        "data": result.result,
        "status": "ok",
    }))
}
