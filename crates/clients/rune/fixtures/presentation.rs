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
    RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind, RyeOsUiError,
};
use ryeos_client_base::ui::event::RyeOsInputAction;
use ryeos_client_base::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
use ryeos_client_base::ui::presentation_initialization::{
    UiPresentationInitialSelection, UiPresentationInitialization,
    UiPresentationInitializationRecord,
};
use ryeos_client_base::ui::seat::{SeatEvent, SeatEventKind};
use ryeos_client_base::ui::timeline::TIMELINE_CONTRACT_VERSION;
use ryeos_client_base::ui::{
    RyeOsEvent, RyeOsUiEvent, UiBindingAttachment, UiBindingRequestBounds, UiBindingSubject,
};
use serde_json::{Value, json};

const PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";
const SURFACE: &str = "22222222-2222-4222-8222-222222222222";

const THREADS: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/history.yaml");
const LAUNCHER: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/ui/launcher.yaml");
const EXECUTION: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/development/execution.yaml");
const BUILD_DEPLOY: &str = include_str!("../../base/tests/fixtures/field/build-deploy-live.json");
const TIMELINE: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/chain/timeline.yaml");
const TRANSCRIPT: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/thread/transcript.yaml");
const CONVERSATION: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/thread/conversation.yaml");
const CONVERSATION_REF: &str = "view:ryeos/thread/conversation";
const SCHEDULES: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/schedules/list.yaml");
const SURFACES: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/views/ryeos/surfaces/list.yaml");

fn view(source: &str) -> Result<Value, String> {
    serde_yaml::from_str(source).map_err(|error| format!("bundle view: {error}"))
}

/// What each view's source answers, by view ref. A `long` scene answers with
/// enough threads and events that lists build only the rows in view.
fn response(view_ref: &str, scene: &str) -> Option<Value> {
    if scene == "long" {
        return long_response(view_ref);
    }
    if is_timeline_scene(scene) {
        return (view_ref == CONVERSATION_REF).then(|| demo_timeline(scene));
    }
    if scene == "states" {
        return states_response(view_ref);
    }
    Some(match view_ref {
        "view:ryeos/threads/history" => json!({ "threads": [
            { "thread_id": "T-7f3a", "kind": "directive_run", "item_ref": "directive:ryeos/ops/base",
              "project": { "name": "ryeos-next" }, "status": "running", "created_at": "2026-11-12T09:12:00Z" },
            { "thread_id": "T-61c0", "kind": "tool_run", "item_ref": "tool:ryeos/dev/gate",
              "project": { "name": "ryeos-next" }, "status": "completed", "created_at": "2026-11-12T08:40:00Z" },
            { "thread_id": "T-5d21", "kind": "directive_run", "item_ref": "directive:ryeos/review/diff",
              "project": { "name": "ryeos-next" }, "status": "failed", "created_at": "2026-11-12T07:03:00Z" }
        ], "coverage": { "complete": false, "returned": 3 } }),
        "view:ryeos/development/execution" => serde_json::from_str(BUILD_DEPLOY).ok()?,
        "view:ryeos/chain/timeline" => {
            json!({ "timeline_contract": TIMELINE_CONTRACT_VERSION, "older": 0, "items": [
                fixture_entry("m1", "message", "x1", json!({ "body": "Review the layout module." })),
                fixture_entry("p1", "prose", "x1", json!({ "body": "Reading the layout module first." })),
                with_state(fixture_entry("a1", "activity", "x1", json!({ "headline": "read_file", "badge": "4.0 KiB" })), "done"),
                fixture_entry("p2", "prose", "x1", json!({ "body": "Tile sizes are pixels in both renderers." }))
            ]})
        }
        _ => return None,
    })
}

/// One timeline-service entry item.
fn fixture_entry(id: &str, kind: &str, exchange: &str, slots: Value) -> Value {
    json!({
        "item": "entry", "id": id, "type": kind, "state": null, "tone": null,
        "thread": "T-7f3a", "parent": null, "exchange": exchange, "counts_as": null,
        "slots": slots, "seq": [1, 1], "ts": ["t", "t"]
    })
}

