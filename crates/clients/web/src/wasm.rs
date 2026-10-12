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
    // Cold history is never applied to the visible core. A failed or truncated
    // stream drops this candidate without publishing effects or a view model.
    static RYEOS_REPLAY_CANDIDATE: RefCell<Option<ColdReplayCandidate>> = const { RefCell::new(None) };
}

struct ColdReplayCandidate {
    core: RyeOsCore,
    staged: ryeos_client_base::ui::cold_replay::ColdSeatReplay,
    refresh: Option<ryeos_client_base::ui::AuthenticatedReproofGuard>,
}

#[wasm_bindgen]
pub fn ryeos_begin_seat_replay(
    session_json: JsValue,
    presentation_id: &str,
    viewport_json: JsValue,
    now_ms: u64,
) -> Result<(), JsValue> {
    let session: RyeOsBrowserSession = serde_wasm_bindgen::from_value(session_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS browser session: {e}")))?;
    if session.ui_binding_contract_revision != ryeos_client_base::UI_BINDING_CONTRACT_REVISION {
        return Err(JsValue::from_str(
            "UI binding contract mismatch during replay",
        ));
    }
    let viewport: BrowserViewport = serde_wasm_bindgen::from_value(viewport_json)
        .map_err(|e| JsValue::from_str(&format!("invalid RyeOS viewport: {e}")))?;
    let mut core = RyeOsCore::new(session, presentation_id, viewport, now_ms);
    core.supports_independent_documents = true;
    RYEOS_REPLAY_CANDIDATE.with(|state| {
        *state.borrow_mut() = Some(ColdReplayCandidate {
            core,
            staged: ryeos_client_base::ui::cold_replay::ColdSeatReplay::new(),
            refresh: None,
        });
    });
    Ok(())
}

/// Same-producer refresh stages into the existing private replay lane. The
/// live Core is neither replaced nor reset before authenticated completion.
#[wasm_bindgen]
pub fn ryeos_begin_seat_reproof(
    session_json: JsValue,
    presentation_id: &str,
    viewport_json: JsValue,
    now_ms: u64,
) -> Result<(), JsValue> {
    RYEOS_REPLAY_CANDIDATE.with(|state| *state.borrow_mut() = None);
    let guard = RYEOS_UI.with(|state| {
        state
            .borrow()
            .as_ref()
            .ok_or_else(|| JsValue::from_str("live core absent"))?
            .begin_authenticated_reproof()
            .map_err(|error| JsValue::from_str(&error))
    })?;
    ryeos_begin_seat_replay(session_json, presentation_id, viewport_json, now_ms)?;
    RYEOS_REPLAY_CANDIDATE.with(|state| {
        let mut state = state.borrow_mut();
        let candidate = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("reproof candidate absent"))?;
        candidate.refresh = Some(guard);
        Ok(())
    })
}

#[wasm_bindgen]
pub fn ryeos_stage_seat_replay_events(events_json: JsValue) -> Result<(), JsValue> {
    RYEOS_REPLAY_CANDIDATE.with(|state| {
        let mut state = state.borrow_mut();
        let candidate = state
            .take()
            .ok_or_else(|| JsValue::from_str("seat replay was not begun"))?;
        // Taking ownership first makes every parse/staging failure discard the
        // private candidate, never leaving a partial prefix available to commit.
        let events: Vec<serde_json::Value> = serde_wasm_bindgen::from_value(events_json)
            .map_err(|e| JsValue::from_str(&format!("invalid RyeOS seat replay batch: {e}")))?;
        let mut candidate = candidate;
        candidate
            .staged
            .append_authenticated_records(&mut candidate.core, &events)
            .map_err(|e| JsValue::from_str(&e))?;
        *state = Some(candidate);
        Ok(())
    })
}

#[wasm_bindgen]
pub fn ryeos_stage_seat_transfer_proof(proof_json: JsValue) -> Result<(), JsValue> {
    RYEOS_REPLAY_CANDIDATE.with(|state| {
        let mut state = state.borrow_mut();
        let mut candidate = state
            .take()
            .ok_or_else(|| JsValue::from_str("seat replay was not begun"))?;
        let proof = serde_wasm_bindgen::from_value(proof_json).map_err(|error| {
            JsValue::from_str(&format!("invalid paired transfer proof: {error}"))
        })?;
        candidate
            .staged
            .stage_transfer_proof(&candidate.core, proof)
            .map_err(|error| JsValue::from_str(&error))?;
        *state = Some(candidate);
        Ok(())
    })
}

