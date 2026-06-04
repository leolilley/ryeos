//! Update reducer — processes events and returns effects.
//!
//! Core receives normalized AppEvents, mutates the model, returns Effects.
//! Platform shells execute the effects.

use crate::effects::Effect;
use crate::ids::ThreadId;
use crate::input::{InputEvent, Key, Mouse, MouseAction, ScrollDirection};
use crate::layout::Rect;
use crate::model::AppModel;
use crate::store::{
    BundleSummaryModel, CockpitSnapshotModel, CommandSummaryModel, DaemonStatus, EventRecord,
    FileBrowserModel, FileEntryModel, FileReadModel, GcStatusModel, GcSummaryModel,
    IdentityInfoModel, IdentityModel, ItemInspectionModel, ItemModel, LocalNodeModel,
    ProjectInfoModel, ProjectModel, ScheduleListModel, ScheduleModel, ScheduleSummaryModel,
    ServiceSummaryModel, SessionModel, SpaceSummaryModel, ThreadInspectionModel, ThreadModel,
    ThreadStatus, ThreadUsage,
};
use crate::workspace::InputCapability;
use crate::workspace::ViewSpec;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// AppEvent — events fed into core by platform shells
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum AppEvent {
    Input(InputEvent),
    Daemon(DaemonEvent),
    DaemonBatch(Vec<DaemonEvent>),
    PollSnapshot(PollSnapshot),
    CockpitSnapshot(CockpitSnapshot),
    CockpitItems(CockpitItemsList),
    CockpitItemInspection(CockpitItemInspection),
    CockpitSchedules(CockpitSchedulesList),
    CockpitGcStatus(CockpitGcStatus),
    CockpitFiles(CockpitFilesList),
    CockpitFileRead(CockpitFileRead),
    CockpitThreadInspection(CockpitThreadInspection),
    Resize {
        width: u16,
        height: u16,
    },
    Tick {
        now_ms: u64,
    },
    /// Surface spec changed (file hot-reload or explicit switch).
    SurfaceChanged {
        spec: crate::surface::SurfaceSpec,
    },
}

#[derive(Debug, Clone)]
pub enum DaemonEvent {
    ThreadCreated {
        id: ThreadId,
        item_ref: Option<String>,
    },
    ThreadStarted {
        id: ThreadId,
    },
    ThreadCompleted {
        id: ThreadId,
    },
    ThreadFailed {
        id: ThreadId,
        error: String,
    },
    TextDelta {
        thread_id: ThreadId,
        text: String,
    },
    ToolCallStart {
        thread_id: ThreadId,
        name: String,
    },
    ToolCallResult {
        thread_id: ThreadId,
        name: String,
        duration_ms: Option<u64>,
    },
    UsageUpdate {
        thread_id: ThreadId,
        usage: ThreadUsage,
    },
}

