//! Opt-in terminal Studio runtime.
//!
//! This runs the same `StudioCore` used by the web UI and renders its
//! `StudioViewModel` in the terminal. It is deliberately parallel to the older
//! TUI loop so the migration can proceed without mixing product models.

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ryeos_client_base::studio::{
    studio_key_command, BrowserSession, BrowserViewport, StudioCore, StudioEffect,
    StudioEffectKind, StudioEffectResult, StudioEffectResultKind, StudioEvent, StudioKey,
    StudioKeyCommand, StudioKeyContext, StudioKeyEvent, StudioKeyModifiers, StudioUiEvent,
};
use ryeos_client_base::studio::{SeatEvent, SeatEventKind};
use ryeos_client_base::surface::LoadedSurface;
use ryeos_client_base::workspace::FocusDirection;

use crate::daemon::{ClientError, DaemonClient};
use crate::studio_render::StudioTerminalRenderer;
use crate::terminal::TerminalGuard;

pub async fn run(
    project_path: &str,
    read_only: bool,
    loaded_surface: LoadedSurface,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = DaemonClient::try_connect().await?;
    let surface_ref = loaded_surface
        .requested_ref()
        .map(str::to_string)
        .unwrap_or_else(|| loaded_surface.spec().name.clone());
    client
        .mint_ui_session(&surface_ref, Some(project_path), read_only)
        .await?;
    let client = std::sync::Arc::new(client);
    let mut term = TerminalGuard::init()?;
    let (width, height) = term.size();
    let mut stdout = std::io::stdout();
    let mut renderer = StudioTerminalRenderer::new();
    let mut core = StudioCore::default();
    let mut dirty = true;

    // Degraded live timeline: tail the route's head thread over SSE.
    // The braid is the truth; this is it arriving now. Retargets abort
    // the old tail and open a new one.
    let (tail_tx, mut tail_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut tail_thread: Option<String> = None;
    let mut tail_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut tail_events_pending: bool = false;

    let start_effects = core.dispatch(StudioEvent::Start {
        session: session_for(project_path, read_only, &loaded_surface),
        viewport: viewport(width, height),
        now_ms: now_ms(),
    });
    dispatch_effects(&mut core, &client, start_effects).await;

    // The seat is itself a thread: braided, owned, replayable. Reattach
    // to the latest running owned seat for this surface when possible;
    // otherwise open one. Mirror every new local seat event into its
    // braid and settle it on clean exit.
    let seeded_events = core.seat.events().len();
    let reattached = reattach_seat_thread(&client, &mut core).await;
    let replayed_events = (reattached.is_some() && core.seat.events().len() > seeded_events)
        .then(|| core.seat.events().len());
    let mut seat_thread = reattached;
    if seat_thread.is_none() {
        seat_thread = open_seat_thread(&client, &core).await;
    }
    let mut seat_synced: usize = replayed_events.unwrap_or(0);
    sync_seat_braid(&client, &core, &seat_thread, &mut seat_synced).await;

    // Session hints: transient "look" signals; bound views declaring
    // `refresh.on_hint` refetch their sources. This replaces polling —
    // content decides its own liveness.
    let (hint_tx, mut hint_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let hint_client = client.clone();
        let tx = hint_tx.clone();
        tokio::spawn(async move {
            let Ok(mut stream) = hint_client.open_session_events().await else {
                return;
            };
            while let Some(event) = stream.next_event().await {
                if let crate::sse::SseEvent::Unknown { event_type, data } = &event {
                    if event_type == "message" || event_type.ends_with(".hint") {
                        let kind = serde_json::from_str::<serde_json::Value>(data)
                            .ok()
                            .and_then(|value| {
                                value
                                    .get("payload")
                                    .and_then(|p| p.get("kind"))
                                    .or_else(|| value.get("kind"))
                                    .and_then(|k| k.as_str())
                                    .map(str::to_string)
                            });
                        if let Some(kind) = kind {
                            if tx.send(kind).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        if dirty {
            let vm = core.envelope(Vec::new()).view_model;
            renderer.render(&mut stdout, &vm, term.size().0, term.size().1)?;
            dirty = false;
        }

        // Reconcile the SSE tail with the route facet.
        let desired = core.seat.fold().input_route().thread;
        if desired != tail_thread {
            if let Some(task) = tail_task.take() {
                task.abort();
            }
            tail_thread = desired.clone();
            if let Some(thread_id) = desired {
                let tail_client = client.clone();
                let tx = tail_tx.clone();
                tail_task = Some(tokio::spawn(async move {
                    let Ok(mut stream) = tail_client.open_thread_events(&thread_id).await else {
                        return;
                    };
                    while let Some(_event) = stream.next_event().await {
                        if tx.send(()).is_err() {
                            break;
                        }
                    }
                }));
            }
        }

        tokio::select! {
            maybe_event = events.next() => {
                let Some(Ok(event)) = maybe_event else { break; };
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if should_quit(&core, &key) {
                            break;
                        }
                        let effects = handle_key(&mut core, key);
                        dispatch_effects(&mut core, &client, effects).await;
                        sync_seat_braid(&client, &core, &seat_thread, &mut seat_synced).await;
                        dirty = true;
                    }
                    Event::Resize(w, h) => {
                        let _ = term.update_size();
                        let effects = core.dispatch(StudioEvent::Resize { viewport: viewport(w, h) });
                        dispatch_effects(&mut core, &client, effects).await;
                        dirty = true;
                    }
                    _ => {}
                }
            }
            Some(()) = tail_rx.recv() => {
                // Braid events arrived; coalesce refreshes to tick rate.
                tail_events_pending = true;
            }
            Some(kind) = hint_rx.recv() => {
                let effects = core.effects_for_hint(&kind);
                if !effects.is_empty() {
                    dispatch_effects(&mut core, &client, effects).await;
                    dirty = true;
                }
            }
            _ = tick.tick() => {
                let effects = core.dispatch(StudioEvent::Tick { now_ms: now_ms() });
                dispatch_effects(&mut core, &client, effects).await;
                sync_seat_braid(&client, &core, &seat_thread, &mut seat_synced).await;
                if tail_events_pending {
                    tail_events_pending = false;
                    if tail_thread.is_some() {
                        let effects = core.effects_for_hint("thread");
                        dispatch_effects(&mut core, &client, effects).await;
                    }
                }
                dirty = true;
            }
        }
    }

    if let Some(thread_id) = &seat_thread {
        let _ = client
            .signed_post(
                "/execute",
                &serde_json::json!({
                    "item_ref": "service:seat/close",
                    "parameters": { "thread_id": thread_id },
                }),
            )
            .await;
    }

    Ok(())
}

/// Open the seat session thread (best effort: an unreachable seat
/// service degrades to a local-only seat, never a crash).
async fn open_seat_thread(client: &DaemonClient, core: &StudioCore) -> Option<String> {
    let surface_ref = core
        .data
        .session
        .as_ref()
        .map(|session| session.surface_ref.clone())?;
    let body = serde_json::json!({
        "item_ref": "service:seat/open",
        "parameters": { "surface_ref": surface_ref, "client_ref": "client:ryeos/tui" },
    });
    let envelope = client.signed_post("/execute", &body).await.ok()?;
    envelope
        .get("result")
        .and_then(|result| result.get("thread_id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

async fn reattach_seat_thread(client: &DaemonClient, core: &mut StudioCore) -> Option<String> {
    let surface_ref = core
        .data
        .session
        .as_ref()
        .map(|session| session.surface_ref.clone())?;
    let body = serde_json::json!({
        "item_ref": "service:threads/list",
        "parameters": { "limit": 100 },
    });
    let envelope = client.signed_post("/execute", &body).await.ok()?;
    let threads = envelope
        .get("result")
        .and_then(|result| result.get("threads"))
        .and_then(serde_json::Value::as_array)?;
    let thread_id = threads
        .iter()
        .filter(|thread| {
            thread.get("kind").and_then(serde_json::Value::as_str) == Some("seat_session")
                && thread.get("status").and_then(serde_json::Value::as_str) == Some("running")
                && thread.get("item_ref").and_then(serde_json::Value::as_str)
                    == Some(surface_ref.as_str())
        })
        .max_by_key(|thread| {
            thread
                .get("updated_at")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        })
        .and_then(|thread| thread.get("thread_id"))
        .and_then(serde_json::Value::as_str)?
        .to_string();

    replay_seat_thread(client, core, &thread_id).await;
    Some(thread_id)
}

async fn replay_seat_thread(client: &DaemonClient, core: &mut StudioCore, thread_id: &str) {
    let body = serde_json::json!({
        "item_ref": "service:events/chain_replay",
        "parameters": { "chain_root_id": thread_id },
    });
    let Ok(envelope) = client.signed_post("/execute", &body).await else {
        return;
    };
    let Some(events) = envelope
        .get("result")
        .and_then(|result| result.get("events"))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    for event in events {
        if let Some(seat_event) = seat_event_from_replay(event) {
            core.seat.append_replayed(seat_event);
        }
    }
}

fn seat_event_from_replay(event: &serde_json::Value) -> Option<SeatEvent> {
    let event_type = event.get("event_type")?.as_str()?;
    if event_type != "seat.facet" {
        return None;
    }
    let payload = event.get("payload")?;
    let facet = payload.get("payload").unwrap_or(payload);
    let key = facet.get("key")?.as_str()?.to_string();
    let value = facet.get("value")?.clone();
    let seq = payload
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| event.get("chain_seq").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    Some(SeatEvent {
        seq,
        kind: SeatEventKind::Facet { key, value },
    })
}

/// Mirror newly-appended seat events into the seat thread's braid. The
/// local log is the write-ahead view; the braid is the durable truth.
async fn sync_seat_braid(
    client: &DaemonClient,
    core: &StudioCore,
    seat_thread: &Option<String>,
    synced: &mut usize,
) {
    let Some(thread_id) = seat_thread else {
        return;
    };
    let events = core.seat.events();
    if events.len() <= *synced {
        return;
    }
    let batch: Vec<serde_json::Value> = events[*synced..]
        .iter()
        .filter_map(|event| serde_json::to_value(event).ok())
        .filter_map(|value| {
            let event_type = value.get("event_type")?.as_str()?.to_string();
            Some(serde_json::json!({
                "event_type": event_type,
                "payload": {
                    "seq": value.get("seq"),
                    "payload": value.get("payload"),
                },
            }))
        })
        .collect();
    let body = serde_json::json!({
        "item_ref": "service:seat/append",
        "parameters": { "thread_id": thread_id, "events": batch },
    });
    if client.signed_post("/execute", &body).await.is_ok() {
        *synced = events.len();
    }
}

fn session_for(
    project_path: &str,
    read_only: bool,
    loaded_surface: &LoadedSurface,
) -> BrowserSession {
    let surface_ref = loaded_surface
        .requested_ref()
        .map(str::to_string)
        .unwrap_or_else(|| loaded_surface.spec().name.clone());
    BrowserSession {
        session_id: format!("terminal:{}", now_ms()),
        surface_ref,
        user_principal_id: None,
        effective_surface: Some(serde_json::to_value(loaded_surface.spec()).unwrap_or_default()),
        project_path: Some(project_path.to_string()),
        read_only,
        granted_caps: Vec::new(),
        events_url: None,
    }
}

fn viewport(width: u16, height: u16) -> BrowserViewport {
    BrowserViewport {
        width: width as u32,
        height: height as u32,
        device_pixel_ratio: 1.0,
    }
}

fn handle_key(core: &mut StudioCore, key: KeyEvent) -> Vec<StudioEffect> {
    let Some(event) = terminal_studio_key_event(key) else {
        return Vec::new();
    };
    match studio_key_command(event, key_context(core)) {
        StudioKeyCommand::Ui { event } => core.dispatch(StudioEvent::Ui { event }),
        StudioKeyCommand::MoveFocusedRowOrFocus {
            delta,
            fallback_direction,
        } => move_focused_row_or_focus(core, delta, fallback_direction),
        StudioKeyCommand::InsertLauncherChar { ch } => {
            let mut query = core.ui.launcher.query.clone();
            query.push(ch);
            core.dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SetLauncherQuery { query },
            })
        }
        StudioKeyCommand::DeleteLauncherChar => {
            let mut query = core.ui.launcher.query.clone();
            query.pop();
            core.dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SetLauncherQuery { query },
            })
        }
        StudioKeyCommand::Quit => Vec::new(),
        StudioKeyCommand::Ignore => Vec::new(),
    }
}

fn terminal_studio_key_event(key: KeyEvent) -> Option<StudioKeyEvent> {
    let studio_key = match key.code {
        KeyCode::Char(ch) => StudioKey::Char(ch),
        KeyCode::Up => StudioKey::ArrowUp,
        KeyCode::Down => StudioKey::ArrowDown,
        KeyCode::Left => StudioKey::ArrowLeft,
        KeyCode::Right => StudioKey::ArrowRight,
        KeyCode::Enter => StudioKey::Enter,
        KeyCode::Esc => StudioKey::Escape,
        KeyCode::Backspace => StudioKey::Backspace,
        KeyCode::Tab => StudioKey::Tab,
        _ => return None,
    };
    Some(StudioKeyEvent {
        key: studio_key,
        modifiers: StudioKeyModifiers {
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            shift: key.modifiers.contains(KeyModifiers::SHIFT),
            meta: key.modifiers.contains(KeyModifiers::SUPER),
        },
    })
}

fn key_context(core: &StudioCore) -> StudioKeyContext {
    StudioKeyContext {
        launcher_open: core.ui.launcher.open,
        input_visible: core.ui.docks.has_visible_input(),
    }
}

fn move_focused_row(core: &mut StudioCore, delta: i32) -> (bool, Vec<StudioEffect>) {
    let vm = core.envelope(Vec::new()).view_model;
    let focused = vm.workspace.focused_tile;
    let row_count = focused_row_count(&vm.workspace.root, &focused).unwrap_or(0);
    if row_count == 0 {
        return (false, Vec::new());
    }
    let current = focused_selected_index(&vm.workspace.root, &focused).unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(1)
    } else {
        (current + 1).min(row_count.saturating_sub(1))
    };
    if next == current {
        return (false, Vec::new());
    }
    let effects = core.dispatch(StudioEvent::Ui {
        event: StudioUiEvent::SetTileCursor {
            tile_id: focused,
            index: next,
        },
    });
    (true, effects)
}

fn move_focused_row_or_focus(
    core: &mut StudioCore,
    delta: i32,
    fallback_direction: FocusDirection,
) -> Vec<StudioEffect> {
    let (handled, effects) = move_focused_row(core, delta);
    if handled {
        effects
    } else {
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::FocusDirection {
                direction: fallback_direction,
            },
        })
    }
}

fn focused_row_count(
    node: &ryeos_client_base::studio::view_model::StudioLayoutNodeVm,
    focused: &str,
) -> Option<usize> {
    match node {
        ryeos_client_base::studio::view_model::StudioLayoutNodeVm::Tile {
            tile_id, view, ..
        } if tile_id == focused => Some(rows_in_view(view).len()),
        ryeos_client_base::studio::view_model::StudioLayoutNodeVm::Tile { .. } => None,
        ryeos_client_base::studio::view_model::StudioLayoutNodeVm::Split {
            first, second, ..
        } => focused_row_count(first, focused).or_else(|| focused_row_count(second, focused)),
    }
}

fn focused_selected_index(
    node: &ryeos_client_base::studio::view_model::StudioLayoutNodeVm,
    focused: &str,
) -> Option<usize> {
    match node {
        ryeos_client_base::studio::view_model::StudioLayoutNodeVm::Tile {
            tile_id, view, ..
        } if tile_id == focused => rows_in_view(view).iter().position(|row| row.selected),
        ryeos_client_base::studio::view_model::StudioLayoutNodeVm::Tile { .. } => None,
        ryeos_client_base::studio::view_model::StudioLayoutNodeVm::Split {
            first, second, ..
        } => focused_selected_index(first, focused)
            .or_else(|| focused_selected_index(second, focused)),
    }
}

fn rows_in_view(
    view: &ryeos_client_base::studio::view_model::StudioViewVm,
) -> &[ryeos_client_base::studio::view_model::StudioRowVm] {
    match view {
        ryeos_client_base::studio::view_model::StudioViewVm::Rows { rows, .. } => rows,
        _ => &[],
    }
}

async fn dispatch_effects(
    core: &mut StudioCore,
    client: &DaemonClient,
    effects: Vec<StudioEffect>,
) {
    let mut pending = effects;
    while let Some(effect) = pending.pop() {
        let project_path = core
            .data
            .session
            .as_ref()
            .and_then(|session| session.project_path.clone());
        let result = run_effect(client, &effect, project_path.as_deref()).await;
        pending.extend(core.dispatch(StudioEvent::EffectResult { result }));
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
            // same execute path; result keyed to the subscribing tile.
            // Project-scoped sources get the seat's project context
            // unless the binding already pinned one.
            let mut params = params.clone();
            if params.get("project_path").is_none() {
                if let (Some(project), Some(map)) = (project_path, params.as_object_mut()) {
                    map.insert(
                        "project_path".to_string(),
                        serde_json::Value::String(project.to_string()),
                    );
                }
            }
            let body = serde_json::json!({ "item_ref": source_ref, "parameters": params });
            let envelope = client.signed_post("/execute", &body).await?;
            Ok(envelope.get("result").cloned().unwrap_or(envelope))
        }
        StudioEffectKind::FetchCommands => {
            let body = serde_json::json!({ "item_ref": "service:commands/list", "parameters": {} });
            let envelope = client.signed_post("/execute", &body).await?;
            Ok(envelope.get("result").cloned().unwrap_or(envelope))
        }
        StudioEffectKind::AddProject { root } => client.signed_post("/ui/api/studio/projects/add", &serde_json::json!({ "root": root })).await,
        StudioEffectKind::OpenProject { local_id } => client.signed_post("/ui/api/studio/projects/open", &serde_json::json!({ "local_id": local_id })).await,
        StudioEffectKind::ListFiles { root, path, .. } => client.signed_post("/ui/api/studio/files/list", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        StudioEffectKind::FetchFileSpace { root, path, max_depth, max_entries } => client.signed_post("/ui/api/studio/files/tree", &serde_json::json!({ "root": file_root(root), "path": path, "max_depth": max_depth, "max_entries": max_entries })).await,
        StudioEffectKind::ReadFile { root, path } => client.signed_post("/ui/api/studio/files/read", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        StudioEffectKind::InvokeAction { command_id, args } => client.signed_post("/ui/api/actions/invoke", &serde_json::json!({ "command_id": command_id, "args": args })).await,
        StudioEffectKind::CancelThread { thread_id } => client.signed_post("/ui/api/studio/thread/cancel", &serde_json::json!({ "thread_id": thread_id })).await,
        StudioEffectKind::Invoke { target, params, .. } => match target {
            ryeos_client_base::studio::effect::InvokeRef::Ref { item_ref } => {
                let mut params = params.clone();
                if let (Some(project), Some(map)) = (project_path, params.as_object_mut()) {
                    // Services receive only their own parameters; project
                    // context rides inside them as declared by the schema.
                    map.entry("project_path".to_string())
                        .or_insert_with(|| serde_json::Value::String(project.to_string()));
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
        StudioEffectKind::FetchCommands => StudioEffectResultKind::Commands,
        StudioEffectKind::FetchSource { .. } => StudioEffectResultKind::SourceData,
        StudioEffectKind::ListFiles { .. } => StudioEffectResultKind::FilesList,
        StudioEffectKind::FetchFileSpace { .. } => StudioEffectResultKind::FileSpace,
        StudioEffectKind::ReadFile { .. } => StudioEffectResultKind::FileRead,
        StudioEffectKind::InvokeAction { .. } => StudioEffectResultKind::ActionInvocation,
        StudioEffectKind::CancelThread { .. } => StudioEffectResultKind::ThreadCancelled,
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

fn should_quit(core: &StudioCore, key: &KeyEvent) -> bool {
    terminal_studio_key_event(*key).is_some_and(|event| {
        matches!(
            studio_key_command(event, key_context(core)),
            StudioKeyCommand::Quit
        )
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_parser_accepts_persisted_seat_facet_shape() {
        let event = json!({
            "chain_seq": 4,
            "event_type": "seat.facet",
            "payload": {
                "seq": 2,
                "payload": {
                    "key": "selection",
                    "value": { "item": "thread-1" }
                }
            }
        });

        let seat_event = seat_event_from_replay(&event).expect("seat event");

        assert_eq!(seat_event.seq, 2);
        assert_eq!(
            seat_event.kind,
            SeatEventKind::Facet {
                key: "selection".to_string(),
                value: json!({ "item": "thread-1" }),
            }
        );
    }

    #[test]
    fn replay_parser_ignores_non_seat_events() {
        let event = json!({
            "chain_seq": 4,
            "event_type": "thread.started",
            "payload": {}
        });

        assert!(seat_event_from_replay(&event).is_none());
    }
}
