//! WASM bridge — the shared RyeOS client core running in the browser.
//!
//! The browser shell owns transport and DOM events. WASM owns the
//! `AppModel`, surface layout, reducer, effects, and shared frame rendering.

use wasm_bindgen::prelude::*;

use ryeos_client_base::frame::build_frame;
use ryeos_client_base::ids::{RemoteId, ThreadId};
use ryeos_client_base::input::{InputEvent, Key};
use ryeos_client_base::layout::Rect;
use ryeos_client_base::model::AppModel;
use ryeos_client_base::surface::LoadedSurface;
use ryeos_client_base::update::{
    self, AppEvent, CockpitFileRead, CockpitFilesList, CockpitGcStatus, CockpitItemInspection,
    CockpitItemsList, CockpitSchedulesList, CockpitSnapshot, CockpitThreadInspection, DaemonEvent,
    PollSnapshot, RemoteSummary, ThreadSummary,
};

use std::cell::RefCell;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// State — single-threaded WASM, safe to use thread_local RefCell
// ---------------------------------------------------------------------------

thread_local! {
    static STATE: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

struct AppState {
    model: AppModel,
    effects: Vec<ryeos_client_base::effects::Effect>,
}

#[derive(Debug, Deserialize)]
struct BrowserSession {
    surface_ref: String,
    #[serde(default)]
    project_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrowserPollSnapshot {
    #[serde(default)]
    threads: Vec<BrowserThreadSummary>,
    #[serde(default)]
    remotes: Vec<BrowserRemoteSummary>,
    #[serde(default)]
    daemon_url: Option<String>,
    #[serde(default = "default_true")]
    daemon_alive: bool,
}

#[derive(Debug, Deserialize)]
struct BrowserThreadSummary {
    id: serde_json::Value,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    item_ref: Option<String>,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    parent_id: Option<serde_json::Value>,
    #[serde(default)]
    started_at_ms: Option<i64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    cost_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct BrowserRemoteSummary {
    #[serde(default)]
    id: Option<serde_json::Value>,
    name: String,
    url: String,
    #[serde(default)]
    alive: bool,
}

#[derive(Debug, Deserialize)]
struct BrowserEventEnvelope {
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    event_type: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// WASM exports — JS calls these
// ---------------------------------------------------------------------------

/// Initialize the browser client from the daemon-resolved effective surface.
#[wasm_bindgen]
pub fn start_with_surface(
    session_json: JsValue,
    effective_surface_json: JsValue,
    width: u16,
    height: u16,
) -> Result<JsValue, JsValue> {
    let session: BrowserSession = serde_wasm_bindgen::from_value(session_json)
        .map_err(|e| JsValue::from_str(&format!("invalid browser session: {e}")))?;
    let effective_surface: serde_json::Value =
        serde_wasm_bindgen::from_value(effective_surface_json)
            .map_err(|e| JsValue::from_str(&format!("invalid effective surface response: {e}")))?;

    let loaded = LoadedSurface::from_daemon(&session.surface_ref, effective_surface)
        .map_err(|diag| JsValue::from_str(&format!("surface rejected: {diag:?}")))?;
    let project_path = session.project_path.as_deref().unwrap_or(".");
    let mut model = AppModel::from_surface(project_path, &loaded);
    model.runtime.viewport = Rect::new(0, 0, width, height);

    STATE.with(|s| {
        *s.borrow_mut() = Some(AppState {
            model,
            effects: Vec::new(),
        });
    });

    render_response()
}

/// Advance animation by dt milliseconds.
#[wasm_bindgen]
pub fn tick(now_ms: u32) -> Result<JsValue, JsValue> {
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(
                &mut state.model,
                AppEvent::Tick {
                    now_ms: now_ms as u64,
                },
            );
            state.effects.extend(effects);
        }
    });
    render_response()
}

/// Dispatch a keyboard event.
#[wasm_bindgen]
pub fn dispatch_key(key_code: u32, shift: bool, ctrl: bool, alt: bool) -> Result<JsValue, JsValue> {
    let key = map_key(key_code, shift, ctrl, alt);
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(&mut state.model, AppEvent::Input(InputEvent::Key(key)));
            state.effects.extend(effects);
        }
    });
    render_response()
}

