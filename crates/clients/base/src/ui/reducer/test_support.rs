//! Shared test fixtures for the reducer cluster test modules.

pub(crate) use crate::ui::dto::RyeOsThreadsDto;
pub(crate) use crate::ui::effect::{RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
pub(crate) use crate::ui::event::{RyeOsEvent, RyeOsUiEvent, RyeOsUiIntent};
pub(crate) use crate::ui::model::{BrowserSession, BrowserViewport, RyeOsCore};
pub(crate) use crate::ui::view_model::{available_command_items, build_view_model};
pub(crate) use crate::view_set::{FocusDirection, ViewSpec};

pub(crate) const FIXTURE_PRESENTATION_ID: &str = "11111111-1111-4111-8111-111111111111";

pub(crate) fn claim_fixture_producer(
    core: &mut RyeOsCore,
) -> crate::ui::binding::UiPresentationProducer {
    let producer = crate::ui::binding::UiPresentationProducer {
        presentation_id: FIXTURE_PRESENTATION_ID.into(),
        thread_id: "fixture-seat".into(),
        producer_incarnation: "ab".repeat(32),
    };
    assert!(
        core.dispatch(RyeOsEvent::PresentationProducerReady {
            producer: producer.clone(),
            next_engine_seq: core
                .presentation_operations
                .acknowledged_engine_frontier
                .unwrap_or(0),
        })
        .is_empty()
    );
    assert_eq!(
        core.presentation_operations.producer.as_ref(),
        Some(&producer)
    );
    producer
}

/// Test transport: acknowledge only the captured, exact outstanding local tail.
/// Driving a prepared edit may append its completion, requiring another ACK.
pub(crate) fn pending_prefix(core: &RyeOsCore) -> Vec<crate::ui::seat::SeatEvent> {
    core.seat.events()[core.presentation_operations.acknowledged_prefix_len..].to_vec()
}

pub(crate) fn settle_pending_prefix(
    core: &mut RyeOsCore,
    producer: &crate::ui::binding::UiPresentationProducer,
) -> Vec<crate::ui::effect::RyeOsEffect> {
    let mut effects = Vec::new();
    for _ in 0..8 {
        let prefix = pending_prefix(core);
        if prefix.is_empty() {
            return effects;
        }
        let before = core.presentation_operations.acknowledged_prefix_len;
        effects.extend(core.acknowledge_seat_prefix(producer, prefix.clone()));
        assert_eq!(
            core.presentation_operations.acknowledged_prefix_len,
            before + prefix.len(),
            "fixture ACK must consume exactly its captured prefix"
        );
    }
    panic!("fixture ACK drive did not settle within its bounded transport steps")
}

fn coordinate(
    attachment: &crate::ui::binding::UiBindingAttachment,
) -> crate::ui::binding::UiBindingAttachmentCoordinate {
    crate::ui::binding::UiBindingAttachmentCoordinate {
        binding_attachment_id: attachment.binding_attachment_id.clone(),
        binding_generation: attachment.binding_generation,
        binding_digest: attachment.binding_digest.clone(),
    }
}

pub(crate) fn core(session: BrowserSession) -> RyeOsCore {
    core_for_presentation(
        session,
        FIXTURE_PRESENTATION_ID,
        BrowserViewport::default(),
        0,
    )
}
pub(crate) fn core_for_presentation(
    mut session: BrowserSession,
    presentation_id: &str,
    viewport: BrowserViewport,
    now_ms: u64,
) -> RyeOsCore {
    let membership = session
        .presentations
        .iter_mut()
        .find(|member| member.presentation_id == presentation_id)
        .expect("fixture presentation exists");
    let mut required =
        std::collections::BTreeSet::from([membership.root_binding.binding_attachment_id.clone()]);
    for surface in &membership.surfaces {
        required.insert(surface.binding.binding_attachment_id.clone());
        if let Some(saved) = &surface.saved_surface {
            for binding in saved.tab_bindings() {
                required.insert(binding.binding_attachment_id.clone());
            }
        }
    }
    let mut attachments = session
        .binding_attachments
        .iter()
        .filter(|entry| required.contains(&entry.binding_attachment_id))
        .cloned()
        .collect::<Vec<_>>();
    attachments.sort_by(|a, b| a.binding_attachment_id.cmp(&b.binding_attachment_id));
    let initialization = crate::ui::presentation_initialization::UiPresentationInitialization {
        schema: 2,
        selection: crate::ui::presentation_initialization::UiPresentationInitialSelection::ConfiguredDefaults,
        presentation_id: presentation_id.into(),
        creation_request_id: "22222222-2222-4222-8222-222222222222".into(),
        root_binding: membership.root_binding.clone(),
        next_mount_id: membership.next_mount_id,
        surfaces: membership.surfaces.clone(),
        binding_attachments: attachments,
    };
    initialization
        .validate()
        .expect("authored fixture initialization is exact");
    membership.initialization_digest = initialization.digest().unwrap();
    let expected_surfaces = membership.surfaces.len();
    let mut core = RyeOsCore::new(session, presentation_id, viewport, now_ms);
    core.replay_seat_events([crate::ui::seat::SeatEvent {
        producer_engine_seq: None,
        seq: 0,
        kind: crate::ui::seat::SeatEventKind::PresentationInitialized(
            crate::ui::presentation_initialization::UiPresentationInitializationRecord {
                operation_id: initialization.creation_request_id.clone(),
                initialization,
            },
        ),
    }]);
    core.require_replayed_initialization()
        .expect("fixture initialization replay succeeded");
    assert_eq!(
        core.presentation.as_ref().unwrap().surfaces().len(),
        expected_surfaces
    );
    core
}

pub(crate) fn session() -> BrowserSession {
    let effective_surface = serde_json::json!({
        "name": "ryeos-base",
        "tiles": ["view:test/home"],
        "views": {
            "view:test/home": {"title":"Fixture home","widget":"text","body":"Fixture home"},
            "view:ryeos/input": {
                "widget": "text",
                "input": { "id": "line", "placeholder": "Ask or run a command", "submit": "route" }
            }
        }
    });
    let mut session = BrowserSession {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
        session_id: "session-1".to_string(),
        user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
        // The actual Shared Config is installed below. Surface content remains
        // at index zero for callers; its slots do not own the shared docks.
        root_attachment_id: "fixture-shared-attachment".into(),
        presentations: vec![],
        binding_attachments: vec![crate::ui::binding::UiBindingAttachment {
            binding_attachment_id: "fixture-attachment".into(),
            binding_generation: 1,
            binding_digest: "11".repeat(32),
            root_ref: "surface:ryeos/ryeos/base".into(),
            root_generation: "22".repeat(32),
            presentation: crate::presentation::UiCompositionPresentation::from_effective_surface(
                effective_surface,
            )
            .unwrap(),
            project_path: Some("/tmp/project".into()),
            subject: crate::ui::binding::UiBindingSubject::UnregisteredProject,
            posture: crate::ui::binding::UiEffectivePosture::ObservationOnly,
            binding_request_bounds: crate::ui::binding::UiBindingRequestBounds {
                max_request_bytes: 64 * 1024,
                max_input_bytes: 16 * 1024,
                max_seat_append_bytes: 64 * 1024,
            },
        }],
        events_url: Some("/ui/events/session/session-1".to_string()),
    };
    install_fixture_membership(&mut session);
    session
}

fn install_fixture_membership(session: &mut BrowserSession) {
    let shared = crate::ui::binding::UiBindingAttachment {
        binding_attachment_id: "fixture-shared-attachment".into(),
        binding_generation: 2,
        binding_digest: "33".repeat(32),
        root_ref: "config:ryeos/test/presentation".into(),
        root_generation: "44".repeat(32),
        presentation: serde_json::from_value(serde_json::json!({
            "definition": {"kind":"shared_presentation", "config": {
                "category":"ui", "shared_presentation": {
                    "schema":1,"title":"Fixture presentation","launcher":"launcher",
                    "dialogs":[{"id":"launcher","title":"Launcher","view":"view:ryeos/launcher"}],
                    "docks": {
                        "bottom":{"content":"view:ryeos/input","open":true,"size":7},
                        "left":{"content":"view:ryeos/threads/list","open":false,"size":32},
                        "right":{"content":"view:ryeos/item/inspector","open":false,"size":40}
                    },
                    "capabilities":{"sources":[],"affordances":[]}
                }
            }},
            "views": {
                "view:ryeos/launcher":{"widget":"rows"},
                "view:ryeos/input":{"widget":"text","input":{"id":"line","placeholder":"Ask or run a command","submit":"route"}},
                "view:ryeos/threads/list":{"widget":"rows"},
                "view:ryeos/item/inspector":{"widget":"rows"}
            }
        })).expect("actual Shared Config fixture decodes"),
        project_path: Some("/tmp/project".into()),
        subject: crate::ui::binding::UiBindingSubject::UnregisteredProject,
        posture: crate::ui::binding::UiEffectivePosture::ObservationOnly,
        binding_request_bounds: crate::ui::binding::UiBindingRequestBounds { max_request_bytes:64*1024,max_input_bytes:16*1024,
max_seat_append_bytes:64*1024,
},
    };
    shared.validate().expect("Shared Config descriptor shape");
    let root_binding = coordinate(&shared);
    let mut next_mount_id = 1;
    let surfaces = session
        .binding_attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            attachment.validate().expect("Surface descriptor shape");
            let surface_instance_id = format!("00000000-0000-4000-8000-{:012}", index + 1);
            let seeds = attachment
                .presentation
                .definition
                .surface()
                .unwrap()
                .to_mount_seeds()
                .unwrap();
            let active = seeds.first().map(|seed| seed.id.clone());
            let mount_manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
                crate::ids::PresentationId::try_new(FIXTURE_PRESENTATION_ID).unwrap(),
                crate::ids::SurfaceInstanceId::try_new(surface_instance_id.clone()).unwrap(),
                next_mount_id,
                seeds
                    .into_iter()
                    .map(|seed| (seed, coordinate(attachment)))
                    .collect(),
                active.as_deref(),
            )
            .unwrap();
            next_mount_id = mount_manifest.next_mount_id;
            crate::ui::binding::UiSurfaceMembership {
                mount_manifest,
                saved_surface: None,
                surface_instance_id,
                ordinal: (index + 1) as u64,
                title: attachment.presentation.definition.title().to_string(),
                binding: coordinate(attachment),
            }
        })
        .collect();
    session.root_attachment_id = shared.binding_attachment_id.clone();
    session.binding_attachments.push(shared);
    session.presentations = vec![crate::ui::binding::UiPresentationMembership {
        presentation_id: FIXTURE_PRESENTATION_ID.into(),
        root_binding,
        thread_id: Some("fixture-seat".into()),
        chain_root_id: Some("fixture-chain".into()),
        next_mount_id,
        initialization_digest: "a".repeat(64),
        surfaces,
    }];
}

