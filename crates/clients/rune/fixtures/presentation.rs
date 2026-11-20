//! An in-memory RyeOS presentation for drawing and interaction checks without
//! a node or a seat. A scene is a checked-in data file (`fixtures/scenes/`):
//! the session and binding facts, the Surface with the views it mounts, the
//! clock, the viewport, the UI steps that leave the shell as a person would,
//! and for each source the response its request is answered with, through the
//! same fetch effects a node answers. Shared by the `fixture` example and the
//! headless tests.
//!
//! A scene file is JSON:
//!
//! - `format`: the file format version (1).
//! - `scene`: the scene's name, the file name without `.json`.
//! - `provenance`: `synthetic` for an authored scene, `recorded` for one
//!   captured from a live session.
//! - `clock_ms`: the core's clock, so relative times read the same every run.
//! - `session`: `session_id`, `project_path`, and the seat's `thread_id` and
//!   `chain_root_id`.
//! - `viewport`: the initial viewport.
//! - `surface`: `root_ref`, the Surface's `title`, which view set is
//!   `current` (`first` or `last`), and its effective `definition`.
//! - `shared`: the shared presentation's binding attachment, as the node
//!   sends it.
//! - `steps`: the UI steps applied once the first fetches are answered.
//! - `sources`: one entry per source request, naming the `view` and
//!   `channel`, with the `service` and the `params` the request carried and
//!   either the `response` body or an `error`; an entry with neither is a
//!   request nobody answers, which stays loading.
//! - `dress`: optional, the name of the view-model change this scene still
//!   needs (see `dress`).
//!
//! Wherever a view or Surface definition appears in `surface.definition` or
//! `shared`, it is either written out or `{ "from_bundle": "<path under
//! .ai>", "patch": [...] }`, which loads that bundle file and applies the
//! patches, so a scene tracks the shipped definitions. A patch sets
//! `value` at the JSON pointer `path`, or copies the value at `path` of
//! another bundle file with `copy_from`.

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
use ryeos_client_base::ui::{
    RyeOsEvent, RyeOsUiEvent, UiBindingAttachment, UiBindingRequestBounds, UiBindingSubject,
};
use serde::Deserialize;
use serde_json::{Value, json};

const PRESENTATION: &str = "11111111-1111-4111-8111-111111111111";
const SURFACE: &str = "22222222-2222-4222-8222-222222222222";

/// The scene file format this loader reads.
const SCENE_FORMAT: u32 = 1;

/// Every scene file, by name (the default scene is `default`). The tests walk
/// it; the fixture example loads one scene by name.
#[allow(dead_code)]
pub const SCENES: &[&str] = &[
    "default",
    "field",
    "launcher",
    "launcher-stage",
    "long",
    "timeline",
    "timeline-draft",
    "timeline-top",
    "timeline-asks",
    "states",
    "project",
    "project-review",
    "project-launcher",
    "tabs",
    "chips",
    "switcher-right",
    "switcher-top",
    "bare",
    "overlay",
];

/// The text of the scene file for `name`.
fn scene_text(name: &str) -> Option<&'static str> {
    Some(match name {
        "default" => include_str!("scenes/default.json"),
        "field" => include_str!("scenes/field.json"),
        "launcher" => include_str!("scenes/launcher.json"),
        "launcher-stage" => include_str!("scenes/launcher-stage.json"),
        "long" => include_str!("scenes/long.json"),
        "timeline" => include_str!("scenes/timeline.json"),
        "timeline-draft" => include_str!("scenes/timeline-draft.json"),
        "timeline-top" => include_str!("scenes/timeline-top.json"),
        "timeline-asks" => include_str!("scenes/timeline-asks.json"),
        "states" => include_str!("scenes/states.json"),
        "project" => include_str!("scenes/project.json"),
        "project-review" => include_str!("scenes/project-review.json"),
        "project-launcher" => include_str!("scenes/project-launcher.json"),
        "tabs" => include_str!("scenes/tabs.json"),
        "chips" => include_str!("scenes/chips.json"),
        "switcher-right" => include_str!("scenes/switcher-right.json"),
        "switcher-top" => include_str!("scenes/switcher-top.json"),
        "bare" => include_str!("scenes/bare.json"),
        "overlay" => include_str!("scenes/overlay.json"),
        _ => return None,
    })
}

