//! The terminal effect executor: each `StudioEffect` from the core maps
//! to one daemon call; results fold back into the core as
//! `EffectResult` events. No studio state lives here — this is the
//! boundary where engine intent becomes transport calls.

use ryeos_client_base::studio::{
    StudioCore, StudioEffect, StudioEffectKind, StudioEffectResult, StudioEffectResultKind,
    StudioEvent,
};

use crate::transport::daemon::{ClientError, DaemonClient};

pub async fn dispatch_effects(
    core: &mut StudioCore,
    client: &DaemonClient,
    effects: Vec<StudioEffect>,
) {
    // Each generation of effects runs as one concurrent batch — a startup
    // burst of independent fetches costs one round trip, not one per view.
    // Results fold back sequentially in emission order, and any effects
    // those folds emit form the next batch.
    let mut pending = effects;
    while !pending.is_empty() {
        let project_path = core
            .data
            .session
            .as_ref()
            .and_then(|session| session.project_path.clone());
        let batch = std::mem::take(&mut pending);
        let results = futures_util::future::join_all(
            batch
                .iter()
                .map(|effect| run_effect(client, effect, project_path.as_deref())),
        )
        .await;
        for result in results {
            pending.extend(core.dispatch(StudioEvent::EffectResult { result }));
        }
    }
}