pub(crate) fn writable_session() -> BrowserSession {
    let mut session = session();
    session.binding_attachments[0].posture = crate::ui::binding::UiEffectivePosture::Interactive;
    session.binding_attachments.last_mut().unwrap().posture =
        crate::ui::binding::UiEffectivePosture::Interactive;
    session
}

/// An admitted Shared Config with its ordinary shared mounts and no Surface.
pub(crate) fn shared_session() -> BrowserSession {
    let mut session = session();
    session.presentations[0].surfaces.clear();
    session.presentations[0].next_mount_id = 1;
    let root = session.root_attachment_id.clone();
    session
        .binding_attachments
        .retain(|attachment| attachment.binding_attachment_id == root);
    session
}

pub(crate) fn session_with_surface(effective_surface: serde_json::Value) -> BrowserSession {
    let mut session = writable_session();
    session.binding_attachments[0].presentation =
        crate::presentation::UiCompositionPresentation::from_effective_surface(effective_surface)
            .unwrap();
    session.presentations[0].surfaces[0].title = session.binding_attachments[0]
        .presentation
        .definition
        .title()
        .into();
    let attachment = &session.binding_attachments[0];
    let member = &mut session.presentations[0];
    let seeds = attachment
        .presentation
        .definition
        .surface()
        .unwrap()
        .to_mount_seeds()
        .unwrap();
    let active = seeds.first().map(|seed| seed.id.clone());
    member.surfaces[0].mount_manifest =
        crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
            crate::ids::PresentationId::try_new(member.presentation_id.clone()).unwrap(),
            crate::ids::SurfaceInstanceId::try_new(member.surfaces[0].surface_instance_id.clone())
                .unwrap(),
            1,
            seeds
                .into_iter()
                .map(|seed| (seed, coordinate(attachment)))
                .collect(),
            active.as_deref(),
        )
        .unwrap();
    member.next_mount_id = member.surfaces[0].mount_manifest.next_mount_id;
    session
}

