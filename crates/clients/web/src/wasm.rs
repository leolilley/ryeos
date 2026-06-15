//! WASM bridge for RyeOS browser clients.
//!
//! Studio is the only product model: Rust owns state, reducer, effects,
//! semantic view model, and scene model; browser JavaScript owns adapters
//! for fetch/EventSource/DOM/Three.js and returns events/effect results.

use wasm_bindgen::prelude::*;

use ryeos_client_base::studio::{
    BrowserSession as StudioBrowserSession, BrowserViewport, StudioCore, StudioEffectResult,
    StudioEvent,
};
use ryeos_client_base::studio::{SeatEvent, SeatEventKind};

use std::cell::RefCell;

// ---------------------------------------------------------------------------
// State — single-threaded WASM, safe to use thread_local RefCell
// ---------------------------------------------------------------------------

thread_local! {
    static STUDIO: RefCell<Option<StudioCore>> = const { RefCell::new(None) };
}

fn studio_envelope(
    core: &StudioCore,
    effects: Vec<ryeos_client_base::studio::StudioEffect>,
) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(&core.envelope(effects))
        .map_err(|e| JsValue::from_str(&format!("serialize RyeOS envelope: {e}")))
}

// ---------------------------------------------------------------------------
// WASM exports — JS calls these
// ---------------------------------------------------------------------------

/// Start RyeOS, returning the semantic view/scene models and initial effects.
#[wasm_bindgen]
pub fn studio_start(
    session_json: JsValue,
    viewport_json: JsValue,
    now_ms: u64,
) -> Result<JsValue, JsValue> {
    let session: StudioBrowserSession = serde_wasm_bindgen::from_value(session_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS browser session: {e}")))?;
    let viewport: BrowserViewport = serde_wasm_bindgen::from_value(viewport_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS viewport: {e}")))?;

    let mut core = StudioCore::new(session, viewport, now_ms);
    core.bump_generation();
    let effects = core.initial_effects();
    let response = studio_envelope(&core, effects)?;

    STUDIO.with(|state| {
        *state.borrow_mut() = Some(core);
    });

    Ok(response)
}

/// Dispatch a browser-neutral RyeOS event into the Rust reducer.
#[wasm_bindgen]
pub fn studio_dispatch(event_json: JsValue) -> Result<JsValue, JsValue> {
    let event: StudioEvent = serde_wasm_bindgen::from_value(event_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS event: {e}")))?;

    STUDIO.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        let effects = core.dispatch(event);
        studio_envelope(core, effects)
    })
}

/// Apply a browser/daemon effect result to RyeOS.
#[wasm_bindgen]
pub fn studio_apply_effect_result(result_json: JsValue) -> Result<JsValue, JsValue> {
    let result: StudioEffectResult = serde_wasm_bindgen::from_value(result_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS effect result: {e}")))?;

    STUDIO.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        let effects = core.dispatch(StudioEvent::EffectResult { result });
        studio_envelope(core, effects)
    })
}

/// Return the current RyeOS view model without mutating state.
#[wasm_bindgen]
pub fn studio_view_model() -> Result<JsValue, JsValue> {
    STUDIO.with(|state| {
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
pub fn studio_scene_model() -> Result<JsValue, JsValue> {
    STUDIO.with(|state| {
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
pub fn studio_seat_events() -> Result<JsValue, JsValue> {
    STUDIO.with(|state| {
        let state = state.borrow();
        let core = state
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        serde_wasm_bindgen::to_value(core.seat.events())
            .map_err(|e| JsValue::from_str(&format!("serialize RyeOS seat events: {e}")))
    })
}

/// Replay durable seat braid events into the in-memory Studio engine.
#[wasm_bindgen]
pub fn studio_replay_seat_events(events_json: JsValue) -> Result<JsValue, JsValue> {
    let events: Vec<serde_json::Value> = serde_wasm_bindgen::from_value(events_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS seat replay: {e}")))?;
    STUDIO.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        for event in events {
            if let Some(seat_event) = seat_event_from_replay(&event) {
                core.seat.append_replayed(seat_event);
            }
        }
        studio_envelope(core, Vec::new())
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