/// A bundle file a scene may load, by its path under `.ai`, or the field
/// fixture a scene's response was authored from.
fn bundle_text(path: &str) -> Option<&'static str> {
    macro_rules! ai {
        ($rel:literal) => {
            include_str!(concat!("../../../../bundles/ryeos-ui/.ai/", $rel))
        };
    }
    Some(match path {
        "views/ryeos/threads/history.yaml" => ai!("views/ryeos/threads/history.yaml"),
        "views/ryeos/threads/detail.yaml" => ai!("views/ryeos/threads/detail.yaml"),
        "views/ryeos/ui/launcher.yaml" => ai!("views/ryeos/ui/launcher.yaml"),
        "views/ryeos/development/execution.yaml" => ai!("views/ryeos/development/execution.yaml"),
        "views/ryeos/chain/timeline.yaml" => ai!("views/ryeos/chain/timeline.yaml"),
        "views/ryeos/thread/transcript.yaml" => ai!("views/ryeos/thread/transcript.yaml"),
        "views/ryeos/thread/conversation.yaml" => ai!("views/ryeos/thread/conversation.yaml"),
        "views/ryeos/schedules/list.yaml" => ai!("views/ryeos/schedules/list.yaml"),
        "views/ryeos/surfaces/list.yaml" => ai!("views/ryeos/surfaces/list.yaml"),
        "surfaces/ryeos/ui/project.yaml" => ai!("surfaces/ryeos/ui/project.yaml"),
        _ => return None,
    })
}

const COVERAGE_WORDING: &str =
    include_str!("../../../../bundles/ryeos-ui/.ai/config/ryeos/ui/coverage-wording.yaml");

/// A scene file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneFile {
    format: u32,
    scene: String,
    provenance: Provenance,
    clock_ms: u64,
    session: SceneSession,
    viewport: BrowserViewport,
    surface: SceneSurface,
    shared: Value,
    steps: Vec<Step>,
    sources: Vec<SceneSource>,
    #[serde(default)]
    dress: Option<String>,
}

/// Where a scene's data came from.
#[derive(Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Provenance {
    /// Authored by hand, not captured from a node.
    Synthetic,
    /// Captured from a live session.
    Recorded,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneSession {
    session_id: String,
    project_path: String,
    thread_id: String,
    chain_root_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneSurface {
    root_ref: String,
    title: String,
    current: Current,
    definition: Value,
}

/// Which of the Surface's view sets is current when the scene opens.
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Current {
    First,
    Last,
}

/// A change to the shell made once the first fetches are answered, as a
/// person would make it.
#[derive(Deserialize)]
#[serde(tag = "do", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    ToggleTopStatusBar,
    ToggleTimelineFold,
    SetComposerText {
        text: String,
    },
    OpenLauncher,
    /// Choose the first launcher row that is a view, staging it.
    StageFirstView,
    /// Type into the find line.
    TypeFind {
        text: String,
    },
}

/// One source request and what answers it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneSource {
    view: String,
    channel: String,
    /// The service the request calls, as the view declares it.
    #[serde(default)]
    #[allow(dead_code)]
    service: Option<String>,
    /// The parameters the request carried.
    #[allow(dead_code)]
    params: Value,
    #[serde(default)]
    response: Option<Value>,
    #[serde(default)]
    error: Option<SceneError>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneError {
    code: String,
    message: String,
}

fn parse_scene(scene: &str) -> Result<SceneFile, String> {
    let name = if scene.is_empty() { "default" } else { scene };
    let text = scene_text(name).ok_or_else(|| format!("unknown fixture scene {name}"))?;
    let file: SceneFile =
        serde_json::from_str(text).map_err(|error| format!("scene {name}: {error}"))?;
    if file.format != SCENE_FORMAT {
        return Err(format!("scene {name}: unsupported format {}", file.format));
    }
    if file.scene != name {
        return Err(format!("scene file {name} names itself {}", file.scene));
    }
    Ok(file)
}

