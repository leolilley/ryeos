//! An in-memory RyeOS presentation for drawing and interaction checks without
//! a node or a seat. The views are the bundle's own definitions; their
//! sources are answered here with fixed records, through the same fetch
//! effects a node answers. Shared by the `fixture` example and the headless
//! tests.

use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId};
use ryeos_client_base::presentation::UiCompositionPresentation;
use ryeos_client_base::ui::binding::{
    UiBindingAttachmentCoordinate, UiBindingCoordinate, UiPresentationMembership,
    UiSurfaceMembership,
};
use ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest;
use ryeos_client_base::ui::effect::{
    RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind,
};
use ryeos_client_base::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
use ryeos_client_base::ui::presentation_initialization::{
    UiPresentationInitialSelection, UiPresentationInitialization,
    UiPresentationInitializationRecord,
};
use ryeos_client_base::ui::seat::{SeatEvent, SeatEventKind};
use ryeos_client_base::ui::{
    RyeOsEvent, RyeOsUiEvent, UiBindingAttachment, UiBindingRequestBounds, UiBindingSubject,
};
use serde_json::{Value, json};

const PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";
const SURFACE: &str = "22222222-2222-4222-8222-222222222222";

const FILES: &str = include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/project/files.yaml");
const THREADS: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/history.yaml");
const LAUNCHER: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/ui/launcher.yaml");
const EXECUTION: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/development/execution.yaml");
const BUILD_DEPLOY: &str = include_str!("../../base/tests/fixtures/field/build-deploy-live.json");
const TIMELINE: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/chain/timeline.yaml");

fn view(source: &str) -> Result<Value, String> {
    serde_yaml::from_str(source).map_err(|error| format!("bundle view: {error}"))
}

/// What each view's source answers, by view ref.
fn response(view_ref: &str) -> Option<Value> {
    Some(match view_ref {
        "view:ryeos/project/files" => json!({ "entries": [
            { "path": "Cargo.toml", "name": "Cargo.toml", "size": 6120, "is_dir": false },
            { "path": "crates", "name": "crates", "size": 0, "is_dir": true },
            { "path": "crates/clients/rune/src/field.rs", "name": "field.rs", "size": 51200, "is_dir": false },
            { "path": "crates/clients/rune/src/shell.rs", "name": "shell.rs", "size": 40960, "is_dir": false },
            { "path": "README.md", "name": "README.md", "size": 2048, "is_dir": false }
        ]}),
        "view:ryeos/threads/history" => json!({ "threads": [
            { "thread_id": "T-7f3a", "kind": "directive_run", "item_ref": "directive:ryeos/ops/base",
              "project": { "name": "ryeos-next" }, "status": "running", "created_at": "2026-11-12T09:12:00Z" },
            { "thread_id": "T-61c0", "kind": "tool_run", "item_ref": "tool:ryeos/dev/gate",
              "project": { "name": "ryeos-next" }, "status": "completed", "created_at": "2026-11-12T08:40:00Z" },
            { "thread_id": "T-5d21", "kind": "directive_run", "item_ref": "directive:ryeos/review/diff",
              "project": { "name": "ryeos-next" }, "status": "failed", "created_at": "2026-11-12T07:03:00Z" }
        ], "coverage": { "complete": false, "returned": 3 } }),
        "view:ryeos/development/execution" => serde_json::from_str(BUILD_DEPLOY).ok()?,
        "view:ryeos/chain/timeline" => json!({ "events": [
            { "event_type": "cognition_in", "chain_seq": 1, "payload": { "turn": 1 } },
            { "event_type": "cognition_out", "chain_seq": 2, "payload": { "content": "Reading the layout module first." } },
            { "event_type": "tool_call_start", "chain_seq": 3, "payload": { "tool": "read_file", "call_id": "c1" } },
            { "event_type": "tool_call_result", "chain_seq": 4, "payload": { "tool": "read_file", "call_id": "c1", "result_size_bytes": 4096 } },
            { "event_type": "cognition_out", "chain_seq": 5, "payload": { "content": "The dock sizes are pixels in both renderers." } }
        ]}),
        _ => return None,
    })
}