fn with_state(mut entry: Value, state: &str) -> Value {
    entry["state"] = json!(state);
    entry
}

/// Rows past the length at which lists build only what is in view.
const LONG_ROWS: usize = 200;

fn long_response(view_ref: &str) -> Option<Value> {
    Some(match view_ref {
        "view:ryeos/threads/history" => {
            let threads: Vec<Value> = (0..LONG_ROWS)
                .map(|index| {
                    json!({
                        "thread_id": format!("T-{index:04}"), "kind": "tool_run",
                        "item_ref": "tool:ryeos/dev/gate", "project": { "name": "ryeos-next" },
                        "status": "completed", "created_at": "2026-11-12T08:40:00Z"
                    })
                })
                .collect();
            json!({ "threads": threads, "coverage": { "complete": true, "returned": LONG_ROWS } })
        }
        "view:ryeos/chain/timeline" => {
            let items: Vec<Value> = (0..LONG_ROWS)
                .map(|index| {
                    fixture_entry(
                        &format!("p{index}"),
                        "prose",
                        &format!("x{index}"),
                        json!({ "body": format!("Step {} of the run.", index + 1) }),
                    )
                })
                .collect();
            json!({ "timeline_contract": TIMELINE_CONTRACT_VERSION, "older": 0, "items": items })
        }
        _ => return None,
    })
}

/// Answer the source fetches among `effects`; returns how many were answered.
fn answer(core: &mut RyeOsCore, effects: Vec<RyeOsEffect>, scene: &str) -> usize {
    let mut answered = 0;
    for effect in effects {
        let RyeOsEffectKind::FetchSource { request, .. } = &effect.kind else {
            continue;
        };
        let UiBindingCoordinate::Source { view_ref, .. } = &request.coordinate else {
            continue;
        };
        let failure = (scene == "states" && view_ref == "view:ryeos/surfaces/list").then(|| {
            RyeOsUiError::definite(
                "service_cap_denied",
                "the surface listing is not admitted here",
            )
        });
        let result = match (failure, response(view_ref, scene)) {
            (Some(error), _) => RyeOsEffectResult {
                id: effect.id,
                ok: false,
                kind: RyeOsEffectResultKind::SourceData,
                data: None,
                error: Some(error),
            },
            (None, Some(data)) => RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(data),
                error: None,
            },
            // A source nobody answers stays loading.
            (None, None) => continue,
        };
        answered += 1;
        let more = core.dispatch(RyeOsEvent::EffectResult { result });
        answered += answer(core, more, scene);
    }
    answered
}

/// The chain timeline. Its source names the input route's chain, which the
/// fixture has no route for; the `long` scene names a chain directly so the
/// timeline has events to show.
fn timeline(named_chain: bool) -> Result<Value, String> {
    let mut definition = view(TIMELINE)?;
    if named_chain {
        definition["sources"]["default"]["params"]["chain_root_id"] = json!("chain-long");
    }
    Ok(definition)
}

/// The conversation view as the resolver gives it: the transcript it extends,
/// with its own title and composer, naming the chain directly.
fn conversation() -> Result<Value, String> {
    let mut definition = view(TRANSCRIPT)?;
    let own = view(CONVERSATION)?;
    definition["title"] = own["title"].clone();
    definition["input"] = own["input"].clone();
    definition["sources"]["default"]["params"]["chain_root_id"] = json!("chain-demo");
    Ok(definition)
}

/// The draft the `timeline-draft` scene types.
const DRAFT: &str = "Retry the fetch with a longer timeout, and read the retry settings first.";

/// The first input anywhere in a serialised view model.
fn find_input(value: &Value) -> Option<ryeos_client_base::ui::view_model::RyeOsInputVm> {
    match value {
        Value::Object(map) => {
            if map.contains_key("address")
                && map.contains_key("placeholder")
                && let Ok(input) = serde_json::from_value(value.clone())
            {
                return Some(input);
            }
            map.values().find_map(find_input)
        }
        Value::Array(items) => items.iter().find_map(find_input),
        _ => None,
    }
}