#[derive(Debug, Clone)]
pub struct PollSnapshot {
    pub threads: Vec<ThreadSummary>,
    pub remotes: Vec<RemoteSummary>,
    pub daemon_url: Option<String>,
    pub daemon_alive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: ThreadId,
    #[serde(default)]
    pub daemon_id: Option<String>,
    pub status: String,
    pub item_ref: Option<String>,
    pub parent_id: Option<ThreadId>,
    pub started_at_ms: Option<i64>,
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSummary {
    pub id: crate::ids::RemoteId,
    pub name: String,
    pub url: String,
    pub alive: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitSnapshot {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub session: CockpitSession,
    #[serde(default)]
    pub local_node: CockpitLocalNode,
    #[serde(default)]
    pub project: Option<CockpitProject>,
    #[serde(default)]
    pub remotes: Vec<CockpitRemote>,
    #[serde(default)]
    pub threads: CockpitThreadSummary,
    #[serde(default)]
    pub schedules: CockpitScheduleSummary,
    #[serde(default)]
    pub gc: CockpitGcSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitSession {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub surface_ref: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub granted_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitLocalNode {
    #[serde(default)]
    pub identity: CockpitIdentity,
    #[serde(default)]
    pub status: serde_json::Value,
    #[serde(default)]
    pub health: serde_json::Value,
    #[serde(default)]
    pub spaces: Vec<CockpitSpace>,
    #[serde(default)]
    pub bundles: Vec<CockpitBundle>,
    #[serde(default)]
    pub services: Vec<CockpitService>,
    #[serde(default)]
    pub commands: Vec<CockpitCommand>,
    #[serde(default)]
    pub command_aliases: Vec<CockpitCommand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitIdentity {
    #[serde(default)]
    pub principal_id: String,
    #[serde(default)]
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitSpace {
    #[serde(default)]
    pub space: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitBundle {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitService {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub service_ref: String,
    #[serde(default)]
    pub availability: String,
    #[serde(default)]
    pub required_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitCommand {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitProject {
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitRemote {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub principal_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitThreadSummary {
    #[serde(default)]
    pub active_count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitScheduleSummary {
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub enabled: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitGcSummary {
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub recent_events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitItemsList {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub items: Vec<CockpitItemSummary>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitItemSummary {
    #[serde(default)]
    pub canonical_ref: String,
    #[serde(default)]
    pub item_kind: String,
    #[serde(default)]
    pub bare_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub executable: bool,
    #[serde(default)]
    pub trust: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitItemInspection {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub item: InspectedCockpitItem,
    #[serde(default)]
    pub raw: Option<CockpitRawContent>,
    #[serde(default)]
    pub effective: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InspectedCockpitItem {
    #[serde(default)]
    pub canonical_ref: String,
    #[serde(default)]
    pub item_kind: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub space: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitRawContent {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitSchedulesList {
    #[serde(default)]
    pub schedules: Vec<CockpitSchedule>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitSchedule {
    #[serde(default)]
    pub schedule_id: String,
    #[serde(default)]
    pub item_ref: String,
    #[serde(default)]
    pub schedule_type: String,
    #[serde(default)]
    pub expression: String,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub last_fire_at: Option<i64>,
    #[serde(default)]
    pub last_fire_status: Option<String>,
    #[serde(default)]
    pub total_fires: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitGcStatus {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub state: Option<serde_json::Value>,
    #[serde(default)]
    pub recent_events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitFilesList {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub entries: Vec<CockpitFileEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitFileEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitFileRead {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: usize,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CockpitThreadInspection {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub thread: serde_json::Value,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub artifacts: Vec<serde_json::Value>,
    #[serde(default)]
    pub children: Vec<serde_json::Value>,
    #[serde(default)]
    pub facets: Option<serde_json::Value>,
    #[serde(default)]
    pub events: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Update entry point
// ---------------------------------------------------------------------------

/// Process an event, mutate the model, return effects to perform.
pub fn update(model: &mut AppModel, event: AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::Resize { width, height } => {
            model.runtime.viewport = Rect::new(0, 0, width, height);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::Tick { now_ms } => {
            let delta = now_ms.saturating_sub(model.visual.animation.time_ms);
            model.visual.animation.tick(delta, &model.store);
            // Only mark dirty if animation would change visuals
            // (always does for now since substrate animates continuously)
            model.dirty = true;
            Vec::new()
        }

        AppEvent::Input(input) => handle_input(model, input),

        AppEvent::Daemon(event) => {
            reduce_daemon_event(model, &event);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::DaemonBatch(events) => {
            for event in &events {
                reduce_daemon_event(model, event);
            }
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::PollSnapshot(snapshot) => {
            apply_poll_snapshot(model, &snapshot);
            model.mark_dirty();
            vec![Effect::RefreshState]
        }

        AppEvent::CockpitSnapshot(snapshot) => {
            apply_cockpit_snapshot(model, snapshot);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::CockpitItems(items) => {
            apply_cockpit_items(model, items);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::CockpitItemInspection(inspection) => {
            apply_cockpit_item_inspection(model, inspection);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::CockpitSchedules(schedules) => {
            apply_cockpit_schedules(model, schedules);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::CockpitGcStatus(gc) => {
            apply_cockpit_gc_status(model, gc);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::CockpitFiles(files) => {
            apply_cockpit_files(model, files);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::CockpitFileRead(file) => {
            apply_cockpit_file_read(model, file);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::CockpitThreadInspection(inspection) => {
            apply_cockpit_thread_inspection(model, inspection);
            model.mark_dirty();
            Vec::new()
        }

        AppEvent::SurfaceChanged { spec } => {
            model.surface.spec = spec;
            model.workspace = model.surface.rebuild_workspace();
            model.mark_dirty();
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

fn handle_input(model: &mut AppModel, event: InputEvent) -> Vec<Effect> {
    match event {
        InputEvent::Key(key) => handle_key(model, key),
        InputEvent::Mouse(mouse) => handle_mouse(model, mouse),
    }
}

fn handle_key(model: &mut AppModel, key: Key) -> Vec<Effect> {
    // Overlay captures input first
    if model.overlay.is_some() {
        return handle_overlay_input(model, key);
    }

    // --- Hardcoded input editing keys (not configurable) ---
    // These are terminal conventions that must always work.
    match key {
        Key::Backspace => {
            model.workspace.input_bar.backspace();
            model.mark_dirty();
            return Vec::new();
        }
        Key::Delete => {
            model.workspace.input_bar.delete();
            model.mark_dirty();
            return Vec::new();
        }
        Key::ArrowLeft => {
            model.workspace.input_bar.move_left();
            model.mark_dirty();
            return Vec::new();
        }
        Key::ArrowRight => {
            model.workspace.input_bar.move_right();
            model.mark_dirty();
            return Vec::new();
        }
        Key::Home => {
            model.workspace.input_bar.move_home();
            model.mark_dirty();
            return Vec::new();
        }
        Key::End => {
            model.workspace.input_bar.move_end();
            model.mark_dirty();
            return Vec::new();
        }
        Key::CtrlEnter | Key::ShiftEnter => {
            model.workspace.input_bar.insert_char('\n');
            model.mark_dirty();
            return Vec::new();
        }
        _ => {}
    }

    // --- Keymap-driven actions ---
    let action = model.keymap.lookup(&key).map(String::from);
    if let Some(action) = action {
        return dispatch_keymap_action(model, &action);
    }

    // --- Char keys: type into input bar if capable ---
    if let Key::Char(ch) = key {
        let cap = model.workspace.focused_capability();
        if cap == InputCapability::Prompt || cap == InputCapability::Filter {
            model.workspace.input_bar.insert_char(ch);
            model.mark_dirty();
        }
    }

    Vec::new()
}

/// Dispatch a keymap action by ID.
fn dispatch_keymap_action(model: &mut AppModel, action: &str) -> Vec<Effect> {
    match action {
        // Navigation actions — inline handling
        "nav.up" => {
            model.workspace.cursor_up();
            model.mark_dirty();
            Vec::new()
        }
        "nav.down" => {
            let count = list_item_count(model);
            model.workspace.cursor_down(count);
            model.mark_dirty();
            Vec::new()
        }
        "nav.page_up" => {
            for _ in 0..10 {
                model.workspace.cursor_up();
            }
            model.mark_dirty();
            Vec::new()
        }
        "nav.page_down" => {
            let count = list_item_count(model);
            for _ in 0..10 {
                model.workspace.cursor_down(count);
            }
            model.mark_dirty();
            Vec::new()
        }
        "nav.toggle_expand" => {
            if let Some(tile) = model.workspace.tiles.get_mut(&model.workspace.focused_tile) {
                if let crate::workspace::ViewLocalState::Thread(ref mut state) = tile.local {
                    let turn_id = state.timeline_cursor as u64;
                    if state.expanded_turns.contains(&turn_id) {
                        state.expanded_turns.remove(&turn_id);
                    } else {
                        state.expanded_turns.insert(turn_id);
                    }
                    model.mark_dirty();
                }
            }
            Vec::new()
        }
        "nav.open" => handle_enter(model),
        // Quit with special case: clear input if non-empty
        "app.quit" => {
            if !model.workspace.input_bar.text.is_empty() {
                model.workspace.input_bar.clear();
                model.mark_dirty();
                Vec::new()
            } else {
                vec![Effect::Quit]
            }
        }
        // All other actions: look up affordance and dispatch
        affordance_id => {
            let affordances = model.active_affordances();
            let invoke = affordances
                .iter()
                .find(|a| a.id == affordance_id)
                .map(|a| a.invoke.clone());
            if let Some(inv) = invoke {
                let (handled, effects) = crate::commands::dispatch_affordance(&inv, model);
                if handled {
                    return effects;
                }
            }
            model.mark_dirty();
            Vec::new()
        }
    }
}

fn handle_enter(model: &mut AppModel) -> Vec<Effect> {
    let cap = model.workspace.focused_capability();
    let text = model.workspace.input_bar.submit();

    match cap {
        InputCapability::Prompt => {
            if text.is_empty() {
                return Vec::new();
            }
            // Return an execute effect with the prompt as parameter
            vec![Effect::Execute {
                project_path: std::path::PathBuf::from("."),
                item_ref: text.clone(),
                parameters: serde_json::json!({ "prompt": text }),
            }]
        }
        InputCapability::Filter => {
            if text.is_empty() {
                // Empty filter + Enter = navigate into selected item
                return handle_list_enter(model);
            }
            // Apply filter to focused list view
            if let Some(tile) = model.workspace.tiles.get_mut(&model.workspace.focused_tile) {
                match &mut tile.local {
                    crate::workspace::ViewLocalState::ThreadList { filter, .. } => {
                        *filter = text;
                    }
                    crate::workspace::ViewLocalState::SpaceBrowser { query, .. } => {
                        *query = text;
                    }
                    _ => {}
                }
            }
            model.mark_dirty();
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Handle mouse events — click to focus, scroll in lists.
fn handle_mouse(model: &mut AppModel, mouse: Mouse) -> Vec<Effect> {
    match mouse.action {
        MouseAction::Press(_) => {
            // Click to focus tile — find which tile contains this point
            let tile_rects =
                crate::layout::layout_rects(&model.workspace.layout, model.runtime.viewport);
            for (tile_id, rect) in &tile_rects {
                let x_in = mouse.x >= rect.x && mouse.x < rect.x + rect.w;
                let y_in = mouse.y >= rect.y && mouse.y < rect.y + rect.h;
                if x_in && y_in {
                    model.workspace.focused_tile = *tile_id;
                    model.mark_dirty();
                    break;
                }
            }
        }
        MouseAction::Scroll(direction) => {
            match direction {
                ScrollDirection::Up => {
                    model.workspace.cursor_up();
                }
                ScrollDirection::Down => {
                    let count = list_item_count(model);
                    model.workspace.cursor_down(count);
                }
            }
            model.mark_dirty();
        }
        MouseAction::Release(_) => {}
    }
    Vec::new()
}

fn handle_overlay_input(model: &mut AppModel, key: Key) -> Vec<Effect> {
    let overlay = model.overlay.take();
    match overlay {
        Some(crate::model::OverlayState::Help) => {
            // Any key dismisses help
            model.mark_dirty();
        }
        Some(crate::model::OverlayState::CommandPalette {
            ref query,
            ref selected,
        }) => {
            match key {
                Key::Escape => {
                    model.mark_dirty();
                }
                Key::Enter => {
                    // Execute the selected matching affordance through the registry
                    let affordances = model.active_affordances();
                    let matches = crate::commands::filter_affordances(&affordances, query);
                    if let Some(aff) = matches.into_iter().nth(*selected) {
                        let (handled, effects) =
                            crate::commands::dispatch_affordance(&aff.invoke, model);
                        // overlay already taken above
                        if handled {
                            return effects;
                        }
                    }
                    model.mark_dirty();
                }
                Key::ArrowDown => {
                    let affordances = model.active_affordances();
                    let matches = crate::commands::filter_affordances(&affordances, query);
                    let max = matches.len().saturating_sub(1);
                    let new_selected = (*selected + 1).min(max);
                    model.overlay = Some(crate::model::OverlayState::CommandPalette {
                        query: query.clone(),
                        selected: new_selected,
                    });
                    model.mark_dirty();
                }
                Key::ArrowUp => {
                    let new_selected = selected.saturating_sub(1);
                    model.overlay = Some(crate::model::OverlayState::CommandPalette {
                        query: query.clone(),
                        selected: new_selected,
                    });
                    model.mark_dirty();
                }
                Key::Char(ch) => {
                    let mut q = query.clone();
                    q.push(ch);
                    model.overlay = Some(crate::model::OverlayState::CommandPalette {
                        query: q,
                        selected: 0,
                    });
                    model.mark_dirty();
                }
                Key::Backspace => {
                    let mut q = query.clone();
                    q.pop();
                    model.overlay = Some(crate::model::OverlayState::CommandPalette {
                        query: q,
                        selected: 0,
                    });
                    model.mark_dirty();
                }
                _ => {
                    model.overlay = Some(crate::model::OverlayState::CommandPalette {
                        query: query.clone(),
                        selected: *selected,
                    });
                }
            }
        }
        Some(crate::model::OverlayState::Confirm {
            ref message,
            ref action,
        }) => {
            match key {
                Key::Char('y') | Key::Char('Y') => {
                    // Dispatch the confirmed affordance
                    let effects = dispatch_affordance_by_id(model, action);
                    model.mark_dirty();
                    return effects;
                }
                Key::Escape | Key::Char('n') | Key::Char('N') => {
                    model.mark_dirty();
                }
                _ => {
                    model.overlay = Some(crate::model::OverlayState::Confirm {
                        message: message.clone(),
                        action: action.clone(),
                    });
                }
            }
        }
        None => {}
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Daemon event reduction
// ---------------------------------------------------------------------------

fn reduce_daemon_event(model: &mut AppModel, event: &DaemonEvent) {
    // Record event
    let (event_type, payload) = match event {
        DaemonEvent::ThreadCreated { id, item_ref } => (
            "thread_created",
            serde_json::json!({ "id": id.0, "item_ref": item_ref }),
        ),
        DaemonEvent::ThreadStarted { id } => ("thread_started", serde_json::json!({ "id": id.0 })),
        DaemonEvent::ThreadCompleted { id } => {
            ("thread_completed", serde_json::json!({ "id": id.0 }))
        }
        DaemonEvent::ThreadFailed { id, error } => (
            "thread_failed",
            serde_json::json!({ "id": id.0, "error": error }),
        ),
        DaemonEvent::TextDelta { thread_id, text } => (
            "text_delta",
            serde_json::json!({ "thread_id": thread_id.0, "text_len": text.len() }),
        ),
        DaemonEvent::ToolCallStart { thread_id, name } => (
            "tool_call_start",
            serde_json::json!({ "thread_id": thread_id.0, "name": name }),
        ),
        DaemonEvent::ToolCallResult {
            thread_id,
            name,
            duration_ms,
        } => (
            "tool_call_result",
            serde_json::json!({ "thread_id": thread_id.0, "name": name, "duration_ms": duration_ms }),
        ),
        DaemonEvent::UsageUpdate { thread_id, usage } => (
            "usage_update",
            serde_json::json!({ "thread_id": thread_id.0, "spend_usd": usage.spend_usd }),
        ),
    };

    model.store.events.push(EventRecord {
        event_type: event_type.to_string(),
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        payload,
    });

    match event {
        DaemonEvent::ThreadCreated { id, item_ref } => {
            let mut thread = ThreadModel::new(*id);
            thread.item_ref = item_ref.clone();
            model.store.threads.insert(*id, thread);
        }
        DaemonEvent::ThreadStarted { id } => {
            if let Some(t) = model.store.threads.get_mut(id) {
                t.status = ThreadStatus::Running;
                t.started_at_ms = Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                );
            }
        }
        DaemonEvent::ThreadCompleted { id } => {
            if let Some(t) = model.store.threads.get_mut(id) {
                t.status = ThreadStatus::Completed;
                t.completed_at_ms = Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                );
                // Flush streaming text into an AssistantMessage part
                if !t.streaming_text.is_empty() {
                    t.parts.push(crate::store::ThreadPart {
                        kind: crate::store::ThreadPartKind::AssistantMessage,
                        text: std::mem::take(&mut t.streaming_text),
                        tool_name: None,
                        child_thread_id: None,
                        duration_ms: None,
                        tokens: None,
                    });
                }
            }
        }
        DaemonEvent::ThreadFailed { id, error } => {
            if let Some(t) = model.store.threads.get_mut(id) {
                t.status = ThreadStatus::Failed;
                t.streaming_text.push_str(&format!("\nERROR: {}", error));
            }
        }
        DaemonEvent::TextDelta { thread_id, text } => {
            if let Some(t) = model.store.threads.get_mut(thread_id) {
                t.streaming_text.push_str(text);
            }
        }
        DaemonEvent::ToolCallStart { thread_id, name } => {
            if let Some(t) = model.store.threads.get_mut(thread_id) {
                t.parts.push(crate::store::ThreadPart {
                    kind: crate::store::ThreadPartKind::ToolCall,
                    text: String::new(),
                    tool_name: Some(name.clone()),
                    child_thread_id: None,
                    duration_ms: None,
                    tokens: None,
                });
            }
        }
        DaemonEvent::ToolCallResult {
            thread_id,
            name,
            duration_ms,
        } => {
            if let Some(t) = model.store.threads.get_mut(thread_id) {
                t.parts.push(crate::store::ThreadPart {
                    kind: crate::store::ThreadPartKind::ToolResult,
                    text: String::new(),
                    tool_name: Some(name.clone()),
                    child_thread_id: None,
                    duration_ms: *duration_ms,
                    tokens: None,
                });
            }
        }
        DaemonEvent::UsageUpdate { thread_id, usage } => {
            if let Some(t) = model.store.threads.get_mut(thread_id) {
                t.usage = usage.clone();
            }
            // Update global budget
            model.store.budget.total_spend_usd = model
                .store
                .threads
                .values()
                .map(|t| t.usage.spend_usd)
                .sum();
            model.store.budget.total_input_tokens = model
                .store
                .threads
                .values()
                .map(|t| t.usage.input_tokens)
                .sum();
            model.store.budget.total_output_tokens = model
                .store
                .threads
                .values()
                .map(|t| t.usage.output_tokens)
                .sum();
        }
    }
}

// ---------------------------------------------------------------------------
// Affordance dispatch
// ---------------------------------------------------------------------------

/// Execute an affordance by ID through the affordance registry.
fn dispatch_affordance_by_id(model: &mut AppModel, affordance_id: &str) -> Vec<Effect> {
    let affordances = model.active_affordances();
    let aff = match affordances.iter().find(|a| a.id == affordance_id) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let (handled, effects) = crate::commands::dispatch_affordance(&aff.invoke, model);
    if handled {
        effects
    } else {
        model.mark_dirty();
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// List navigation
// ---------------------------------------------------------------------------

/// Handle Enter on a list view — open the selected item.
fn handle_list_enter(model: &mut AppModel) -> Vec<Effect> {
    let focused = model.workspace.focused_tile;

    if let Some(tile) = model.workspace.tiles.get(&focused) {
        match &tile.view {
            ViewSpec::ThreadList => {
                // Find the thread at cursor and switch the thread tile to it
                let cursor = match &tile.local {
                    crate::workspace::ViewLocalState::ThreadList { cursor, .. } => *cursor,
                    _ => return Vec::new(),
                };

                let threads = model.store.recent_threads();
                if let Some(selected) = threads.into_iter().nth(cursor) {
                    let thread_id = selected.id;
                    let inspect_thread_id = selected
                        .daemon_id
                        .clone()
                        .unwrap_or_else(|| thread_id.0.to_string());
                    // Find the thread tile (ViewSpec::Thread) and update its thread_id
                    for (tid, t) in model.workspace.tiles.iter_mut() {
                        if matches!(t.view, ViewSpec::Thread { .. }) {
                            t.view = ViewSpec::Thread {
                                thread_id: Some(thread_id),
                            };
                            // Focus the thread tile
                            model.workspace.focused_tile = *tid;
                            break;
                        }
                    }
                    model.mark_dirty();
                    return vec![Effect::InspectThread {
                        thread_id: inspect_thread_id,
                    }];
                }
            }
            ViewSpec::SpaceBrowser { .. } => {
                // Open selected item for execution
                let cursor = match &tile.local {
                    crate::workspace::ViewLocalState::SpaceBrowser { cursor, .. } => *cursor,
                    _ => return Vec::new(),
                };

                let mut items: Vec<_> = model.store.items.values().collect();
                items.sort_by(|a, b| a.name.cmp(&b.name));

                // Apply the same filter the view uses
                let filter = match &tile.local {
                    crate::workspace::ViewLocalState::SpaceBrowser { query, .. } => query.clone(),
                    _ => String::new(),
                };

                let filtered: Vec<_> = items
                    .into_iter()
                    .filter(|item| {
                        if filter.is_empty() {
                            return true;
                        }
                        let f = filter.to_lowercase();
                        item.kind.to_lowercase().contains(&f)
                            || item.name.to_lowercase().contains(&f)
                            || item
                                .description
                                .as_ref()
                                .is_some_and(|d| d.to_lowercase().contains(&f))
                    })
                    .collect();

                if let Some(selected) = filtered.into_iter().nth(cursor) {
                    let item_ref = format!("{}:{}", selected.kind, selected.name);
                    for (tid, tile) in model.workspace.tiles.iter_mut() {
                        if matches!(tile.view, ViewSpec::Thread { .. } | ViewSpec::ItemInspector) {
                            tile.view = ViewSpec::ItemInspector;
                            tile.local = crate::workspace::ViewLocalState::GenericList {
                                cursor: 0,
                                scroll: 0,
                            };
                            model.workspace.focused_tile = *tid;
                            break;
                        }
                    }
                    model.workspace.input_bar.clear();
                    model.mark_dirty();
                    return vec![Effect::InspectItem { item_ref }];
                }
            }
            ViewSpec::Files => {
                let cursor = match &tile.local {
                    crate::workspace::ViewLocalState::GenericList { cursor, .. } => *cursor,
                    _ => return Vec::new(),
                };
                let Some(files) = &model.store.files else {
                    return Vec::new();
                };
                let root = files.root.clone();
                if !files.path.is_empty() && cursor == 0 {
                    let path = parent_relative_path(&files.path);
                    model.workspace.input_bar.clear();
                    model.mark_dirty();
                    return vec![Effect::ListFiles { root, path }];
                }
                let entry_index = cursor.saturating_sub(usize::from(!files.path.is_empty()));
                let Some(entry) = files.entries.get(entry_index) else {
                    return Vec::new();
                };
                let path = join_relative_path(&files.path, &entry.name);
                let is_dir = entry.is_dir;
                model.workspace.input_bar.clear();
                model.mark_dirty();
                if is_dir {
                    return vec![Effect::ListFiles { root, path }];
                }
                return vec![Effect::ReadFile { root, path }];
            }
            _ => {}
        }
    }
    Vec::new()
}

fn join_relative_path(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), name)
    }
}

fn parent_relative_path(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Poll snapshot
// ---------------------------------------------------------------------------

fn apply_poll_snapshot(model: &mut AppModel, snapshot: &PollSnapshot) {
    // Daemon status
    model.store.daemon.status = if snapshot.daemon_alive {
        DaemonStatus::Connected
    } else {
        DaemonStatus::Disconnected
    };
    if let Some(url) = &snapshot.daemon_url {
        model.store.daemon.url = url.clone();
    }

    // Upsert threads from snapshot
    for ts in &snapshot.threads {
        let id = ts.id;
        let thread = model
            .store
            .threads
            .entry(id)
            .or_insert_with(|| ThreadModel::new(id));
        thread.daemon_id = ts.daemon_id.clone().or(thread.daemon_id.clone());
        thread.item_ref = ts.item_ref.clone().or(thread.item_ref.clone());
        thread.started_at_ms = ts.started_at_ms.or(thread.started_at_ms);
        // Parse status string
        if let Ok(status) = parse_thread_status(&ts.status) {
            thread.status = status;
        }
    }

    // Upsert remotes
    for rs in &snapshot.remotes {
        let id = rs.id;
        let remote = model
            .store
            .remotes
            .entry(id)
            .or_insert_with(|| crate::store::RemoteModel {
                id,
                name: rs.name.clone(),
                url: rs.url.clone(),
                alive: rs.alive,
                last_seen_ms: None,
                sync_state: crate::store::RemoteSyncState::Unknown,
                capabilities: Vec::new(),
                trust_fingerprint: String::new(),
                trust_pinned: false,
            });
        remote.alive = rs.alive;
        remote.name = rs.name.clone();
        remote.url = rs.url.clone();
    }
}

fn apply_cockpit_snapshot(model: &mut AppModel, snapshot: CockpitSnapshot) {
    let health_status = snapshot
        .local_node
        .health
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let operational_services = snapshot
        .local_node
        .health
        .get("operational_services")
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            _ => v.to_string(),
        })
        .unwrap_or_else(|| "unknown".into());
    let missing_services = snapshot
        .local_node
        .health
        .get("missing_services")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    model.store.daemon.status = if health_status == "healthy" || health_status == "degraded" {
        DaemonStatus::Connected
    } else {
        DaemonStatus::Disconnected
    };
    model.store.daemon.active_threads = snapshot.threads.active_count.max(0) as u32;

    if !snapshot.local_node.identity.fingerprint.is_empty() {
        model.store.identity = Some(IdentityModel {
            fingerprint: snapshot.local_node.identity.fingerprint.clone(),
            has_signing_key: true,
        });
    }

    if let Some(project) = &snapshot.project {
        let id = crate::ids::ProjectId::new(stable_hash_id(&project.path));
        let name = project
            .path
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .unwrap_or(&project.path)
            .to_string();
        model.store.projects.insert(
            id,
            ProjectModel {
                id,
                path: project.path.clone(),
                name,
                item_counts: Default::default(),
            },
        );
    }

    for remote in &snapshot.remotes {
        let id = crate::ids::RemoteId::new(stable_hash_id(&format!(
            "{}\0{}\0{}",
            remote.name, remote.url, remote.principal_id
        )));
        let entry = model
            .store
            .remotes
            .entry(id)
            .or_insert_with(|| crate::store::RemoteModel {
                id,
                name: remote.name.clone(),
                url: remote.url.clone(),
                alive: true,
                last_seen_ms: None,
                sync_state: crate::store::RemoteSyncState::Unknown,
                capabilities: Vec::new(),
                trust_fingerprint: remote.principal_id.clone(),
                trust_pinned: false,
            });
        entry.name = remote.name.clone();
        entry.url = remote.url.clone();
        entry.alive = true;
        entry.trust_fingerprint = remote.principal_id.clone();
    }

    model.store.cockpit = Some(CockpitSnapshotModel {
        schema_version: snapshot.schema_version,
        generated_at: snapshot.generated_at,
        session: SessionModel {
            session_id: snapshot.session.session_id,
            surface_ref: snapshot.session.surface_ref,
            read_only: snapshot.session.read_only,
            granted_caps: snapshot.session.granted_caps,
        },
        local_node: LocalNodeModel {
            identity: IdentityInfoModel {
                principal_id: snapshot.local_node.identity.principal_id,
                fingerprint: snapshot.local_node.identity.fingerprint,
            },
            health_status,
            operational_services,
            missing_services,
            spaces: snapshot
                .local_node
                .spaces
                .into_iter()
                .map(|space| SpaceSummaryModel {
                    space: space.space,
                    label: space.label,
                    path: space.path,
                })
                .collect(),
            bundles: snapshot
                .local_node
                .bundles
                .into_iter()
                .map(|bundle| BundleSummaryModel {
                    name: bundle.name,
                    path: bundle.path,
                })
                .collect(),
            services: snapshot
                .local_node
                .services
                .into_iter()
                .map(|service| ServiceSummaryModel {
                    endpoint: service.endpoint,
                    service_ref: service.service_ref,
                    availability: service.availability,
                    required_caps: service.required_caps,
                })
                .collect(),
            commands: snapshot
                .local_node
                .commands
                .into_iter()
                .map(|verb| CommandSummaryModel {
                    name: verb.name,
                    target: verb.target,
                })
                .collect(),
            command_aliases: snapshot
                .local_node
                .command_aliases
                .into_iter()
                .map(|alias| CommandSummaryModel {
                    name: alias.name,
                    target: alias.target,
                })
                .collect(),
        },
        project: snapshot
            .project
            .map(|project| ProjectInfoModel { path: project.path }),
        schedules: ScheduleSummaryModel {
            total: snapshot.schedules.total,
            enabled: snapshot.schedules.enabled,
        },
        gc: GcSummaryModel {
            running: snapshot.gc.running,
            recent_event_count: snapshot.gc.recent_events.len(),
        },
    });
}

fn apply_cockpit_items(model: &mut AppModel, list: CockpitItemsList) {
    model.store.items.clear();

    for item in list.items {
        let item_ref = if item.canonical_ref.is_empty() {
            format!("{}:{}", item.item_kind, item.bare_id)
        } else {
            item.canonical_ref.clone()
        };
        let id = crate::ids::ItemId::new(stable_hash_id(&item_ref));
        let signed = item
            .trust
            .as_ref()
            .is_some_and(|trust| trust.get("signer").and_then(|v| v.as_str()).is_some());
        let description = if item.source_path.is_empty() {
            item.namespace.clone()
        } else {
            Some(item.source_path.clone())
        };

        model.store.items.insert(
            id,
            ItemModel {
                id,
                kind: item.item_kind,
                name: item.bare_id,
                category: item.namespace,
                description,
                signed,
            },
        );
    }
}

fn apply_cockpit_item_inspection(model: &mut AppModel, inspection: CockpitItemInspection) {
    model.store.item_inspection = Some(ItemInspectionModel {
        canonical_ref: inspection.item.canonical_ref,
        item_kind: inspection.item.item_kind,
        source_path: inspection.item.source_path,
        space: inspection.item.space,
        raw_content: inspection.raw.as_ref().map(|raw| raw.content.clone()),
        raw_truncated: inspection.raw.is_some_and(|raw| raw.truncated),
        effective: inspection.effective,
    });
}

fn apply_cockpit_schedules(model: &mut AppModel, list: CockpitSchedulesList) {
    model.store.schedules = ScheduleListModel {
        schedules: list
            .schedules
            .into_iter()
            .map(|schedule| ScheduleModel {
                schedule_id: schedule.schedule_id,
                item_ref: schedule.item_ref,
                schedule_type: schedule.schedule_type,
                expression: schedule.expression,
                timezone: schedule.timezone,
                enabled: schedule.enabled,
                last_fire_at: schedule.last_fire_at,
                last_fire_status: schedule.last_fire_status,
                total_fires: schedule.total_fires,
            })
            .collect(),
    };
}

fn apply_cockpit_gc_status(model: &mut AppModel, gc: CockpitGcStatus) {
    model.store.gc_status = Some(GcStatusModel {
        running: gc.running,
        state: gc.state,
        recent_events: gc.recent_events,
    });
}

fn apply_cockpit_files(model: &mut AppModel, files: CockpitFilesList) {
    let listing_changed = model
        .store
        .files
        .as_ref()
        .map(|current| current.root != files.root || current.path != files.path)
        .unwrap_or(true);
    model.store.files = Some(FileBrowserModel {
        root: files.root,
        path: files.path,
        truncated: files.truncated,
        entries: files
            .entries
            .into_iter()
            .map(|entry| FileEntryModel {
                name: entry.name,
                is_dir: entry.is_dir,
                size: entry.size,
                modified: entry.modified,
            })
            .collect(),
    });
    if listing_changed {
        model.store.file_read = None;
    }
}

fn apply_cockpit_file_read(model: &mut AppModel, file: CockpitFileRead) {
    model.store.file_read = Some(FileReadModel {
        root: file.root,
        path: file.path,
        size: file.size,
        truncated: file.truncated,
        content: file.content,
    });
}

fn apply_cockpit_thread_inspection(model: &mut AppModel, inspection: CockpitThreadInspection) {
    let thread = &inspection.thread;
    model.store.thread_inspection = Some(ThreadInspectionModel {
        thread_id: string_field(thread, "thread_id"),
        status: string_field(thread, "status"),
        item_ref: string_field(thread, "item_ref"),
        kind: string_field(thread, "kind"),
        created_at: string_field(thread, "created_at"),
        started_at: optional_string_field(thread, "started_at"),
        finished_at: optional_string_field(thread, "finished_at"),
        children: inspection.children,
        events: inspection.events,
        result: inspection.result,
        artifacts: inspection.artifacts,
        facets: inspection.facets,
    });
}

fn string_field(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn optional_string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    value.get(field).and_then(|v| v.as_str()).map(String::from)
}

fn stable_hash_id(raw: &str) -> u64 {
    raw.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        hash.wrapping_mul(0x100000001b3) ^ u64::from(byte)
    })
}

fn parse_thread_status(s: &str) -> Result<ThreadStatus, ()> {
    match s {
        "created" => Ok(ThreadStatus::Created),
        "running" => Ok(ThreadStatus::Running),
        "completed" => Ok(ThreadStatus::Completed),
        "failed" => Ok(ThreadStatus::Failed),
        "cancelled" => Ok(ThreadStatus::Cancelled),
        "killed" => Ok(ThreadStatus::Killed),
        "timed_out" => Ok(ThreadStatus::TimedOut),
        "continued" => Ok(ThreadStatus::Continued),
        _ => Err(()),
    }
}

/// Get the number of list items for the focused view, for cursor bounds.
fn list_item_count(model: &AppModel) -> usize {
    model
        .workspace
        .tiles
        .get(&model.workspace.focused_tile)
        .map(|tile| match &tile.view {
            ViewSpec::ThreadList => model.store.recent_threads().len(),
            ViewSpec::SpaceBrowser { .. } => model.store.items.len(),
            ViewSpec::Files => model
                .store
                .files
                .as_ref()
                .map(|files| files.entries.len() + usize::from(!files.path.is_empty()))
                .unwrap_or(0),
            ViewSpec::Projects => model.store.projects.len(),
            ViewSpec::Overview => 0,
            ViewSpec::Services => model
                .store
                .cockpit
                .as_ref()
                .map(|snapshot| snapshot.local_node.services.len())
                .unwrap_or(0),
            ViewSpec::Remotes => model.store.remotes.len(),
            ViewSpec::Trust => model.store.trust_alerts.len(),
            ViewSpec::Atlas => model.store.items.len(),
            ViewSpec::Thread { .. } => {
                // Parts count for thread view
                model
                    .store
                    .threads
                    .values()
                    .next()
                    .map(|t| t.parts.len())
                    .unwrap_or(0)
            }
            _ => 0,
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_updates_viewport_and_marks_dirty() {
        let mut model = AppModel::new_default("/tmp/test");
        model.dirty = false;
        let effects = update(
            &mut model,
            AppEvent::Resize {
                width: 100,
                height: 40,
            },
        );
        assert!(effects.is_empty());
        assert_eq!(model.runtime.viewport.w, 100);
        assert_eq!(model.runtime.viewport.h, 40);
        assert!(model.dirty);
    }

    #[test]
    fn tick_updates_animation_and_marks_dirty() {
        let mut model = AppModel::new_default("/tmp/test");
        model.dirty = false;
        let effects = update(&mut model, AppEvent::Tick { now_ms: 1000 });
        assert!(effects.is_empty());
        assert!(model.dirty);
    }

    #[test]
    fn daemon_batch_reduces_in_order_and_increments_generation_once() {
        let mut model = AppModel::new_default("/tmp/test");
        let gen_before = model.generation;

        let tid = ThreadId::new(42);
        let effects = update(
            &mut model,
            AppEvent::DaemonBatch(vec![
                DaemonEvent::ThreadCreated {
                    id: tid,
                    item_ref: Some("test:directive".into()),
                },
                DaemonEvent::ThreadStarted { id: tid },
                DaemonEvent::TextDelta {
                    thread_id: tid,
                    text: "hello".into(),
                },
            ]),
        );

        assert!(effects.is_empty());
        assert_eq!(model.generation, gen_before + 1); // only one increment
        assert_eq!(model.store.threads.len(), 1);
        let thread = &model.store.threads[&tid];
        assert_eq!(thread.status, ThreadStatus::Running);
        assert_eq!(thread.streaming_text, "hello");
    }

    #[test]
    fn poll_snapshot_populates_threads_and_remotes() {
        let mut model = AppModel::new_default("/tmp/test");

        let effects = update(
            &mut model,
            AppEvent::PollSnapshot(PollSnapshot {
                threads: vec![ThreadSummary {
                    id: ThreadId::new(1),
                    daemon_id: Some("T-test-1".into()),
                    status: "completed".into(),
                    item_ref: Some("test:tool".into()),
                    parent_id: None,
                    started_at_ms: Some(1000),
                    duration_ms: Some(5000),
                    cost_usd: Some(0.03),
                }],
                remotes: vec![RemoteSummary {
                    id: crate::ids::RemoteId::new(1),
                    name: "default".into(),
                    url: "http://remote:7400".into(),
                    alive: true,
                }],
                daemon_url: Some("http://localhost:7400".into()),
                daemon_alive: true,
            }),
        );

        assert_eq!(effects.len(), 1); // RefreshState
        assert_eq!(model.store.daemon.status, DaemonStatus::Connected);
        assert_eq!(model.store.threads.len(), 1);
        assert_eq!(
            model
                .store
                .threads
                .values()
                .next()
                .unwrap()
                .daemon_id
                .as_deref(),
            Some("T-test-1")
        );
        assert_eq!(model.store.remotes.len(), 1);
    }

    #[test]
    fn enter_on_thread_list_requests_thread_inspection() {
        let mut model = AppModel::new_default("/tmp/test");
        update(
            &mut model,
            AppEvent::PollSnapshot(PollSnapshot {
                threads: vec![ThreadSummary {
                    id: ThreadId::new(42),
                    daemon_id: Some("T-thread-42".into()),
                    status: "completed".into(),
                    item_ref: Some("directive:apps/demo/build".into()),
                    parent_id: None,
                    started_at_ms: Some(1000),
                    duration_ms: Some(5000),
                    cost_usd: Some(0.03),
                }],
                remotes: Vec::new(),
                daemon_url: None,
                daemon_alive: true,
            }),
        );

        let thread_list_tile = model.workspace.focused_tile;
        let tile = model.workspace.tiles.get_mut(&thread_list_tile).unwrap();
        tile.view = ViewSpec::ThreadList;
        tile.local = crate::workspace::ViewLocalState::ThreadList {
            cursor: 0,
            filter: String::new(),
        };

        let effects = update(&mut model, AppEvent::Input(InputEvent::Key(Key::Enter)));
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::InspectThread { thread_id } if thread_id == "T-thread-42"
        ));
        assert!(model
            .workspace
            .tiles
            .values()
            .any(|tile| matches!(tile.view, ViewSpec::Thread { thread_id: Some(id) } if id == ThreadId::new(42))));
    }

    #[test]
    fn cockpit_thread_inspection_populates_detail_model() {
        let mut model = AppModel::new_default("/tmp/test");

        let effects = update(
            &mut model,
            AppEvent::CockpitThreadInspection(CockpitThreadInspection {
                schema_version: "cockpit.thread.inspect.v1".into(),
                thread: serde_json::json!({
                    "thread_id": "T-thread-42",
                    "status": "completed",
                    "item_ref": "directive:apps/demo/build",
                    "kind": "directive",
                    "created_at": "2026-05-29T00:00:00Z"
                }),
                result: Some(serde_json::json!({ "ok": true })),
                children: vec![serde_json::json!({ "thread_id": "T-child" })],
                events: vec![serde_json::json!({ "event_type": "thread.completed" })],
                ..Default::default()
            }),
        );

        assert!(effects.is_empty());
        let inspection = model.store.thread_inspection.unwrap();
        assert_eq!(inspection.thread_id, "T-thread-42");
        assert_eq!(inspection.children.len(), 1);
        assert_eq!(inspection.events.len(), 1);
        assert_eq!(inspection.result.unwrap()["ok"], true);
    }

    #[test]
    fn cockpit_snapshot_populates_operational_model() {
        let mut model = AppModel::new_default("/tmp/test");

        let effects = update(
            &mut model,
            AppEvent::CockpitSnapshot(CockpitSnapshot {
                schema_version: "cockpit.snapshot.v1".into(),
                generated_at: "2026-05-29T00:00:00Z".into(),
                session: CockpitSession {
                    session_id: "session-1".into(),
                    surface_ref: "surface:ryeos/cockpit/base".into(),
                    read_only: false,
                    granted_caps: vec!["execute".into()],
                },
                local_node: CockpitLocalNode {
                    identity: CockpitIdentity {
                        principal_id: "principal".into(),
                        fingerprint: "abcdef1234567890".into(),
                    },
                    health: serde_json::json!({
                        "status": "healthy",
                        "operational_services": "all",
                        "missing_services": [],
                    }),
                    spaces: vec![CockpitSpace {
                        space: "project".into(),
                        label: "Project".into(),
                        path: "/tmp/test/.ai".into(),
                    }],
                    services: vec![CockpitService {
                        endpoint: "ui.cockpit.snapshot".into(),
                        service_ref: "service:ui/cockpit/snapshot".into(),
                        availability: "DaemonOnly".into(),
                        required_caps: Vec::new(),
                    }],
                    ..Default::default()
                },
                project: Some(CockpitProject {
                    path: "/tmp/test".into(),
                }),
                remotes: vec![CockpitRemote {
                    name: "default".into(),
                    url: "http://remote:7400".into(),
                    principal_id: "remote-principal".into(),
                }],
                threads: CockpitThreadSummary { active_count: 2 },
                schedules: CockpitScheduleSummary {
                    total: 3,
                    enabled: 1,
                },
                gc: CockpitGcSummary {
                    running: false,
                    recent_events: vec![serde_json::json!({"event": "sweep"})],
                },
            }),
        );

        assert!(effects.is_empty());
        assert_eq!(model.store.daemon.status, DaemonStatus::Connected);
        assert_eq!(model.store.daemon.active_threads, 2);
        assert_eq!(model.store.projects.len(), 1);
        assert_eq!(model.store.remotes.len(), 1);
        let cockpit = model
            .store
            .cockpit
            .expect("cockpit snapshot should be stored");
        assert_eq!(cockpit.local_node.health_status, "healthy");
        assert_eq!(cockpit.local_node.services.len(), 1);
        assert_eq!(cockpit.schedules.enabled, 1);
        assert_eq!(cockpit.gc.recent_event_count, 1);
    }

    #[test]
    fn cockpit_items_populate_space_browser() {
        let mut model = AppModel::new_default("/tmp/test");

        let effects = update(
            &mut model,
            AppEvent::CockpitItems(CockpitItemsList {
                schema_version: "cockpit.items.v1".into(),
                items: vec![CockpitItemSummary {
                    canonical_ref: "tool:apps/demo/build".into(),
                    item_kind: "tool".into(),
                    bare_id: "apps/demo/build".into(),
                    label: "build".into(),
                    namespace: Some("apps/demo".into()),
                    source_path: "/tmp/test/.ai/tools/apps/demo/build.py".into(),
                    executable: true,
                    trust: Some(serde_json::json!({
                        "class": "trusted",
                        "signer": "abcdef123456",
                    })),
                }],
            }),
        );

        assert!(effects.is_empty());
        assert_eq!(model.store.items.len(), 1);
        let item = model.store.items.values().next().unwrap();
        assert_eq!(item.kind, "tool");
        assert_eq!(item.name, "apps/demo/build");
        assert!(item.signed);
    }

    #[test]
    fn enter_on_item_requests_read_only_inspection() {
        let mut model = AppModel::new_default("/tmp/test");
        update(
            &mut model,
            AppEvent::CockpitItems(CockpitItemsList {
                schema_version: "cockpit.items.v1".into(),
                items: vec![CockpitItemSummary {
                    canonical_ref: "directive:apps/demo/deploy".into(),
                    item_kind: "directive".into(),
                    bare_id: "apps/demo/deploy".into(),
                    namespace: Some("apps/demo".into()),
                    ..Default::default()
                }],
            }),
        );
        let item_tile = model.workspace.focused_tile;
        let tile = model.workspace.tiles.get_mut(&item_tile).unwrap();
        tile.view = ViewSpec::SpaceBrowser { project: None };
        tile.local = crate::workspace::ViewLocalState::SpaceBrowser {
            cursor: 0,
            query: String::new(),
            kind: String::new(),
            path: String::new(),
            scroll: 0,
        };

        let effects = update(&mut model, AppEvent::Input(InputEvent::Key(Key::Enter)));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::InspectItem { item_ref } if item_ref == "directive:apps/demo/deploy"
        ));
        assert!(model
            .workspace
            .tiles
            .values()
            .any(|tile| matches!(tile.view, ViewSpec::ItemInspector)));
    }

    #[test]
    fn cockpit_item_inspection_populates_inspector_model() {
        let mut model = AppModel::new_default("/tmp/test");

        let effects = update(
            &mut model,
            AppEvent::CockpitItemInspection(CockpitItemInspection {
                schema_version: "cockpit.item.inspect.v1".into(),
                item: InspectedCockpitItem {
                    canonical_ref: "tool:apps/demo/build".into(),
                    item_kind: "tool".into(),
                    source_path: "/tmp/test/.ai/tools/apps/demo/build.py".into(),
                    space: "project".into(),
                },
                raw: Some(CockpitRawContent {
                    content: "print('hello')".into(),
                    truncated: false,
                }),
                effective: Some(serde_json::json!({ "name": "build" })),
            }),
        );

        assert!(effects.is_empty());
        let inspection = model.store.item_inspection.unwrap();
        assert_eq!(inspection.canonical_ref, "tool:apps/demo/build");
        assert_eq!(inspection.raw_content.as_deref(), Some("print('hello')"));
        assert_eq!(inspection.effective.unwrap()["name"], "build");
    }

    #[test]
    fn cockpit_schedules_and_gc_populate_operational_models() {
        let mut model = AppModel::new_default("/tmp/test");

        let effects = update(
            &mut model,
            AppEvent::CockpitSchedules(CockpitSchedulesList {
                schedules: vec![CockpitSchedule {
                    schedule_id: "nightly".into(),
                    item_ref: "directive:apps/demo/nightly".into(),
                    schedule_type: "cron".into(),
                    expression: "0 0 * * *".into(),
                    timezone: Some("UTC".into()),
                    enabled: true,
                    last_fire_status: Some("completed".into()),
                    total_fires: 7,
                    ..Default::default()
                }],
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(model.store.schedules.schedules.len(), 1);
        assert_eq!(model.store.schedules.schedules[0].schedule_id, "nightly");

        let effects = update(
            &mut model,
            AppEvent::CockpitGcStatus(CockpitGcStatus {
                schema_version: "cockpit.gc.status.v1".into(),
                running: true,
                state: Some(serde_json::json!({ "phase": "sweep" })),
                recent_events: vec![serde_json::json!({ "event": "started" })],
            }),
        );
        assert!(effects.is_empty());
        let gc = model.store.gc_status.unwrap();
        assert!(gc.running);
        assert_eq!(gc.state.unwrap()["phase"], "sweep");
        assert_eq!(gc.recent_events.len(), 1);
    }

    #[test]
    fn cockpit_files_populate_safe_file_browser_model() {
        let mut model = AppModel::new_default("/tmp/test");

        let effects = update(
            &mut model,
            AppEvent::CockpitFiles(CockpitFilesList {
                root: "project_ai".into(),
                path: "directives".into(),
                truncated: false,
                entries: vec![CockpitFileEntry {
                    name: "build.md".into(),
                    is_dir: false,
                    size: Some(2048),
                    modified: Some("now".into()),
                }],
            }),
        );

        assert!(effects.is_empty());
        let files = model.store.files.unwrap();
        assert_eq!(files.root, "project_ai");
        assert_eq!(files.path, "directives");
        assert_eq!(files.entries.len(), 1);
        assert_eq!(files.entries[0].name, "build.md");
        assert_eq!(files.entries[0].size, Some(2048));
    }

    #[test]
    fn enter_on_files_requests_directory_listing_or_file_read() {
        let mut model = AppModel::new_default("/tmp/test");
        update(
            &mut model,
            AppEvent::CockpitFiles(CockpitFilesList {
                root: "project_ai".into(),
                path: "".into(),
                entries: vec![
                    CockpitFileEntry {
                        name: "directives".into(),
                        is_dir: true,
                        ..Default::default()
                    },
                    CockpitFileEntry {
                        name: "README.md".into(),
                        is_dir: false,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
        );

        let files_tile = model.workspace.focused_tile;
        let tile = model.workspace.tiles.get_mut(&files_tile).unwrap();
        tile.view = ViewSpec::Files;
        tile.local = crate::workspace::ViewLocalState::GenericList {
            cursor: 0,
            scroll: 0,
        };

        let effects = update(&mut model, AppEvent::Input(InputEvent::Key(Key::Enter)));
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::ListFiles { root, path } if root == "project_ai" && path == "directives"
        ));

        let tile = model.workspace.tiles.get_mut(&files_tile).unwrap();
        tile.local = crate::workspace::ViewLocalState::GenericList {
            cursor: 1,
            scroll: 0,
        };
        let effects = update(&mut model, AppEvent::Input(InputEvent::Key(Key::Enter)));
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::ReadFile { root, path } if root == "project_ai" && path == "README.md"
        ));

        update(
            &mut model,
            AppEvent::CockpitFiles(CockpitFilesList {
                root: "project_ai".into(),
                path: "directives/build".into(),
                entries: Vec::new(),
                ..Default::default()
            }),
        );
        let tile = model.workspace.tiles.get_mut(&files_tile).unwrap();
        tile.local = crate::workspace::ViewLocalState::GenericList {
            cursor: 0,
            scroll: 0,
        };
        let effects = update(&mut model, AppEvent::Input(InputEvent::Key(Key::Enter)));
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::ListFiles { root, path } if root == "project_ai" && path == "directives"
        ));
    }

    #[test]
    fn cockpit_file_read_populates_file_preview_model() {
        let mut model = AppModel::new_default("/tmp/test");

        let effects = update(
            &mut model,
            AppEvent::CockpitFileRead(CockpitFileRead {
                root: "project_ai".into(),
                path: "README.md".into(),
                size: 12,
                truncated: false,
                content: "hello world".into(),
            }),
        );

        assert!(effects.is_empty());
        let file = model.store.file_read.unwrap();
        assert_eq!(file.path, "README.md");
        assert_eq!(file.content, "hello world");
    }

    #[test]
    fn cockpit_files_refresh_preserves_preview_for_same_directory() {
        let mut model = AppModel::new_default("/tmp/test");
        update(
            &mut model,
            AppEvent::CockpitFiles(CockpitFilesList {
                root: "project_ai".into(),
                path: "directives".into(),
                entries: vec![CockpitFileEntry {
                    name: "README.md".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        );
        update(
            &mut model,
            AppEvent::CockpitFileRead(CockpitFileRead {
                root: "project_ai".into(),
                path: "directives/README.md".into(),
                content: "preview".into(),
                ..Default::default()
            }),
        );

        update(
            &mut model,
            AppEvent::CockpitFiles(CockpitFilesList {
                root: "project_ai".into(),
                path: "directives".into(),
                entries: Vec::new(),
                ..Default::default()
            }),
        );
        assert!(model.store.file_read.is_some());

        update(
            &mut model,
            AppEvent::CockpitFiles(CockpitFilesList {
                root: "project_ai".into(),
                path: "tools".into(),
                entries: Vec::new(),
                ..Default::default()
            }),
        );
        assert!(model.store.file_read.is_none());
    }

    #[test]
    fn input_enter_on_thread_returns_execute_effect() {
        let mut model = AppModel::new_default("/tmp/test");
        // Focus the thread tile (find it by view spec, not hardcoded ID)
        let thread_tile = model
            .workspace
            .tiles
            .iter()
            .find(|(_, t)| matches!(t.view, ViewSpec::Thread { .. }))
            .map(|(id, _)| *id)
            .expect("workspace must have a thread tile");
        model.workspace.focused_tile = thread_tile;
        model.workspace.input_bar.text = "test prompt".into();
        model.workspace.input_bar.cursor = 12;

        let effects = update(&mut model, AppEvent::Input(InputEvent::Key(Key::Enter)));

        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::Execute { item_ref, .. } => {
                assert_eq!(item_ref, "test prompt");
            }
            _ => panic!("expected Execute effect"),
        }
    }

    #[test]
    fn tab_cycles_focus() {
        let mut model = AppModel::new_default("/tmp/test");
        let initial = model.workspace.focused_tile;
        update(&mut model, AppEvent::Input(InputEvent::Key(Key::Tab)));
        assert_ne!(
            model.workspace.focused_tile, initial,
            "Tab should cycle focus"
        );
    }
}