fn bundle_value(path: &str) -> Result<Value, String> {
    let text = bundle_text(path).ok_or_else(|| format!("no bundle file {path} in the fixture"))?;
    serde_yaml::from_str(text).map_err(|error| format!("bundle file {path}: {error}"))
}

/// `value` with every `{ "from_bundle": ... }` object replaced by the bundle
/// file it names, patched.
fn resolve(value: &Value) -> Result<Value, String> {
    match value {
        Value::Object(map) if map.contains_key("from_bundle") => {
            let path = map["from_bundle"]
                .as_str()
                .ok_or("from_bundle is not a path")?;
            let mut loaded = bundle_value(path)?;
            for patch in map
                .get("patch")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                apply_patch(&mut loaded, patch)?;
            }
            Ok(loaded)
        }
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| Ok((key.clone(), resolve(value)?)))
            .collect::<Result<_, String>>()
            .map(Value::Object),
        Value::Array(items) => items
            .iter()
            .map(resolve)
            .collect::<Result<_, String>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// Set the value a patch names at its pointer.
fn apply_patch(target: &mut Value, patch: &Value) -> Result<(), String> {
    let path = patch["path"].as_str().ok_or("a patch has no path")?;
    let value = if let Some(source) = patch.get("copy_from") {
        let file = source["from_bundle"]
            .as_str()
            .ok_or("copy_from names no bundle file")?;
        let from = source["path"].as_str().ok_or("copy_from names no path")?;
        bundle_value(file)?
            .pointer(from)
            .cloned()
            .ok_or_else(|| format!("{file} has nothing at {from}"))?
    } else {
        resolve(patch.get("value").ok_or("a patch has no value")?)?
    };
    let (parent, key) = path
        .rsplit_once('/')
        .ok_or_else(|| format!("patch path {path} is not a pointer"))?;
    match target.pointer_mut(parent) {
        Some(Value::Object(map)) => {
            map.insert(key.to_owned(), value);
            Ok(())
        }
        _ => Err(format!("patch path {path} has no object to set it in")),
    }
}

/// The signed coverage wording, read as the daemon serves it: the config body
/// without its envelope fields, under the wording contract.
fn coverage_wording() -> Result<ryeos_client_base::ui::dto::RyeOsCoverageWordingDto, String> {
    let mut body: Value = serde_yaml::from_str(COVERAGE_WORDING)
        .map_err(|error| format!("coverage wording: {error}"))?;
    let object = body
        .as_object_mut()
        .ok_or("coverage wording is not a mapping")?;
    for envelope in ["category", "version", "description"] {
        object.remove(envelope);
    }
    object.insert(
        "schema_version".into(),
        json!(ryeos_client_base::ui::dto::COVERAGE_WORDING_CONTRACT),
    );
    ryeos_client_base::ui::dto::RyeOsCoverageWordingDto::from_response(&body)
}

/// Answer the source fetches among `effects` from the scene's sources;
/// returns how many were answered.
fn answer(core: &mut RyeOsCore, effects: Vec<RyeOsEffect>, sources: &[SceneSource]) -> usize {
    let mut answered = 0;
    for effect in effects {
        let RyeOsEffectKind::FetchSource { request, .. } = &effect.kind else {
            continue;
        };
        let UiBindingCoordinate::Source { view_ref, channel } = &request.coordinate else {
            continue;
        };
        let Some(source) = sources
            .iter()
            .find(|source| &source.view == view_ref && &source.channel == channel)
        else {
            continue;
        };
        let result = match (&source.error, &source.response) {
            (Some(error), _) => RyeOsEffectResult {
                id: effect.id,
                ok: false,
                kind: RyeOsEffectResultKind::SourceData,
                data: None,
                error: Some(RyeOsUiError::definite(&error.code, &error.message)),
            },
            (None, Some(data)) => RyeOsEffectResult {
                id: effect.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(data.clone()),
                error: None,
            },
            // A source nobody answers stays loading.
            (None, None) => continue,
        };
        answered += 1;
        let more = core.dispatch(RyeOsEvent::EffectResult { result });
        answered += answer(core, more, sources);
    }
    answered
}

