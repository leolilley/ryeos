//! The terminal effect executor: each `RyeOsEffect` from the core maps
//! to one daemon call; results come home over the loop's effect channel
//! and fold back into the core as `EffectResult` events. No ryeos state
//! lives here — this is the boundary where engine intent becomes
//! transport calls, and the boundary where the render loop stops
//! waiting: a round trip in flight never blocks a frame.

use std::sync::Arc;

use ryeos_client_base::ui::{
    RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind,
};

use crate::transport::daemon::{ClientError, DaemonClient};

/// Launch one generation of effects as a concurrent batch off the loop —
/// a startup burst of independent fetches costs one round trip, not one
/// per view. The joined results arrive as a single message in emission
/// order; the loop folds them and spawns any follow-up generation the
/// folds emit. Freshness is the core's job (per-key epochs), so batches
/// from different generations may resolve in any order.
pub fn spawn_effects(
    client: &Arc<DaemonClient>,
    project_path: Option<String>,
    effects: Vec<RyeOsEffect>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<RyeOsEffectResult>>,
) {
    if effects.is_empty() {
        return;
    }
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let results = futures_util::future::join_all(
            effects
                .iter()
                .map(|effect| run_effect(&client, effect, project_path.as_deref())),
        )
        .await;
        let _ = tx.send(results);
    });
}

async fn run_effect(
    client: &DaemonClient,
    effect: &RyeOsEffect,
    project_path: Option<&str>,
) -> RyeOsEffectResult {
    let kind = result_kind_for(&effect.kind);
    match effect_data(client, &effect.kind, project_path).await {
        Ok(data) => RyeOsEffectResult {
            id: effect.id,
            ok: true,
            kind,
            data: Some(data),
            error: None,
        },
        Err(error) => RyeOsEffectResult {
            id: effect.id,
            ok: false,
            kind,
            data: None,
            error: Some(error.to_string()),
        },
    }
}

