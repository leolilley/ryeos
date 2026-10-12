//! Source regressions for explicit reusable-tab subject inputs.
use super::{
    attachment::SelectionAttachment,
    binding::UiBindingAttachment,
    composition_delta::{UiCompositionSubjectAssignment, stage_composition_delta},
    composition_operation::*,
    presentation_state::PresentationComposition,
};
use crate::surface::view_sets::SavedViewSetTemplate;
use serde_json::json;
use std::collections::BTreeMap;

fn fixture() -> (
    PresentationComposition,
    UiCompositionRequest,
    BTreeMap<String, UiBindingAttachment>,
    u64,
) {
    let session = super::reducer::test_support::session_with_surface(
        json!({"name":"fresh-inputs","tiles":["view:test/subject"],"views":{"view:test/subject":{"widget":"rows","sources":{"default":{"ref":"service:test/subject","params":{"subject":"@facet:selection.work.id"}}}}}}),
    );
    let membership = &session.presentations[0];
    let descriptors = session
        .binding_attachments
        .iter()
        .map(|entry| (entry.binding_attachment_id.clone(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut composition = PresentationComposition::from_membership(
        membership,
        &descriptors[&membership.root_binding.binding_attachment_id],
    )
    .unwrap();
    let surface = &membership.surfaces[0];
    composition
        .open_surface(
            surface,
            &descriptors[&surface.binding.binding_attachment_id],
        )
        .unwrap();
    let template:SavedViewSetTemplate=serde_json::from_value(json!({"id":"fresh","name":"Fresh","composition":{"schema":2,"id":"fresh","title":"Fresh","root":{"type":"group","views":["view:test/subject"],"active":0}},"relationships":[{"mount":{"kind":"tile","index":0},"source":{"mode":"required_subject","input":"subject","facets":["selection.work.id"]}}]})).unwrap();
    let request = UiCompositionRequest {
        schema: 1,
        presentation_id: composition.id().clone(),
        operation_id: "33333333-3333-4333-8333-333333333333".into(),
        expected_revision: 0,
        expected_next_mount_id: membership.next_mount_id,
        owner: CompositionOwner::Surface {
            surface_instance_id: composition.active_surface().unwrap().id().clone(),
            binding: surface.binding.clone(),
        },
        mutation: CompositionMutation::InsertTab {
            index: 1,
            activate: true,
            tab: CompositionTabSeed {
                template,
                binding: surface.binding.clone(),
                focused_view: None,
                fresh_subjects: BTreeMap::from([(
                    "subject".into(),
                    BTreeMap::from([("selection.work.id".into(), json!("fresh-work"))]),
                )]),
                followed_sets: BTreeMap::new(),
            },
        },
    };
    (composition, request, descriptors, membership.next_mount_id)
}

#[test]
fn supplied_fresh_subject_becomes_exact_pin_only_in_staged_result() {
    let (original, request, descriptors, frontier) = fixture();
    let before = original.active_surface().unwrap().view_sets().len();
    let delta =
        stage_composition_delta(&original, &request, 0, frontier, &descriptors, 65536).unwrap();
    assert_eq!(original.active_surface().unwrap().view_sets().len(), before);
    assert_eq!(
        delta
            .composition
            .active_surface()
            .unwrap()
            .view_sets()
            .len(),
        before + 1
    );
    assert!(
        matches!(&delta.subjects[..],[UiCompositionSubjectAssignment::Exact {attachment:SelectionAttachment::Pinned {values,..},..}] if values["selection.work.id"]==json!("fresh-work"))
    );
    assert!(delta.next_mount_id > frontier);
}

#[test]
fn null_missing_dependency_and_unknown_fresh_input_refuse_atomically() {
    for failure in ["null", "dependency", "unknown"] {
        let (original, mut request, descriptors, frontier) = fixture();
        let CompositionMutation::InsertTab { tab, .. } = &mut request.mutation else {
            unreachable!()
        };
        match failure {
            "null" => {
                tab.fresh_subjects
                    .get_mut("subject")
                    .unwrap()
                    .insert("selection.work.id".into(), json!(null));
            }
            "dependency" => {
                tab.fresh_subjects.insert(
                    "subject".into(),
                    BTreeMap::from([("selection.other.id".into(), json!("wrong"))]),
                );
            }
            _ => {
                tab.fresh_subjects.insert("unknown".into(), BTreeMap::new());
            }
        }
        let ids = original
            .active_surface()
            .unwrap()
            .view_sets()
            .iter()
            .map(|set| set.id)
            .collect::<Vec<_>>();
        assert!(
            stage_composition_delta(&original, &request, 0, frontier, &descriptors, 65536).is_err(),
            "{failure} must refuse"
        );
        assert_eq!(
            original
                .active_surface()
                .unwrap()
                .view_sets()
                .iter()
                .map(|set| set.id)
                .collect::<Vec<_>>(),
            ids
        );
    }
}

#[test]
fn external_follow_requires_exact_existing_same_surface_target() {
    let (original, mut request, descriptors, frontier) = fixture();
    let target = original.active_view_set().unwrap().id;
    let CompositionMutation::InsertTab { tab, .. } = &mut request.mutation else {
        unreachable!()
    };
    tab.fresh_subjects.clear();
    tab.template.relationships[0].source =
        crate::surface::view_sets::SavedViewSelectionSource::FollowSet {
            saved_view_set_id: "retained".into(),
        };
    tab.followed_sets.insert("retained".into(), target);
    let delta =
        stage_composition_delta(&original, &request, 0, frontier, &descriptors, 65536).unwrap();
    assert!(
        matches!(&delta.subjects[..],[UiCompositionSubjectAssignment::Portable {resolved_follow_set:Some(view_set_id),..}] if *view_set_id==target)
    );
    let CompositionMutation::InsertTab { tab, .. } = &mut request.mutation else {
        unreachable!()
    };
    tab.followed_sets
        .insert("retained".into(), crate::ids::ViewSetId::new(u64::MAX));
    assert!(
        stage_composition_delta(&original, &request, 0, frontier, &descriptors, 65536).is_err()
    );
}

#[test]
fn restore_internal_follow_target_refuses_contradictory_external_resolution() {
    let (original, mut request, descriptors, frontier) = fixture();
    let CompositionMutation::InsertTab { tab, .. } = request.mutation else {
        unreachable!()
    };
    let mut follower = tab.clone();
    follower.template.id = "follower".into();
    follower.template.composition.id = "follower".into();
    follower.fresh_subjects.clear();
    follower.template.relationships[0].source =
        crate::surface::view_sets::SavedViewSelectionSource::FollowSet {
            saved_view_set_id: "fresh".into(),
        };
    request.mutation = CompositionMutation::RestoreTabs {
        tabs: vec![tab, follower],
        active_seed_id: "fresh".into(),
    };
    let delta =
        stage_composition_delta(&original, &request, 0, frontier, &descriptors, 65536).unwrap();
    let target = delta.allocated_manifest.as_ref().unwrap().tabs[0].view_set_id;
    assert!(
        matches!(&delta.subjects[1],UiCompositionSubjectAssignment::Portable {resolved_follow_set:Some(id),..} if *id==target)
    );
    let CompositionMutation::RestoreTabs { tabs, .. } = &mut request.mutation else {
        unreachable!()
    };
    tabs[1]
        .followed_sets
        .insert("fresh".into(), original.active_view_set().unwrap().id);
    assert!(
        stage_composition_delta(&original, &request, 0, frontier, &descriptors, 65536).is_err()
    );
}
