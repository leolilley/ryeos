//! WASM bridge for RyeOS browser clients.
//!
//! RyeOS is the only product model: Rust owns state, reducer, effects,
//! semantic view model, and scene model; browser JavaScript owns adapters
//! for fetch/EventSource/DOM/Three.js and returns events/effect results.

use serde::Serialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use ryeos_client_base::ui::{
    BrowserSession as RyeOsBrowserSession, BrowserViewport, RyeOsCore, RyeOsEffectResult,
    RyeOsEnvelope, RyeOsEvent, RyeOsKeyCommand, RyeOsKeyEvent, ryeos_key_command,
};
use ryeos_client_base::ui::{SeatEvent, SeatEventKind};

use std::cell::RefCell;

fn to_js_value<T: Serialize + ?Sized>(value: &T, context: &str) -> Result<JsValue, JsValue> {
    let serializer = serde_wasm_bindgen::Serializer::new()
        .serialize_missing_as_null(true)
        // Maps first cross as real JS Maps so prototype-sensitive string keys
        // cannot invoke Object.prototype setters. normalize_js_data then turns
        // every map/record into the plain-object ABI generated TS declares,
        // defining each field as an own data property.
        .serialize_maps_as_objects(false)
        .serialize_large_number_types_as_bigints(true);
    let serialized = value
        .serialize(&serializer)
        .map_err(|error| JsValue::from_str(&format!("{context}: {error}")))?;
    normalize_js_data(serialized)
        .map_err(|error| JsValue::from_str(&format!("{context}: normalize JS data: {error:?}")))
}

fn normalize_js_data(value: JsValue) -> Result<JsValue, JsValue> {
    if value.is_null() || value.is_undefined() || !value.is_object() {
        return Ok(value);
    }
    if js_sys::Array::is_array(&value) {
        let source: js_sys::Array = value.unchecked_into();
        let result = js_sys::Array::new_with_length(source.length());
        for index in 0..source.length() {
            result.set(index, normalize_js_data(source.get(index))?);
        }
        return Ok(result.into());
    }

    let result = js_sys::Object::new();
    if value.is_instance_of::<js_sys::Map>() {
        let entries = js_sys::try_iter(&value)?
            .ok_or_else(|| JsValue::from_str("serialized map is not iterable"))?;
        for entry in entries {
            let entry = js_sys::Array::from(&entry?);
            let key = entry
                .get(0)
                .as_string()
                .ok_or_else(|| JsValue::from_str("serialized map has a non-string key"))?;
            define_data_property(&result, &key, normalize_js_data(entry.get(1))?)?;
        }
    } else {
        let source: js_sys::Object = value.unchecked_into();
        let keys = js_sys::Object::keys(&source);
        for index in 0..keys.length() {
            let key = keys
                .get(index)
                .as_string()
                .ok_or_else(|| JsValue::from_str("serialized object has a non-string key"))?;
            let field = js_sys::Reflect::get(&source, &JsValue::from_str(&key))?;
            define_data_property(&result, &key, normalize_js_data(field)?)?;
        }
    }
    Ok(result.into())
}

fn define_data_property(target: &js_sys::Object, key: &str, value: JsValue) -> Result<(), JsValue> {
    let descriptor = js_sys::Object::new();
    js_sys::Reflect::set(&descriptor, &JsValue::from_str("value"), &value)?;
    for attribute in ["enumerable", "configurable", "writable"] {
        js_sys::Reflect::set(&descriptor, &JsValue::from_str(attribute), &JsValue::TRUE)?;
    }
    if js_sys::Reflect::define_property(target, &JsValue::from_str(key), &descriptor)? {
        Ok(())
    } else {
        Err(JsValue::from_str(
            "failed to define serialized object field",
        ))
    }
}

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
    to_js_value(&core.envelope(effects), "serialize RyeOS envelope")
}

// ---------------------------------------------------------------------------
// WASM exports — JS calls these
// ---------------------------------------------------------------------------

#[wasm_bindgen]
pub fn ryeos_layout_preference_key() -> Result<String, JsValue> {
    RYEOS_UI.with(|state| {
        state
            .borrow()
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?
            .layout_preference_key()
            .map_err(|e| JsValue::from_str(&e))
    })
}

#[wasm_bindgen]
pub fn ryeos_export_layout_preferences() -> Result<String, JsValue> {
    RYEOS_UI.with(|state| {
        state
            .borrow()
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?
            .export_layout_preferences()
            .map_err(|e| JsValue::from_str(&e))
    })
}

#[wasm_bindgen]
pub fn ryeos_restore_layout_preferences(encoded: &str) -> Result<JsValue, JsValue> {
    RYEOS_UI.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        let effects = core
            .restore_layout_preferences(encoded)
            .map_err(|e| JsValue::from_str(&e))?;
        ryeos_envelope(core, effects)
    })
}

#[wasm_bindgen]
pub fn ryeos_export_active_view_set_template(id: &str, name: &str) -> Result<String, JsValue> {
    RYEOS_UI.with(|state| {
        let template = state
            .borrow()
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?
            .export_active_view_set_template(id.to_owned(), name.to_owned())
            .map_err(|error| JsValue::from_str(&error))?;
        serde_json::to_string(&template)
            .map_err(|error| JsValue::from_str(&format!("serialize view-set template: {error}")))
    })
}

#[wasm_bindgen]
pub fn ryeos_open_saved_view_set_template(encoded: &str) -> Result<JsValue, JsValue> {
    let template = serde_json::from_str(encoded)
        .map_err(|error| JsValue::from_str(&format!("invalid view-set template: {error}")))?;
    RYEOS_UI.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        let effects = core
            .open_saved_view_set_template(&template)
            .map_err(|error| JsValue::from_str(&error))?;
        ryeos_envelope(core, effects)
    })
}

/// Start RyeOS, returning the semantic view/scene models and initial effects.
#[wasm_bindgen]
pub fn ryeos_start(
    session_json: JsValue,
    viewport_json: JsValue,
    now_ms: u64,
) -> Result<JsValue, JsValue> {
    let session: RyeOsBrowserSession = serde_wasm_bindgen::from_value(session_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS browser session: {e}")))?;
    if session.ui_binding_contract_revision != ryeos_client_base::UI_BINDING_CONTRACT_REVISION {
        return Err(JsValue::from_str(&format!(
            "UI binding contract mismatch: session advertised '{}', client requires '{}'",
            session.ui_binding_contract_revision,
            ryeos_client_base::UI_BINDING_CONTRACT_REVISION
        )));
    }
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
        to_js_value(&outcome, "serialize RyeOS key outcome")
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
        to_js_value(
            &core.envelope(Vec::new()).view_model,
            "serialize RyeOS view model",
        )
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
        to_js_value(
            &core.envelope(Vec::new()).scene_model,
            "serialize RyeOS scene model",
        )
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
        to_js_value(core.seat.events(), "serialize RyeOS seat events")
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
        let replayed = events
            .iter()
            .filter_map(seat_event_from_replay)
            .collect::<Vec<_>>();
        let effects = core.replay_seat_events(replayed);
        ryeos_envelope(core, effects)
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