async fn effect_data(
    client: &DaemonClient,
    kind: &RyeOsEffectKind,
    project_path: Option<&str>,
) -> Result<serde_json::Value, ClientError> {
    match kind {
        RyeOsEffectKind::FetchDimension => client.get_json("/ui/api/ryeos-ui/dimension").await,
        RyeOsEffectKind::FetchProjects => client.get_json("/ui/api/ryeos-ui/projects/list").await.or_else(|_| Ok(serde_json::json!({ "version": 1, "projects": [] }))),
        RyeOsEffectKind::FetchTopology => client.get_json("/ui/api/graph/topology").await,
        RyeOsEffectKind::FetchThreads { limit } => client.get_json(&format!("/ui/api/ryeos-ui/threads/list?limit={limit}")).await,
        RyeOsEffectKind::FetchItems { query, kind, limit, .. } => {
            let mut path = format!("/ui/api/ryeos-ui/items/list?limit={limit}");
            if let Some(query) = query.as_ref().filter(|value| !value.is_empty()) { path.push_str("&query="); path.push_str(&url_encode(query)); }
            if let Some(kind) = kind.as_ref().filter(|value| !value.is_empty()) { path.push_str("&kind="); path.push_str(&url_encode(kind)); }
            client.get_json(&path).await
        }
        RyeOsEffectKind::FetchSource { source_ref, params, .. } => {
            // Polling sources must not enter the generic `/execute` admission
            // lane.  That path pins a project snapshot before it can inspect a
            // service's `record_thread: false` contract, so a live threads
            // view can otherwise starve real work with snapshot/store churn.
            // The UI invocation lane enforces the resolved service's
            // read-only policy without admitting a service thread.
            let mut params = params.clone();
            if let (Some(project), Some(slot)) = (
                project_path,
                params
                    .as_object_mut()
                    .and_then(|map| map.get_mut("project_path")),
            ) {
                if slot.as_str().map(str::is_empty).unwrap_or(slot.is_null()) {
                    *slot = serde_json::Value::String(project.to_string());
                }
            }
            let body = serde_json::json!({
                "target": { "kind": "ref", "ref": source_ref },
                "ref_bindings": {},
                "read_only": true,
                "params": params,
            });
            let envelope = client
                .signed_post("/ui/api/invocations/dispatch", &body)
                .await?;
            Ok(envelope
                .pointer("/result/result")
                .or_else(|| envelope.get("result"))
                .cloned()
                .unwrap_or(envelope))
        }
        RyeOsEffectKind::AddProject { root } => client.signed_post("/ui/api/ryeos-ui/projects/add", &serde_json::json!({ "root": root })).await,
        RyeOsEffectKind::OpenProject { local_id } => client.signed_post("/ui/api/ryeos-ui/projects/open", &serde_json::json!({ "local_id": local_id })).await,
        RyeOsEffectKind::ListFiles { root, path, .. } => client.signed_post("/ui/api/ryeos-ui/files/list", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        RyeOsEffectKind::FetchFileSpace { root, path, max_depth, max_entries, .. } => client.signed_post("/ui/api/ryeos-ui/files/tree", &serde_json::json!({ "root": file_root(root), "path": path, "max_depth": max_depth, "max_entries": max_entries })).await,
        RyeOsEffectKind::ReadFile { root, path } => client.signed_post("/ui/api/ryeos-ui/files/read", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        RyeOsEffectKind::DispatchInvocation {
            item_ref,
            ref_bindings,
            params,
        } => client.signed_post(
            "/ui/api/invocations/dispatch",
            &serde_json::json!({
                "target": { "kind": "ref", "ref": item_ref },
                "ref_bindings": ref_bindings,
                "params": params,
            }),
        ).await,
        RyeOsEffectKind::SubmitThreadCommand { thread_id, command_type } => {
            // Steer the head thread through the shared control channel. Authority
            // == the CLI's `commands submit`; see .tmp/thread-authorization-review.md
            // for the authz model. The control service denies unknown fields (no
            // project_path), so it receives only its declared params.
            let body = serde_json::json!({
                "item_ref": "service:commands/submit",
                "ref_bindings": {},
                "parameters": { "thread_id": thread_id, "command_type": command_type },
            });
            let envelope = client.signed_post("/execute", &body).await?;
            Ok(envelope.get("result").cloned().unwrap_or(envelope))
        }
        RyeOsEffectKind::Invoke { target, params, .. } => match target {
            ryeos_client_base::ui::effect::InvokeRef::Ref { item_ref } => {
                let mut params = params.clone();
                // Opt-in project scoping (mirrors FetchSource): fill param-level
                // project_path ONLY when the params object explicitly declares an
                // empty/null one. Services that don't declare it (e.g.
                // threads.cancel / commands.submit, both deny_unknown_fields with
                // no project_path field) must not receive it, or the request is
                // rejected. Top-level /execute project_path still rides as
                // transport context below.
                if let Some(project) = project_path {
                    fill_project_path_slot(&mut params, project);
                }
                let mut body = serde_json::json!({
                    "item_ref": item_ref,
                    "ref_bindings": {},
                    "parameters": params,
                });
                if let Some(project) = project_path {
                    body["project_path"] = serde_json::Value::String(project.to_string());
                }
                // Services execute through plain /execute; the dispatch
                // wraps the handler result in { thread: {...}, result }.
                // The submit contract lives in `result`.
                let envelope = client.signed_post("/execute", &body).await?;
                Ok(envelope.get("result").cloned().unwrap_or(envelope))
            }
            ryeos_client_base::ui::effect::InvokeRef::Tokens { tokens } => {
                // One daemon path: tokens resolve + bind server-side.
                let mut command = serde_json::json!({
                    "tokens": tokens,
                    "ref_bindings": {},
                    "arguments": params,
                });
                if let Some(project) = project_path {
                    command["project_path"] = serde_json::Value::String(project.to_string());
                }
                let body = serde_json::json!({
                    "item_ref": "service:commands/dispatch",
                    "ref_bindings": {},
                    "parameters": command,
                });
                let envelope = client.signed_post("/execute", &body).await?;
                Ok(envelope.get("result").cloned().unwrap_or(envelope))
            }
        },
        RyeOsEffectKind::SetLocationHash { .. } | RyeOsEffectKind::CopyToClipboard { .. } | RyeOsEffectKind::OpenUrl { .. } => Ok(serde_json::Value::Null),
    }
}

fn fill_project_path_slot(params: &mut serde_json::Value, project: &str) {
    if let Some(slot) = params.pointer_mut("/target/project_path") {
        if slot.as_str().map(str::is_empty).unwrap_or(slot.is_null()) {
            *slot = serde_json::Value::String(project.to_string());
        }
        return;
    }
    if let Some(slot) = params.get_mut("project_path") {
        if slot.as_str().map(str::is_empty).unwrap_or(slot.is_null()) {
            *slot = serde_json::Value::String(project.to_string());
        }
    }
}

fn result_kind_for(kind: &RyeOsEffectKind) -> RyeOsEffectResultKind {
    match kind {
        RyeOsEffectKind::FetchDimension => RyeOsEffectResultKind::Dimension,
        RyeOsEffectKind::FetchProjects => RyeOsEffectResultKind::Projects,
        RyeOsEffectKind::FetchTopology => RyeOsEffectResultKind::Topology,
        RyeOsEffectKind::AddProject { .. } => RyeOsEffectResultKind::ProjectAdded,
        RyeOsEffectKind::OpenProject { .. } => RyeOsEffectResultKind::ProjectOpened,
        RyeOsEffectKind::FetchThreads { .. } => RyeOsEffectResultKind::Threads,
        RyeOsEffectKind::FetchItems { .. } => RyeOsEffectResultKind::Items,
        RyeOsEffectKind::FetchSource { .. } => RyeOsEffectResultKind::SourceData,
        RyeOsEffectKind::ListFiles { .. } => RyeOsEffectResultKind::FilesList,
        RyeOsEffectKind::FetchFileSpace { .. } => RyeOsEffectResultKind::FileSpace,
        RyeOsEffectKind::ReadFile { .. } => RyeOsEffectResultKind::FileRead,
        RyeOsEffectKind::DispatchInvocation { .. } => RyeOsEffectResultKind::InvocationDispatch,
        RyeOsEffectKind::SubmitThreadCommand { .. } => {
            RyeOsEffectResultKind::ThreadCommandSubmitted
        }
        RyeOsEffectKind::Invoke { .. } => RyeOsEffectResultKind::Invoked,
        RyeOsEffectKind::SetLocationHash { .. }
        | RyeOsEffectKind::CopyToClipboard { .. }
        | RyeOsEffectKind::OpenUrl { .. } => RyeOsEffectResultKind::BrowserOnly,
    }
}

fn file_root(root: &str) -> &str {
    if root == "project_ai" {
        "project"
    } else {
        root
    }
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            b' ' => vec!['+'],
            other => format!("%{other:02X}").chars().collect(),
        })
        .collect()
}