/// Resize the viewport.
#[wasm_bindgen]
pub fn dispatch_resize(width: u16, height: u16) -> Result<JsValue, JsValue> {
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(&mut state.model, AppEvent::Resize { width, height });
            state.effects.extend(effects);
        }
    });
    render_response()
}

/// Apply a browser-fetched daemon snapshot.
///
/// `PollSnapshot` currently returns `Effect::RefreshState` in the shared
/// reducer. The browser calls this function as the result of a refresh, so the
/// returned refresh effect is deliberately not re-enqueued here.
#[wasm_bindgen]
pub fn dispatch_poll_snapshot(snapshot_json: JsValue) -> Result<JsValue, JsValue> {
    let snapshot: BrowserPollSnapshot = serde_wasm_bindgen::from_value(snapshot_json)
        .map_err(|e| JsValue::from_str(&format!("invalid poll snapshot: {e}")))?;
    let snapshot = snapshot.into_core();

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let _ = update::update(&mut state.model, AppEvent::PollSnapshot(snapshot));
        }
    });
    render_response()
}

/// Apply the daemon's renderer-neutral operational cockpit snapshot.
#[wasm_bindgen]
pub fn dispatch_cockpit_snapshot(snapshot_json: JsValue) -> Result<JsValue, JsValue> {
    let snapshot: CockpitSnapshot = serde_wasm_bindgen::from_value(snapshot_json)
        .map_err(|e| JsValue::from_str(&format!("invalid cockpit snapshot: {e}")))?;

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(&mut state.model, AppEvent::CockpitSnapshot(snapshot));
            state.effects.extend(effects);
        }
    });
    render_response()
}

/// Apply the daemon's real item inventory for the existing Items pane.
#[wasm_bindgen]
pub fn dispatch_cockpit_items(items_json: JsValue) -> Result<JsValue, JsValue> {
    let items: CockpitItemsList = serde_wasm_bindgen::from_value(items_json)
        .map_err(|e| JsValue::from_str(&format!("invalid cockpit items list: {e}")))?;

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(&mut state.model, AppEvent::CockpitItems(items));
            state.effects.extend(effects);
        }
    });
    render_response()
}

/// Apply the daemon's read-only item inspection response.
#[wasm_bindgen]
pub fn dispatch_cockpit_item_inspection(inspection_json: JsValue) -> Result<JsValue, JsValue> {
    let inspection: CockpitItemInspection = serde_wasm_bindgen::from_value(inspection_json)
        .map_err(|e| JsValue::from_str(&format!("invalid cockpit item inspection: {e}")))?;

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(
                &mut state.model,
                AppEvent::CockpitItemInspection(inspection),
            );
            state.effects.extend(effects);
        }
    });
    render_response()
}

#[wasm_bindgen]
pub fn dispatch_cockpit_schedules(schedules_json: JsValue) -> Result<JsValue, JsValue> {
    let schedules: CockpitSchedulesList = serde_wasm_bindgen::from_value(schedules_json)
        .map_err(|e| JsValue::from_str(&format!("invalid cockpit schedules: {e}")))?;

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(&mut state.model, AppEvent::CockpitSchedules(schedules));
            state.effects.extend(effects);
        }
    });
    render_response()
}

#[wasm_bindgen]
pub fn dispatch_cockpit_gc_status(gc_json: JsValue) -> Result<JsValue, JsValue> {
    let gc: CockpitGcStatus = serde_wasm_bindgen::from_value(gc_json)
        .map_err(|e| JsValue::from_str(&format!("invalid cockpit GC status: {e}")))?;

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(&mut state.model, AppEvent::CockpitGcStatus(gc));
            state.effects.extend(effects);
        }
    });
    render_response()
}

#[wasm_bindgen]
pub fn dispatch_cockpit_files(files_json: JsValue) -> Result<JsValue, JsValue> {
    let files: CockpitFilesList = serde_wasm_bindgen::from_value(files_json)
        .map_err(|e| JsValue::from_str(&format!("invalid cockpit files list: {e}")))?;

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(&mut state.model, AppEvent::CockpitFiles(files));
            state.effects.extend(effects);
        }
    });
    render_response()
}

