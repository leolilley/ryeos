use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct DispatchActionParams {
    callback_token: String,
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

    let project_path = crate::execution::project_source::normalize_project_path(&params.project_path);

    state.callback_tokens.validate_primary(
        &params.callback_token,
        &params.thread_id,
        &project_path,
        &params.action.primary,
    )?;

    match params.action.primary.as_str() {
        "execute" => handle_execute(params, state),
        "fetch" => handle_fetch(params, state),
        "sign" => handle_sign(params, state),
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

fn handle_fetch(params: DispatchActionParams, state: &AppState) -> Result<Value> {
    let project_path =
        crate::execution::project_source::normalize_project_path(&params.project_path);

    let query = params.action.params.get("query").and_then(|q| q.as_str());
    let scope = params.action.params.get("scope").and_then(|s| s.as_str());

    if let Some(query_str) = query {
        return handle_fetch_query(query_str, scope, &project_path, state);
    }

    handle_fetch_id(&params.action.item_id, &project_path, state)
}

fn handle_fetch_id(
    item_id: &str,
    project_path: &std::path::Path,
    state: &AppState,
) -> Result<Value> {
    let canon_ref = rye_engine::canonical_ref::CanonicalRef::parse(item_id)
        .map_err(|e| anyhow::anyhow!("invalid item ref '{item_id}': {e}"))?;

    let ctx = build_plan_context(project_path, state);
    let resolved = state
        .engine
        .resolve(&ctx, &canon_ref)
        .map_err(|e| anyhow::anyhow!("resolution failed: {e}"))?;

    let content = std::fs::read_to_string(&resolved.source_path)
        .with_context(|| format!("failed to read {}", resolved.source_path.display()))?;

    Ok(json!({
        "status": "success",
        "content": content,
        "path": resolved.source_path.to_string_lossy(),
        "source": format!("{:?}", resolved.source_space),
        "metadata": {
            "kind": resolved.kind,
            "content_hash": resolved.content_hash,
            "canonical_ref": resolved.canonical_ref.to_string(),
        },
    }))
}

fn handle_fetch_query(
    query: &str,
    scope: Option<&str>,
    project_path: &std::path::Path,
    state: &AppState,
) -> Result<Value> {
    let roots = state.engine.resolution_roots(Some(project_path.to_path_buf()));

    let search_roots: Vec<(PathBuf, &str)> = match scope {
        Some("system") => roots.system.iter().map(|r| (r.clone(), "system")).collect(),
        Some("user") => roots
            .user
            .as_ref()
            .map(|r| (r.clone(), "user"))
            .into_iter()
            .collect(),
        Some("project") => roots
            .project
            .as_ref()
            .map(|r| (r.clone(), "project"))
            .into_iter()
            .collect(),
        _ => {
            let mut r = Vec::new();
            if let Some(ref p) = roots.project {
                r.push((p.clone(), "project"));
            }
            if let Some(ref u) = roots.user {
                r.push((u.clone(), "user"));
            }
            for s in &roots.system {
                r.push((s.clone(), "system"));
            }
            r
        }
    };

    let query_lower = query.to_lowercase();
    let mut matches = Vec::new();

    for (root, space) in &search_roots {
        let ai_dir = root.join(".ai");
        if !ai_dir.is_dir() {
            continue;
        }
        scan_for_query(&ai_dir, &query_lower, space, &mut matches, 0, 10)?;
    }

    Ok(json!({
        "status": "success",
        "results": matches,
        "query": query,
    }))
}

fn scan_for_query(
    dir: &std::path::Path,
    query: &str,
    space: &str,
    results: &mut Vec<Value>,
    depth: usize,
    max_results: usize,
) -> Result<()> {
    if depth > 8 || results.len() >= max_results {
        return Ok(());
    }

    for entry in std::fs::read_dir(dir)? {
        if results.len() >= max_results {
            return Ok(());
        }
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('.') || name == "state" {
            continue;
        }

        if path.is_dir() {
            scan_for_query(&path, query, space, results, depth + 1, max_results)?;
        } else if name.to_lowercase().contains(query) {
            results.push(json!({
                "path": path.to_string_lossy(),
                "source": space,
                "name": name,
            }));
        }
    }

    Ok(())
}

fn handle_sign(params: DispatchActionParams, state: &AppState) -> Result<Value> {
    let project_path =
        crate::execution::project_source::normalize_project_path(&params.project_path);

    let canon_ref = rye_engine::canonical_ref::CanonicalRef::parse(&params.action.item_id)
        .map_err(|e| anyhow::anyhow!("invalid item ref '{}': {e}", params.action.item_id))?;

    let ctx = build_plan_context(&project_path, state);
    let resolved = state
        .engine
        .resolve(&ctx, &canon_ref)
        .map_err(|e| anyhow::anyhow!("resolution failed: {e}"))?;

    let content = std::fs::read_to_string(&resolved.source_path)
        .with_context(|| format!("failed to read {}", resolved.source_path.display()))?;

    let envelope = &resolved.source_format.signature;

    let signed = lillux::signature::sign_content(
        &content,
        state.identity.signing_key(),
        &envelope.prefix,
        envelope.suffix.as_deref(),
    );

    let tmp_path = resolved.source_path.with_extension("signed.tmp");
    std::fs::write(&tmp_path, &signed)
        .with_context(|| format!("failed to write signed tempfile {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &resolved.source_path)
        .with_context(|| format!("failed to rename signed file {}", resolved.source_path.display()))?;

    let fingerprint = state.identity.fingerprint().to_string();

    Ok(json!({
        "status": "success",
        "path": resolved.source_path.to_string_lossy(),
        "fingerprint": fingerprint,
    }))
}

fn build_plan_context(
    project_path: &std::path::Path,
    state: &AppState,
) -> rye_engine::contracts::PlanContext {
    use rye_engine::contracts::{EffectivePrincipal, Principal, ProjectContext};

    rye_engine::contracts::PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: state.identity.fingerprint().to_string(),
            scopes: vec![
                "execute".to_string(),
                "fetch".to_string(),
                "sign".to_string(),
            ],
        }),
        project_context: ProjectContext::LocalPath {
            path: project_path.to_path_buf(),
        },
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: Default::default(),
        validate_only: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_thread_is_inline() {
        assert_eq!(default_thread(), "inline");
    }
}
