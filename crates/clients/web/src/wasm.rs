//! WASM bridge for RyeOS browser clients.
//!
//! RyeOS is the only product model: Rust owns state, reducer, effects,
//! semantic view model, and scene model; browser JavaScript owns adapters
//! for fetch/EventSource/DOM/Three.js and returns events/effect results.

use serde::Serialize;
use wasm_bindgen::prelude::*;

use ryeos_client_base::ui::{
    ryeos_key_command, BrowserSession as RyeOsBrowserSession, BrowserViewport, RyeOsCore,
    RyeOsEffectResult, RyeOsEnvelope, RyeOsEvent, RyeOsKeyCommand, RyeOsKeyEvent,
};
use ryeos_client_base::ui::{SeatEvent, SeatEventKind};

use std::cell::RefCell;

// ---------------------------------------------------------------------------
// State — single-threaded WASM, safe to use thread_local RefCell
// ---------------------------------------------------------------------------

thread_local! {
    static RYEOS_UI: RefCell<Option<RyeOsCore>> = const { RefCell::new(None) };
}

fn ryeos_envelope(
    core: &RyeOsCore,
    effects: Vec<ryeos_client_base::ui::RyeOsEffect>,
) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(&core.envelope(effects))
        .map_err(|e| JsValue::from_str(&format!("serialize RyeOS envelope: {e}")))
}

// ---------------------------------------------------------------------------
// WASM exports — JS calls these
// ---------------------------------------------------------------------------

/// Start RyeOS, returning the semantic view/scene models and initial effects.
#[wasm_bindgen]
pub fn ryeos_start(
    session_json: JsValue,
    viewport_json: JsValue,
    now_ms: u64,
) -> Result<JsValue, JsValue> {
    let session: RyeOsBrowserSession = serde_wasm_bindgen::from_value(session_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS browser session: {e}")))?;
    let viewport: BrowserViewport = serde_wasm_bindgen::from_value(viewport_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS viewport: {e}")))?;

    let mut core = RyeOsCore::new(session, viewport, now_ms);
    core.bump_generation();
    let effects = core.initial_effects();
    let response = ryeos_envelope(&core, effects)?;

    RYEOS_UI.with(|state| {
        *state.borrow_mut() = Some(core);
    });

    Ok(response)
}

/// Dispatch a browser-neutral RyeOS event into the Rust reducer.
#[wasm_bindgen]
pub fn ryeos_dispatch(event_json: JsValue) -> Result<JsValue, JsValue> {
    let event: RyeOsEvent = serde_wasm_bindgen::from_value(event_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS event: {e}")))?;

    RYEOS_UI.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        let effects = core.dispatch(event);
        ryeos_envelope(core, effects)
    })
}

/// Apply a browser/daemon effect result to RyeOS.
#[wasm_bindgen]
pub fn ryeos_apply_effect_result(result_json: JsValue) -> Result<JsValue, JsValue> {
    let result: RyeOsEffectResult = serde_wasm_bindgen::from_value(result_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS effect result: {e}")))?;

    RYEOS_UI.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        let effects = core.dispatch(RyeOsEvent::EffectResult { result });
        ryeos_envelope(core, effects)
    })
}

/// The resolved outcome of a key press: whether the shared keymap consumed the
/// key (so the browser suppresses its native default) plus the updated
/// envelope to commit.
#[derive(Serialize)]
struct RyeOsKeyOutcome {
    handled: bool,
    envelope: RyeOsEnvelope,
}

/// Route a browser key press through the SHARED ryeos keymap.
///
/// JavaScript translates a DOM `KeyboardEvent` into a neutral `RyeOsKeyEvent`
/// (`{ key, modifiers }`) and calls this. The binding table lives in
/// `ryeos_client_base::ui::ryeos_key_command` — the exact function the
/// terminal uses — so the two renderers never diverge on what a key does. The
/// focus-context capabilities are resolved by the shared `key_context()`.
/// Genuinely-web key handling (native text-input editing, launcher search,
/// pointer, focus capture) stays in JavaScript and never reaches here.
#[wasm_bindgen]
pub fn ryeos_key(event_json: JsValue) -> Result<JsValue, JsValue> {
    let event: RyeOsKeyEvent = serde_wasm_bindgen::from_value(event_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS key event: {e}")))?;

    RYEOS_UI.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        let command = ryeos_key_command(event, core.key_context());
        // Quit is a terminal affordance (Ctrl+C); the browser has nothing to
        // quit and leaves the key native. Ignore is an unbound key — also
        // native, so browser chords (Ctrl+R, F5, copy) still work.
        let handled = !matches!(command, RyeOsKeyCommand::Quit | RyeOsKeyCommand::Ignore);
        // Interpretation is shared: `RyeOsCore::apply_key_command` owns the
        // row-cursor walk, focus fallback, and launcher edits for BOTH
        // renderers.
        let effects = core.apply_key_command(command);
        let outcome = RyeOsKeyOutcome {
            handled,
            envelope: core.envelope(effects),
        };
        serde_wasm_bindgen::to_value(&outcome)
            .map_err(|e| JsValue::from_str(&format!("serialize RyeOS key outcome: {e}")))
    })
}

/// Return the current RyeOS view model without mutating state.
#[wasm_bindgen]
pub fn ryeos_view_model() -> Result<JsValue, JsValue> {
    RYEOS_UI.with(|state| {
        let state = state.borrow();
        let core = state
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        serde_wasm_bindgen::to_value(&core.envelope(Vec::new()).view_model)
            .map_err(|e| JsValue::from_str(&format!("serialize RyeOS view model: {e}")))
    })
}

/// Return the current RyeOS scene model without mutating state.
#[wasm_bindgen]
pub fn ryeos_scene_model() -> Result<JsValue, JsValue> {
    RYEOS_UI.with(|state| {
        let state = state.borrow();
        let core = state
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        serde_wasm_bindgen::to_value(&core.envelope(Vec::new()).scene_model)
            .map_err(|e| JsValue::from_str(&format!("serialize RyeOS scene model: {e}")))
    })
}

/// Return the local seat event log so JS can mirror it into the seat braid.
#[wasm_bindgen]
pub fn ryeos_seat_events() -> Result<JsValue, JsValue> {
    RYEOS_UI.with(|state| {
        let state = state.borrow();
        let core = state
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        serde_wasm_bindgen::to_value(core.seat.events())
            .map_err(|e| JsValue::from_str(&format!("serialize RyeOS seat events: {e}")))
    })
}

/// Replay durable seat braid events into the in-memory RyeOs engine.
#[wasm_bindgen]
pub fn ryeos_replay_seat_events(events_json: JsValue) -> Result<JsValue, JsValue> {
    let events: Vec<serde_json::Value> = serde_wasm_bindgen::from_value(events_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS seat replay: {e}")))?;
    RYEOS_UI.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        for event in events {
            if let Some(seat_event) = seat_event_from_replay(&event) {
                core.seat.append_replayed(seat_event);
            }
        }
        ryeos_envelope(core, Vec::new())
    })
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