#[wasm_bindgen]
pub fn ryeos_stage_seat_move_freeze_cancellation_proof(proof_json: JsValue) -> Result<(), JsValue> {
    RYEOS_REPLAY_CANDIDATE.with(|state| {
        let mut state = state.borrow_mut();
        let mut candidate = state
            .take()
            .ok_or_else(|| JsValue::from_str("seat replay was not begun"))?;
        let proof = serde_wasm_bindgen::from_value(proof_json).map_err(|error| {
            JsValue::from_str(&format!("invalid move freeze cancellation proof: {error}"))
        })?;
        candidate
            .staged
            .stage_move_freeze_cancellation_proof(&candidate.core, proof)
            .map_err(|error| JsValue::from_str(&error))?;
        *state = Some(candidate);
        Ok(())
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayCompletion {
    tip_hash: String,
    thread_seq: u64,
    event_count: u64,
}

#[wasm_bindgen]
pub fn ryeos_commit_seat_replay(completion_json: JsValue) -> Result<JsValue, JsValue> {
    RYEOS_REPLAY_CANDIDATE.with(|candidate| {
        let mut candidate = candidate.borrow_mut();
        let candidate = candidate
            .take()
            .ok_or_else(|| JsValue::from_str("seat replay was not begun"))?;
        let completion: ReplayCompletion = serde_wasm_bindgen::from_value(completion_json)
            .map_err(|error| {
                JsValue::from_str(&format!("invalid seat replay completion: {error}"))
            })?;
        let mut core = candidate.core;
        let effects = candidate
            .staged
            .finish_at_authenticated_tip(
                &mut core,
                &completion.tip_hash,
                completion.thread_seq,
                completion.event_count,
            )
            .map_err(|e| JsValue::from_str(&e))?;
        core.require_replayed_initialization()
            .map_err(|e| JsValue::from_str(&e))?;
        if let Some(guard) = candidate.refresh {
            return RYEOS_UI.with(|state| {
                let mut state = state.borrow_mut();
                let live = state
                    .as_mut()
                    .ok_or_else(|| JsValue::from_str("live core absent"))?;
                let effects = live
                    .promote_authenticated_reproof(core, &guard)
                    .map_err(|error| JsValue::from_str(&error))?;
                ryeos_envelope(live, effects)
            });
        }
        core.bump_generation();
        let effects = core.initial_effects_after_cold_replay(effects);
        let response = ryeos_envelope(&core, effects)?;
        RYEOS_UI.with(|state| *state.borrow_mut() = Some(core));
        Ok(response)
    })
}

#[wasm_bindgen]
pub fn ryeos_abort_seat_replay() {
    RYEOS_REPLAY_CANDIDATE.with(|state| *state.borrow_mut() = None);
}

#[wasm_bindgen]
pub fn ryeos_commit_seat_reproof(completion_json: JsValue) -> Result<JsValue, JsValue> {
    let refresh = RYEOS_REPLAY_CANDIDATE.with(|state| {
        let mut state = state.borrow_mut();
        if state
            .as_ref()
            .is_some_and(|candidate| candidate.refresh.is_some())
        {
            true
        } else {
            *state = None;
            false
        }
    });
    if !refresh {
        return Err(JsValue::from_str("same-producer reproof was not begun"));
    }
    ryeos_commit_seat_replay(completion_json)
}

#[wasm_bindgen]
pub fn ryeos_acknowledged_seat_prefix_len() -> Result<u64, JsValue> {
    RYEOS_UI.with(|state| {
        let state = state.borrow();
        let core = state
            .as_ref()
            .ok_or_else(|| JsValue::from_str("live core absent"))?;
        u64::try_from(core.acknowledged_seat_prefix_len())
            .map_err(|_| JsValue::from_str("acknowledged prefix exceeds native u64"))
    })
}

/// Browser readiness is a positive reducer fact, not the absence of an error
/// from an event whose mismatch path intentionally emits no effects.
#[wasm_bindgen]
pub fn ryeos_require_presentation_producer_ready(
    producer_json: JsValue,
    next_engine_seq: u64,
) -> Result<(), JsValue> {
    let producer: ryeos_client_base::ui::UiPresentationProducer =
        serde_wasm_bindgen::from_value(producer_json)
            .map_err(|error| JsValue::from_str(&format!("invalid producer coordinate: {error}")))?;
    RYEOS_UI.with(|state| {
        let state = state.borrow();
        let core = state
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS core has not started"))?;
        core.require_presentation_producer_ready(&producer, next_engine_seq)
            .map_err(|error| JsValue::from_str(&error))
    })
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
pub fn ryeos_layout_preference_key() -> Result<Option<String>, JsValue> {
    RYEOS_UI.with(|state| {
        let state = state.borrow();
        let core = state
            .as_ref()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        if core.active_surface().is_none() {
            return Ok(None);
        }
        core.layout_preference_key()
            .map(Some)
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
            .open_saved_view_set_template_in_active_context(&template)
            .map_err(|error| JsValue::from_str(&error))?;
        ryeos_envelope(core, effects)
    })
}

/// Start RyeOS, returning the semantic view/scene models and initial effects.
#[wasm_bindgen]
pub fn ryeos_start(
    session_json: JsValue,
    presentation_id: &str,
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

    let mut core = RyeOsCore::new(session, presentation_id, viewport, now_ms);
    core.supports_independent_documents = true;
    core.bump_generation();
    let response = ryeos_envelope(&core, Vec::new())?;

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

/// Derive the identity of one frozen transport batch in the shared Rust owner.
#[wasm_bindgen]
pub fn ryeos_seat_append_identity(identity: JsValue) -> Result<String, JsValue> {
    let identity: ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity =
        serde_wasm_bindgen::from_value(identity).map_err(|error| {
            JsValue::from_str(&format!("invalid seat append identity: {error}"))
        })?;
    identity.derive_identity().map_err(JsValue::from_str)
}

/// Derive an exact control intent in the same Rust owner used by its verifier.
#[wasm_bindgen]
pub fn ryeos_seat_control_identity(request: JsValue) -> Result<String, JsValue> {
    let request: ryeos_client_base::ui::UiSeatControlRequest =
        serde_wasm_bindgen::from_value(request).map_err(|error| {
            JsValue::from_str(&format!("invalid seat control request: {error}"))
        })?;
    request.derive_identity().map_err(JsValue::from_str)
}

/// Replay durable seat braid events into the in-memory RyeOs engine.
#[wasm_bindgen]
pub fn ryeos_replay_acknowledged_seat_events(
    producer_json: JsValue,
    captured_json: JsValue,
    events_json: JsValue,
) -> Result<JsValue, JsValue> {
    let producer = serde_wasm_bindgen::from_value(producer_json)
        .map_err(|error| JsValue::from_str(&format!("invalid acknowledged producer: {error}")))?;
    let captured = serde_wasm_bindgen::from_value(captured_json)
        .map_err(|error| JsValue::from_str(&format!("invalid captured seat prefix: {error}")))?;
    let mut events: Vec<serde_json::Value> =
        serde_wasm_bindgen::from_value(events_json).map_err(|error| {
            JsValue::from_str(&format!("invalid acknowledged seat interval: {error}"))
        })?;
    let retained = events
        .iter_mut()
        .map(|row| {
            for field in ["chain_seq", "thread_seq"] {
                if let Some(value) = row.get(field).and_then(serde_json::Value::as_str) {
                    let position = value
                        .parse::<u64>()
                        .map_err(|_| "invalid original seat position")?;
                    if position.to_string() != value {
                        return Err("noncanonical original seat position".to_owned());
                    }
                    row[field] = serde_json::json!(position);
                }
            }
            if let Some(value) = row
                .get("payload")
                .and_then(|payload| payload.get("seq"))
                .and_then(serde_json::Value::as_str)
            {
                let position = value
                    .parse::<u64>()
                    .map_err(|_| "invalid original producer position")?;
                if position.to_string() != value {
                    return Err("noncanonical original producer position".to_owned());
                }
                row["payload"]["seq"] = serde_json::json!(position);
            }
            ryeos_client_base::ui::seat::retained_seat_event_with_evidence(row)?
                .ok_or_else(|| "acknowledged interval contains a nonproducer row".to_owned())
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(|error| JsValue::from_str(&error))?;
    RYEOS_UI.with(|state| {
        let mut state = state.borrow_mut();
        let core = state
            .as_mut()
            .ok_or_else(|| JsValue::from_str("RyeOS has not been started"))?;
        let effects = core
            .acknowledge_canonical_seat_prefix(&producer, captured, retained)
            .map_err(|error| JsValue::from_str(&error))?;
        ryeos_envelope(core, effects)
    })
}