fn is_timeline_scene(scene: &str) -> bool {
    matches!(
        scene,
        "timeline" | "timeline-draft" | "timeline-top" | "timeline-asks"
    )
}

/// The `states` scene: the first table answers with no rows, the second is
/// never answered and stays loading, the third fails (see `answer`).
fn states_response(view_ref: &str) -> Option<Value> {
    match view_ref {
        "view:ryeos/threads/history" => {
            Some(json!({ "threads": [], "coverage": { "state": "complete", "reason": "none" } }))
        }
        _ => None,
    }
}

/// A tone and state on an entry item.
fn with_tone(mut entry: Value, tone: &str) -> Value {
    entry["tone"] = json!(tone);
    entry
}

/// A conversation that draws every entry type and group: an earlier exchange
/// folded into a recap, then an operator message, a thought, prose,
/// activities in each state, a link, long entries, an answered ask, an
/// outcome, one pending ask, and a closing cluster. The lane waits on the
/// pending ask. The `timeline-asks` scene adds a second one, which the dock
/// stacks; the `timeline-top` scene keeps the start of the conversation.
fn demo_timeline(scene: &str) -> Value {
    let entry = |id: &str, kind: &str, slots: Value| fixture_entry(id, kind, "x1", slots);
    let done = |id: &str, headline: &str, detail: &str, badge: Option<&str>| {
        let mut slots = json!({ "headline": headline, "detail": detail });
        if let Some(badge) = badge {
            slots["badge"] = json!(badge);
        }
        with_tone(with_state(entry(id, "activity", slots), "done"), "good")
    };
    let bare = |mut value: Value| {
        value.as_object_mut().map(|map| map.remove("item"));
        value
    };
    let long_body = "The retry loop waits a fixed interval between attempts and gives up after the third one, so a slow response that would have arrived on the fourth is reported as a failure. Raising the limit helps only when the server is merely slow; when it is refusing connections the same change just delays the error. ".repeat(3);
    let pending_ask = |id: &str, headline: &str, detail: &str, allowed: bool| {
        with_tone(
            with_state(
                entry(
                    id,
                    "ask",
                    json!({
                        "headline": headline, "detail": detail,
                        "approval_id": format!("approval-{id}"),
                        "request_digest": format!("digest-{id}"),
                        "accept_allowed": if allowed { "true" } else { "false" }
                    }),
                ),
                "pending",
            ),
            "warn",
        )
    };
    let items = vec![
        json!({
            "item": "recap", "id": "rc:e0", "exchange": "x0", "work": 7, "duration_s": 124,
            "seq": 12,
            "members": [
                fixture_entry("e0", "activity", "x0", json!({ "headline": "Read", "detail": "docs/overview.md" })),
                fixture_entry("e1", "activity", "x0", json!({ "headline": "Read", "detail": "src/lib.rs" })),
            ]
        }),
        with_state(
            entry(
                "mk1",
                "marker",
                json!({ "headline": "continued in a new thread" }),
            ),
            "noted",
        ),
        entry(
            "m1",
            "message",
            json!({ "body": "Check why the fetch of the release notes keeps failing, and fix it if the cause is ours." }),
        ),
        entry(
            "t1",
            "thought",
            json!({ "body": "The failure looks like a timeout rather than a missing page. I should read the retry settings before changing anything." }),
        ),
        entry(
            "p1",
            "prose",
            json!({ "body": "I will fetch the page once more to see the failure myself, then read the retry settings.\nIf the cause is the timeout I will say so before I change it." }),
        ),
        with_state(
            entry(
                "a1",
                "activity",
                json!({ "headline": "Fetching the release notes", "detail": "docs.example.org/releases" }),
            ),
            "running",
        ),
        done("a2", "Read", "src/net/retry.rs", None),
        with_tone(
            with_state(
                entry(
                    "a3",
                    "activity",
                    json!({ "headline": "Fetch", "detail": "docs.example.org/releases: timed out after 30 s" }),
                ),
                "failed",
            ),
            "danger",
        ),
        done("a4", "Read", "crates/net/src/client.rs", Some("412 lines")),
        with_tone(
            with_state(
                entry(
                    "l1",
                    "link",
                    json!({ "headline": "review", "target": "T-e84f17c6" }),
                ),
                "completed",
            ),
            "good",
        ),
        done(
            "a5",
            "Searched the project for every place that sets a request timeout, including the defaults in the shared client configuration and the per-service overrides that the build script generates",
            "src/**/*.rs",
            None,
        ),
        entry("p2", "prose", json!({ "body": long_body })),
        with_state(
            entry(
                "k0",
                "ask",
                json!({ "headline": "Write a file in the project", "detail": "src/net/retry.rs" }),
            ),
            "approved",
        ),
        with_tone(
            with_state(
                entry(
                    "o1",
                    "outcome",
                    json!({ "headline": "Turn finished", "reason": "Three steps, one failed" }),
                ),
                "completed",
            ),
            "good",
        ),
        pending_ask(
            "k1",
            "Write a file outside the workspace",
            "~/.config/example/settings.yaml",
            true,
        ),
        pending_ask(
            "k2",
            "Run a command with network access",
            "curl https://example.org/install.sh",
            false,
        ),
        json!({
            "item": "cluster", "id": "cl:c1", "headline": "3 reads · 2 searches",
            "members": [
                bare(done("c1", "Read", "src/config.rs", None)),
                bare(done("c2", "Read", "src/net/mod.rs", None)),
                bare(done("c3", "Searched", "retry", None)),
            ]
        }),
    ];
    // One pending ask, as most conversations have; the second joins it in the
    // `timeline-asks` scene only.
    let items: Vec<Value> = items
        .into_iter()
        .filter(|item| scene == "timeline-asks" || item["id"].as_str() != Some("k2"))
        .collect();
    // The top scene keeps the start of the conversation and the ask, so the
    // whole of it fits one frame.
    let items: Vec<Value> = if scene == "timeline-top" {
        items
            .into_iter()
            .filter(|item| {
                matches!(
                    item["id"].as_str(),
                    Some("rc:e0" | "mk1" | "m1" | "t1" | "p1" | "a1" | "a2" | "a3" | "l1" | "k1")
                )
            })
            .collect()
    } else {
        items
    };
    json!({
        "timeline_contract": TIMELINE_CONTRACT_VERSION, "chain_root_id": "chain-demo",
        "older": 0, "next_limit": null, "highest_chain_seq": 40,
        "summary": { "status": "running", "input_tokens": 184220, "output_tokens": 21840, "spend_usd": 1.62, "turns": 7 },
        "lane": { "label": "Waiting for approval", "entry": "k1" },
        "items": items
    })
}