/// Explicit fixture inputs for distinct authored Surface and Shared Config.
/// Docks are never read from or admitted through the Surface declaration.
pub(crate) fn session_with_surface_and_docks(
    effective_surface: serde_json::Value,
    docks: serde_json::Value,
) -> BrowserSession {
    let mut session = session_with_surface(effective_surface);
    let surface_views = session.binding_attachments[0].presentation.views.clone();
    let root = session.binding_attachments.last_mut().unwrap();
    let crate::presentation::UiCompositionDefinition::SharedPresentation { config } =
        &mut root.presentation.definition
    else {
        panic!("fixture root must be actual Shared Config");
    };
    config.shared_presentation.docks =
        serde_json::from_value(docks).expect("explicit authored Config docks");
    root.presentation.views.extend(surface_views);
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
        root_ref: format!("surface:test/{id}"),
        root_generation: format!("surface-generation-{generation}"),
        presentation: crate::presentation::UiCompositionPresentation::from_effective_surface(
            effective_surface,
        )
        .unwrap(),
        project_path: project_path.map(str::to_string),
        subject: if project_path.is_some() {
            crate::ui::binding::UiBindingSubject::UnregisteredProject
        } else {
            crate::ui::binding::UiBindingSubject::Projectless
        },
        posture: crate::ui::binding::UiEffectivePosture::Interactive,
        binding_request_bounds: crate::ui::binding::UiBindingRequestBounds {
            max_request_bytes: 64 * 1024,
            max_input_bytes: 16 * 1024,
            max_seat_append_bytes: 64 * 1024,
        },
    }
}