/// Answer the source fetches among `effects`; returns how many were answered.
fn answer(core: &mut RyeOsCore, effects: Vec<RyeOsEffect>) -> usize {
    let mut answered = 0;
    for effect in effects {
        let RyeOsEffectKind::FetchSource { request, .. } = &effect.kind else {
            continue;
        };
        let UiBindingCoordinate::Source { view_ref, .. } = &request.coordinate else {
            continue;
        };
        let Some(data) = response(view_ref) else {
            continue;
        };
        answered += 1;
        let more = core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(data),
                error: None,
            },
        });
        answered += answer(core, more);
    }
    answered
}

/// The execution field with its thread given directly: the fixture has no
/// selection facet to resolve it from.
fn execution() -> Result<Value, String> {
    let mut definition = view(EXECUTION)?;
    definition["sources"]["execution"]["params"]["thread_id"] = json!("T-build-root");
    Ok(definition)
}

/// The presentation for `scene`: `"field"` docks the execution field,
/// `"launcher"` opens the launcher, anything else is the default shell.
pub fn core(scene: &str) -> Result<RyeOsCore, String> {
    let field = scene == "field";
    let tiles = if field {
        json!([
            "view:ryeos/development/execution",
            "view:ryeos/threads/history"
        ])
    } else {
        json!(["view:ryeos/threads/history", "view:ryeos/chain/timeline"])
    };
    let presentation = UiCompositionPresentation::from_effective_surface(json!({
        "name": "workbench", "title": "Workbench", "version": "1.0.0",
        "tiling": { "mode": "master_stack",
            "master": { "side": "left", "arrange": "vertical", "ratio": 0.6, "count": 1 },
            "stack": { "arrange": "vertical" }, "insert": "end" },
        "tiles": tiles,
        "navigation": [
            { "id": "threads", "label": "Threads", "view": "view:ryeos/threads/history" },
            { "id": "timeline", "label": "Timeline", "view": "view:ryeos/chain/timeline" }
        ],
        "views": {
            "view:ryeos/threads/history": view(THREADS)?,
            "view:ryeos/development/execution": execution()?,
            "view:ryeos/chain/timeline": view(TIMELINE)?
        }
    }))
    .map_err(|error| format!("surface fixture: {error}"))?;
    let surface_attachment = UiBindingAttachment {
        binding_attachment_id: "surface-attachment".into(),
        binding_generation: 1,
        binding_digest: "surface-binding".into(),
        root_ref: "surface:ryeos/ui/workbench".to_string(),
        root_generation: "surface-generation".into(),
        project_path: Some("/home/fixture/ryeos-next".into()),
        subject: UiBindingSubject::UnregisteredProject,
        posture: Default::default(),
        binding_request_bounds: UiBindingRequestBounds {
            max_request_bytes: 4096,
            max_input_bytes: 1024,
            max_seat_append_bytes: 64 * 1024,
        },
        presentation,
    };
    let shared = serde_json::from_value(json!({
        "binding_attachment_id": "shared-attachment", "binding_generation": 2,
        "binding_digest": "shared-binding", "root_ref": "config:fixture/presentation",
        "root_generation": "shared-generation", "project_path": "/home/fixture/ryeos-next",
        "subject": { "kind": "unregistered_project" }, "posture": "observation_only",
        "binding_request_bounds": { "max_request_bytes": 4096, "max_input_bytes": 1024, "max_seat_append_bytes": 65536 },
        "presentation": { "definition": { "kind": "shared_presentation", "config": {
            "category": "ui", "shared_presentation": { "schema": 1, "title": "Fixture", "launcher": "launcher",
                "dialogs": [{ "id": "launcher", "title": "Launch", "view": "view:ryeos/ui/launcher" }],
                "docks": {
                    "left": { "content": "view:ryeos/project/files", "open": true, "size": 260 },
                    "bottom": { "content": "view:ryeos/input", "open": true, "size": 180 }
                },
                "capabilities": { "sources": [], "affordances": [] } }
        }}, "views": {
            "view:ryeos/ui/launcher": view(LAUNCHER)?,
            "view:ryeos/project/files": view(FILES)?,
            "view:ryeos/input": { "widget": "text",
                "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route" } }
        }}
    }))
    .map_err(|error| format!("shared fixture: {error}"))?;
    let mut session = BrowserSession {
        session_id: "S-fixture".to_string(),
        ui_binding_contract_revision: ryeos_client_base::UI_BINDING_CONTRACT_REVISION.into(),
        root_attachment_id: "shared-attachment".into(),
        // Canonical order: by attachment id.
        binding_attachments: vec![shared, surface_attachment],
        ..Default::default()
    };
    let surface_binding = UiBindingAttachmentCoordinate {
        binding_attachment_id: "surface-attachment".into(),
        binding_generation: 1,
        binding_digest: "surface-binding".into(),
    };
    let surface = session
        .binding_attachments
        .iter()
        .find(|attachment| attachment.binding_attachment_id == "surface-attachment")
        .ok_or("the fixture has no surface attachment")?
        .presentation
        .definition
        .surface()
        .ok_or("the fixture surface is not a surface")?;
    let seeds = surface
        .to_mount_seeds()
        .map_err(|error| error.to_string())?;
    let first = seeds.first().map(|seed| seed.id.to_string());
    let mount_manifest = UiSurfaceMountManifest::allocate(
        PresentationId::try_new(PRESENTATION).map_err(|error| error.to_string())?,
        SurfaceInstanceId::try_new(SURFACE).map_err(|error| error.to_string())?,
        1,
        seeds
            .into_iter()
            .map(|seed| (seed, surface_binding.clone()))
            .collect(),
        first.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    session.presentations = vec![UiPresentationMembership {
        presentation_id: PRESENTATION.into(),
        next_mount_id: mount_manifest.next_mount_id,
        initialization_digest: "a".repeat(64),
        root_binding: UiBindingAttachmentCoordinate {
            binding_attachment_id: "shared-attachment".into(),
            binding_generation: 2,
            binding_digest: "shared-binding".into(),
        },
        thread_id: Some("fixture-seat".into()),
        chain_root_id: Some("fixture-chain".into()),
        surfaces: vec![UiSurfaceMembership {
            saved_surface: None,
            surface_instance_id: SURFACE.into(),
            ordinal: 1,
            title: "Workbench".into(),
            binding: surface_binding,
            mount_manifest,
        }],
    }];
    let membership = &mut session.presentations[0];
    let initialization = UiPresentationInitialization {
        schema: 2,
        selection: UiPresentationInitialSelection::ConfiguredDefaults,
        presentation_id: PRESENTATION.into(),
        creation_request_id: "33333333-3333-4333-8333-333333333333".into(),
        root_binding: membership.root_binding.clone(),
        next_mount_id: membership.next_mount_id,
        surfaces: membership.surfaces.clone(),
        binding_attachments: session.binding_attachments.clone(),
    };
    initialization
        .validate()
        .map_err(|error| format!("fixture initialization: {error}"))?;
    membership.initialization_digest =
        initialization.digest().map_err(|error| error.to_string())?;
    let mut core = RyeOsCore::new(session, PRESENTATION, BrowserViewport::default(), 0);
    core.replay_seat_events([SeatEvent {
        producer_engine_seq: None,
        seq: 0,
        kind: SeatEventKind::PresentationInitialized(UiPresentationInitializationRecord {
            operation_id: initialization.creation_request_id.clone(),
            initialization,
        }),
    }]);
    core.require_replayed_initialization()
        .map_err(|error| format!("fixture replay: {error}"))?;
    // As the runtime does after a start: the mounted views' first fetches.
    let effects = core.initial_effects();
    let mut answered = answer(&mut core, effects);
    let effects = core.dispatch(RyeOsEvent::Ui {
        event: RyeOsUiEvent::FocusInput,
    });
    answered += answer(&mut core, effects);
    // The scene picks what is open when the frame is drawn.
    match scene {
        "" | "field" => {}
        "launcher" => {
            let effects = core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::OpenLauncher,
            });
            answered += answer(&mut core, effects);
        }
        other => return Err(format!("unknown fixture scene {other}")),
    }
    eprintln!("fixture: answered {answered} source fetches");
    Ok(core)
}