/// The execution field with its thread given directly: the fixture has no
/// selection facet to resolve it from.
fn execution() -> Result<Value, String> {
    let mut definition = view(EXECUTION)?;
    definition["sources"]["execution"]["params"]["thread_id"] = json!("T-build-root");
    Ok(definition)
}

/// The presentation for `scene`: `"field"` tiles the execution field,
/// `"launcher"` opens the launcher, anything else is the default shell.
pub fn core(scene: &str) -> Result<RyeOsCore, String> {
    let field = scene == "field";
    let tiles = if field {
        json!([
            "view:ryeos/development/execution",
            "view:ryeos/threads/history"
        ])
    } else if is_timeline_scene(scene) {
        json!([CONVERSATION_REF])
    } else if scene == "tabs" {
        json!([])
    } else if scene == "states" {
        json!([
            "view:ryeos/threads/history",
            "view:ryeos/schedules/list",
            "view:ryeos/surfaces/list"
        ])
    } else {
        json!(["view:ryeos/threads/history", "view:ryeos/chain/timeline"])
    };
    let mut surface_json = json!({
        "name": "workbench", "title": "Workbench", "version": "1.0.0",
        "tiling": { "mode": "master_stack",
            "master": { "side": "left", "arrange": "vertical", "ratio": 0.6, "count": 1 },
            "stack": { "arrange": "vertical" }, "insert": "end" },
        "tiles": tiles,
        "views": {
            "view:ryeos/threads/history": view(THREADS)?,
            "view:ryeos/development/execution": execution()?,
            "view:ryeos/chain/timeline": timeline(scene == "long")?,
            CONVERSATION_REF: conversation()?,
            "view:ryeos/schedules/list": view(SCHEDULES)?,
            "view:ryeos/surfaces/list": view(SURFACES)?
        }
    });
    if scene == "tabs" {
        // More view sets than the band fits, each empty.
        surface_json["view_sets"] = (1..=15)
            .map(|n| {
                json!({
                    "schema": ryeos_client_base::surface::view_sets::VIEW_SET_COMPOSITION_SCHEMA,
                    "id": format!("set_{n}"),
                    "title": format!("View set {n}"),
                })
            })
            .collect();
    }
    let presentation = UiCompositionPresentation::from_effective_surface(surface_json)
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
            "category": "ui", "shared_presentation": { "schema": 2, "title": "Fixture", "launcher": "launcher",
                "dialogs": [{ "id": "launcher", "title": "Launch", "view": "view:ryeos/ui/launcher" }],
                "capabilities": { "sources": [], "affordances": [] } }
        }}, "views": {
            "view:ryeos/ui/launcher": view(LAUNCHER)?
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
    // The tabs scene opens on its last view set, so the current tab is the
    // one furthest along the band.
    let first = if scene == "tabs" {
        seeds.last()
    } else {
        seeds.first()
    }
    .map(|seed| seed.id.to_string());
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
    let mut answered = answer(&mut core, effects, scene);
    let effects = core.dispatch(RyeOsEvent::Ui {
        event: RyeOsUiEvent::FocusInput,
    });
    answered += answer(&mut core, effects, scene);
    // The scene picks what is open when the frame is drawn.
    match scene {
        "" | "field" | "long" | "states" | "chips" | "switcher-right" | "switcher-top" | "bare"
        | "overlay" => {}
        "timeline" | "timeline-draft" | "timeline-top" | "timeline-asks" => {
            // The newest group is opened, so an opened group is drawn too.
            if scene != "timeline-top" {
                core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::ToggleTimelineFold,
                });
            }
            if scene == "timeline-draft" {
                let vm = ryeos_client_base::ui::view_model::build_view_model(&core);
                let input = serde_json::to_value(&vm)
                    .ok()
                    .and_then(|vm| find_input(&vm))
                    .ok_or("the timeline scene has no composer")?;
                core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::InputAt {
                        address: input.address,
                        action: RyeOsInputAction::SetText {
                            text: DRAFT.to_owned(),
                            cursor: DRAFT.chars().count(),
                        },
                    },
                });
            }
        }
        "tabs" => {
            // The tab band shown, as Alt+T leaves it.
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::Activate {
                    intent: ryeos_client_base::ui::event::RyeOsUiIntent::ToggleTopStatusBar,
                },
            });
        }
        "launcher" => {
            let effects = core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::OpenLauncher,
            });
            answered += answer(&mut core, effects, scene);
        }
        other => return Err(format!("unknown fixture scene {other}")),
    }
    eprintln!("fixture: answered {answered} source fetches");
    Ok(core)
}