async fn run_effect(
    client: &DaemonClient,
    effect: &StudioEffect,
    project_path: Option<&str>,
) -> StudioEffectResult {
    let kind = result_kind_for(&effect.kind);
    match effect_data(client, &effect.kind, project_path).await {
        Ok(data) => StudioEffectResult {
            id: effect.id,
            ok: true,
            kind,
            data: Some(data),
            error: None,
        },
        Err(error) => StudioEffectResult {
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
    kind: &StudioEffectKind,
    project_path: Option<&str>,
) -> Result<serde_json::Value, ClientError> {
    match kind {
        StudioEffectKind::FetchDimension => client.get_json("/ui/api/studio/dimension").await,
        StudioEffectKind::FetchProjects => client.get_json("/ui/api/studio/projects/list").await.or_else(|_| Ok(serde_json::json!({ "version": 1, "projects": [] }))),
        StudioEffectKind::FetchTopology => client.get_json("/ui/api/graph/topology").await,
        StudioEffectKind::FetchThreads { limit } => client.get_json(&format!("/ui/api/studio/threads/list?limit={limit}")).await,
        StudioEffectKind::FetchItems { query, kind, limit, .. } => {
            let mut path = format!("/ui/api/studio/items/list?limit={limit}");
            if let Some(query) = query.as_ref().filter(|value| !value.is_empty()) { path.push_str("&query="); path.push_str(&url_encode(query)); }
            if let Some(kind) = kind.as_ref().filter(|value| !value.is_empty()) { path.push_str("&kind="); path.push_str(&url_encode(kind)); }
            client.get_json(&path).await
        }
        StudioEffectKind::FetchSource { source_ref, params, .. } => {
            // ONE generic source mechanism: any service ref through the
            // same execute path; result keyed to the subscribing tile. A
            // view OPTS INTO project scoping by declaring an (empty)
            // `project_path` param — the executor fills it with the seat's
            // project. Sources that don't declare it never receive it, so
            // substrate ops that reject the field don't break.
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
            let body = serde_json::json!({ "item_ref": source_ref, "parameters": params });
            let envelope = client.signed_post("/execute", &body).await?;
            Ok(envelope.get("result").cloned().unwrap_or(envelope))
        }
        StudioEffectKind::AddProject { root } => client.signed_post("/ui/api/studio/projects/add", &serde_json::json!({ "root": root })).await,
        StudioEffectKind::OpenProject { local_id } => client.signed_post("/ui/api/studio/projects/open", &serde_json::json!({ "local_id": local_id })).await,
        StudioEffectKind::ListFiles { root, path, .. } => client.signed_post("/ui/api/studio/files/list", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        StudioEffectKind::FetchFileSpace { root, path, max_depth, max_entries, .. } => client.signed_post("/ui/api/studio/files/tree", &serde_json::json!({ "root": file_root(root), "path": path, "max_depth": max_depth, "max_entries": max_entries })).await,
        StudioEffectKind::ReadFile { root, path } => client.signed_post("/ui/api/studio/files/read", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        StudioEffectKind::InvokeAction { command_id, args } => client.signed_post("/ui/api/actions/invoke", &serde_json::json!({ "command_id": command_id, "args": args })).await,
        StudioEffectKind::SubmitThreadCommand { thread_id, command_type } => {
            // Steer the head thread through the shared control channel. Authority
            // == the CLI's `commands submit`; see .tmp/thread-authorization-review.md
            // for the authz model. The control service denies unknown fields (no
            // project_path), so it receives only its declared params.
            let body = serde_json::json!({
                "item_ref": "service:commands/submit",
                "parameters": { "thread_id": thread_id, "command_type": command_type },
            });
            let envelope = client.signed_post("/execute", &body).await?;
            Ok(envelope.get("result").cloned().unwrap_or(envelope))
        }
        StudioEffectKind::Invoke { target, params, .. } => match target {
            ryeos_client_base::studio::effect::InvokeRef::Ref { item_ref } => {
                let mut params = params.clone();
                // Opt-in project scoping (mirrors FetchSource): fill param-level
                // project_path ONLY when the params object explicitly declares an
                // empty/null one. Services that don't declare it (e.g.
                // threads.cancel / commands.submit, both deny_unknown_fields with
                // no project_path field) must not receive it, or the request is
                // rejected. Top-level /execute project_path still rides as
                // transport context below.
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
                let mut body = serde_json::json!({
                    "item_ref": item_ref,
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
            ryeos_client_base::studio::effect::InvokeRef::Tokens { tokens } => {
                // One daemon path: tokens resolve + bind server-side.
                let mut params = serde_json::json!({ "tokens": tokens });
                if let Some(project) = project_path {
                    params["project_path"] = serde_json::Value::String(project.to_string());
                }
                let body = serde_json::json!({
                    "item_ref": "service:commands/dispatch",
                    "parameters": params,
                });
                let envelope = client.signed_post("/execute", &body).await?;
                Ok(envelope.get("result").cloned().unwrap_or(envelope))
            }
        },
        StudioEffectKind::SetLocationHash { .. } | StudioEffectKind::CopyToClipboard { .. } | StudioEffectKind::OpenUrl { .. } => Ok(serde_json::Value::Null),
    }
}

fn result_kind_for(kind: &StudioEffectKind) -> StudioEffectResultKind {
    match kind {
        StudioEffectKind::FetchDimension => StudioEffectResultKind::Dimension,
        StudioEffectKind::FetchProjects => StudioEffectResultKind::Projects,
        StudioEffectKind::FetchTopology => StudioEffectResultKind::Topology,
        StudioEffectKind::AddProject { .. } => StudioEffectResultKind::ProjectAdded,
        StudioEffectKind::OpenProject { .. } => StudioEffectResultKind::ProjectOpened,
        StudioEffectKind::FetchThreads { .. } => StudioEffectResultKind::Threads,
        StudioEffectKind::FetchItems { .. } => StudioEffectResultKind::Items,
        StudioEffectKind::FetchSource { .. } => StudioEffectResultKind::SourceData,
        StudioEffectKind::ListFiles { .. } => StudioEffectResultKind::FilesList,
        StudioEffectKind::FetchFileSpace { .. } => StudioEffectResultKind::FileSpace,
        StudioEffectKind::ReadFile { .. } => StudioEffectResultKind::FileRead,
        StudioEffectKind::InvokeAction { .. } => StudioEffectResultKind::ActionInvocation,
        StudioEffectKind::SubmitThreadCommand { .. } => {
            StudioEffectResultKind::ThreadCommandSubmitted
        }
        StudioEffectKind::Invoke { .. } => StudioEffectResultKind::Invoked,
        StudioEffectKind::SetLocationHash { .. }
        | StudioEffectKind::CopyToClipboard { .. }
        | StudioEffectKind::OpenUrl { .. } => StudioEffectResultKind::BrowserOnly,
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
