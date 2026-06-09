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
    let mut term = TerminalGuard::init()?;
    let (width, height) = term.size();
    let mut stdout = std::io::stdout();
    let mut renderer = StudioTerminalRenderer::new();
    let mut core = StudioCore::default();
    let mut dirty = true;

    let start_effects = core.dispatch(StudioEvent::Start {
        session: session_for(project_path, read_only, &loaded_surface),
        viewport: viewport(width, height),
        now_ms: now_ms(),
    });
    dispatch_effects(&mut core, &client, start_effects).await;

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        if dirty {
            let vm = core.envelope(Vec::new()).view_model;
            renderer.render(&mut stdout, &vm, term.size().0, term.size().1)?;
            dirty = false;
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
            _ = tick.tick() => {
                let effects = core.dispatch(StudioEvent::Tick { now_ms: now_ms() });
                dispatch_effects(&mut core, &client, effects).await;
                dirty = true;
            }
        }
    }

    Ok(())
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
        ryeos_client_base::studio::view_model::StudioViewVm::ThreadList { rows }
        | ryeos_client_base::studio::view_model::StudioViewVm::Items { rows, .. }
        | ryeos_client_base::studio::view_model::StudioViewVm::Files { rows, .. }
        | ryeos_client_base::studio::view_model::StudioViewVm::Rows { rows, .. } => rows,
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
        let result = run_effect(client, &effect).await;
        pending.extend(core.dispatch(StudioEvent::EffectResult { result }));
    }
}

async fn run_effect(client: &DaemonClient, effect: &StudioEffect) -> StudioEffectResult {
    let kind = result_kind_for(&effect.kind);
    match effect_data(client, &effect.kind).await {
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
        StudioEffectKind::FetchSchedules => client.get_json("/ui/api/studio/schedules/list").await,
        StudioEffectKind::FetchGcStatus => client.get_json("/ui/api/studio/gc/status").await,
        StudioEffectKind::AddProject { root } => client.signed_post("/ui/api/studio/projects/add", &serde_json::json!({ "root": root })).await,
        StudioEffectKind::OpenProject { local_id } => client.signed_post("/ui/api/studio/projects/open", &serde_json::json!({ "local_id": local_id })).await,
        StudioEffectKind::ListFiles { root, path, .. } => client.signed_post("/ui/api/studio/files/list", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        StudioEffectKind::FetchFileSpace { root, path, max_depth, max_entries } => client.signed_post("/ui/api/studio/files/tree", &serde_json::json!({ "root": file_root(root), "path": path, "max_depth": max_depth, "max_entries": max_entries })).await,
        StudioEffectKind::ReadFile { root, path } => client.signed_post("/ui/api/studio/files/read", &serde_json::json!({ "root": file_root(root), "path": path })).await,
        StudioEffectKind::InspectItem { canonical_ref, include_raw, include_effective } => client.signed_post("/ui/api/studio/item/inspect", &serde_json::json!({ "canonical_ref": canonical_ref, "include_raw": include_raw, "include_effective": include_effective })).await,
        StudioEffectKind::InspectThread { thread_id, event_limit } => client.signed_post("/ui/api/studio/thread/inspect", &serde_json::json!({ "thread_id": thread_id, "event_limit": event_limit })).await,
        StudioEffectKind::InvokeAction { command_id, args } => client.signed_post("/ui/api/actions/invoke", &serde_json::json!({ "command_id": command_id, "args": args })).await,
        StudioEffectKind::CancelThread { thread_id } => client.signed_post("/ui/api/studio/thread/cancel", &serde_json::json!({ "thread_id": thread_id })).await,
        StudioEffectKind::SubmitInput { route, text } => Ok(serde_json::json!({ "route": route, "text": text })),
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
        StudioEffectKind::FetchSchedules => StudioEffectResultKind::Schedules,
        StudioEffectKind::FetchGcStatus => StudioEffectResultKind::GcStatus,
        StudioEffectKind::ListFiles { .. } => StudioEffectResultKind::FilesList,
        StudioEffectKind::FetchFileSpace { .. } => StudioEffectResultKind::FileSpace,
        StudioEffectKind::ReadFile { .. } => StudioEffectResultKind::FileRead,
        StudioEffectKind::InspectItem { .. } => StudioEffectResultKind::ItemInspection,
        StudioEffectKind::InspectThread { .. } => StudioEffectResultKind::ThreadInspection,
        StudioEffectKind::InvokeAction { .. } => StudioEffectResultKind::ActionInvocation,
        StudioEffectKind::CancelThread { .. } => StudioEffectResultKind::ThreadCancelled,
        StudioEffectKind::SubmitInput { .. } => StudioEffectResultKind::InputSubmitted,
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