/// Whether a launcher row lists a view, not a command or a place.
fn is_a_view(row: &ryeos_client_base::ui::view_model::RyeOsRowVm) -> bool {
    row.launch
        .as_ref()
        .is_some_and(|launch| !launch.place && launch.group != "Commands")
}

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

/// The presentation for `scene`, read from its scene file (`""` is the
/// default scene).
pub fn core(scene: &str) -> Result<RyeOsCore, String> {
    let file = parse_scene(scene)?;
    let surface_json = resolve(&file.surface.definition)?;
    let presentation = UiCompositionPresentation::from_effective_surface(surface_json)
        .map_err(|error| format!("surface fixture: {error}"))?;
    let surface_attachment = UiBindingAttachment {
        binding_attachment_id: "surface-attachment".into(),
        binding_generation: 1,
        binding_digest: "surface-binding".into(),
        root_ref: file.surface.root_ref.clone(),
        root_generation: "surface-generation".into(),
        project_path: Some(file.session.project_path.clone()),
        subject: UiBindingSubject::UnregisteredProject,
        posture: Default::default(),
        binding_request_bounds: UiBindingRequestBounds {
            max_request_bytes: 4096,
            max_input_bytes: 1024,
            max_seat_append_bytes: 64 * 1024,
        },
        presentation,
    };
    let shared = serde_json::from_value(resolve(&file.shared)?)
        .map_err(|error| format!("shared fixture: {error}"))?;
    let mut session = BrowserSession {
        session_id: file.session.session_id.clone(),
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
    // The scene opens on its first view set, or on the last, so the current
    // tab is the one furthest along the band.
    let first = match file.surface.current {
        Current::First => seeds.first(),
        Current::Last => seeds.last(),
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
        thread_id: Some(file.session.thread_id.clone()),
        chain_root_id: Some(file.session.chain_root_id.clone()),
        surfaces: vec![UiSurfaceMembership {
            saved_surface: None,
            surface_instance_id: SURFACE.into(),
            ordinal: 1,
            title: file.surface.title.clone(),
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
    let mut core = RyeOsCore::new(session, PRESENTATION, file.viewport, file.clock_ms);
    core.data.coverage_wording = Some(coverage_wording()?);
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
    let mut answered = answer(&mut core, effects, &file.sources);
    let effects = core.dispatch(RyeOsEvent::Ui {
        event: RyeOsUiEvent::FocusInput,
    });
    answered += answer(&mut core, effects, &file.sources);
    for step in &file.steps {
        answered += apply_step(&mut core, step, &file.sources)?;
    }
    let provenance = match file.provenance {
        Provenance::Synthetic => "synthetic",
        Provenance::Recorded => "recorded",
    };
    eprintln!(
        "fixture: scene {} ({provenance}), answered {answered} source fetches",
        file.scene
    );
    Ok(core)
}

/// One scene step; returns how many source fetches it caused to be answered.
fn apply_step(core: &mut RyeOsCore, step: &Step, sources: &[SceneSource]) -> Result<usize, String> {
    use ryeos_client_base::ui::event::RyeOsUiIntent;
    let mut answered = 0;
    match step {
        Step::ToggleTopStatusBar => {
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::Activate {
                    intent: RyeOsUiIntent::ToggleTopStatusBar,
                },
            });
        }
        Step::ToggleTimelineFold => {
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::ToggleTimelineFold,
            });
        }
        Step::SetComposerText { text } => {
            let vm = ryeos_client_base::ui::view_model::build_view_model(core);
            let input = serde_json::to_value(&vm)
                .ok()
                .and_then(|vm| find_input(&vm))
                .ok_or("the scene has no composer")?;
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::InputAt {
                    address: input.address,
                    action: RyeOsInputAction::SetText {
                        text: text.clone(),
                        cursor: text.chars().count(),
                    },
                },
            });
        }
        Step::OpenLauncher => {
            let effects = core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::OpenLauncher,
            });
            answered += answer(core, effects, sources);
        }
        Step::StageFirstView => {
            // The first view that can be chosen is chosen, which stages it
            // and draws where it lands.
            use ryeos_client_base::ui::view_model::{RyeOsViewVm, build_view_model};
            let vm = build_view_model(core);
            let dialog = vm.dialogs.first().ok_or("the launcher did not open")?;
            let RyeOsViewVm::Rows { rows, .. } = &dialog.view else {
                return Err("the launcher is not a list".into());
            };
            let staging = rows
                .iter()
                .find(|row| row.intent.is_some() && is_a_view(row))
                .ok_or("the launcher lists no view that can be staged")?;
            let effects = core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::ChooseViewItem {
                    instance_key: dialog.instance_key.clone(),
                    item_id: staging.id.clone(),
                    activate: true,
                },
            });
            answered += answer(core, effects, sources);
        }
        Step::TypeFind { text } => {
            for ch in text.chars() {
                core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::InsertInputChar { ch },
                });
            }
        }
    }
    Ok(answered)
}

