//! Canonical nested presentation composition. Admission remains daemon-owned.
//!
//! This owner replaces the flat core composition at integration; it must not
//! be installed beside a second writable copy of its ViewSets.

use std::collections::BTreeMap;

use crate::ids::{PresentationId, RyeOsViewInstanceKey, SurfaceInstanceId, TileId, ViewSetId};
use crate::presentation::UiCompositionDefinition;
use crate::view_set::{TileState, ViewLocalState, ViewSet};

use super::binding::{
    UiBindingAttachment, UiBindingAttachmentCoordinate, UiPresentationMembership,
    UiSurfaceMembership,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationInsertionDestination {
    pub surface_instance_id: SurfaceInstanceId,
    pub view_set_id: ViewSetId,
}

/// Exact inventory supplied by the existing composition admission owner.
/// This payload grants no binding or cross-Surface mutation authority.
#[derive(Debug, Clone)]
pub struct PresentationSurfaceViewSetUpdate {
    pub surface_instance_id: SurfaceInstanceId,
    pub view_sets: Vec<ViewSet>,
    pub active_view_set: Option<ViewSetId>,
    pub bindings: BTreeMap<TileId, UiBindingAttachmentCoordinate>,
}

#[cfg(test)]
mod tests {
    fn list_cursor(local: &crate::view_set::ViewLocalState) -> usize {
        match local {
            crate::view_set::ViewLocalState::GenericList { cursor, .. } => *cursor,
            _ => panic!("fixture requires list-local state"),
        }
    }

    fn set_list_cursor(local: &mut crate::view_set::ViewLocalState, value: usize) {
        // These tests exercise the pure composition owner without Core's
        // widget initialization. Explicitly supply list-local ephemera before
        // testing their preservation; the production mount remains neutral.
        if matches!(local, crate::view_set::ViewLocalState::None) {
            *local = crate::view_set::ViewSpec::bound("view:test/shared").initial_local_state();
        }
        match local {
            crate::view_set::ViewLocalState::GenericList { cursor, .. } => *cursor = value,
            _ => panic!("fixture requires list-local state"),
        }
    }
    use super::*;
    use serde_json::json;

    fn root() -> UiBindingAttachment {
        serde_json::from_value(json!({
            "binding_attachment_id":"shared", "binding_generation":1, "binding_digest":"digest",
            "root_ref":"config:test/shared", "root_generation":"generation", "project_path":null, "subject":{"kind":"projectless"},
            "posture":"observation_only", "binding_request_bounds":{"max_request_bytes":4096,"max_input_bytes":1024,"max_seat_append_bytes":65536},
            "presentation":{"definition":{"kind":"shared_presentation","config":{
                "category":"ui", "shared_presentation":{"schema":1,"title":"RyeOS","launcher":"launch",
                    "dialogs":[{"id":"launch","title":"Launch","view":"view:test/shared"},{"id":"help","title":"Help","view":"view:test/shared"}],
                    "capabilities":{"sources":[],"affordances":[]}}
            }},"views":{"view:test/shared":{"widget":"rows"}}}
        })).unwrap()
    }

    fn membership(root: &UiBindingAttachment) -> UiPresentationMembership {
        UiPresentationMembership {
            presentation_id: "00000000-0000-4000-8000-000000000001".into(),
            root_binding: coordinate(root),
            thread_id: None,
            chain_root_id: None,
            next_mount_id: 1,
            initialization_digest: "a".repeat(64),
            surfaces: vec![],
        }
    }

    fn surface(root: &UiBindingAttachment) -> UiBindingAttachment {
        let mut result = root.clone();
        result.binding_attachment_id = "surface".into();
        result.root_ref = "surface:test/work".into();
        result.presentation = crate::presentation::UiCompositionPresentation::from_effective_surface(json!({
            "name":"Work", "view_sets":[
                {"schema":2,"id":"one","title":"One","root":{"type":"group","views":["view:test/shared"],"active":0}},
                {"schema":2,"id":"two","title":"Two","root":{"type":"group","views":["view:test/shared"],"active":0}}
            ], "views":{"view:test/shared":{"widget":"rows"}}
        })).unwrap();
        result
    }

    #[test]
    fn retained_bootstrap_mount_identity_survives_unrelated_allocations() {
        let root = root();
        let descriptor = surface(&root);
        let member = UiSurfaceMembership {
            surface_instance_id: "00000000-0000-4000-8000-000000000002".into(),
            ordinal: 1,
            title: "Work".into(),
            binding: coordinate(&descriptor),
            mount_manifest: manifest(&descriptor, "00000000-0000-4000-8000-000000000002", 1),
            saved_surface: None,
        };
        let mut retained = membership(&root);
        retained.next_mount_id = member.mount_manifest.next_mount_id;
        retained.surfaces.push(member.clone());
        let mut before = PresentationComposition::from_membership(&retained, &root).unwrap();
        before.open_surface(&member, &descriptor).unwrap();
        for _ in 0..4 {
            let _ = ViewSet::from_tiling(
                crate::surface::TilingSpec::default(),
                Vec::new(),
                &crate::ids::PresentationId::try_new("allocation-fixture").unwrap(),
                &crate::ids::SurfaceInstanceId::try_new("allocation-surface").unwrap(),
            );
        }
        let mut after = PresentationComposition::from_membership(&retained, &root).unwrap();
        after.open_surface(&member, &descriptor).unwrap();
        let first = before.active_view_set().unwrap();
        let second = after.active_view_set().unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.focused_tile, second.focused_tile);
        assert_eq!(
            first.tiles[&first.focused_tile].instance_key,
            second.tiles[&second.focused_tile].instance_key
        );
        let mut invalid = retained.clone();
        invalid.surfaces[0].mount_manifest.next_mount_id += 1;
        assert!(PresentationComposition::from_membership(&invalid, &root).is_err());
    }
    fn manifest(
        descriptor: &UiBindingAttachment,
        id: &str,
        start: u64,
    ) -> super::super::composition_identity::UiSurfaceMountManifest {
        let seeds = descriptor
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active = seeds.first().map(|seed| seed.id.clone());
        super::super::composition_identity::UiSurfaceMountManifest::allocate(
            PresentationId::try_new("00000000-0000-4000-8000-000000000001").unwrap(),
            SurfaceInstanceId::try_new(id).unwrap(),
            start,
            seeds
                .into_iter()
                .map(|seed| (seed, coordinate(descriptor)))
                .collect(),
            active.as_deref(),
        )
        .unwrap()
    }
    #[test]
    fn saved_surface_batch_keeps_authored_origin_and_distinct_tab_bindings_atomic() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        let origin = surface(&root);
        let mut first = origin.clone();
        first.binding_attachment_id = "tab-one".into();
        let mut second = origin.clone();
        second.binding_attachment_id = "tab-two".into();
        second.root_ref = "surface:test/other-project".into();
        let tab = |descriptor: &UiBindingAttachment, id: &str| {
            crate::ui::binding::UiSavedSurfaceTab {
            saved_set_id:id.into(),binding:coordinate(descriptor),
            resolved:serde_json::from_value(json!({"id":id,"name":id,
                "composition":{"schema":2,"id":id,"title":id,"root":{"type":"group","views":["view:test/shared"],"active":0}},
                "relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"follow_own_set"}}]})).unwrap(),
        }
        };
        let saved = crate::ui::binding::UiSavedSurfaceComposition {
            provenance: crate::ui::binding::UiSurfaceCompositionProvenance::Library {
                saved_id: "saved".into(),
                library_revision: 2,
                record_digest: "a".repeat(64),
            },
            active_view_set_id: "second".into(),
            body: crate::ui::binding::UiSavedSurfaceCompositionBody::Particular {
                tabs: vec![tab(&first, "first"), tab(&second, "second")],
            },
        };
        let admitted = UiSurfaceMembership {
            surface_instance_id: "00000000-0000-4000-8000-000000000002".into(),
            ordinal: 1,
            title: "Saved alias".into(),
            binding: coordinate(&origin),
            mount_manifest: super::super::composition_identity::UiSurfaceMountManifest::allocate(
                PresentationId::try_new("00000000-0000-4000-8000-000000000001").unwrap(),
                SurfaceInstanceId::try_new("00000000-0000-4000-8000-000000000002").unwrap(),
                1,
                saved
                    .particular_tabs()
                    .unwrap()
                    .iter()
                    .map(|tab| (tab.resolved.composition.clone(), tab.binding.clone()))
                    .collect(),
                Some(&saved.active_view_set_id),
            )
            .unwrap(),
            saved_surface: Some(saved),
        };
        let mut descriptors = BTreeMap::from([
            (first.binding_attachment_id.clone(), first.clone()),
            (second.binding_attachment_id.clone(), second.clone()),
        ]);
        descriptors.get_mut("tab-two").unwrap().binding_generation += 1;
        assert!(
            owner
                .open_saved_surface(&admitted, &origin, &descriptors)
                .is_err()
        );
        assert!(
            owner.surfaces().is_empty(),
            "invalid later tab never publishes earlier tab/origin"
        );
        descriptors.insert("tab-two".into(), second.clone());
        let ids = owner
            .open_saved_surface(&admitted, &origin, &descriptors)
            .unwrap();
        let surface = owner.active_surface().unwrap();
        assert_eq!(surface.title(), "Saved alias");
        assert_eq!(surface.binding(), &coordinate(&origin));
        assert_eq!(
            surface.view_sets().len(),
            2,
            "authored seed tabs are replaced, not appended"
        );
        assert_eq!(surface.active_view_set().unwrap().id, ids["second"]);
        let tile = surface.view_sets()[0].tiles.values().next().unwrap();
        assert_eq!(
            owner.mount(&tile.instance_key).unwrap().binding(),
            &coordinate(&first)
        );
        let tile = surface.view_sets()[1].tiles.values().next().unwrap();
        assert_eq!(
            owner.mount(&tile.instance_key).unwrap().binding(),
            &coordinate(&second)
        );
    }

    #[test]
    fn reusable_mixed_origin_bootstrap_and_cold_fold_preserve_subjects_atomically() {
        use crate::ui::attachment::SelectionAttachment;
        use crate::ui::binding::{
            UiReusableSurfaceTab, UiSavedSurfaceComposition, UiSavedSurfaceCompositionBody,
        };
        use crate::ui::composition_replay::UiCompositionReplay;
        use crate::ui::presentation_initialization::{
            UiPresentationInitialization, UiPresentationInitializationRecord,
        };
        use crate::ui::seat::{SeatEvent, SeatEventKind};

        // Admitted descriptor/manifest fixture, not a compiler or grant proof.
        let mut root = root();
        root.binding_digest = "a".repeat(64);
        root.binding_request_bounds.max_request_bytes = 65536;
        let origin = surface(&root);
        let tab_descriptor = |id: &str, project: &str| {
            let mut descriptor = origin.clone();
            descriptor.binding_attachment_id = id.into();
            descriptor.root_ref = format!("surface:test/{id}");
            descriptor.project_path = Some(format!("/fixture/{project}"));
            descriptor.subject = super::super::binding::UiBindingSubject::RegisteredProject {
                local_id: project.into(),
            };
            descriptor.presentation =
                crate::presentation::UiCompositionPresentation::from_effective_surface(json!({
                    "name":id,"tiles":["view:test/subject"],
                    "views":{"view:test/subject":{"widget":"rows","sources":{"default":{
                        "ref":"service:test/read","params":{"id":"@facet:selection.work.id"}
                    }}}}
                }))
                .unwrap();
            descriptor
        };
        let first = tab_descriptor("first", "registered-first");
        let second = tab_descriptor("second", "registered-second");
        let tab = |descriptor: &UiBindingAttachment, id: &str, relationships, views| {
            UiReusableSurfaceTab {
                saved_set_id: id.into(),
                binding: coordinate(descriptor),
                origin: crate::surface::saved_surfaces::SavedSurfaceOrigin {
                    surface_ref: descriptor.root_ref.clone(),
                    project: Some(crate::surface::view_sets::ParticularProjectRef {
                        local_id: match &descriptor.subject {
                            super::super::binding::UiBindingSubject::RegisteredProject { local_id } => local_id.clone(),
                            _ => unreachable!(),
                        },
                    }),
                },
                template: serde_json::from_value(json!({"id":id,"name":id,
                    "composition":{"schema":2,"id":id,"title":id,"root":{"type":"group","views":views,"active":0}},
                    "relationships":relationships})).unwrap(),
                fresh_subjects: BTreeMap::new(),
            }
        };
        let mut first_tab = tab(
            &first,
            "first",
            json!([
                {"mount":{"kind":"tile","index":0},"source":{"mode":"required_subject","input":"fresh","facets":["selection.work.id"]}},
                {"mount":{"kind":"tile","index":1},"source":{"mode":"required_subject","input":"unresolved","facets":["selection.work.id"]}}
            ]),
            json!(["view:test/subject", "view:test/subject"]),
        );
        first_tab.fresh_subjects.insert(
            "fresh".into(),
            BTreeMap::from([("selection.work.id".into(), json!("explicit-new-work"))]),
        );
        let second_tab = tab(
            &second,
            "second",
            json!([
                {"mount":{"kind":"tile","index":0},"source":{"mode":"follow_set","saved_view_set_id":"first"}}
            ]),
            json!(["view:test/subject"]),
        );
        let saved = UiSavedSurfaceComposition {
            provenance: crate::ui::binding::UiSurfaceCompositionProvenance::Library {
                saved_id: "mixed-reusable".into(),
                library_revision: 3,
                record_digest: "b".repeat(64),
            },
            active_view_set_id: "second".into(),
            body: UiSavedSurfaceCompositionBody::Reusable {
                tabs: vec![first_tab, second_tab],
            },
        };
        let member = UiSurfaceMembership {
            surface_instance_id: "00000000-0000-4000-8000-000000000002".into(),
            ordinal: 1,
            title: "Mixed reusable".into(),
            binding: coordinate(&origin),
            mount_manifest: super::super::composition_identity::UiSurfaceMountManifest::allocate(
                PresentationId::try_new(membership(&root).presentation_id).unwrap(),
                SurfaceInstanceId::try_new("00000000-0000-4000-8000-000000000002").unwrap(),
                1,
                saved
                    .reusable_tabs()
                    .unwrap()
                    .iter()
                    .map(|tab| (tab.template.composition.clone(), tab.binding.clone()))
                    .collect(),
                Some("second"),
            )
            .unwrap(),
            saved_surface: Some(saved),
        };
        let mut descriptors = BTreeMap::from([
            (first.binding_attachment_id.clone(), first.clone()),
            (second.binding_attachment_id.clone(), second.clone()),
        ]);
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        descriptors.get_mut("second").unwrap().binding_generation += 1;
        assert!(
            owner
                .open_saved_surface(&member, &origin, &descriptors)
                .is_err()
        );
        assert!(
            owner.surfaces().is_empty(),
            "a bad later tab must not publish the first"
        );
        descriptors.insert("second".into(), second.clone());
        let ids = owner
            .open_saved_surface(&member, &origin, &descriptors)
            .unwrap();
        assert_eq!(
            owner.active_surface().unwrap().binding(),
            &coordinate(&origin)
        );
        assert_eq!(owner.active_view_set().unwrap().id, ids["second"]);

        let initial = UiPresentationInitialization {
            schema: 2,
            selection: crate::ui::presentation_initialization::UiPresentationInitialSelection::ConfiguredDefaults,
            presentation_id: membership(&root).presentation_id,
            creation_request_id: "22222222-2222-4222-8222-222222222222".into(),
            root_binding: coordinate(&root),
            next_mount_id: 1,
            surfaces: vec![],
            binding_attachments: vec![root.clone()],
        };
        let event = SeatEvent {
            seq: 4,
            producer_engine_seq: None,
            kind: SeatEventKind::PresentationInitialized(UiPresentationInitializationRecord {
                operation_id: initial.creation_request_id.clone(),
                initialization: initial.clone(),
            }),
        };
        let empty = UiCompositionReplay::begin(&initial, &event).unwrap();
        let invocation_origin = empty.composition.dialogs()["launch"]
            .view()
            .instance_key()
            .clone();
        let mut prepared: crate::ui::admission_operation::UiPreparedPresentationOperation = serde_json::from_value(json!({
            "schema":2,"operation_id":"55555555-5555-4555-8555-555555555555","prepared_engine_seq":0,
            "producer":{"presentation_id":initial.presentation_id,"thread_id":"fixture-seat","producer_incarnation":"a".repeat(64)},
            "root_binding":coordinate(&root),"request_bounds":root.binding_request_bounds,
            "body":{"kind":"admission","purpose":"open_surface","surface_open_target":{"selection":{
                "kind":"reusable","id":"mixed-reusable","expected_revision":3,
                "fresh_subjects":{"first":{"fresh":{"selection.work.id":"explicit-new-work"}}}
            },"placement":{"kind":"new_surface"}},"particular_selection":null,"invocation_origin":invocation_origin,
                "request":{"binding_attachment_id":root.binding_attachment_id,"binding_generation":root.binding_generation,"binding_digest":root.binding_digest,
                    "coordinate":{"kind":"affordance","view_ref":"view:test/shared","affordance_id":"open"},
                    "payload":{"kind":"selection","record":{"creation_request_id":"55555555-5555-4555-8555-555555555555"}}},
                "success_notice":null}
        })).unwrap();
        prepared.assign_identity().unwrap();
        let preparation = SeatEvent {
            seq: 5,
            producer_engine_seq: None,
            kind: SeatEventKind::PresentationOperationPrepared(prepared.clone()),
        };
        let completion = SeatEvent {
            seq: 6,
            producer_engine_seq: None,
            kind: SeatEventKind::PresentationOperationCompleted(
                crate::ui::admission_operation::UiCompletedPresentationOperation {
                    operation_id: prepared.operation_id.clone(),
                    result: json!({"ui_transition":{
                        "kind":"open_surface","presentation_id":initial.presentation_id,"creation_request_id":prepared.operation_id,
                        "opening":{"creation_request_id":prepared.operation_id,"selection":prepared.admission().unwrap().surface_open_target.as_ref().unwrap().selection,"placement":{"kind":"new_surface"}},
                        "surface":member,"attachment":origin,"tab_attachments":[first,second]
                    }}),
                },
            ),
        };
        let mut pending = empty;
        pending.apply_ordered_event(&preparation).unwrap();
        assert!(pending.composition.surfaces().is_empty());
        let mut bad = completion.clone();
        if let SeatEventKind::PresentationOperationCompleted(completed) = &mut bad.kind {
            completed.result["ui_transition"]["tab_attachments"][1]["binding_generation"] =
                json!(2);
        }
        assert!(pending.apply_ordered_event(&bad).is_err());
        assert_eq!(pending.next_mount_id, 1);
        assert!(pending.composition.surfaces().is_empty());
        let mut cold = pending;
        cold.apply_ordered_event(&completion).unwrap();
        let mut again = UiCompositionReplay::begin(&initial, &event).unwrap();
        again.apply_ordered_event(&preparation).unwrap();
        again.apply_ordered_event(&completion).unwrap();
        assert_eq!(cold.subjects, again.subjects);
        assert_eq!(cold.next_mount_id, member.mount_manifest.next_mount_id);
        assert!(cold.pending.is_empty());
        let surface = cold.composition.active_surface().unwrap();
        assert_eq!(surface.active_view_set().unwrap().id, ids["second"]);
        let first_set = &surface.view_sets()[0];
        let first_tiles = first_set.root.as_ref().unwrap().tile_ids();
        let fresh = &cold.subjects[&first_set.tiles[&first_tiles[0]].instance_key];
        assert_eq!(fresh.binding, coordinate(&first));
        assert!(
            matches!(&fresh.attachment, SelectionAttachment::Pinned { values, .. }
            if values["selection.work.id"] == json!("explicit-new-work"))
        );
        let missing = &cold.subjects[&first_set.tiles[&first_tiles[1]].instance_key];
        assert!(
            matches!(&missing.attachment, SelectionAttachment::RequiredSubject { input, .. }
            if input == "unresolved")
        );
        let follower = &cold.subjects[&surface.view_sets()[1]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key];
        assert_eq!(follower.binding, coordinate(&second));
        assert!(
            matches!(&follower.attachment, SelectionAttachment::FollowViewSet { view_set_id }
            if *view_set_id == ids["first"])
        );
        assert!(
            cold.work_selections.is_empty(),
            "reuse must not invent old work selection"
        );
        // Import the same portable collection into the existing Surface. The
        // second allocation must not replace its origin or previously mounted
        // tabs, and followers must point inside the imported collection.
        let destination = cold.composition.active_surface().unwrap();
        let destination_id = destination.id().clone();
        let destination_binding = destination.binding().clone();
        let old_tabs = destination
            .view_sets()
            .iter()
            .map(|set| set.id)
            .collect::<Vec<_>>();
        let old_subjects = cold.subjects.clone();
        let before_frontier = cold.next_mount_id;
        let mut import_prepared = prepared.clone();
        if let crate::ui::admission_operation::UiPreparedPresentationOperationBody::Admission(
            admission,
        ) = &mut import_prepared.body
        {
            admission.purpose =
                crate::ui::admission_operation::UiPresentationOperationKind::ImportSurfaceTabs;
            admission.surface_open_target.as_mut().unwrap().placement =
                crate::surface::opening::SurfaceOpenPlacement::AppendTabs {
                    destination_surface_instance_id: destination_id.clone(),
                    destination_binding: destination_binding.clone(),
                    expected_composition_revision: cold.revision,
                    expected_next_mount_id: before_frontier,
                    insertion_index: old_tabs.len(),
                    activate_imported_tab: true,
                };
        }
        import_prepared.assign_identity().unwrap();
        let target = import_prepared
            .admission()
            .unwrap()
            .surface_open_target
            .as_ref()
            .unwrap();
        let opening = crate::surface::opening::SurfaceOpenRequest {
            creation_request_id: import_prepared.operation_id.clone(),
            selection: target.selection.clone(),
            placement: target.placement.clone(),
        };
        let saved = member.saved_surface.clone().unwrap();
        let imported_manifest =
            super::super::composition_identity::UiSurfaceMountManifest::allocate(
                cold.composition.id().clone(),
                destination_id.clone(),
                before_frontier,
                saved.mount_seeds(),
                Some(&saved.active_view_set_id),
            )
            .unwrap();
        let imported = crate::ui::binding::UiSurfaceTabsAdmissionTransition {
            kind: crate::ui::binding::UiSurfaceTabsAdmissionKind::ImportSurfaceTabs,
            presentation_id: initial.presentation_id.clone(),
            creation_request_id: import_prepared.operation_id.clone(),
            opening,
            composition: Some(saved),
            origin_attachment: origin.clone(),
            tab_attachments: vec![first.clone(), second.clone()],
            manifest: imported_manifest.clone(),
        };
        imported.validate().unwrap();
        let import_preparation = SeatEvent {
            seq: 7,
            producer_engine_seq: None,
            kind: SeatEventKind::PresentationOperationPrepared(import_prepared.clone()),
        };
        let import_completion = SeatEvent {
            seq: 8,
            producer_engine_seq: None,
            kind: SeatEventKind::PresentationOperationCompleted(
                crate::ui::admission_operation::UiCompletedPresentationOperation {
                    operation_id: import_prepared.operation_id.clone(),
                    result: json!({"ui_transition":imported}),
                },
            ),
        };
        cold.apply_ordered_event(&import_preparation).unwrap();
        assert_eq!(
            cold.composition.surfaces()[&destination_id]
                .view_sets()
                .len(),
            2
        );
        let mut invalid = import_completion.clone();
        if let SeatEventKind::PresentationOperationCompleted(result) = &mut invalid.kind {
            result.result["ui_transition"]["tab_attachments"][1]["binding_generation"] = json!(900);
        }
        assert!(cold.apply_ordered_event(&invalid).is_err());
        assert_eq!(cold.next_mount_id, before_frontier);
        assert_eq!(cold.subjects, old_subjects);
        for stale_owner in [false, true] {
            let mut rejected = again.clone();
            let before = format!("{:?}", rejected.composition);
            let mut operation = import_prepared.clone();
            let crate::ui::admission_operation::UiPreparedPresentationOperationBody::Admission(
                admission,
            ) = &mut operation.body
            else {
                unreachable!()
            };
            let placement = &mut admission.surface_open_target.as_mut().unwrap().placement;
            let crate::surface::opening::SurfaceOpenPlacement::AppendTabs {
                expected_composition_revision,
                destination_surface_instance_id,
                ..
            } = placement
            else {
                unreachable!()
            };
            if stale_owner {
                *destination_surface_instance_id =
                    SurfaceInstanceId::try_new("99999999-9999-4999-8999-999999999999").unwrap();
            } else {
                *expected_composition_revision += 1;
            }
            operation.assign_identity().unwrap();
            let target = operation
                .admission()
                .unwrap()
                .surface_open_target
                .as_ref()
                .unwrap();
            let mut receipt = imported.clone();
            receipt.creation_request_id = operation.operation_id.clone();
            receipt.opening = crate::surface::opening::SurfaceOpenRequest {
                creation_request_id: operation.operation_id.clone(),
                selection: target.selection.clone(),
                placement: target.placement.clone(),
            };
            if let crate::surface::opening::SurfaceOpenPlacement::AppendTabs {
                destination_surface_instance_id,
                ..
            } = &target.placement
            {
                receipt.manifest.surface_instance_id = destination_surface_instance_id.clone();
            }
            receipt.validate().unwrap();
            let prepare = SeatEvent {
                seq: 7,
                producer_engine_seq: None,
                kind: SeatEventKind::PresentationOperationPrepared(operation.clone()),
            };
            rejected.apply_ordered_event(&prepare).unwrap();
            let finish = SeatEvent {
                seq: 8,
                producer_engine_seq: None,
                kind: SeatEventKind::PresentationOperationCompleted(
                    crate::ui::admission_operation::UiCompletedPresentationOperation {
                        operation_id: operation.operation_id,
                        result: json!({"ui_transition":receipt}),
                    },
                ),
            };
            assert!(rejected.apply_ordered_event(&finish).is_err());
            assert_eq!(format!("{:?}", rejected.composition), before);
            assert_eq!(rejected.next_mount_id, before_frontier);
            assert_eq!(rejected.subjects, old_subjects);
        }
        cold.apply_ordered_event(&import_completion).unwrap();
        let destination = &cold.composition.surfaces()[&destination_id];
        assert_eq!(destination.binding(), &destination_binding);
        assert_eq!(
            destination
                .view_sets()
                .iter()
                .take(2)
                .map(|set| set.id)
                .collect::<Vec<_>>(),
            old_tabs
        );
        assert_eq!(destination.view_sets().len(), 4);
        for (mount, subject) in old_subjects {
            assert_eq!(cold.subjects[&mount], subject);
        }
        let imported_first = &destination.view_sets()[2];
        let imported_tiles = imported_first.root.as_ref().unwrap().tile_ids();
        let imported_pin = &cold.subjects[&imported_first.tiles[&imported_tiles[0]].instance_key];
        assert_eq!(imported_pin.binding, coordinate(&first));
        assert!(
            matches!(&imported_pin.attachment,SelectionAttachment::Pinned{values,..} if values["selection.work.id"]==json!("explicit-new-work"))
        );
        let imported_required =
            &cold.subjects[&imported_first.tiles[&imported_tiles[1]].instance_key];
        assert_eq!(imported_required.binding, coordinate(&first));
        assert!(
            matches!(&imported_required.attachment,SelectionAttachment::RequiredSubject{input,facets} if input=="unresolved" && facets==&vec!["selection.work.id".to_owned()])
        );
        let follower = &destination.view_sets()[3]
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key;
        assert!(
            matches!(&cold.subjects[follower].attachment,SelectionAttachment::FollowViewSet{view_set_id} if *view_set_id==imported_manifest.tabs[0].view_set_id)
        );
        again.apply_ordered_event(&import_preparation).unwrap();
        again.apply_ordered_event(&import_completion).unwrap();
        assert_eq!(cold.subjects, again.subjects);
        assert_eq!(cold.next_mount_id, again.next_mount_id);
    }

    #[test]
    fn zero_surfaces_and_last_close_preserve_shared_mounts() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        assert!(owner.active_surface().is_none());
        assert!(owner.active_view_set().is_none());
        assert_eq!(owner.dialogs().len(), 2);
        let descriptor = surface(&root);
        let admitted = UiSurfaceMembership {
            saved_surface: None,
            mount_manifest: manifest(&descriptor, "00000000-0000-4000-8000-000000000002", 1),
            surface_instance_id: "00000000-0000-4000-8000-000000000002".into(),
            ordinal: 1,
            title: "Work".into(),
            binding: coordinate(&descriptor),
        };
        owner.open_surface(&admitted, &descriptor).unwrap();
        let id = SurfaceInstanceId::try_new(admitted.surface_instance_id.clone()).unwrap();
        let second_tab = owner.active_surface().unwrap().view_sets()[1].id;
        owner
            .active_surface_mut()
            .unwrap()
            .select_view_set(second_tab)
            .unwrap();
        assert_eq!(owner.active_view_set().unwrap().id, second_tab);
        for tile in owner.active_view_set().unwrap().tiles.values() {
            assert!(matches!(
                tile.instance_key.mount_owner(),
                Some(crate::ids::UiViewMountOwner::Surface { .. })
            ));
        }
        owner.close_surface(&id).unwrap();
        assert!(owner.active_view_set().is_none());
        assert_eq!(owner.launcher(), "launch");
        assert_eq!(owner.dialogs().len(), 2);
    }

    #[test]
    fn dialog_frame_supersession_preserves_mounted_state_and_refuses_stale_close() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        owner.open_dialog("launch", None).unwrap();
        set_list_cursor(owner.dialog_mut("launch").unwrap().local_mut(), 7);
        let old = owner.dialogs()["launch"].view().instance_key().clone();
        owner.open_dialog("help", None).unwrap();
        assert!(!owner.dialogs()["launch"].open);
        assert!(owner.dialogs()["help"].open);
        assert_eq!(list_cursor(&owner.dialogs()["launch"].view().local), 7);
        assert!(!owner.close_dialog(&old));
        assert!(owner.dialogs()["help"].open);
    }

    #[test]
    fn stale_dialog_context_refuses_before_switching_the_open_frame() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        owner.open_dialog("launch", None).unwrap();
        let stale = RyeOsViewInstanceKey::shared_dialog(owner.id(), "removed-origin").unwrap();

        assert!(
            owner
                .open_dialog_with_context("help", None, Some(stale))
                .is_err()
        );
        assert!(owner.dialogs()["launch"].open);
        assert!(!owner.dialogs()["help"].open);
        assert!(owner.dialogs()["help"].context_origin().is_none());
    }

    #[test]
    fn transient_dialog_with_removed_origin_closes_without_retargeting() {
        let root = root();
        let mut previous =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        let origin = previous.dialogs()["help"].view().instance_key().clone();
        previous
            .open_dialog_with_context("launch", None, Some(origin))
            .unwrap();
        set_list_cursor(previous.dialog_mut("launch").unwrap().local_mut(), 7);
        let mut current = previous.clone();
        current.dialogs.remove("help");
        current.preserve_transient_state_from(&previous);
        assert!(!current.dialogs()["launch"].open);
        assert!(current.dialogs()["launch"].context_origin().is_none());
        assert_eq!(list_cursor(&current.dialogs()["launch"].view().local), 7);

        previous
            .open_dialog_with_context("launch", None, None)
            .unwrap();
        let mut global = previous.clone();
        global.dialogs.remove("help");
        global.preserve_transient_state_from(&previous);
        assert!(
            global.dialogs()["launch"].open,
            "an origin-free shared frame remains independent"
        );
    }

    #[test]
    fn equal_refs_have_independent_mount_ephemera_and_wrong_roots_refuse() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        owner.open_dialog("launch", None).unwrap();
        assert!(!owner.dialogs()["help"].open);
        assert_ne!(
            owner.dialogs()["launch"].view.instance_key,
            owner.dialogs()["help"].view.instance_key
        );
        let mut wrong = membership(&root);
        wrong.root_binding.binding_generation += 1;
        assert!(PresentationComposition::from_membership(&wrong, &root).is_err());
        let descriptor = surface(&root);
        let mut wrong_kind = membership(&root);
        wrong_kind.root_binding = coordinate(&descriptor);
        assert!(PresentationComposition::from_membership(&wrong_kind, &descriptor).is_err());
        let admitted = UiSurfaceMembership {
            saved_surface: None,
            mount_manifest: manifest(&descriptor, "00000000-0000-4000-8000-000000000002", 1),
            surface_instance_id: "00000000-0000-4000-8000-000000000002".into(),
            ordinal: 1,
            title: "Work".into(),
            binding: coordinate(&root),
        };
        assert!(owner.open_surface(&admitted, &root).is_err());
    }

    #[test]
    fn same_surface_definition_has_distinct_instance_tile_addresses() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        let descriptor = surface(&root);
        for (ordinal, suffix) in [(1, "2"), (2, "3")] {
            owner
                .open_surface(
                    &UiSurfaceMembership {
                        saved_surface: None,
                        mount_manifest: manifest(
                            &descriptor,
                            &format!("00000000-0000-4000-8000-00000000000{suffix}"),
                            ordinal * 100,
                        ),
                        surface_instance_id: format!("00000000-0000-4000-8000-00000000000{suffix}"),
                        ordinal,
                        title: "Work".into(),
                        binding: coordinate(&descriptor),
                    },
                    &descriptor,
                )
                .unwrap();
        }
        let keys: Vec<_> = owner
            .surfaces()
            .values()
            .map(|surface| {
                surface
                    .active_view_set()
                    .unwrap()
                    .tiles
                    .values()
                    .next()
                    .unwrap()
                    .instance_key
                    .clone()
            })
            .collect();
        assert_ne!(keys[0], keys[1]);
        let absent = SurfaceInstanceId::try_new("00000000-0000-4000-8000-000000000004").unwrap();
        assert!(owner.select_surface(&absent).is_err());
        assert_eq!(owner.surfaces().len(), 2);
    }

    #[test]
    fn contextual_dialogs_keep_exact_surface_binding_and_independent_state() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        let mut descriptor = surface(&root);
        let UiCompositionDefinition::Surface { surface } = &mut descriptor.presentation.definition
        else {
            panic!("Surface fixture");
        };
        surface.dialogs = vec![crate::presentation::PresentationDialogSpec {
            id: "launch".into(),
            title: "Context actions".into(),
            view: crate::surface::ViewKindSpec("view:test/shared".into()),
        }];
        let mut ids = Vec::new();
        for (ordinal, suffix) in [(1, "2"), (2, "3")] {
            let id =
                SurfaceInstanceId::try_new(format!("00000000-0000-4000-8000-00000000000{suffix}"))
                    .unwrap();
            owner
                .open_surface(
                    &UiSurfaceMembership {
                        saved_surface: None,
                        mount_manifest: manifest(&descriptor, id.as_str(), ordinal * 100),
                        surface_instance_id: id.as_str().into(),
                        ordinal,
                        title: "Work".into(),
                        binding: coordinate(&descriptor),
                    },
                    &descriptor,
                )
                .unwrap();
            ids.push(id);
        }
        let first = RyeOsViewInstanceKey::surface_dialog(owner.id(), &ids[0], "launch").unwrap();
        let second = RyeOsViewInstanceKey::surface_dialog(owner.id(), &ids[1], "launch").unwrap();
        let shared = owner.dialogs()["launch"].view().instance_key().clone();
        assert_ne!(first, second);
        assert_ne!(first, shared);
        assert_eq!(
            owner.mount(&first).unwrap().binding(),
            &coordinate(&descriptor)
        );
        assert_eq!(owner.mount(&shared).unwrap().binding(), &coordinate(&root));
        assert!(
            owner.open_dialog(first.as_str(), None).is_err(),
            "inactive sibling cannot contribute a dialog"
        );
        owner.open_dialog(second.as_str(), None).unwrap();
        set_list_cursor(owner.dialog_mut(second.as_str()).unwrap().local_mut(), 8);
        owner.select_surface(&ids[0]).unwrap();
        assert!(!owner.mount(&second).unwrap().visible());
        let first_origin = owner
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        assert_eq!(
            owner
                .resolve_dialog("launch", Some(&first_origin))
                .unwrap()
                .1
                .view()
                .instance_key(),
            &first
        );
        assert_eq!(
            owner
                .resolve_dialog(shared.as_str(), Some(&first_origin))
                .unwrap()
                .1
                .view()
                .instance_key(),
            &shared
        );
        assert_eq!(
            owner
                .resolve_dialog("launch", None)
                .unwrap()
                .1
                .view()
                .instance_key(),
            &shared
        );
        assert!(owner.resolve_dialog(second.as_str(), None).is_none());
        owner.open_dialog(first.as_str(), None).unwrap();
        assert!(
            matches!(owner.mount(&first).unwrap().local(), ViewLocalState::None),
            "the untouched sibling has no list-local ephemera yet"
        );
        owner.select_surface(&ids[1]).unwrap();
        assert!(
            owner
                .resolve_dialog("launch", Some(&first_origin))
                .is_none(),
            "stale origin cannot borrow another Surface"
        );
        owner.open_dialog(second.as_str(), None).unwrap();
        assert_eq!(list_cursor(owner.mount(&second).unwrap().local()), 8);
        owner.close_surface(&ids[1]).unwrap();
        assert!(owner.mount(&second).is_none());
        assert!(owner.mount(&first).is_some());
        owner.open_dialog("launch", None).unwrap();
        assert!(owner.mount(&shared).unwrap().visible());
    }

    #[test]
    fn missing_or_duplicate_contextual_dialogs_refuse_without_partial_membership() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        let mut descriptor = surface(&root);
        let dialog = crate::presentation::PresentationDialogSpec {
            id: "context".into(),
            title: "Context".into(),
            view: crate::surface::ViewKindSpec("view:test/missing".into()),
        };
        if let UiCompositionDefinition::Surface { surface } =
            &mut descriptor.presentation.definition
        {
            surface.dialogs = vec![dialog.clone()];
        }
        let member = UiSurfaceMembership {
            saved_surface: None,
            mount_manifest: manifest(&descriptor, "00000000-0000-4000-8000-000000000002", 1),
            surface_instance_id: "00000000-0000-4000-8000-000000000002".into(),
            ordinal: 1,
            title: "Work".into(),
            binding: coordinate(&descriptor),
        };
        assert!(owner.open_surface(&member, &descriptor).is_err());
        assert!(owner.surfaces().is_empty());
        assert_eq!(owner.dialogs().len(), 2);
        descriptor
            .presentation
            .views
            .insert(dialog.view.0.clone(), serde_json::json!({}));
        if let UiCompositionDefinition::Surface { surface } =
            &mut descriptor.presentation.definition
        {
            surface.dialogs.push(dialog);
        }
        assert!(owner.open_surface(&member, &descriptor).is_err());
        assert!(owner.surfaces().is_empty());
        assert_eq!(owner.dialogs().len(), 2);
    }

    #[test]
    fn tab_mutations_keep_coordinates_and_fail_atomically() {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        let descriptor = surface(&root);
        let admitted = UiSurfaceMembership {
            saved_surface: None,
            mount_manifest: manifest(&descriptor, "00000000-0000-4000-8000-000000000002", 1),
            surface_instance_id: "00000000-0000-4000-8000-000000000002".into(),
            ordinal: 1,
            title: "Work".into(),
            binding: coordinate(&descriptor),
        };
        owner.open_surface(&admitted, &descriptor).unwrap();
        let surface_id = SurfaceInstanceId::try_new(admitted.surface_instance_id).unwrap();
        let first = owner.active_view_set().unwrap().id;
        let key = owner
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();
        assert_eq!(
            owner.mount(&key).unwrap().binding(),
            &coordinate(&descriptor)
        );
        assert!(
            owner
                .view_set_for_instance(owner.dialogs()["launch"].view.instance_key())
                .is_none()
        );
        assert!(
            owner
                .mutate_view_set(&surface_id, first, BTreeMap::new(), |set| {
                    set.root = None;
                    Ok(())
                })
                .is_err()
        );
        assert!(owner.mount(&key).is_some());
        owner.reorder_view_set(&surface_id, first, 1).unwrap();
        assert_eq!(owner.active_view_set().unwrap().id, first);
        owner.close_view_set(&surface_id, first).unwrap();
        assert!(owner.mount(&key).is_none());
        assert!(owner.dialogs().contains_key("launch"));
    }

    fn two_surface_move_fixture() -> (
        PresentationComposition,
        PresentationSurfaceViewSetUpdate,
        PresentationSurfaceViewSetUpdate,
        RyeOsViewInstanceKey,
        RyeOsViewInstanceKey,
    ) {
        let root = root();
        let mut owner =
            PresentationComposition::from_membership(&membership(&root), &root).unwrap();
        let source_descriptor = surface(&root);
        let mut destination_descriptor = source_descriptor.clone();
        destination_descriptor.binding_attachment_id = "surface-other".into();
        let source_id = "00000000-0000-4000-8000-000000000002";
        let destination_id = "00000000-0000-4000-8000-000000000003";
        let source_manifest = manifest(&source_descriptor, source_id, 1);
        let destination_manifest = manifest(
            &destination_descriptor,
            destination_id,
            source_manifest.next_mount_id,
        );
        for (id, ordinal, descriptor, mount_manifest) in [
            (source_id, 1, &source_descriptor, source_manifest),
            (
                destination_id,
                2,
                &destination_descriptor,
                destination_manifest,
            ),
        ] {
            owner
                .open_surface(
                    &UiSurfaceMembership {
                        surface_instance_id: id.into(),
                        ordinal,
                        title: "Work".into(),
                        binding: coordinate(descriptor),
                        mount_manifest,
                        saved_surface: None,
                    },
                    descriptor,
                )
                .unwrap();
        }
        let source_id = SurfaceInstanceId::try_new(source_id).unwrap();
        let destination_id = SurfaceInstanceId::try_new(destination_id).unwrap();
        let mut source_sets = owner.surfaces[&source_id].view_sets.clone();
        let mut destination_sets = owner.surfaces[&destination_id].view_sets.clone();
        let tile = source_sets[0].focused_tile;
        let old_key = source_sets[0].tiles[&tile].instance_key.clone();
        set_list_cursor(&mut source_sets[0].tiles.get_mut(&tile).unwrap().local, 11);
        let mut source_bindings = owner.tile_bindings(&source_id).unwrap();
        let mut destination_bindings = owner.tile_bindings(&destination_id).unwrap();
        let exact_binding = source_bindings.remove(&tile).unwrap();
        destination_bindings.insert(tile, exact_binding);
        assert!(source_sets[0].move_tile_to_view_set(&mut destination_sets[0], tile));
        let source_active = Some(source_sets[0].id);
        let destination_active = Some(destination_sets[0].id);
        let new_key = RyeOsViewInstanceKey::surface_tile(owner.id(), &destination_id, tile);
        (
            owner,
            PresentationSurfaceViewSetUpdate {
                surface_instance_id: source_id,
                view_sets: source_sets,
                active_view_set: source_active,
                bindings: source_bindings,
            },
            PresentationSurfaceViewSetUpdate {
                surface_instance_id: destination_id,
                view_sets: destination_sets,
                active_view_set: destination_active,
                bindings: destination_bindings,
            },
            old_key,
            new_key,
        )
    }

    #[test]
    fn two_surface_inventory_update_publishes_exact_move_and_binding_once() {
        let (mut owner, source, destination, old_key, new_key) = two_surface_move_fixture();
        let expected_binding = owner.mount(&old_key).unwrap().binding().clone();
        let active = owner.active_surface().unwrap().id().clone();
        owner
            .replace_two_surface_view_sets(source.clone(), destination.clone())
            .unwrap();
        assert!(owner.mount(&old_key).is_none());
        assert_eq!(owner.mount(&new_key).unwrap().binding(), &expected_binding);
        assert_eq!(list_cursor(owner.mount(&new_key).unwrap().local()), 11);
        assert_eq!(owner.active_surface().unwrap().id(), &active);
        assert_eq!(
            owner.surfaces[&source.surface_instance_id].active_view_set,
            source.active_view_set
        );
        assert_eq!(
            owner.surfaces[&destination.surface_instance_id].active_view_set,
            destination.active_view_set
        );
    }

    #[test]
    fn two_surface_inventory_invalid_second_side_never_commits_first_side() {
        let (mut owner, source, destination, old_key, new_key) = two_surface_move_fixture();
        let before = format!("{owner:?}");
        let mut invalid = destination.clone();
        invalid.bindings.clear();
        assert!(
            owner
                .replace_two_surface_view_sets(source.clone(), invalid)
                .is_err()
        );
        assert_eq!(format!("{owner:?}"), before);
        assert!(owner.mount(&old_key).is_some());
        assert!(owner.mount(&new_key).is_none());
        let mut invalid = destination.clone();
        invalid.active_view_set = None;
        assert!(
            owner
                .replace_two_surface_view_sets(source.clone(), invalid)
                .is_err()
        );
        assert_eq!(format!("{owner:?}"), before);
        let mut same_owner = destination.clone();
        same_owner.surface_instance_id = source.surface_instance_id.clone();
        assert!(
            owner
                .replace_two_surface_view_sets(source.clone(), same_owner)
                .is_err()
        );
        assert_eq!(format!("{owner:?}"), before);
        let mut missing_owner = destination;
        missing_owner.surface_instance_id =
            SurfaceInstanceId::try_new("00000000-0000-4000-8000-000000000004").unwrap();
        assert!(
            owner
                .replace_two_surface_view_sets(source, missing_owner)
                .is_err()
        );
        assert_eq!(format!("{owner:?}"), before);
    }
}

