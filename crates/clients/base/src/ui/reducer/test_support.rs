//! Shared test fixtures for the reducer cluster test modules.

pub(crate) use crate::ui::dto::RyeOsThreadsDto;
pub(crate) use crate::ui::effect::{RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
pub(crate) use crate::ui::event::{RyeOsEvent, RyeOsUiEvent, RyeOsUiIntent};
pub(crate) use crate::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
pub(crate) use crate::ui::view_model::{
    build_view_model, command_overlay_items_for, view_overlay_items,
};
pub(crate) use crate::view_set::{FocusDirection, ViewSpec};

pub(crate) fn session() -> BrowserSession {
    let effective_surface = serde_json::json!({
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
    });
    BrowserSession {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
        session_id: "session-1".to_string(),
        user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
        // A realistic session carries its surface as data: the engine's
        // default slot set is now empty (it names no views), so the test
        // session declares its slots here as fixture data — the input,
        // threads, and inspector slots the suite was written against.
        surface_attachment_id: "fixture-attachment".into(),
        binding_attachments: vec![crate::ui::binding::UiBindingAttachment {
            binding_attachment_id: "fixture-attachment".into(),
            binding_generation: 1,
            binding_digest: "11".repeat(32),
            surface_ref: "surface:ryeos/ryeos/base".into(),
            surface_generation: "22".repeat(32),
            effective_surface,
            project_path: Some("/tmp/project".into()),
            posture: crate::ui::binding::UiEffectivePosture::ObservationOnly,
            binding_request_bounds: crate::ui::binding::UiBindingRequestBounds {
                max_request_bytes: 64 * 1024,
                max_input_bytes: 16 * 1024,
            },
        }],
        events_url: Some("/ui/events/session/session-1".to_string()),
    }
}

pub(crate) fn writable_session() -> BrowserSession {
    let mut session = session();
    session.binding_attachments[0].posture = crate::ui::binding::UiEffectivePosture::Interactive;
    session
}

pub(crate) fn session_with_surface(effective_surface: serde_json::Value) -> BrowserSession {
    let mut session = writable_session();
    session.binding_attachments[0].effective_surface = effective_surface;
    session
}

pub(crate) fn fixture_attachment(
    id: &str,
    generation: u64,
    digest: &str,
    project_path: Option<&str>,
    effective_surface: serde_json::Value,
) -> crate::ui::binding::UiBindingAttachment {
    crate::ui::binding::UiBindingAttachment {
        binding_attachment_id: id.to_string(),
        binding_generation: generation,
        binding_digest: digest.to_string(),
        surface_ref: format!("surface:test/{id}"),
        surface_generation: format!("surface-generation-{generation}"),
        effective_surface,
        project_path: project_path.map(str::to_string),
        posture: crate::ui::binding::UiEffectivePosture::Interactive,
        binding_request_bounds: crate::ui::binding::UiBindingRequestBounds {
            max_request_bytes: 64 * 1024,
            max_input_bytes: 16 * 1024,
        },
    }
}

pub(crate) fn session_with_attachments(
    surface_attachment_id: &str,
    attachments: Vec<crate::ui::binding::UiBindingAttachment>,
) -> BrowserSession {
    BrowserSession {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
        session_id: "session-1".to_string(),
        user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
        binding_attachments: attachments,
        surface_attachment_id: surface_attachment_id.to_string(),
        events_url: Some("/ui/events/session/session-1".to_string()),
    }
}

pub(crate) fn seed_view(core: &mut RyeOsCore, view_ref: &str) {
    seed_view_value(
        core,
        view_ref,
        serde_json::json!({
            "widget": "rows",
            "sources": { "default": { "ref": "service:test/source", "params": {}, "collection": "rows" } }
        }),
    );
}

pub(crate) fn seed_view_value(core: &mut RyeOsCore, view_ref: &str, value: serde_json::Value) {
    let attachment_id = core
        .insertion_attachment_id(core.view_sets[core.active_view_set].id)
        .expect("fixture view set has an insertion attachment")
        .to_string();
    core.binding_attachments
        .get_mut(&attachment_id)
        .expect("fixture attachment is retained")
        .views
        .insert(view_ref.to_string(), serde_json::from_value(value).unwrap());
}

/// Seed the `view:ryeos/input` chat box (`submit: route`) so the
/// bottom slot instance owns input.
pub(crate) fn seed_input_view(core: &mut RyeOsCore) {
    seed_view_value(
        core,
        "view:ryeos/input",
        serde_json::json!({
            "widget": "text",
            "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route",
                       "completion": { "ref": "service:commands/list", "collection": "commands" },
                       "target": { "cycle": "route_chains" } }
        }),
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

pub(crate) fn active_selection(core: &RyeOsCore) -> serde_json::Value {
    let key = crate::ui::seat::selection_facet_key(core.view_sets[core.active_view_set].id);
    core.seat
        .fold()
        .get(&key)
        .cloned()
        .expect("active view set has selection")
}

/// Seed the `view:ryeos/input` completion source (the slash grammar) into
/// the generic keyed source store, as `initial_effects`' FetchSource would.
pub(crate) fn seed_commands(core: &mut RyeOsCore, commands: serde_json::Value) {
    core.data.sources.insert(
        crate::ui::source_key::RyeOsSourceInstanceKey::completion(
            crate::ui::model::dock_view_instance_key(
                core.view_sets[core.active_view_set].id,
                crate::ui::model::RyeOsDockEdge::Bottom,
            ),
            "line",
        )
        .encode(),
        commands,
    );
}

pub(crate) fn seed_service_route(core: &mut RyeOsCore) {
    seed_input_view(core);
    let (key, _) = core.focused_input_instance().expect("seeded input");
    let route = serde_json::from_value(serde_json::json!({
        "invoke": { "type": "service", "ref": "service:threads/input" },
        "params": {
            "target": {
                "kind": "fresh",
                "item_ref": "directive:demo/base",
                "project_path": "/tmp/project",
                "ref_bindings": { "model": "directive:demo/base" }
            }
        }
    }))
    .expect("valid route");
    core.set_input_route(&key, &route);
}

pub(crate) fn set_focused_route_value(core: &mut RyeOsCore, value: serde_json::Value) {
    let instance = core
        .focused_view_instance_key()
        .expect("a mounted view instance is focused");
    core.seat
        .append_facet(crate::ui::seat::input_route_facet_key(&instance), value);
}

pub(crate) fn focused_route_value(core: &RyeOsCore) -> serde_json::Value {
    let instance = core
        .focused_view_instance_key()
        .expect("a mounted view instance is focused");
    core.seat
        .fold()
        .get(&crate::ui::seat::input_route_facet_key(&instance))
        .cloned()
        .expect("focused view has a route")
}

/// Focus a center tile the way `FocusChanged` would: both the view_set
/// pointer and the explicit UI target move. Tests that poke
/// `view_set.focused_tile` alone leave the initial dock focus standing.
pub(crate) fn focus_tile(core: &mut RyeOsCore, tile_id: crate::ids::TileId) {
    core.view_sets[core.active_view_set].focused_tile = tile_id;
    core.view_sets[core.active_view_set].focus_target =
        Some(crate::ui::model::RyeOsFocusTarget::ViewSetTile {
            tile_id: tile_id.0.to_string(),
        });
}

/// Seed a filtered-list view (`feeds` -> source param) into a focused
/// center tile and return the tile id string (buffer instance id).
pub(crate) fn seed_filter_tile(core: &mut RyeOsCore) -> String {
    seed_view_value(
        core,
        "view:test/filter",
        serde_json::json!({
            "widget": "rows",
            "sources": { "default": { "ref": "service:test/items", "params": { "limit": 50 }, "collection": "items" } },
            "input": { "id": "q", "placeholder": "filter…", "feeds": { "param": "query", "debounce_ms": 120 } }
        }),
    );
    let tile_id = core.view_sets[core.active_view_set]
        .add_tile(ViewSpec {
            view_ref: "view:test/filter".to_string(),
        })
        .expect("fixture layout accepts view");
    focus_tile(core, tile_id);
    tile_id.0.to_string()
}
