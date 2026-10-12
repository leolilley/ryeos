//! Live import publication follows the same retained two-ACK owner as opening.
use super::test_support::*;
use crate::ui::binding::*;
use crate::ui::effect::{RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
use crate::ui::model::{InputBufferKey, RyeOsInputState};
use crate::ui::seat::SeatEventKind;
use serde_json::json;

#[test]
fn imported_tabs_wait_for_both_acks_and_keep_existing_and_shared_drafts() {
    let mut session = session_with_surface(
        json!({"name":"Existing","tiles":["view:test/existing"],"views":{
            "view:test/existing":{"widget":"text","input":{"id":"draft","submit":"route"}}
        }}),
    );
    for descriptor in &mut session.binding_attachments {
        descriptor.subject = UiBindingSubject::Projectless;
        descriptor.project_path = None;
        if descriptor.binding_attachment_id == session.root_attachment_id {
            descriptor.presentation.views.insert("view:ryeos/launcher".into(),json!({"widget":"rows","input":{"id":"query"},"affordances":[{
                "id":"import-tabs","presentation_operation":"surface_opening","invoke":{"plane":"rye","ref":"service:ui/session/surfaces/open","args":{
                    "selection":{"kind":"authored","surface_ref":"surface:test/imported"},"placement":"{record.opening_placement}","creation_request_id":"{record.creation_request_id}"
                }}
            }]}));
        }
    }
    let mut core = core(session);
    let producer = claim_fixture_producer(&mut core);
    settle_pending_prefix(&mut core, &producer);
    let original = core.active_view_set().unwrap();
    let old_tab = original.id;
    let old_mount = original.tiles.values().next().unwrap().instance_key.clone();
    let old_binding = core.active_surface().unwrap().binding().clone();
    let old_key = InputBufferKey::new(
        old_mount.clone(),
        "view:test/existing",
        "draft",
        core.binding_attachment_for_instance(&old_mount).unwrap(),
    )
    .storage_key();
    core.input_buffers.insert(
        old_key.clone(),
        RyeOsInputState {
            text: "existing unsent draft".into(),
            ..Default::default()
        },
    );
    core.open_dialog("launcher");
    let shared = core.presentation.as_ref().unwrap().dialogs()["launcher"]
        .view()
        .instance_key()
        .clone();
    let shared_key = InputBufferKey::new(
        shared.clone(),
        "view:ryeos/launcher",
        "query",
        core.binding_attachment_for_instance(&shared).unwrap(),
    )
    .storage_key();
    core.input_buffers.insert(
        shared_key.clone(),
        RyeOsInputState {
            text: "shared unsent draft".into(),
            ..Default::default()
        },
    );
    let shared_mounts = core
        .presentation
        .as_ref()
        .unwrap()
        .docks()
        .values()
        .map(|dock| dock.view().instance_key().clone())
        .collect::<Vec<_>>();
    let before = core.seat.events().len();
    assert!(
        core.invoke_affordance(
            &shared,
            "view:ryeos/launcher",
            "import-tabs",
            &json!({"opening_placement":{"kind":"new_surface"}})
        )
        .is_empty()
    );
    assert_eq!(
        core.seat.events().len(),
        before,
        "browser row cannot replace captured placement"
    );
    let effects = core.invoke_affordance(&shared, "view:ryeos/launcher", "import-tabs", &json!({}));
    assert!(effects.iter().all(|effect| !matches!(
        effect.kind,
        RyeOsEffectKind::InvokeBinding { .. }
            | RyeOsEffectKind::InvokePresentationOperation { .. }
            | RyeOsEffectKind::FetchSource { .. }
    )));
    let prepared = pending_prefix(&core)
        .iter()
        .find_map(|event| match &event.kind {
            SeatEventKind::PresentationOperationPrepared(operation) => Some(operation.clone()),
            _ => None,
        })
        .expect("import preparation");
    assert_eq!(core.active_surface().unwrap().view_sets().len(), 1);
    assert!(core.presentation_operation_is_live(&prepared));
    let effects = core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
    let invocation = effects
        .iter()
        .find(|effect| matches!(&effect.kind,
            RyeOsEffectKind::InvokePresentationOperation { operation } if operation == &prepared))
        .unwrap_or_else(|| panic!("import did not dispatch after prepared ACK: effects={effects:?}; notices={:?}; live={}", core.ui.notices, core.presentation_operation_is_live(&prepared)));
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect.kind, RyeOsEffectKind::FetchSource { .. }))
    );
    let mut origin = core
        .binding_attachment(&old_binding.binding_attachment_id)
        .unwrap()
        .clone();
    origin.binding_attachment_id = "imported-origin".into();
    origin.binding_generation += 20;
    origin.root_ref = "surface:test/imported".into();
    origin.presentation=crate::presentation::UiCompositionPresentation::from_effective_surface(json!({"name":"Imported","tiles":["view:test/imported"],"views":{
        "view:test/imported":{"widget":"rows","sources":{"default":{"ref":"service:test/imported","params":{}}}}
    }})).unwrap();
    let coordinate = UiBindingAttachmentCoordinate {
        binding_attachment_id: origin.binding_attachment_id.clone(),
        binding_generation: origin.binding_generation,
        binding_digest: origin.binding_digest.clone(),
    };
    let target = prepared
        .admission()
        .unwrap()
        .surface_open_target
        .as_ref()
        .unwrap();
    let crate::surface::opening::SurfaceOpenPlacement::AppendTabs {
        destination_surface_instance_id,
        expected_next_mount_id,
        ..
    } = &target.placement
    else {
        panic!("import placement")
    };
    let seeds = origin
        .presentation
        .definition
        .surface()
        .unwrap()
        .to_mount_seeds()
        .unwrap();
    let active = seeds[0].id.clone();
    let manifest = crate::ui::composition_identity::UiSurfaceMountManifest::allocate(
        crate::ids::PresentationId::try_new(producer.presentation_id.clone()).unwrap(),
        destination_surface_instance_id.clone(),
        *expected_next_mount_id,
        seeds
            .into_iter()
            .map(|seed| (seed, coordinate.clone()))
            .collect(),
        Some(&active),
    )
    .unwrap();
    let receipt = UiSurfaceTabsAdmissionTransition {
        kind: UiSurfaceTabsAdmissionKind::ImportSurfaceTabs,
        presentation_id: producer.presentation_id.clone(),
        creation_request_id: prepared.operation_id.clone(),
        opening: crate::surface::opening::SurfaceOpenRequest {
            creation_request_id: prepared.operation_id.clone(),
            selection: target.selection.clone(),
            placement: target.placement.clone(),
        },
        composition: None,
        origin_attachment: origin,
        tab_attachments: Vec::new(),
        manifest,
    };
    receipt.validate().unwrap();
    let effects = core.apply_effect_result(RyeOsEffectResult {
        id: invocation.id,
        ok: true,
        kind: RyeOsEffectResultKind::BindingInvoked,
        data: Some(json!({"ui_transition":receipt})),
        error: None,
    });
    assert!(effects.is_empty(), "completion is not mount publication");
    assert_eq!(core.active_surface().unwrap().view_sets().len(), 1);
    assert!(
        pending_prefix(&core)
            .iter()
            .any(|event| matches!(event.kind, SeatEventKind::PresentationOperationCompleted(_)))
    );
    let effects = core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
    assert!(
        !core.presentation_operations.replay_blocked,
        "import replay blocked: {:?}",
        core.ui.notices
    );
    let surface = core.active_surface().unwrap();
    assert_eq!(surface.binding(), &old_binding);
    assert_eq!(surface.view_sets().len(), 2);
    assert_eq!(surface.view_sets()[0].id, old_tab);
    let imported = &surface.view_sets()[1]
        .tiles
        .values()
        .next()
        .unwrap()
        .instance_key;
    assert!(effects.iter().any(|effect|matches!(&effect.kind,RyeOsEffectKind::FetchSource{tile_id,request,..}
        if request.binding_attachment_id=="imported-origin" && crate::ui::source_key::RyeOsSourceInstanceKey::decode(tile_id).is_some_and(|key|&key.view_instance==imported))),"imported source begins only after exact completion ACK");
    assert_eq!(core.input_buffers[&old_key].text, "existing unsent draft");
    assert_eq!(core.input_buffers[&shared_key].text, "shared unsent draft");
    assert_eq!(
        core.presentation
            .as_ref()
            .unwrap()
            .docks()
            .values()
            .map(|dock| dock.view().instance_key().clone())
            .collect::<Vec<_>>(),
        shared_mounts
    );
    assert!(core.presentation.as_ref().unwrap().mount(&shared).is_some());
}