pub(crate) fn session_with_attachments(
    root_attachment_id: &str,
    attachments: Vec<crate::ui::binding::UiBindingAttachment>,
) -> BrowserSession {
    let mut session = BrowserSession {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
        session_id: "session-1".to_string(),
        user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
        binding_attachments: attachments,
        root_attachment_id: root_attachment_id.to_string(),
        presentations: vec![],
        events_url: Some("/ui/events/session/session-1".to_string()),
    };
    install_fixture_membership(&mut session);
    // Every supplied Surface descriptor has an actual initialized owner;
    // an unused descriptor in session JSON alone is not admitted composition.
    session
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
        .insertion_attachment_id(
            core.active_view_set()
                .expect("fixture Surface has a view set")
                .id,
        )
        .expect("fixture view set has an insertion attachment")
        .to_string();
    core.binding_attachments
        .get_mut(&attachment_id)
        .expect("fixture attachment is retained")
        .views
        .insert(view_ref.to_string(), serde_json::from_value(value).unwrap());
    if matches!(
        view_ref,
        "view:ryeos/input" | "view:ryeos/threads/list" | "view:ryeos/item/inspector"
    ) {
        let root_id = core.data.session.as_ref().unwrap().presentations[0]
            .root_binding
            .binding_attachment_id
            .clone();
        let value = core.binding_attachments[&attachment_id].views[view_ref].clone();
        core.binding_attachments
            .get_mut(&root_id)
            .unwrap()
            .views
            .insert(view_ref.into(), value);
    }
}

/// Seed the `view:ryeos/input` chat box (`submit: route`) so the
/// shared bottom dock instance owns input; it never targets another View.
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
    let key = crate::ui::seat::selection_facet_key(
        core.active_view_set()
            .expect("fixture Surface has a view set")
            .id,
    );
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
            core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom)
                .expect("fixture shared input dock"),
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
    serde_json::to_value(core.route_for_instance(&instance)).expect("InputRoute serializes")
}

/// Focus a center tile the way `FocusChanged` would: both the view_set
/// pointer and the explicit UI target move. Tests that poke
/// `view_set.focused_tile` alone leave the initial dock focus standing.
pub(crate) fn focus_tile(core: &mut RyeOsCore, tile_id: crate::ids::TileId) {
    core.focused_input = None;
    let set = core
        .active_view_set_mut()
        .expect("fixture Surface has a view set");
    set.focused_tile = tile_id;
    set.focus_target = Some(crate::ui::model::RyeOsFocusTarget::ViewSetTile {
        tile_id: tile_id.0.to_string(),
    });
}

#[test]
fn fixture_shared_root_and_surface_have_distinct_actual_kinds() {
    let session = session();
    let membership = &session.presentations[0];
    let surface = &session.binding_attachments[0];
    let root = session
        .binding_attachments
        .iter()
        .find(|attachment| attachment.binding_attachment_id == session.root_attachment_id)
        .unwrap();
    assert_eq!(root.presentation.definition.expected_kind(), "config");
    assert_eq!(surface.presentation.definition.expected_kind(), "surface");
    assert_eq!(membership.root_binding, coordinate(root));
    assert_eq!(membership.surfaces[0].binding, coordinate(surface));
    assert!(matches!(
        surface.presentation.definition,
        crate::presentation::UiCompositionDefinition::Surface { .. }
    ));
    let shared = core(shared_session());
    assert!(shared.active_surface().is_none());
    assert!(
        shared
            .shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom)
            .is_some()
    );
}