/// What `scene` adds to each view model the in-memory core builds: counts,
/// marks and flags whose producers are a node's, not the core's.
pub fn dress(scene: &str) -> ryeos_client_rune::Dress {
    use ryeos_client_base::ui::view_model::RyeOsEdgeVm as Edge;
    match scene {
        "chips" => dress_chips,
        "tabs" => dress_tabs,
        "switcher-right" => |vm| {
            vm.presentation.chrome.switcher.edge = Edge::Right;
            vm.presentation.chrome.status_bar.edge = Edge::Top;
        },
        "switcher-top" => |vm| {
            dress_chips(vm);
            vm.presentation.chrome.switcher.edge = Edge::Top;
        },
        // The website's Surface: no switcher, no status bar, tabs with no
        // plus.
        "bare" => |vm| {
            vm.presentation.chrome.switcher.edge = Edge::Hidden;
            vm.presentation.chrome.status_bar.edge = Edge::Hidden;
            vm.presentation.chrome.top_bar.visible = true;
            vm.presentation.chrome.top_bar.add_tab = false;
        },
        "overlay" => dress_overlay,
        _ => |_| {},
    }
}

/// Four open Surfaces, one of each chip: Home's grid with work needing you,
/// the node's key with a run going, the project's (current, project-authored)
/// and a plain one.
fn dress_chips(vm: &mut ryeos_client_base::ui::view_model::RyeOsViewModel) {
    use ryeos_client_base::ui::view_model::RyeOsChipMarkVm as Mark;
    let Some(template) = vm.session.surfaces.first().cloned() else {
        return;
    };
    let choices = [
        ("Home", Mark::Grid, false, Some(2), Some(1)),
        ("rye", Mark::Key, false, Some(0), Some(3)),
        ("ryeos", Mark::Initial, true, Some(0), Some(0)),
        ("notes", Mark::Initial, false, None, None),
    ];
    vm.session.surfaces = choices
        .into_iter()
        .enumerate()
        .map(|(index, (title, mark, project, needs_you, running))| {
            let mut choice = template.clone();
            if index > 0 {
                choice.surface_instance_id = serde_json::from_value(json!(format!(
                    "33333333-3333-4333-8333-33333333333{index}"
                )))
                .expect("a fixture Surface id");
            }
            choice.ordinal = index as u64 + 1;
            choice.title = title.to_owned();
            choice.active = project;
            choice.chip.mark = mark;
            choice.chip.authored_in_project = project;
            choice.chip.fingerprint = (mark == Mark::Key).then(|| "ed25519:3f9a0c71e2".to_owned());
            choice.needs_you = needs_you;
            choice.running = running;
            choice
        })
        .collect();
}

