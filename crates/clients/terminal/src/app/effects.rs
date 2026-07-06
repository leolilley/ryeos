//! The terminal effect executor: each `RyeOsEffect` from the core maps
//! to one daemon call; results fold back into the core as
//! `EffectResult` events. No ryeos state lives here — this is the
//! boundary where engine intent becomes transport calls.

use ryeos_client_base::ui::{
    RyeOsCore, RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind, RyeOsEvent,
};

use crate::transport::daemon::{ClientError, DaemonClient};

pub async fn dispatch_effects(
    core: &mut RyeOsCore,
    client: &DaemonClient,
    effects: Vec<RyeOsEffect>,
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
            pending.extend(core.dispatch(RyeOsEvent::EffectResult { result }));
        }
    }
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
        RyeOsEffectKind::FetchDimension => client.get_json("/ui/api/ryeos/dimension").await,
        RyeOsEffectKind::FetchProjects => client.get_json("/ui/api/ryeos/projects/list").await.or_else(|_| Ok(serde_json::json!({ "version": 1, "projects": [] }))),
        RyeOsEffectKind::FetchTopology => client.get_json("/ui/api/graph/topology").await,
        RyeOsEffectKind::FetchThreads { limit } => client.get_json(&format!("/ui/api/ryeos/threads/list?limit={limit}")).await,
        RyeOsEffectKind::FetchItems { query, kind, limit, .. } => {
            let mut path = format!("/ui/api/ryeos/items/list?limit={limit}");
            if let Some(query) = query.as_ref().filter(|value| !value.is_empty()) { path.push_str("&query="); path.push_str(&url_encode(query)); }
            if let Some(kind) = kind.as_ref().filter(|value| !value.is_empty()) { path.push_str("&kind="); path.push_str(&url_encode(kind)); }
            client.get_json(&path).await
        }
        RyeOsEffectKind::FetchSource { source_ref, params, .. } => {
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
        RyeOsEffectKind::AddProject { root } => client.signed_post("/ui/api/ryeos/projects/add", &serde_json::json!({ "root": root })).await,
        RyeOsEffectKind::OpenProject { local_id } => client.signed_post("/ui/api/ryeos/projects/open", &serde_json::json!({ "local_id": local_id })).await,
        RyeOsEffectKind::ListFiles { root, path, .. } => client.signed_post("/ui/api/ryeos/files/list", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        RyeOsEffectKind::FetchFileSpace { root, path, max_depth, max_entries, .. } => client.signed_post("/ui/api/ryeos/files/tree", &serde_json::json!({ "root": file_root(root), "path": path, "max_depth": max_depth, "max_entries": max_entries })).await,
        RyeOsEffectKind::ReadFile { root, path } => client.signed_post("/ui/api/ryeos/files/read", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        RyeOsEffectKind::InvokeAction { command_id, args } => client.signed_post("/ui/api/actions/invoke", &serde_json::json!({ "command_id": command_id, "args": args })).await,
        RyeOsEffectKind::SubmitThreadCommand { thread_id, command_type } => {
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
            ryeos_client_base::ui::effect::InvokeRef::Tokens { tokens } => {
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
        RyeOsEffectKind::SetLocationHash { .. } | RyeOsEffectKind::CopyToClipboard { .. } | RyeOsEffectKind::OpenUrl { .. } => Ok(serde_json::Value::Null),
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
        RyeOsEffectKind::InvokeAction { .. } => RyeOsEffectResultKind::ActionInvocation,
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