#[wasm_bindgen]
pub fn dispatch_cockpit_file_read(file_json: JsValue) -> Result<JsValue, JsValue> {
    let file: CockpitFileRead = serde_wasm_bindgen::from_value(file_json)
        .map_err(|e| JsValue::from_str(&format!("invalid cockpit file read: {e}")))?;

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(&mut state.model, AppEvent::CockpitFileRead(file));
            state.effects.extend(effects);
        }
    });
    render_response()
}

#[wasm_bindgen]
pub fn dispatch_cockpit_thread_inspection(inspection_json: JsValue) -> Result<JsValue, JsValue> {
    let inspection: CockpitThreadInspection = serde_wasm_bindgen::from_value(inspection_json)
        .map_err(|e| JsValue::from_str(&format!("invalid cockpit thread inspection: {e}")))?;

    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let effects = update::update(
                &mut state.model,
                AppEvent::CockpitThreadInspection(inspection),
            );
            state.effects.extend(effects);
        }
    });
    render_response()
}

/// Apply a daemon/session event received by the browser shell.
#[wasm_bindgen]
pub fn dispatch_daemon_event(event_json: JsValue) -> Result<JsValue, JsValue> {
    let event: BrowserEventEnvelope = serde_wasm_bindgen::from_value(event_json)
        .map_err(|e| JsValue::from_str(&format!("invalid daemon event: {e}")))?;

    if let Some(event) = event.into_core_event() {
        STATE.with(|s| {
            if let Some(state) = s.borrow_mut().as_mut() {
                let effects = update::update(&mut state.model, AppEvent::Daemon(event));
                state.effects.extend(effects);
            }
        });
    }
    render_response()
}

/// Render the current shared frame as browser HTML.
#[wasm_bindgen]
pub fn render_html() -> Result<String, JsValue> {
    STATE.with(|s| {
        let mut state = s.borrow_mut();
        let state = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("web client is not initialized"))?;
        let frame = build_frame(&mut state.model);
        Ok(crate::render_grid::render_frame_html(&frame))
    })
}

/// Drain platform effects produced by the shared reducer.
#[wasm_bindgen]
pub fn take_effects() -> Result<JsValue, JsValue> {
    STATE.with(|s| {
        let mut state = s.borrow_mut();
        let state = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("web client is not initialized"))?;
        let effects = std::mem::take(&mut state.effects);
        serde_wasm_bindgen::to_value(&effects)
            .map_err(|e| JsValue::from_str(&format!("serialize effects: {e}")))
    })
}

// ---------------------------------------------------------------------------
// Internal: render current state → JS callbacks
// ---------------------------------------------------------------------------

fn render_response() -> Result<JsValue, JsValue> {
    let html = render_html()?;
    serde_wasm_bindgen::to_value(&serde_json::json!({
        "html": html,
    }))
    .map_err(|e| JsValue::from_str(&format!("serialize render response: {e}")))
}

impl BrowserPollSnapshot {
    fn into_core(self) -> PollSnapshot {
        PollSnapshot {
            threads: self
                .threads
                .into_iter()
                .map(BrowserThreadSummary::into_core)
                .collect(),
            remotes: self
                .remotes
                .into_iter()
                .enumerate()
                .map(|(idx, remote)| remote.into_core(idx as u64))
                .collect(),
            daemon_url: self.daemon_url,
            daemon_alive: self.daemon_alive,
        }
    }
}

impl BrowserThreadSummary {
    fn into_core(self) -> ThreadSummary {
        let daemon_id = self.id.as_str().map(String::from);
        ThreadSummary {
            id: thread_id_from_value(&self.id),
            daemon_id,
            status: self.status.unwrap_or_else(|| "unknown".into()),
            item_ref: self.item_ref.or(self.item_id),
            parent_id: self.parent_id.as_ref().map(thread_id_from_value),
            started_at_ms: self.started_at_ms,
            duration_ms: self.duration_ms,
            cost_usd: self.cost_usd,
        }
    }
}