#[derive(Debug, Clone)]
pub struct PresentationViewMount {
    instance_key: RyeOsViewInstanceKey,
    view_ref: String,
    binding: UiBindingAttachmentCoordinate,
    pub local: ViewLocalState,
}

impl PresentationViewMount {
    pub fn instance_key(&self) -> &RyeOsViewInstanceKey {
        &self.instance_key
    }
    pub fn view_ref(&self) -> &str {
        &self.view_ref
    }
    pub fn binding(&self) -> &UiBindingAttachmentCoordinate {
        &self.binding
    }
}

#[derive(Debug, Clone)]
pub struct SharedDockMount {
    view: PresentationViewMount,
    pub visible: bool,
    pub size: u16,
}

impl SharedDockMount {
    pub fn view(&self) -> &PresentationViewMount {
        &self.view
    }
    pub fn local_mut(&mut self) -> &mut ViewLocalState {
        &mut self.view.local
    }
}

#[derive(Debug, Clone)]
pub struct DialogMount {
    view: PresentationViewMount,
    title: String,
    open: bool,
    insertion: Option<PresentationInsertionDestination>,
    context_origin: Option<RyeOsViewInstanceKey>,
    /// None for a Config-owned shared mount; otherwise its exact live instance.
    surface_owner: Option<SurfaceInstanceId>,
}