/// The tabs scene with one tab needing you and one running.
fn dress_tabs(vm: &mut ryeos_client_base::ui::view_model::RyeOsViewModel) {
    for (index, tab) in vm.presentation.chrome.top_bar.tabs.iter_mut().enumerate() {
        tab.needs_you = Some(index == 2);
        tab.running = Some(index == 4);
    }
}

/// The overlay open over the chips scene: two rows needing you (the first
/// selected, with its actions), more than are shown, and one running.
fn dress_overlay(vm: &mut ryeos_client_base::ui::view_model::RyeOsViewModel) {
    use ryeos_client_base::ui::event::RyeOsUiIntent;
    use ryeos_client_base::ui::view_model::{RyeOsOverlayActionVm, RyeOsOverlayRowVm, RyeOsTone};
    dress_chips(vm);
    let now = i64::try_from(vm.now_ms).unwrap_or(i64::MAX);
    let row = |id: &str, title: &str, where_: &str, tone, minutes: i64| RyeOsOverlayRowVm {
        id: id.to_owned(),
        title: title.to_owned(),
        where_: where_.to_owned(),
        tone,
        since_ms: now - minutes * 60_000,
        actions: vec![
            RyeOsOverlayActionVm {
                keys: "enter".to_owned(),
                word: "open".to_owned(),
                intent: RyeOsUiIntent::ToggleTopStatusBar,
            },
            RyeOsOverlayActionVm {
                keys: "a".to_owned(),
                word: "acknowledge".to_owned(),
                intent: RyeOsUiIntent::ToggleTopStatusBar,
            },
        ],
    };
    let overlay = &mut vm.overlay;
    overlay.open = true;
    overlay.needs_you = vec![
        row("ask-1", "Approve the deploy", "ryeos", RyeOsTone::Warn, 4),
        row(
            "failed-1",
            "Nightly build failed",
            "this node",
            RyeOsTone::Danger,
            130,
        ),
    ];
    overlay.needs_you_count = Some(5);
    overlay.running = vec![row(
        "run-1",
        "Review the branch",
        "rye",
        RyeOsTone::Accent,
        12,
    )];
    overlay.running_count = Some(1);
    overlay.selected = Some(0);
}