impl BrowserRemoteSummary {
    fn into_core(self, fallback_id: u64) -> RemoteSummary {
        RemoteSummary {
            id: self
                .id
                .as_ref()
                .map(remote_id_from_value)
                .unwrap_or_else(|| RemoteId::new(fallback_id)),
            name: self.name,
            url: self.url,
            alive: self.alive,
        }
    }
}

impl BrowserEventEnvelope {
    fn into_core_event(self) -> Option<DaemonEvent> {
        let event_type = self.event_type.or(self.event)?;
        let payload = self.payload;
        match event_type.as_str() {
            "thread.created" | "thread_created" | "thread.upsert" => {
                Some(DaemonEvent::ThreadCreated {
                    id: thread_id_from_field(&payload, "id")
                        .or_else(|| thread_id_from_field(&payload, "thread_id"))?,
                    item_ref: payload
                        .get("item_ref")
                        .or_else(|| payload.get("item_id"))
                        .and_then(|v| v.as_str())
                        .map(String::from),
                })
            }
            "thread.started" | "thread_started" => Some(DaemonEvent::ThreadStarted {
                id: thread_id_from_field(&payload, "id")
                    .or_else(|| thread_id_from_field(&payload, "thread_id"))?,
            }),
            "thread.completed" | "thread_completed" => Some(DaemonEvent::ThreadCompleted {
                id: thread_id_from_field(&payload, "id")
                    .or_else(|| thread_id_from_field(&payload, "thread_id"))?,
            }),
            "thread.failed" | "thread_failed" => Some(DaemonEvent::ThreadFailed {
                id: thread_id_from_field(&payload, "id")
                    .or_else(|| thread_id_from_field(&payload, "thread_id"))?,
                error: payload
                    .get("error")
                    .or_else(|| payload.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("thread failed")
                    .to_string(),
            }),
            "text_delta" | "thread.text_delta" => Some(DaemonEvent::TextDelta {
                thread_id: thread_id_from_field(&payload, "thread_id")
                    .or_else(|| thread_id_from_field(&payload, "id"))?,
                text: payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            }),
            _ => None,
        }
    }
}

fn thread_id_from_field(value: &serde_json::Value, field: &str) -> Option<ThreadId> {
    value.get(field).map(thread_id_from_value)
}

fn thread_id_from_value(value: &serde_json::Value) -> ThreadId {
    ThreadId::new(id_from_value(value))
}

fn remote_id_from_value(value: &serde_json::Value) -> RemoteId {
    RemoteId::new(id_from_value(value))
}

fn id_from_value(value: &serde_json::Value) -> u64 {
    if let Some(n) = value.as_u64() {
        return n;
    }
    let raw = value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string());
    raw.parse().unwrap_or_else(|_| stable_hash_id(&raw))
}

fn stable_hash_id(raw: &str) -> u64 {
    raw.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        hash.wrapping_mul(0x100000001b3) ^ u64::from(byte)
    })
}

// ---------------------------------------------------------------------------
// Key mapping — JS keyCode → core Key enum
// ---------------------------------------------------------------------------

fn map_key(key_code: u32, _shift: bool, ctrl: bool, alt: bool) -> Key {
    if ctrl {
        // Ctrl+letter: key_code is the uppercase ASCII code
        if let Some(c) = char::from_u32(key_code) {
            let lc = c.to_ascii_lowercase();
            if lc >= 'a' && lc <= 'z' {
                return Key::Ctrl(lc);
            }
        }
    }

    if alt {
        if let Some(c) = char::from_u32(key_code) {
            return Key::Alt(c);
        }
    }

    match key_code {
        13 => Key::Enter,
        9 => Key::Tab,
        8 => Key::Backspace,
        46 => Key::Delete,
        27 => Key::Escape,
        37 => Key::ArrowLeft,
        38 => Key::ArrowUp,
        39 => Key::ArrowRight,
        40 => Key::ArrowDown,
        33 => Key::PageUp,
        34 => Key::PageDown,
        36 => Key::Home,
        35 => Key::End,
        32 => Key::Char(' '),
        kc => {
            if let Some(c) = char::from_u32(kc) {
                Key::Char(c)
            } else {
                Key::Escape
            }
        }
    }
}