/// The view-model change `scene` still needs, named by its scene file: counts,
/// marks and flags whose producers are a node's, not the core's.
pub fn dress(scene: &str) -> ryeos_client_rune::Dress {
    match parse_scene(scene).ok().and_then(|file| file.dress) {
        Some(name) => dress_named(&name),
        None => |_| {},
    }
}

fn dress_named(name: &str) -> ryeos_client_rune::Dress {
    use ryeos_client_base::ui::view_model::RyeOsEdgeVm as Edge;
    match name {
        "chips" => |vm| {
            dress_chips(vm);
            dress_status(vm);
        },
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
        other => panic!("scene names an unknown dress {other}"),
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

/// The status island's segments: what is running, what needs you, the
/// operator and the time.
fn dress_status(vm: &mut ryeos_client_base::ui::view_model::RyeOsViewModel) {
    use ryeos_client_base::ui::view_model::{RyeOsStatusSegmentVm, RyeOsTone};
    let segment = |id: &str, value: &str, tone| RyeOsStatusSegmentVm {
        id: id.to_owned(),
        label: None,
        value: value.to_owned(),
        tone,
        grow: false,
    };
    vm.presentation.chrome.status_bar.segments = vec![
        segment("running", "3 running", RyeOsTone::Accent),
        segment("needs-you", "3 need you", RyeOsTone::Warn),
        segment("operator", "leo", RyeOsTone::Neutral),
        segment("time", "14:32", RyeOsTone::Neutral),
    ];
}

/// The tabs scene with the last tab (the current one) needing you and one
/// running.
fn dress_tabs(vm: &mut ryeos_client_base::ui::view_model::RyeOsViewModel) {
    let last = vm.presentation.chrome.top_bar.tabs.len().saturating_sub(1);
    for (index, tab) in vm.presentation.chrome.top_bar.tabs.iter_mut().enumerate() {
        tab.needs_you = Some(index == last);
        tab.running = Some(index == 4);
    }
}

/// The overlay open over the chips scene: three rows needing you (the first
/// selected, with its actions) and three running.
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
        row("ask-2", "Allow network access", "rye", RyeOsTone::Warn, 25),
    ];
    overlay.needs_you_count = Some(3);
    overlay.running = vec![
        row("run-1", "Review the branch", "rye", RyeOsTone::Accent, 12),
        row("run-2", "Index the project", "ryeos", RyeOsTone::Accent, 47),
        row("run-3", "Build the bundle", "rye", RyeOsTone::Accent, 61),
    ];
    overlay.running_count = Some(3);
    overlay.selected = Some(0);
}