impl DialogMount {
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn view(&self) -> &PresentationViewMount {
        &self.view
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn local_mut(&mut self) -> &mut ViewLocalState {
        &mut self.view.local
    }
    pub fn insertion(&self) -> Option<&PresentationInsertionDestination> {
        self.insertion.as_ref()
    }
    pub fn context_origin(&self) -> Option<&RyeOsViewInstanceKey> {
        self.context_origin.as_ref()
    }
    pub fn surface_owner(&self) -> Option<&SurfaceInstanceId> {
        self.surface_owner.as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct SurfaceComposition {
    allocation_interval: (u64, u64),
    id: SurfaceInstanceId,
    ordinal: u64,
    title: String,
    binding: UiBindingAttachmentCoordinate,
    view_sets: Vec<ViewSet>,
    active_view_set: Option<ViewSetId>,
    mount_bindings: BTreeMap<RyeOsViewInstanceKey, UiBindingAttachmentCoordinate>,
    /// Immutable admission provenance, not another selection/layout owner.
    /// Seat facets override this baseline when the operator changes subject.
    admitted_saved_tabs: BTreeMap<ViewSetId, AdmittedSavedSurfaceTab>,
}

/// Borrowed projection of the single canonical mounted payload. Not a second
/// writable composition and not an authority admission result.
pub enum MountRef<'a> {
    SharedDock {
        edge: &'a str,
        mount: &'a SharedDockMount,
    },
    SharedDialog {
        id: &'a str,
        mount: &'a DialogMount,
    },
    SurfaceDialog {
        surface_id: &'a SurfaceInstanceId,
        mount: &'a DialogMount,
    },
    SurfaceTile {
        surface_id: &'a SurfaceInstanceId,
        view_set_id: ViewSetId,
        tile_id: TileId,
        tile: &'a TileState,
        binding: &'a UiBindingAttachmentCoordinate,
        visible: bool,
    },
}

impl<'a> MountRef<'a> {
    pub fn instance_key(&self) -> &'a RyeOsViewInstanceKey {
        match self {
            Self::SharedDock { mount, .. } => mount.view.instance_key(),
            Self::SharedDialog { mount, .. } | Self::SurfaceDialog { mount, .. } => {
                mount.view.instance_key()
            }
            Self::SurfaceTile { tile, .. } => &tile.instance_key,
        }
    }
    pub fn view_ref(&self) -> &'a str {
        match self {
            Self::SharedDock { mount, .. } => mount.view.view_ref(),
            Self::SharedDialog { mount, .. } | Self::SurfaceDialog { mount, .. } => {
                mount.view.view_ref()
            }
            Self::SurfaceTile { tile, .. } => &tile.view.view_ref,
        }
    }
    pub fn binding(&self) -> &'a UiBindingAttachmentCoordinate {
        match self {
            Self::SharedDock { mount, .. } => mount.view.binding(),
            Self::SharedDialog { mount, .. } | Self::SurfaceDialog { mount, .. } => {
                mount.view.binding()
            }
            Self::SurfaceTile { binding, .. } => binding,
        }
    }
    pub fn local(&self) -> &'a ViewLocalState {
        match self {
            Self::SharedDock { mount, .. } => &mount.view.local,
            Self::SharedDialog { mount, .. } | Self::SurfaceDialog { mount, .. } => {
                &mount.view.local
            }
            Self::SurfaceTile { tile, .. } => &tile.local,
        }
    }
    pub fn visible(&self) -> bool {
        match self {
            Self::SharedDock { mount, .. } => mount.visible,
            Self::SharedDialog { mount, .. } | Self::SurfaceDialog { mount, .. } => mount.open,
            Self::SurfaceTile { visible, .. } => *visible,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AdmittedSavedSurfaceTab {
    Particular(crate::ui::binding::UiSavedSurfaceTab),
    Reusable(crate::ui::binding::UiReusableSurfaceTab),
}
impl AdmittedSavedSurfaceTab {
    pub fn particular(&self) -> Option<&crate::ui::binding::UiSavedSurfaceTab> {
        match self {
            Self::Particular(tab) => Some(tab),
            Self::Reusable(_) => None,
        }
    }
    pub fn reusable(&self) -> Option<&crate::ui::binding::UiReusableSurfaceTab> {
        match self {
            Self::Reusable(tab) => Some(tab),
            Self::Particular(_) => None,
        }
    }
    pub fn binding(&self) -> &crate::ui::binding::UiBindingAttachmentCoordinate {
        match self {
            Self::Particular(tab) => &tab.binding,
            Self::Reusable(tab) => &tab.binding,
        }
    }
    pub fn saved_set_id(&self) -> &str {
        match self {
            Self::Particular(tab) => &tab.saved_set_id,
            Self::Reusable(tab) => &tab.saved_set_id,
        }
    }
    pub fn template(&self) -> crate::surface::view_sets::SavedViewSetTemplate {
        match self {
            Self::Particular(tab) => crate::surface::view_sets::SavedViewSetTemplate {
                id: tab.resolved.id.clone(),
                name: tab.resolved.name.clone(),
                composition: tab.resolved.composition.clone(),
                relationships: tab.resolved.relationships.clone(),
            },
            Self::Reusable(tab) => tab.template.clone(),
        }
    }
}
impl SurfaceComposition {
    pub fn admitted_saved_tab(&self, id: ViewSetId) -> Option<&AdmittedSavedSurfaceTab> {
        self.view_sets
            .iter()
            .any(|set| set.id == id)
            .then(|| self.admitted_saved_tabs.get(&id))
            .flatten()
    }
    pub fn id(&self) -> &SurfaceInstanceId {
        &self.id
    }
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn binding(&self) -> &UiBindingAttachmentCoordinate {
        &self.binding
    }
    pub fn view_sets(&self) -> &[ViewSet] {
        &self.view_sets
    }
    pub fn active_view_set(&self) -> Option<&ViewSet> {
        let id = self.active_view_set?;
        self.view_sets.iter().find(|set| set.id == id)
    }
    pub fn active_view_set_mut(&mut self) -> Option<&mut ViewSet> {
        let id = self.active_view_set?;
        self.view_sets.iter_mut().find(|set| set.id == id)
    }
    pub fn select_view_set(&mut self, id: ViewSetId) -> Result<(), String> {
        if !self.view_sets.iter().any(|set| set.id == id) {
            return Err("selected tab is not a member of this Surface".into());
        }
        self.active_view_set = Some(id);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PresentationComposition {
    presentation_id: PresentationId,
    root_binding: UiBindingAttachmentCoordinate,
    docks: BTreeMap<String, SharedDockMount>,
    dialogs: BTreeMap<String, DialogMount>,
    launcher: String,
    surfaces: BTreeMap<SurfaceInstanceId, SurfaceComposition>,
    active_surface: Option<SurfaceInstanceId>,
}

fn coordinate(descriptor: &UiBindingAttachment) -> UiBindingAttachmentCoordinate {
    UiBindingAttachmentCoordinate {
        binding_attachment_id: descriptor.binding_attachment_id.clone(),
        binding_generation: descriptor.binding_generation,
        binding_digest: descriptor.binding_digest.clone(),
    }
}

// Wire shape only; daemon uses its canonical UUID owner and retained membership.
fn uuid_shape(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
}

impl PresentationComposition {
    /// Stage tab payload and all exact mount coordinates before publication.
    /// Limits and admission are supplied by the existing caller, never inferred
    /// from a root attachment or renderer-selected policy.
    fn prepare_tabs(
        &self,
        surface: &SurfaceInstanceId,
        mut sets: Vec<ViewSet>,
        bindings: &BTreeMap<TileId, UiBindingAttachmentCoordinate>,
    ) -> Result<
        (
            Vec<ViewSet>,
            BTreeMap<RyeOsViewInstanceKey, UiBindingAttachmentCoordinate>,
        ),
        String,
    > {
        let mut set_ids = std::collections::BTreeSet::new();
        let mut tile_ids = std::collections::BTreeSet::new();
        let mut coordinates = BTreeMap::new();
        for set in &mut sets {
            if !set_ids.insert(set.id) {
                return Err("duplicate tab identity".into());
            }
            if let Some(root) = &set.root {
                root.validate().map_err(str::to_owned)?;
                let layout_tiles: std::collections::BTreeSet<_> =
                    root.tile_ids().into_iter().collect();
                if layout_tiles != set.tiles.keys().copied().collect() {
                    return Err("tab tree and mounted tiles disagree".into());
                }
            } else if !set.tiles.is_empty() {
                return Err("mounted tiles require a layout tree".into());
            }
            for (id, tile) in &mut set.tiles {
                if !tile_ids.insert(*id) {
                    return Err("duplicate tile identity within Surface".into());
                }
                let binding = bindings
                    .get(id)
                    .ok_or("tile has no exact admitted attachment")?;
                if binding.binding_attachment_id.is_empty()
                    || binding.binding_generation == 0
                    || binding.binding_digest.is_empty()
                {
                    return Err("tile attachment coordinate is incomplete".into());
                }
                tile.instance_key =
                    RyeOsViewInstanceKey::surface_tile(&self.presentation_id, surface, *id);
                coordinates.insert(tile.instance_key.clone(), binding.clone());
            }
        }
        if bindings.len() != tile_ids.len() {
            return Err("attachment coordinates contain unmounted tiles".into());
        }
        Ok((sets, coordinates))
    }

    /// Replace only the selected Surface's tab composition atomically. This
    /// does not stop executions or modify any shared mount.
    pub fn replace_view_sets(
        &mut self,
        surface: &SurfaceInstanceId,
        sets: Vec<ViewSet>,
        active: Option<ViewSetId>,
        bindings: BTreeMap<TileId, UiBindingAttachmentCoordinate>,
    ) -> Result<(), String> {
        if !self.surfaces.contains_key(surface) {
            return Err("Surface instance is not mounted".into());
        }
        if active.is_some_and(|id| !sets.iter().any(|set| set.id == id))
            || (active.is_none() && !sets.is_empty())
        {
            return Err("active tab contradicts replacement composition".into());
        }
        let (sets, coordinates) = self.prepare_tabs(surface, sets, &bindings)?;
        let owner = self
            .surfaces
            .get_mut(surface)
            .expect("membership checked before staging");
        owner.view_sets = sets;
        owner.mount_bindings = coordinates;
        owner.active_view_set = active;
        let live_sets = owner
            .view_sets
            .iter()
            .map(|set| set.id)
            .collect::<std::collections::BTreeSet<_>>();
        owner
            .admitted_saved_tabs
            .retain(|id, _| live_sets.contains(id));
        for dialog in self.dialogs.values_mut() {
            if dialog.insertion.as_ref().is_some_and(|destination| {
                &destination.surface_instance_id == surface
                    && !live_sets.contains(&destination.view_set_id)
            }) {
                dialog.insertion = None;
            }
        }
        Ok(())
    }

    /// Publish two distinct mounted Surface inventories together. Both exact
    /// attachment maps and layout trees validate on a private composition;
    /// failure on either side leaves every mount and dialog context unchanged.
    /// Active Surface selection remains with the existing caller.
    pub fn replace_two_surface_view_sets(
        &mut self,
        source: PresentationSurfaceViewSetUpdate,
        destination: PresentationSurfaceViewSetUpdate,
    ) -> Result<(), String> {
        if source.surface_instance_id == destination.surface_instance_id {
            return Err("two-Surface replacement requires distinct exact owners".into());
        }
        if !self.surfaces.contains_key(&source.surface_instance_id)
            || !self.surfaces.contains_key(&destination.surface_instance_id)
        {
            return Err("two-Surface replacement owner is not mounted".into());
        }
        let mut staged = self.clone();
        staged.replace_view_sets(
            &source.surface_instance_id,
            source.view_sets,
            source.active_view_set,
            source.bindings,
        )?;
        staged.replace_view_sets(
            &destination.surface_instance_id,
            destination.view_sets,
            destination.active_view_set,
            destination.bindings,
        )?;
        *self = staged;
        Ok(())
    }

    fn tile_bindings(
        &self,
        surface: &SurfaceInstanceId,
    ) -> Result<BTreeMap<TileId, UiBindingAttachmentCoordinate>, String> {
        let owner = self
            .surfaces
            .get(surface)
            .ok_or("Surface instance is not mounted")?;
        owner
            .view_sets
            .iter()
            .flat_map(|set| set.tiles.iter())
            .map(|(id, tile)| {
                owner
                    .mount_bindings
                    .get(&tile.instance_key)
                    .cloned()
                    .map(|binding| (*id, binding))
                    .ok_or_else(|| "mounted tile has no exact attachment".into())
            })
            .collect()
    }

    pub fn insert_view_set(
        &mut self,
        surface: &SurfaceInstanceId,
        set: ViewSet,
        index: usize,
        bindings: BTreeMap<TileId, UiBindingAttachmentCoordinate>,
        select: bool,
    ) -> Result<(), String> {
        let owner = self
            .surfaces
            .get(surface)
            .ok_or("Surface instance is not mounted")?;
        if index > owner.view_sets.len()
            || owner.view_sets.iter().any(|existing| existing.id == set.id)
        {
            return Err("invalid tab insertion position or identity".into());
        }
        let active = if select || owner.active_view_set.is_none() {
            Some(set.id)
        } else {
            owner.active_view_set
        };
        let mut sets = owner.view_sets.clone();
        let mut exact = self.tile_bindings(surface)?;
        for (tile, binding) in bindings {
            if exact.insert(tile, binding).is_some() {
                return Err("inserted tab collides with mounted tile identity".into());
            }
        }
        sets.insert(index, set);
        self.replace_view_sets(surface, sets, active, exact)
    }

    pub fn close_view_set(
        &mut self,
        surface: &SurfaceInstanceId,
        id: ViewSetId,
    ) -> Result<ViewSet, String> {
        let owner = self
            .surfaces
            .get(surface)
            .ok_or("Surface instance is not mounted")?;
        let index = owner
            .view_sets
            .iter()
            .position(|set| set.id == id)
            .ok_or("tab is not mounted")?;
        let mut sets = owner.view_sets.clone();
        let removed = sets.remove(index);
        let active = if owner.active_view_set == Some(id) {
            sets.get(index.min(sets.len().saturating_sub(1)))
                .map(|set| set.id)
        } else {
            owner.active_view_set
        };
        let mut bindings = self.tile_bindings(surface)?;
        for tile in removed.tiles.keys() {
            bindings.remove(tile);
        }
        self.replace_view_sets(surface, sets, active, bindings)?;
        for dialog in self.dialogs.values_mut() {
            if dialog.insertion.as_ref().is_some_and(|destination| {
                &destination.surface_instance_id == surface && destination.view_set_id == id
            }) {
                dialog.insertion = None;
            }
        }
        Ok(removed)
    }

    pub fn reorder_view_set(
        &mut self,
        surface: &SurfaceInstanceId,
        id: ViewSetId,
        index: usize,
    ) -> Result<(), String> {
        let owner = self
            .surfaces
            .get_mut(surface)
            .ok_or("Surface instance is not mounted")?;
        if index >= owner.view_sets.len() {
            return Err("tab reorder position is out of range".into());
        }
        let old = owner
            .view_sets
            .iter()
            .position(|set| set.id == id)
            .ok_or("tab is not mounted")?;
        let set = owner.view_sets.remove(old);
        owner.view_sets.insert(index, set);
        Ok(())
    }

    pub fn select_view_set(
        &mut self,
        surface: &SurfaceInstanceId,
        id: ViewSetId,
    ) -> Result<(), String> {
        self.surfaces
            .get_mut(surface)
            .ok_or("Surface instance is not mounted")?
            .select_view_set(id)
    }

    /// Layout edits stage on a temporary payload. New/rebound tile coordinates
    /// must be explicitly supplied; retained tile coordinates stay unchanged.
    pub fn mutate_view_set<R>(
        &mut self,
        surface: &SurfaceInstanceId,
        id: ViewSetId,
        admitted: BTreeMap<TileId, UiBindingAttachmentCoordinate>,
        mutate: impl FnOnce(&mut ViewSet) -> Result<R, String>,
    ) -> Result<R, String> {
        let owner = self
            .surfaces
            .get(surface)
            .ok_or("Surface instance is not mounted")?;
        let mut sets = owner.view_sets.clone();
        let target = sets
            .iter_mut()
            .find(|set| set.id == id)
            .ok_or("tab is not mounted")?;
        let result = mutate(target)?;
        if target.id != id {
            return Err("layout mutation cannot replace tab identity".into());
        }
        if admitted.keys().any(|tile| !target.tiles.contains_key(tile)) {
            return Err("supplied attachment is outside the edited tab".into());
        }
        let active = owner.active_view_set;
        let mut bindings = self.tile_bindings(surface)?;
        bindings.extend(admitted);
        bindings.retain(|tile, _| sets.iter().any(|set| set.tiles.contains_key(tile)));
        self.replace_view_sets(surface, sets, active, bindings)?;
        Ok(result)
    }

    /// Stage an ordinary same-Surface move across two tabs. Both trees and
    /// the selected tab publish together; the mounted tile's binding remains
    /// keyed by its unchanged Surface-scoped instance identity.
    pub fn mutate_two_view_sets<R>(
        &mut self,
        surface: &SurfaceInstanceId,
        source_id: ViewSetId,
        destination_id: ViewSetId,
        mutate: impl FnOnce(&mut ViewSet, &mut ViewSet) -> Result<R, String>,
    ) -> Result<R, String> {
        if source_id == destination_id {
            return Err("two-tab mutation requires distinct tabs".into());
        }
        let owner = self
            .surfaces
            .get(surface)
            .ok_or("Surface instance is not mounted")?;
        let mut sets = owner.view_sets.clone();
        let source_index = sets
            .iter()
            .position(|set| set.id == source_id)
            .ok_or("source tab is not mounted")?;
        let destination_index = sets
            .iter()
            .position(|set| set.id == destination_id)
            .ok_or("destination tab is not mounted")?;
        let result = if source_index < destination_index {
            let (left, right) = sets.split_at_mut(destination_index);
            mutate(&mut left[source_index], &mut right[0])?
        } else {
            let (left, right) = sets.split_at_mut(source_index);
            mutate(&mut right[0], &mut left[destination_index])?
        };
        if sets[source_index].id != source_id || sets[destination_index].id != destination_id {
            return Err("two-tab mutation changed a tab identity".into());
        }
        let bindings = self.tile_bindings(surface)?;
        self.replace_view_sets(surface, sets, Some(destination_id), bindings)?;
        Ok(result)
    }

    pub fn from_membership(
        membership: &UiPresentationMembership,
        descriptor: &UiBindingAttachment,
    ) -> Result<Self, String> {
        descriptor.validate().map_err(str::to_owned)?;
        if !uuid_shape(&membership.presentation_id)
            || membership.root_binding != coordinate(descriptor)
        {
            return Err("presentation membership contradicts its root coordinate".into());
        }
        let UiCompositionDefinition::SharedPresentation { config } =
            &descriptor.presentation.definition
        else {
            return Err("presentation requires an admitted Shared Config root".into());
        };
        let presentation_id =
            PresentationId::try_new(membership.presentation_id.clone()).map_err(str::to_owned)?;
        if membership.next_mount_id == 0 {
            return Err("presentation allocation frontier is invalid".into());
        }
        let mut intervals = Vec::new();
        for member in &membership.surfaces {
            let manifest = &member.mount_manifest;
            manifest.validate()?;
            if manifest.presentation_id != presentation_id
                || manifest.surface_instance_id.as_str() != member.surface_instance_id
                || manifest.next_mount_id > membership.next_mount_id
            {
                return Err(
                    "retained mount manifest contradicts presentation frontier or owner".into(),
                );
            }
            intervals.push((manifest.allocation_start, manifest.next_mount_id));
        }
        intervals.sort_unstable();
        if intervals.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err("retained mount allocation intervals overlap".into());
        }
        let root_binding = membership.root_binding.clone();
        let spec = &config.shared_presentation;
        if spec.schema != crate::presentation::SHARED_PRESENTATION_SCHEMA {
            return Err("shared presentation schema is not current".into());
        }
        let mut dialogs = BTreeMap::new();
        for dialog in &spec.dialogs {
            if !descriptor.presentation.views.contains_key(&dialog.view.0) {
                return Err("shared dialog View is absent from its admitted closure".into());
            }
            let mount = DialogMount {
                view: PresentationViewMount {
                    instance_key: RyeOsViewInstanceKey::shared_dialog(&presentation_id, &dialog.id)
                        .map_err(str::to_owned)?,
                    view_ref: dialog.view.0.clone(),
                    binding: root_binding.clone(),
                    local: ViewLocalState::default(),
                },
                title: dialog.title.clone(),
                open: false,
                insertion: None,
                context_origin: None,
                surface_owner: None,
            };
            if dialogs.insert(dialog.id.clone(), mount).is_some() {
                return Err("shared dialog mount id is duplicated".into());
            }
        }
        if !dialogs.contains_key(&spec.launcher) {
            return Err("shared launcher is not a mounted dialog".into());
        }
        let mut docks = BTreeMap::new();
        for (edge, slot) in [
            ("top", &spec.docks.top),
            ("left", &spec.docks.left),
            ("right", &spec.docks.right),
            ("bottom", &spec.docks.bottom),
        ] {
            if let Some(slot) = slot {
                let crate::surface::SlotContentSpec::View(view_ref) = &slot.content;
                if slot.size == 0 || !descriptor.presentation.views.contains_key(view_ref) {
                    return Err("shared dock shape or admitted View closure is invalid".into());
                }
                docks.insert(
                    edge.into(),
                    SharedDockMount {
                        view: PresentationViewMount {
                            instance_key: RyeOsViewInstanceKey::shared_dock(&presentation_id, edge)
                                .map_err(str::to_owned)?,
                            view_ref: view_ref.clone(),
                            binding: root_binding.clone(),
                            local: ViewLocalState::default(),
                        },
                        visible: slot.open,
                        size: slot.size,
                    },
                );
            }
        }
        // Surface projections are admitted separately, never fabricated from
        // membership hints or the shared Config's initial opening selections.
        Ok(Self {
            presentation_id,
            root_binding,
            docks,
            dialogs,
            launcher: spec.launcher.clone(),
            surfaces: BTreeMap::new(),
            active_surface: None,
        })
    }

    pub fn id(&self) -> &PresentationId {
        &self.presentation_id
    }
    pub fn root_binding(&self) -> &UiBindingAttachmentCoordinate {
        &self.root_binding
    }
    pub fn docks(&self) -> &BTreeMap<String, SharedDockMount> {
        &self.docks
    }
    pub fn dialogs(&self) -> &BTreeMap<String, DialogMount> {
        &self.dialogs
    }
    pub fn launcher(&self) -> &str {
        &self.launcher
    }
    pub fn dock_mut(&mut self, edge: &str) -> Option<&mut SharedDockMount> {
        self.docks.get_mut(edge)
    }
    /// Pure structural removal for an exactly authenticated transfer commit.
    /// The caller must check the exported mount before invoking this method.
    pub fn remove_shared_dock(&mut self, edge: &str) -> Option<SharedDockMount> {
        self.docks.remove(edge)
    }
    /// Install a precisely allocated shared dock in a staged composition.
    /// Admission and export/descriptor checks belong to the transfer reducer.
    pub fn install_shared_dock(
        &mut self,
        edge: &str,
        view_ref: String,
        binding: UiBindingAttachmentCoordinate,
        local: ViewLocalState,
        visible: bool,
        size: u16,
    ) -> Result<(), String> {
        if !matches!(edge, "top" | "bottom" | "left" | "right")
            || size == 0
            || self.docks.contains_key(edge)
        {
            return Err("transfer shared dock placement is occupied or invalid".into());
        }
        let instance_key = RyeOsViewInstanceKey::shared_dock(&self.presentation_id, edge)
            .map_err(str::to_owned)?;
        self.docks.insert(
            edge.to_owned(),
            SharedDockMount {
                view: PresentationViewMount {
                    instance_key,
                    view_ref,
                    binding,
                    local,
                },
                visible,
                size,
            },
        );
        Ok(())
    }
    pub fn dialog_mut(&mut self, id: &str) -> Option<&mut DialogMount> {
        self.dialogs.get_mut(id)
    }
    pub fn dialog_available(&self, mount: &DialogMount) -> bool {
        mount
            .surface_owner
            .as_ref()
            .is_none_or(|id| self.active_surface.as_ref() == Some(id))
    }
    /// Exact projected mount keys are never retargeted. An authored local ID
    /// resolves only inside the invoking live Surface, then the shared Config.
    /// An inactive or missing origin cannot borrow the currently selected one.
    pub fn resolve_dialog<'a>(
        &'a self,
        id: &str,
        origin: Option<&RyeOsViewInstanceKey>,
    ) -> Option<(&'a str, &'a DialogMount)> {
        if id.starts_with("seat:") {
            return self
                .dialogs
                .iter()
                .find(|(_, mount)| {
                    mount.view.instance_key().as_str() == id && self.dialog_available(mount)
                })
                .map(|(key, mount)| (key.as_str(), mount));
        }
        if let Some(origin) = origin {
            let mount = self.mount(origin)?;
            if !mount.visible() {
                return None;
            }
            if let Some(crate::ids::UiViewMountOwner::Surface {
                surface_instance_id,
                ..
            }) = origin.mount_owner()
            {
                let key = RyeOsViewInstanceKey::surface_dialog(
                    &self.presentation_id,
                    &surface_instance_id,
                    id,
                )
                .ok()?;
                if let Some((key, mount)) = self.dialogs.get_key_value(key.as_str()) {
                    return self
                        .dialog_available(mount)
                        .then_some((key.as_str(), mount));
                }
            }
        }
        self.dialogs
            .get_key_value(id)
            .filter(|(_, mount)| mount.surface_owner.is_none())
            .map(|(key, mount)| (key.as_str(), mount))
    }
    /// One ordinary dialog frame is active at a time; mounted state is retained.
    pub fn open_dialog(
        &mut self,
        id: &str,
        destination: Option<PresentationInsertionDestination>,
    ) -> Result<(), String> {
        let mount = self.dialogs.get(id).ok_or("dialog is not mounted")?;
        if !self.dialog_available(mount)
            || mount.surface_owner.as_ref().is_some_and(|owner| {
                destination
                    .as_ref()
                    .is_some_and(|target| &target.surface_instance_id != owner)
            })
        {
            return Err("contextual dialog belongs to another Surface instance".into());
        }
        self.set_dialog_insertion(id, destination)?;
        for (mount_id, dialog) in &mut self.dialogs {
            dialog.open = mount_id == id;
        }
        Ok(())
    }
    pub(crate) fn open_dialog_with_context(
        &mut self,
        id: &str,
        destination: Option<PresentationInsertionDestination>,
        context_origin: Option<RyeOsViewInstanceKey>,
    ) -> Result<(), String> {
        if context_origin
            .as_ref()
            .is_some_and(|key| self.mount(key).is_none())
        {
            return Err("dialog context origin is no longer mounted".into());
        }
        let mount = self.dialogs.get(id).ok_or("dialog is not mounted")?;
        if !self.dialog_available(mount)
            || mount.surface_owner.as_ref().is_some_and(|owner| {
                destination
                    .as_ref()
                    .is_some_and(|target| &target.surface_instance_id != owner)
            })
        {
            return Err("contextual dialog belongs to another Surface instance".into());
        }
        self.set_dialog_insertion(id, destination)?;
        for (mount_id, dialog) in &mut self.dialogs {
            dialog.open = mount_id == id;
        }
        self.dialogs
            .get_mut(id)
            .expect("validated dialog remains mounted")
            .context_origin = context_origin;
        Ok(())
    }
    pub fn close_dialog(&mut self, instance: &RyeOsViewInstanceKey) -> bool {
        let Some(dialog) = self
            .dialogs
            .values_mut()
            .find(|dialog| dialog.open && dialog.view.instance_key() == instance)
        else {
            return false;
        };
        dialog.open = false;
        true
    }
    pub fn surfaces(&self) -> &BTreeMap<SurfaceInstanceId, SurfaceComposition> {
        &self.surfaces
    }
    pub fn ordered_surfaces(&self) -> impl Iterator<Item = &SurfaceComposition> {
        let mut surfaces: Vec<_> = self.surfaces.values().collect();
        surfaces.sort_by_key(|surface| surface.ordinal);
        surfaces.into_iter()
    }
    pub fn view_sets(&self) -> impl Iterator<Item = (&SurfaceInstanceId, &ViewSet)> {
        self.ordered_surfaces()
            .flat_map(|surface| surface.view_sets.iter().map(move |set| (&surface.id, set)))
    }
    pub fn view_set(&self, surface: &SurfaceInstanceId, set: ViewSetId) -> Option<&ViewSet> {
        self.surfaces
            .get(surface)?
            .view_sets
            .iter()
            .find(|candidate| candidate.id == set)
    }
    pub fn view_set_mut(
        &mut self,
        surface: &SurfaceInstanceId,
        set: ViewSetId,
    ) -> Option<&mut ViewSet> {
        self.surfaces
            .get_mut(surface)?
            .view_sets
            .iter_mut()
            .find(|candidate| candidate.id == set)
    }
    pub fn view_set_for_instance(
        &self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<(&SurfaceInstanceId, &ViewSet)> {
        self.view_sets().find(|(_, set)| {
            set.tiles
                .values()
                .any(|tile| &tile.instance_key == instance)
        })
    }
    pub fn mount(&self, instance: &RyeOsViewInstanceKey) -> Option<MountRef<'_>> {
        for (edge, mount) in &self.docks {
            if mount.view.instance_key() == instance {
                return Some(MountRef::SharedDock { edge, mount });
            }
        }
        for (id, mount) in &self.dialogs {
            if mount.view.instance_key() == instance {
                if let Some(surface_id) = mount.surface_owner.as_ref() {
                    self.surfaces.get(surface_id)?;
                    return Some(MountRef::SurfaceDialog { surface_id, mount });
                }
                return Some(MountRef::SharedDialog { id, mount });
            }
        }
        for surface in self.surfaces.values() {
            for set in &surface.view_sets {
                for (tile_id, tile) in &set.tiles {
                    if &tile.instance_key == instance {
                        if tile.instance_key
                            != RyeOsViewInstanceKey::surface_tile(
                                &self.presentation_id,
                                &surface.id,
                                *tile_id,
                            )
                        {
                            return None;
                        }
                        let visible = self.active_surface.as_ref() == Some(&surface.id)
                            && surface.active_view_set == Some(set.id)
                            && set
                                .root
                                .as_ref()
                                .is_some_and(|root| root.active_tile_ids().contains(tile_id));
                        return Some(MountRef::SurfaceTile {
                            surface_id: &surface.id,
                            view_set_id: set.id,
                            tile_id: *tile_id,
                            tile,
                            binding: surface.mount_bindings.get(instance)?,
                            visible,
                        });
                    }
                }
            }
        }
        None
    }
    pub fn mount_local_mut(
        &mut self,
        instance: &RyeOsViewInstanceKey,
    ) -> Option<&mut ViewLocalState> {
        for mount in self.docks.values_mut() {
            if mount.view.instance_key() == instance {
                return Some(&mut mount.view.local);
            }
        }
        for mount in self.dialogs.values_mut() {
            if mount.view.instance_key() == instance {
                return Some(&mut mount.view.local);
            }
        }
        for surface in self.surfaces.values_mut() {
            for set in &mut surface.view_sets {
                for tile in set.tiles.values_mut() {
                    if &tile.instance_key == instance {
                        return Some(&mut tile.local);
                    }
                }
            }
        }
        None
    }
    pub fn active_surface(&self) -> Option<&SurfaceComposition> {
        self.surfaces.get(self.active_surface.as_ref()?)
    }
    pub fn active_surface_mut(&mut self) -> Option<&mut SurfaceComposition> {
        self.surfaces.get_mut(self.active_surface.as_ref()?)
    }
    pub fn active_view_set(&self) -> Option<&ViewSet> {
        self.active_surface()?.active_view_set()
    }
    pub fn active_view_set_mut(&mut self) -> Option<&mut ViewSet> {
        self.active_surface_mut()?.active_view_set_mut()
    }

    pub fn set_dialog_insertion(
        &mut self,
        dialog_id: &str,
        destination: Option<PresentationInsertionDestination>,
    ) -> Result<(), String> {
        if let Some(destination) = &destination {
            let surface = self
                .surfaces
                .get(&destination.surface_instance_id)
                .ok_or("insertion Surface is not mounted")?;
            if !surface
                .view_sets
                .iter()
                .any(|set| set.id == destination.view_set_id)
            {
                return Err("insertion tab is not a member of the selected Surface".into());
            }
        }
        self.dialogs
            .get_mut(dialog_id)
            .ok_or("dialog is not mounted")?
            .insertion = destination;
        Ok(())
    }

    pub fn open_surface(
        &mut self,
        membership: &UiSurfaceMembership,
        descriptor: &UiBindingAttachment,
    ) -> Result<(), String> {
        self.open_surface_with_particular(membership, descriptor, None)
    }

    pub fn open_particular_surface(
        &mut self,
        membership: &UiSurfaceMembership,
        descriptor: &UiBindingAttachment,
        resolved: &crate::surface::view_sets::ResolvedParticularViewSet,
    ) -> Result<(), String> {
        if membership.saved_surface.is_some() {
            return Err("single particular admission contains saved-Surface metadata".into());
        }
        let template = crate::surface::view_sets::SavedViewSetTemplate {
            id: resolved.id.clone(),
            name: resolved.name.clone(),
            composition: resolved.composition.clone(),
            relationships: resolved.relationships.clone(),
        };
        crate::surface::view_sets::validate_saved_view_set_templates(std::slice::from_ref(
            &template,
        ))?;
        self.open_surface_with_particular(membership, descriptor, Some(resolved))
    }

    fn open_surface_with_particular(
        &mut self,
        membership: &UiSurfaceMembership,
        descriptor: &UiBindingAttachment,
        particular: Option<&crate::surface::view_sets::ResolvedParticularViewSet>,
    ) -> Result<(), String> {
        descriptor.validate().map_err(str::to_owned)?;
        if !uuid_shape(&membership.surface_instance_id)
            || membership.ordinal == 0
            || membership.binding != coordinate(descriptor)
        {
            return Err("Surface membership contradicts its exact attachment".into());
        }
        let id = SurfaceInstanceId::try_new(membership.surface_instance_id.clone())
            .map_err(str::to_owned)?;
        if self.surfaces.contains_key(&id)
            || self
                .surfaces
                .values()
                .any(|surface| surface.ordinal == membership.ordinal)
        {
            return Err("Surface instance identity or ordinal is already mounted".into());
        }
        let surface = descriptor
            .presentation
            .definition
            .surface()
            .ok_or("Surface membership cannot mount a Config root")?;
        let manifest = &membership.mount_manifest;
        manifest.validate()?;
        if self.surfaces.values().any(|surface| {
            manifest.allocation_start < surface.allocation_interval.1
                && surface.allocation_interval.0 < manifest.next_mount_id
        }) {
            return Err("mount allocation interval overlaps an existing Surface".into());
        }
        if manifest.presentation_id != self.presentation_id || manifest.surface_instance_id != id {
            return Err("mount manifest owner differs from Surface membership".into());
        }
        let expected = if let Some(resolved) = particular {
            vec![(resolved.composition.clone(), membership.binding.clone())]
        } else if let Some(saved) = &membership.saved_surface {
            match &saved.body {
                crate::ui::binding::UiSavedSurfaceCompositionBody::Particular { tabs } => tabs
                    .iter()
                    .map(|tab| (tab.resolved.composition.clone(), tab.binding.clone()))
                    .collect(),
                crate::ui::binding::UiSavedSurfaceCompositionBody::Reusable { tabs }
                | crate::ui::binding::UiSavedSurfaceCompositionBody::Composed { tabs } => tabs
                    .iter()
                    .map(|tab| (tab.template.composition.clone(), tab.binding.clone()))
                    .collect(),
            }
        } else {
            surface
                .to_mount_seeds()?
                .into_iter()
                .map(|seed| (seed, membership.binding.clone()))
                .collect()
        };
        if manifest.tabs.len() != expected.len()
            || manifest
                .tabs
                .iter()
                .zip(&expected)
                .any(|(tab, (seed, binding))| &tab.seed != seed || &tab.binding != binding)
        {
            return Err("mount manifest contradicts admitted seed or binding".into());
        }
        let existing_ids = self
            .surfaces
            .values()
            .flat_map(|surface| surface.view_sets.iter())
            .flat_map(|set| std::iter::once(set.id.0).chain(set.tiles.keys().map(|id| id.0)))
            .collect::<std::collections::BTreeSet<_>>();
        if manifest.tabs.iter().any(|tab| {
            existing_ids.contains(&tab.view_set_id.0)
                || tab.tile_ids.iter().any(|id| existing_ids.contains(&id.0))
                || tab.group_ids.iter().any(|id| existing_ids.contains(&id.0))
        }) {
            return Err("mount manifest aliases an existing composition".into());
        }
        let mut view_sets = manifest.instantiate_with_exact_ids(&surface.tiling)?;
        let mut mount_bindings = BTreeMap::new();
        for set in &mut view_sets {
            for (tile_id, tile) in &mut set.tiles {
                if membership.saved_surface.is_none()
                    && !descriptor
                        .presentation
                        .views
                        .contains_key(&tile.view.view_ref)
                {
                    return Err("Surface tile View is absent from its admitted closure".into());
                }
                tile.instance_key =
                    RyeOsViewInstanceKey::surface_tile(&self.presentation_id, &id, *tile_id);
                let tab_binding = manifest
                    .tabs
                    .iter()
                    .find(|tab| tab.view_set_id == set.id)
                    .ok_or("manifest tab disappeared")?
                    .binding
                    .clone();
                mount_bindings.insert(tile.instance_key.clone(), tab_binding);
            }
        }
        let active_view_set = manifest.active_view_set_id;
        let mut contextual = BTreeMap::new();
        for dialog in &surface.dialogs {
            if !descriptor.presentation.views.contains_key(&dialog.view.0) {
                return Err("contextual dialog View is absent from its admitted closure".into());
            }
            let instance_key =
                RyeOsViewInstanceKey::surface_dialog(&self.presentation_id, &id, &dialog.id)
                    .map_err(str::to_owned)?;
            let key = instance_key.as_str().to_owned();
            if self.dialogs.contains_key(&key) || contextual.contains_key(&key) {
                return Err("contextual dialog mount id is duplicated".into());
            }
            contextual.insert(
                key,
                DialogMount {
                    view: PresentationViewMount {
                        instance_key,
                        view_ref: dialog.view.0.clone(),
                        binding: membership.binding.clone(),
                        local: ViewLocalState::default(),
                    },
                    title: dialog.title.clone(),
                    open: false,
                    insertion: None,
                    context_origin: None,
                    surface_owner: Some(id.clone()),
                },
            );
        }
        self.surfaces.insert(
            id.clone(),
            SurfaceComposition {
                allocation_interval: (manifest.allocation_start, manifest.next_mount_id),
                id: id.clone(),
                ordinal: membership.ordinal,
                title: membership.title.clone(),
                binding: membership.binding.clone(),
                view_sets,
                active_view_set,
                mount_bindings,
                admitted_saved_tabs: BTreeMap::new(),
            },
        );
        self.dialogs.extend(contextual);
        for dialog in self.dialogs.values_mut() {
            if dialog
                .surface_owner
                .as_ref()
                .is_some_and(|owner| owner != &id)
            {
                dialog.open = false;
            }
        }
        self.active_surface = Some(id);
        Ok(())
    }

    /// Stage a saved collection using its real authored origin and independently
    /// admitted tab closures. Nothing becomes visible until every tab is valid.
    pub fn open_saved_surface(
        &mut self,
        membership: &UiSurfaceMembership,
        origin: &UiBindingAttachment,
        descriptors: &BTreeMap<String, UiBindingAttachment>,
    ) -> Result<BTreeMap<String, ViewSetId>, String> {
        let saved = membership
            .saved_surface
            .as_ref()
            .ok_or("saved Surface composition is absent")?;
        let tabs: Vec<AdmittedSavedSurfaceTab> = match &saved.body {
            crate::ui::binding::UiSavedSurfaceCompositionBody::Particular { tabs } => tabs
                .iter()
                .cloned()
                .map(AdmittedSavedSurfaceTab::Particular)
                .collect(),
            crate::ui::binding::UiSavedSurfaceCompositionBody::Reusable { tabs }
            | crate::ui::binding::UiSavedSurfaceCompositionBody::Composed { tabs } => tabs
                .iter()
                .cloned()
                .map(AdmittedSavedSurfaceTab::Reusable)
                .collect(),
        };
        if tabs.is_empty()
            || tabs.len() > crate::surface::view_sets::MAX_VIEW_SETS
            || tabs.len() != membership.mount_manifest.tabs.len()
        {
            return Err("saved Surface tab count is invalid".into());
        }
        let mut staged = self.clone();
        staged.open_surface(membership, origin)?;
        let id = SurfaceInstanceId::try_new(membership.surface_instance_id.clone())
            .map_err(str::to_owned)?;
        let mut saved_ids = BTreeMap::new();
        for (tab, mount) in tabs.iter().zip(&membership.mount_manifest.tabs) {
            let template = tab.template();
            let descriptor = descriptors
                .get(&tab.binding().binding_attachment_id)
                .ok_or("saved tab attachment is absent")?;
            descriptor.validate().map_err(str::to_owned)?;
            if &coordinate(descriptor) != tab.binding()
                || tab.saved_set_id() != template.id
                || template.composition.id != tab.saved_set_id()
                || saved_ids.contains_key(tab.saved_set_id())
            {
                return Err("saved tab contradicts its exact attachment or stable identity".into());
            }
            let surface = descriptor
                .presentation
                .definition
                .surface()
                .ok_or("saved tab attachment is not a Surface")?;
            crate::surface::view_sets::validate_saved_view_set_templates(std::slice::from_ref(
                &template,
            ))?;
            let set = staged
                .view_set_mut(&id, mount.view_set_id)
                .ok_or("manifest tab is absent")?;
            set.tiling = surface.tiling.clone();
            for tile in set.tiles.values() {
                if !descriptor
                    .presentation
                    .views
                    .contains_key(&tile.view.view_ref)
                {
                    return Err("saved tab View is outside its exact admitted closure".into());
                }
            }
            saved_ids.insert(tab.saved_set_id().to_owned(), set.id);
        }
        let active = saved_ids
            .get(&saved.active_view_set_id)
            .copied()
            .ok_or("saved active tab is absent")?;
        if membership.mount_manifest.active_view_set_id != Some(active) {
            return Err("saved active tab contradicts mount manifest".into());
        }
        staged
            .surfaces
            .get_mut(&id)
            .ok_or("saved Surface disappeared")?
            .admitted_saved_tabs = tabs
            .iter()
            .map(|tab| (saved_ids[tab.saved_set_id()], tab.clone()))
            .collect();
        *self = staged;
        Ok(saved_ids)
    }

    pub(crate) fn rename_surface(
        &mut self,
        id: &SurfaceInstanceId,
        title: String,
    ) -> Result<(), String> {
        self.surfaces.get_mut(id).ok_or("Surface is absent")?.title = title;
        Ok(())
    }

    pub fn select_surface(&mut self, id: &SurfaceInstanceId) -> Result<(), String> {
        if !self.surfaces.contains_key(id) {
            return Err("Surface instance is not mounted".into());
        }
        for dialog in self.dialogs.values_mut() {
            if dialog
                .surface_owner
                .as_ref()
                .is_some_and(|owner| owner != id)
            {
                dialog.open = false;
            }
        }
        self.active_surface = Some(id.clone());
        Ok(())
    }

    pub fn close_surface(&mut self, id: &SurfaceInstanceId) -> Option<SurfaceComposition> {
        let removed = self.surfaces.remove(id)?;
        self.dialogs
            .retain(|_, mount| mount.surface_owner.as_ref() != Some(id));
        if self.active_surface.as_ref() == Some(id) {
            self.active_surface = self
                .surfaces
                .values()
                .min_by_key(|surface| surface.ordinal)
                .map(|surface| surface.id.clone());
        }
        // Insertion hints are presentation destinations, not executable routes.
        for dialog in self.dialogs.values_mut() {
            if dialog
                .insertion
                .as_ref()
                .is_some_and(|destination| &destination.surface_instance_id == id)
            {
                dialog.insertion = None;
            }
        }
        Some(removed)
    }

    /// Reconciliation changes canonical structure, not the renderer-local
    /// state of an unchanged exact payload. Never preserve state by label,
    /// position or View ref alone, and never roll back journalled focus/layout.
    pub fn preserve_transient_state_from(&mut self, previous: &Self) {
        if self.presentation_id != previous.presentation_id {
            return;
        }
        let same = |new: &PresentationViewMount, old: &PresentationViewMount| {
            new.instance_key == old.instance_key
                && new.view_ref == old.view_ref
                && new.binding == old.binding
        };
        let valid_origins = self
            .docks
            .values()
            .map(|mount| mount.view.instance_key.clone())
            .chain(
                self.dialogs
                    .values()
                    .map(|mount| mount.view.instance_key.clone()),
            )
            .chain(
                self.surfaces
                    .values()
                    .flat_map(|surface| surface.view_sets.iter())
                    .flat_map(|set| set.tiles.values().map(|tile| tile.instance_key.clone())),
            )
            .filter(|key| {
                self.mount(key)
                    .zip(previous.mount(key))
                    .is_some_and(|(new, old)| {
                        new.view_ref() == old.view_ref() && new.binding() == old.binding()
                    })
            })
            .collect::<std::collections::BTreeSet<_>>();
        for (edge, dock) in &mut self.docks {
            if let Some(old) = previous
                .docks
                .get(edge)
                .filter(|old| same(&dock.view, &old.view))
            {
                dock.view.local = old.view.local.clone();
                dock.visible = old.visible;
                dock.size = old.size;
            }
        }
        for (id, dialog) in &mut self.dialogs {
            if let Some(old) = previous.dialogs.get(id).filter(|old| {
                same(&dialog.view, &old.view) && dialog.surface_owner == old.surface_owner
            }) {
                dialog.view.local = old.view.local.clone();
                dialog.open = old.open
                    && dialog
                        .surface_owner
                        .as_ref()
                        .is_none_or(|owner| self.active_surface.as_ref() == Some(owner));
                dialog.context_origin = old
                    .context_origin
                    .as_ref()
                    .filter(|key| valid_origins.contains(*key))
                    .cloned();
                // Losing a captured origin is not permission to reinterpret
                // this frame as an origin-free launcher/settings view. Keep
                // its mounted local state, but require explicit reopening.
                if old.context_origin.is_some() && dialog.context_origin.is_none() {
                    dialog.open = false;
                }
                dialog.insertion = old
                    .insertion
                    .as_ref()
                    .filter(|destination| {
                        self.surfaces
                            .get(&destination.surface_instance_id)
                            .is_some_and(|surface| {
                                surface
                                    .view_sets
                                    .iter()
                                    .any(|set| set.id == destination.view_set_id)
                            })
                    })
                    .cloned();
            }
        }
        for surface in self.surfaces.values_mut() {
            for set in &mut surface.view_sets {
                for tile in set.tiles.values_mut() {
                    let Some(old) = previous.mount(&tile.instance_key) else {
                        continue;
                    };
                    if old.view_ref() == tile.view.view_ref
                        && surface.mount_bindings.get(&tile.instance_key) == Some(old.binding())
                    {
                        tile.local = old.local().clone();
                    }
                }
            }
        }
    }
}
