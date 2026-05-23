//! Update reducer — processes events and returns effects.
//!
//! Core receives normalized AppEvents, mutates the model, returns Effects.
//! Platform shells execute the effects.

use crate::effects::Effect;
use crate::ids::ThreadId;
use crate::input::{InputEvent, Key, Mouse, MouseAction, ScrollDirection};
use crate::layout::Rect;
use crate::model::AppModel;
use crate::store::{DaemonStatus, EventRecord, ThreadModel, ThreadStatus, ThreadUsage};
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
    Resize { width: u16, height: u16 },
    Tick { now_ms: u64 },
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

    match key {
        // Global keybindings
        Key::Ctrl('c') | Key::Escape => {
            // If focused view is a thread and input is empty, quit
            if model.workspace.input_bar.text.is_empty() {
                return vec![Effect::Quit];
            }
            // Otherwise clear input
            model.workspace.input_bar.clear();
            model.mark_dirty();
            Vec::new()
        }

        Key::Ctrl('n') => {
            // New session — clear input
            model.workspace.input_bar.clear();
            model.mark_dirty();
            Vec::new()
        }

        Key::Ctrl('p') => {
            // Command palette
            model.overlay = Some(crate::model::OverlayState::CommandPalette {
                query: String::new(),
                cursor: 0,
            });
            model.mark_dirty();
            Vec::new()
        }

        Key::Char('?') if model.workspace.input_bar.text.is_empty() => {
            let inv = crate::commands::InvocationSpec::Ui(
                crate::commands::UiInvocation { verb: crate::commands::UiVerb::ToggleHelp, args: serde_json::Value::Null }
            );
            let (_, effects) = crate::commands::dispatch_affordance(&inv, model);
            effects
        }

        // Focus navigation
        Key::Ctrl('w') => {
            let inv = crate::commands::InvocationSpec::Ui(
                crate::commands::UiInvocation { verb: crate::commands::UiVerb::FocusNext, args: serde_json::Value::Null }
            );
            let (_, effects) = crate::commands::dispatch_affordance(&inv, model);
            effects
        }

        // Tile management
        Key::Ctrl('s') => {
            let inv = crate::commands::InvocationSpec::Ui(
                crate::commands::UiInvocation {
                    verb: crate::commands::UiVerb::SplitPane,
                    args: serde_json::json!({ "axis": "horizontal", "view": "thread_list" }),
                }
            );
            let (_, effects) = crate::commands::dispatch_affordance(&inv, model);
            effects
        }

        Key::Ctrl('v') => {
            let inv = crate::commands::InvocationSpec::Ui(
                crate::commands::UiInvocation {
                    verb: crate::commands::UiVerb::SplitPane,
                    args: serde_json::json!({ "axis": "vertical", "view": "event_inspector" }),
                }
            );
            let (_, effects) = crate::commands::dispatch_affordance(&inv, model);
            effects
        }

        Key::Ctrl('x') => {
            let inv = crate::commands::InvocationSpec::Ui(
                crate::commands::UiInvocation { verb: crate::commands::UiVerb::ClosePane, args: serde_json::Value::Null }
            );
            let (_, effects) = crate::commands::dispatch_affordance(&inv, model);
            effects
        }

        Key::Ctrl('r') => {
            let inv = crate::commands::InvocationSpec::Ui(
                crate::commands::UiInvocation { verb: crate::commands::UiVerb::ResetLayout, args: serde_json::Value::Null }
            );
            let (_, effects) = crate::commands::dispatch_affordance(&inv, model);
            effects
        }

        // List navigation (when input is empty)
        Key::Char('j') if model.workspace.input_bar.text.is_empty() => {
            let count = list_item_count(model);
            model.workspace.cursor_down(count);
            model.mark_dirty();
            Vec::new()
        }

        Key::Char('k') if model.workspace.input_bar.text.is_empty() => {
            model.workspace.cursor_up();
            model.mark_dirty();
            Vec::new()
        }

        Key::Char(' ') if model.workspace.input_bar.text.is_empty() => {
            // Toggle expand/collapse in thread view
            if let Some(tile) = model.workspace.tiles.get_mut(&model.workspace.focused_tile) {
                if let crate::workspace::ViewLocalState::Thread(ref mut state) = tile.local {
                    // Toggle the turn at the cursor
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

        // Scrolling
        Key::PageUp => {
            // Move cursor up by ~10
            for _ in 0..10 {
                model.workspace.cursor_up();
            }
            model.mark_dirty();
            Vec::new()
        }

        Key::PageDown => {
            // Move cursor down by ~10
            let count = list_item_count(model);
            for _ in 0..10 {
                model.workspace.cursor_down(count);
            }
            model.mark_dirty();
            Vec::new()
        }

        Key::Tab => {
            model.workspace.focus_next();
            model.mark_dirty();
            Vec::new()
        }

        Key::ShiftTab => {
            model.workspace.focus_prev();
            model.mark_dirty();
            Vec::new()
        }

        // Input bar editing
        Key::Char(ch) => {
            let cap = model.workspace.focused_capability();
            if cap == InputCapability::Prompt || cap == InputCapability::Filter {
                model.workspace.input_bar.insert_char(ch);
                model.mark_dirty();
            }
            Vec::new()
        }

        Key::Backspace => {
            model.workspace.input_bar.backspace();
            model.mark_dirty();
            Vec::new()
        }

        Key::Delete => {
            model.workspace.input_bar.delete();
            model.mark_dirty();
            Vec::new()
        }

        Key::ArrowLeft => {
            model.workspace.input_bar.move_left();
            model.mark_dirty();
            Vec::new()
        }

        Key::ArrowRight => {
            model.workspace.input_bar.move_right();
            model.mark_dirty();
            Vec::new()
        }

        Key::Home => {
            model.workspace.input_bar.move_home();
            model.mark_dirty();
            Vec::new()
        }

        Key::End => {
            model.workspace.input_bar.move_end();
            model.mark_dirty();
            Vec::new()
        }

        Key::ArrowUp => {
            model.workspace.input_bar.history_prev();
            model.mark_dirty();
            Vec::new()
        }

        Key::ArrowDown => {
            model.workspace.input_bar.history_next();
            model.mark_dirty();
            Vec::new()
        }

        Key::Enter => handle_enter(model),

        Key::CtrlEnter | Key::ShiftEnter => {
            // Insert newline (for future multiline input)
            model.workspace.input_bar.insert_char('\n');
            model.mark_dirty();
            Vec::new()
        }

        _ => Vec::new(),
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
        Some(crate::model::OverlayState::CommandPalette { ref query, .. }) => {
            match key {
                Key::Escape => {
                    model.mark_dirty();
                }
                Key::Enter => {
                    // Execute the first matching affordance through the registry
                    let affordances = crate::commands::builtin_affordances();
                    let matches = crate::commands::filter_affordances(&affordances, query);
                    if let Some(aff) = matches.first() {
                        let (handled, effects) = crate::commands::dispatch_affordance(&aff.invoke, model);
                        // overlay already taken above
                        if handled {
                            return effects;
                        }
                    }
                    model.mark_dirty();
                }
                Key::ArrowDown => {
                    // TODO: cycle through matches
                    let cursor = query.len();
                    model.overlay =
                        Some(crate::model::OverlayState::CommandPalette { query: query.clone(), cursor });
                    model.mark_dirty();
                }
                Key::ArrowUp => {
                    let cursor = query.len();
                    model.overlay =
                        Some(crate::model::OverlayState::CommandPalette { query: query.clone(), cursor });
                    model.mark_dirty();
                }
                Key::Char(ch) => {
                    let mut q = query.clone();
                    q.push(ch);
                    let cursor = q.len();
                    model.overlay =
                        Some(crate::model::OverlayState::CommandPalette { query: q, cursor });
                    model.mark_dirty();
                }
                Key::Backspace => {
                    let mut q = query.clone();
                    q.pop();
                    let cursor = q.len();
                    model.overlay =
                        Some(crate::model::OverlayState::CommandPalette { query: q, cursor });
                    model.mark_dirty();
                }
                _ => {
                    let cursor = query.len();
                    model.overlay =
                        Some(crate::model::OverlayState::CommandPalette { query: query.clone(), cursor });
                }
            }
        }
        Some(crate::model::OverlayState::Confirm { ref message, ref action }) => {
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
    let affordances = crate::commands::builtin_affordances();
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
                }
            }
            ViewSpec::SpaceBrowser { .. } => {
                // TODO: Open item detail view (future feature)
                model.mark_dirty();
            }
            _ => {}
        }
    }
    Vec::new()
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
            ViewSpec::Projects => model.store.projects.len(),
            ViewSpec::Remotes => model.store.remotes.len(),
            ViewSpec::Trust => model.store.trust_alerts.len(),
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
    use crate::ids::TileId;

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
        assert_eq!(model.store.remotes.len(), 1);
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
        assert_ne!(model.workspace.focused_tile, initial, "Tab should cycle focus");
    }
}
