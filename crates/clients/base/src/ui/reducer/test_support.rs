//! Shared test fixtures for the reducer cluster test modules.

pub(crate) use crate::ui::dto::RyeOsThreadsDto;
pub(crate) use crate::ui::effect::{RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
pub(crate) use crate::ui::event::{RyeOsUiIntent, RyeOsEvent, RyeOsUiEvent};
pub(crate) use crate::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
pub(crate) use crate::ui::view_model::{
    build_view_model, command_overlay_items_for, view_overlay_items,
};
pub(crate) use crate::workspace::{FocusDirection, ViewSpec};

pub(crate) fn session() -> BrowserSession {
    BrowserSession {
        session_id: "session-1".to_string(),
        surface_ref: "surface:ryeos/ryeos/base".to_string(),
        user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
        // A realistic session carries its surface as data: the engine's
        // default slot set is now empty (it names no views), so the test
        // session declares its slots here as fixture data — the input,
        // threads, and inspector slots the suite was written against.
        effective_surface: Some(serde_json::json!({
            "name": "ryeos-base",
            "slots": {
                "bottom": { "content": "view:ryeos/input", "open": true, "size": 7 },
                "left": { "content": "view:ryeos/threads/list", "open": false, "size": 32 },
                "right": { "content": "view:ryeos/item/inspector", "open": false, "size": 40 }
            },
            "views": {
                "view:ryeos/input": {
                    "widget": "text",
                    "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route" }
                }
            }
        })),
        project_path: Some("/tmp/project".to_string()),
        read_only: true,
        granted_caps: Vec::new(),
        events_url: Some("/ui/events/session/session-1".to_string()),
    }
}

pub(crate) fn writable_session() -> BrowserSession {
    BrowserSession {
        read_only: false,
        ..session()
    }
}

pub(crate) fn atlas_session() -> BrowserSession {
    BrowserSession {
        surface_ref: "surface:ryeos/ryeos/atlas".to_string(),
        effective_surface: Some(serde_json::json!({
            "name": "ryeos-atlas",
            "version": "1.0.0",
            "tiles": [],
            "ambient": {
                "show_background": true,
                "opacity": 1.0,
                "mode": "namespace_atlas",
                "atlas": { "style": "flat_2d" }
            }
        })),
        project_path: None,
        ..session()
    }
}

pub(crate) fn seed_view(core: &mut RyeOsCore, view_ref: &str) {
    core.views.insert(
        view_ref.to_string(),
        serde_json::from_value(serde_json::json!({
            "widget": "rows",
            "source": { "ref": "service:test/source", "params": {}, "collection": "rows" }
        }))
        .unwrap(),
    );
}

pub(crate) fn seed_view_value(core: &mut RyeOsCore, view_ref: &str, value: serde_json::Value) {
    core.views
        .insert(view_ref.to_string(), serde_json::from_value(value).unwrap());
}

/// Seed the `view:ryeos/input` chat box (`submit: route`) so the
/// bottom slot instance owns input.
pub(crate) fn seed_input_view(core: &mut RyeOsCore) {
    core.views.insert(
        "view:ryeos/input".to_string(),
        serde_json::from_value(serde_json::json!({
            "widget": "text",
            "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route",
                       "completion": { "ref": "service:commands/list", "collection": "commands" },
                       "target": { "cycle": "route_chains" } }
        }))
        .unwrap(),
    );
}

/// Write the focused input instance's transient buffer.
pub(crate) fn set_focused_input(core: &mut RyeOsCore, text: &str) {
    let len = text.len();
    core.focused_input_buffer_mut()
        .expect("an input instance is focused")
        .set_text(text.to_string(), len);
}

/// Read the focused input instance's buffer text.
pub(crate) fn focused_input_text(core: &RyeOsCore) -> String {
    core.focused_input_buffer()
        .map(|buffer| buffer.text.clone())
        .unwrap_or_default()
}

/// Seed the `view:ryeos/input` completion source (the slash grammar) into
/// the generic keyed source store, as `initial_effects`' FetchSource would.
pub(crate) fn seed_commands(core: &mut RyeOsCore, commands: serde_json::Value) {
    core.data.sources.insert(
        crate::ui::content::completion_source_key("view:ryeos/input", "line"),
        commands,
    );
}

pub(crate) fn seed_service_route(core: &mut RyeOsCore) {
    seed_input_view(core);
    core.seat.append_facet(
        crate::ui::seat::KEY_INPUT_ROUTE,
        serde_json::json!({
            "invoke": { "type": "service", "ref": "service:threads/input" },
            "params": { "directive": "directive:demo/base" }
        }),
    );
}

/// Seed a filtered-list view (`feeds` -> source param) into a focused
/// center tile and return the tile id string (buffer instance id).
pub(crate) fn seed_filter_tile(core: &mut RyeOsCore) -> String {
    seed_view_value(
        core,
        "view:test/filter",
        serde_json::json!({
            "widget": "rows",
            "source": { "ref": "service:test/items", "params": { "limit": 50 }, "collection": "items" },
            "input": { "id": "q", "placeholder": "filter…", "feeds": { "param": "query", "debounce_ms": 120 } }
        }),
    );
    let tile_id = core.workspace.add_tile(ViewSpec {
        view_ref: "view:test/filter".to_string(),
    });
    core.workspace.focused_tile = tile_id;
    tile_id.0.to_string()
}
