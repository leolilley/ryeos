use super::effect::RyeOsEffect;
use super::model::RyeOsCore;
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};

impl RyeOsCore {
    fn active_attachment_view(&self, instance: &RyeOsViewInstanceKey) -> Option<String> {
        let mount = self.presentation.as_ref()?.mount(instance)?;
        mount.visible().then(|| mount.view_ref().to_string())
    }

    pub(crate) fn pin_view_selection(
        &mut self,
        instance: RyeOsViewInstanceKey,
    ) -> Vec<RyeOsEffect> {
        if self.active_attachment_view(&instance).is_none() {
            return Vec::new();
        }
        if self.instance_has_unresolved_required_subject(&instance) {
            self.notice(
                "Cannot pin selection until the required subject is supplied.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let pinned = match self.capture_pinned_selection(&instance) {
            Ok(pinned) => pinned,
            Err(error) => {
                self.notice(
                    format!("Cannot pin selection: {error}"),
                    super::view_model::RyeOsTone::Warn,
                );
                return Vec::new();
            }
        };
        if self.selection_attachments.get(&instance) == Some(&pinned) {
            return Vec::new();
        }
        self.prepare_subject_operation(instance, pinned)
    }

    pub(crate) fn open_pinned_view_alongside(
        &mut self,
        instance: RyeOsViewInstanceKey,
    ) -> Vec<RyeOsEffect> {
        // Alongside is a Surface-local placement operation. A shared mount
        // must not acquire the active Surface merely because one is visible.
        if self.view_set_for_instance(&instance).is_none() {
            return Vec::new();
        }
        let Some(view_ref) = self.active_attachment_view(&instance) else {
            return Vec::new();
        };
        let Some(binding) = self
            .binding_attachment_for_instance(&instance)
            .map(
                |attachment| crate::ui::binding::UiBindingAttachmentCoordinate {
                    binding_attachment_id: attachment.binding_attachment_id.clone(),
                    binding_generation: attachment.binding_generation,
                    binding_digest: attachment.binding_digest.clone(),
                },
            )
        else {
            return Vec::new();
        };
        let pinned = match self.capture_pinned_selection(&instance) {
            Ok(pinned) => pinned,
            Err(error) => {
                self.notice(
                    format!("Cannot open pinned view: {error}"),
                    super::view_model::RyeOsTone::Warn,
                );
                return Vec::new();
            }
        };
        let Some(set) = self.view_set_for_instance(&instance) else {
            return Vec::new();
        };
        if self
            .active_view_set()
            .is_none_or(|active| active.id != set.id)
        {
            return Vec::new();
        }
        let Some(target) = set
            .tiles
            .iter()
            .find_map(|(id, tile)| (tile.instance_key == instance).then_some(*id))
        else {
            return Vec::new();
        };
        self.prepare_composition_mutation(
            crate::ui::composition_operation::CompositionMutation::InsertPinnedView {
                tab_id: set.id,
                placement: crate::ui::composition_operation::CompositionPlacement::Split {
                    target,
                    axis: match set.tiling.stack.arrange {
                        crate::surface::ArrangeSpec::Horizontal => {
                            crate::layout::SplitAxis::Horizontal
                        }
                        crate::surface::ArrangeSpec::Vertical => crate::layout::SplitAxis::Vertical,
                    },
                    before: false,
                    ratio: 0.5,
                },
                view_ref,
                binding,
                attachment: pinned,
                route_assignment:
                    crate::ui::composition_operation::UiCompositionRouteAssignment::Authored,
            },
        )
    }

    pub(crate) fn follow_view_set_selection(
        &mut self,
        instance: RyeOsViewInstanceKey,
        view_set_id: ViewSetId,
    ) -> Vec<RyeOsEffect> {
        let Some(view_ref) = self.active_attachment_view(&instance) else {
            return Vec::new();
        };
        if !self.same_surface(&instance, view_set_id) {
            return Vec::new();
        }
        if let Some((input, facets)) = self.unresolved_required_subject(&instance) {
            let Some(binding) = self.binding_for_instance(&instance, &view_ref) else {
                return Vec::new();
            };
            let declared = super::super::attachment::selection_dependencies(binding);
            let compatible = declared.len() == facets.len()
                && facets.iter().all(|facet| declared.contains(facet))
                && self
                    .subject_values_from_view_set(view_set_id, &facets)
                    .and_then(|values| self.bounded_pinned_subject(&instance, values).ok())
                    .is_some();
            if !compatible {
                self.notice(
                    format!(
                        "Cannot follow selection for {input}: the source set is not compatible."
                    ),
                    super::view_model::RyeOsTone::Warn,
                );
                return Vec::new();
            }
        }
        let next = super::super::attachment::SelectionAttachment::FollowViewSet { view_set_id };
        if self.selection_attachments.get(&instance) == Some(&next)
            || (!self.selection_attachments.contains_key(&instance)
                && self
                    .view_set_for_instance(&instance)
                    .is_some_and(|set| set.id == view_set_id))
        {
            return Vec::new();
        }
        self.prepare_subject_operation(instance, next)
    }

    pub(crate) fn supply_required_subject(
        &mut self,
        instance: RyeOsViewInstanceKey,
        source_view_set_id: ViewSetId,
    ) -> Vec<RyeOsEffect> {
        let Some(view_ref) = self.active_attachment_view(&instance) else {
            return Vec::new();
        };
        let Some((input, facets)) = self.unresolved_required_subject(&instance) else {
            return Vec::new();
        };
        let Some(binding) = self.binding_for_instance(&instance, &view_ref) else {
            return Vec::new();
        };
        let declared = super::super::attachment::selection_dependencies(binding);
        if declared.len() != facets.len() || facets.iter().any(|facet| !declared.contains(facet)) {
            self.notice(
                format!(
                    "Cannot supply {input}: the admitted view now requires a different subject."
                ),
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let Some(values) = self.subject_values_from_view_set(source_view_set_id, &facets) else {
            self.notice(
                format!(
                    "Cannot supply {input}: that view set has no complete compatible selection."
                ),
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        };
        let pinned = match self.bounded_pinned_subject(&instance, values) {
            Ok(pinned) => pinned,
            Err(error) => {
                self.notice(
                    format!("Cannot supply {input}: {error}"),
                    super::view_model::RyeOsTone::Warn,
                );
                return Vec::new();
            }
        };
        // Recheck the exact relationship immediately before mutation. Nothing
        // above may turn a stale UI action into a subject change.
        if self.unresolved_required_subject(&instance) != Some((input.clone(), facets)) {
            return Vec::new();
        }
        self.prepare_subject_operation(instance, pinned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::binding::UiBindingPayload;
    use crate::ui::effect::RyeOsEffectKind;
    use crate::ui::event::{RyeOsEvent, RyeOsUiEvent, RyeOsUiIntent};
    use crate::ui::reducer::test_support::core as test_support_core;
    use crate::ui::reducer::test_support::*;

    fn core_with_selection_view() -> (RyeOsCore, RyeOsViewInstanceKey) {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"selection-fixture", "tiles":["view:test/selection"],
            "views":{"view:test/selection":{
                "widget": "rows",
                "input":{"id":"query","feeds":{"param":"query"}},
                "sources": {
                    "initial": {
                        "ref": "service:test/initial",
                        "params": {"thread": "@facet:selection.work.thread"}
                    },
                    "later": {
                        "ref": "service:test/later",
                        "activation": "on_demand",
                        "params": {"thread": "@facet:selection.work.thread"}
                    }
                }
            }}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let tile = core.active_view_set().unwrap().focused_tile;
        let instance = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        (core, instance)
    }

    fn settle_subject(core: &mut RyeOsCore) -> Vec<RyeOsEffect> {
        let producer = core
            .presentation_operations
            .producer
            .clone()
            .expect("fixture producer claimed");
        settle_pending_prefix(core, &producer)
    }

    fn set_selection(core: &mut RyeOsCore, view_set_id: ViewSetId, thread: &str) {
        let key = crate::ui::seat::selection_storage_key(view_set_id, "selection.work").unwrap();
        core.seat
            .append_facet(key, serde_json::json!({"thread": thread}));
    }

    #[test]
    fn unresolved_subject_is_a_source_and_attachment_action_fence() {
        let (mut core, instance) = core_with_selection_view();
        let owner = core.active_view_set().unwrap().id;
        core.selection_attachments.insert(
            instance.clone(),
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );

        assert!(
            core.emit_fetch_source_for_instance(instance.clone(), "view:test/selection")
                .is_empty()
        );
        assert!(
            core.compiled_binding_operation(
                &instance,
                crate::ui::binding::UiBindingCoordinate::Source {
                    view_ref: "view:test/selection".into(),
                    channel: "initial".into(),
                },
                UiBindingPayload::SourceParameters {
                    params: serde_json::json!({}),
                },
            )
            .is_none()
        );
        assert!(core.pin_view_selection(instance.clone()).is_empty());
        assert!(
            core.follow_view_set_selection(instance.clone(), owner)
                .is_empty()
        );
        assert!(matches!(
            core.selection_attachments.get(&instance),
            Some(crate::ui::attachment::SelectionAttachment::RequiredSubject { .. })
        ));
    }

    #[test]
    fn unresolved_subject_suppresses_every_alternate_source_path() {
        let mut core = test_support_core(session_with_surface(serde_json::json!({
            "name":"source-fence", "tiles":["view:test/source-fence"],
            "views":{"view:test/source-fence":{
                "widget": "rows",
                "sources": {
                    "independent": {"ref": "service:test/independent", "params": {}},
                    "detail": {
                        "ref": "service:test/detail",
                        "role": "detail",
                        "activation": "on_demand",
                        "params": {"thread": "@facet:selection.work.thread"}
                    }
                },
                "input": {
                    "id": "query",
                    "submit": "route",
                    "completion": {"ref": "service:test/completion", "collection": "rows"},
                    "mentions": {
                        "ref": "service:test/mentions",
                        "collection": "rows",
                        "reference": "id",
                        "label": "label"
                    }
                }
            }}
        })));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let tile = core.active_view_set().unwrap().focused_tile;
        let instance = core.active_view_set().unwrap().tiles[&tile]
            .instance_key
            .clone();
        core.selection_attachments.insert(
            instance.clone(),
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );

        let effects = core.initial_effects();
        assert!(effects.iter().all(|effect| {
            let RyeOsEffectKind::FetchSource { tile_id, .. } = &effect.kind else {
                return true;
            };
            !crate::ui::source_key::RyeOsSourceInstanceKey::decode(tile_id)
                .is_some_and(|key| key.belongs_to(&instance))
        }));
        assert!(
            core.refresh_source_channel(instance.clone(), "view:test/source-fence", "detail",)
                .is_empty()
        );
        assert!(
            core.fetch_view_source_role(
                instance.clone(),
                "view:test/source-fence",
                "detail",
                serde_json::json!({"thread": "T-forged"}),
            )
            .is_none()
        );
        let deferred_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "detail")
                .encode();
        core.deferred_source_fetches.insert(
            deferred_key.clone(),
            crate::ui::model::DeferredSourceFetch {
                view_ref: "view:test/source-fence".into(),
                channel: "detail".into(),
                params: serde_json::json!({"thread": "T-forged"}),
            },
        );
        assert!(core.release_deferred_source_fetch(&deferred_key).is_none());
        assert!(!core.deferred_source_fetches.contains_key(&deferred_key));
    }

    #[test]
    fn supply_required_subject_reads_the_named_set_and_pins_exact_facets() {
        let (mut core, instance) = core_with_selection_view();
        let source = core.active_view_set().unwrap().id;
        set_selection(&mut core, source, "T-current");
        settle_subject(&mut core);
        core.prepare_subject_operation(
            instance.clone(),
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );
        settle_subject(&mut core);

        assert!(
            core.supply_required_subject(instance.clone(), source)
                .is_empty()
        );
        assert!(
            matches!(
                core.selection_attachments.get(&instance),
                Some(crate::ui::attachment::SelectionAttachment::RequiredSubject { .. })
            ),
            "subject remains unresolved before ACK"
        );
        let effects = settle_subject(&mut core);
        assert_eq!(
            effects.len(),
            2,
            "settled subject replacement refreshes every declared channel"
        );
        let Some(crate::ui::attachment::SelectionAttachment::Pinned { values, .. }) =
            core.selection_attachments.get(&instance)
        else {
            panic!("required subject must become a pin")
        };
        assert_eq!(values.len(), 1);
        assert_eq!(values["selection.work.thread"], "T-current");

        set_selection(&mut core, source, "T-later");
        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work.thread"),
            Some(serde_json::json!("T-current"))
        );
    }

    #[test]
    fn incomplete_source_set_leaves_required_subject_unresolved() {
        let (mut core, instance) = core_with_selection_view();
        let source = core.active_view_set().unwrap().id;
        core.selection_attachments.insert(
            instance.clone(),
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );

        assert!(
            core.supply_required_subject(instance.clone(), source)
                .is_empty()
        );
        assert!(matches!(
            core.selection_attachments.get(&instance),
            Some(crate::ui::attachment::SelectionAttachment::RequiredSubject { .. })
        ));
    }

    #[test]
    fn stale_source_or_changed_binding_cannot_complete_required_subject() {
        let (mut core, instance) = core_with_selection_view();
        let source = core.active_view_set().unwrap().id;
        set_selection(&mut core, source, "T-current");
        let unresolved = crate::ui::attachment::SelectionAttachment::RequiredSubject {
            input: "subject_tile_0".into(),
            facets: vec!["selection.work.thread".into()],
        };
        core.selection_attachments
            .insert(instance.clone(), unresolved.clone());

        assert!(
            core.supply_required_subject(instance.clone(), ViewSetId::new(u64::MAX))
                .is_empty()
        );
        assert_eq!(core.selection_attachments.get(&instance), Some(&unresolved));

        seed_view_value(
            &mut core,
            "view:test/selection",
            serde_json::json!({
                "widget": "rows",
                "sources": {"initial": {
                    "ref": "service:test/initial",
                    "params": {"item": "@facet:selection.item.id"}
                }}
            }),
        );
        assert!(
            core.supply_required_subject(instance.clone(), source)
                .is_empty()
        );
        assert_eq!(core.selection_attachments.get(&instance), Some(&unresolved));

        let tile_id = instance.surface_tile_id().unwrap();
        core.active_view_set_mut().unwrap().tiles.remove(&tile_id);
        assert!(core.supply_required_subject(instance, source).is_empty());
    }

    #[test]
    fn supply_required_subject_enforces_the_exact_target_byte_bound() {
        let (mut core, instance) = core_with_selection_view();
        let source = core.active_view_set().unwrap().id;
        set_selection(&mut core, source, "T-bounded");
        let facets = vec!["selection.work.thread".to_string()];
        let values = core.subject_values_from_view_set(source, &facets).unwrap();
        let encoded_len = serde_json::to_vec(&values).unwrap().len();
        let unresolved = crate::ui::attachment::SelectionAttachment::RequiredSubject {
            input: "subject_tile_0".into(),
            facets,
        };
        core.selection_attachments
            .insert(instance.clone(), unresolved.clone());

        let full_envelope_bound = core.binding_attachments["fixture-attachment"]
            .descriptor
            .binding_request_bounds
            .max_request_bytes;

        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = u64::try_from(encoded_len - 1).unwrap();
        assert!(
            core.supply_required_subject(instance.clone(), source)
                .is_empty()
        );
        assert_eq!(core.selection_attachments.get(&instance), Some(&unresolved));

        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = u64::try_from(encoded_len).unwrap();
        assert_eq!(
            core.subject_values_from_view_set(source, &["selection.work.thread".into()])
                .unwrap(),
            values,
            "portable subject values fit their exact encoded bound"
        );
        let journal = core.seat.events().to_vec();
        // Subject values use the exact target descriptor; the retained
        // composition envelope is independently bounded by the shared owner.
        let shared_id = core
            .data
            .session
            .as_ref()
            .unwrap()
            .root_attachment_id
            .clone();
        let shared_bound = core.binding_attachments[&shared_id]
            .descriptor
            .binding_request_bounds
            .max_request_bytes;
        core.binding_attachments
            .get_mut(&shared_id)
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = encoded_len as u64;
        assert!(
            core.supply_required_subject(instance.clone(), source)
                .is_empty()
        );
        assert_eq!(
            core.seat.events(),
            journal.as_slice(),
            "subject-only capacity cannot admit the larger prepared envelope"
        );
        assert_eq!(core.selection_attachments.get(&instance), Some(&unresolved));
        core.binding_attachments
            .get_mut(&shared_id)
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = shared_bound;
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .descriptor
            .binding_request_bounds
            .max_request_bytes = full_envelope_bound;
        assert!(
            core.supply_required_subject(instance.clone(), source)
                .is_empty()
        );
        assert!(
            !pending_prefix(&core).is_empty(),
            "full signed envelope bound admits preparation"
        );
        settle_subject(&mut core);
        assert!(matches!(
            core.selection_attachments.get(&instance),
            Some(crate::ui::attachment::SelectionAttachment::Pinned { .. })
        ));
    }

    #[test]
    fn compatible_follow_replaces_required_subject_without_pinning_values() {
        let (mut core, instance) = core_with_selection_view();
        let source = core.active_view_set().unwrap().id;
        set_selection(&mut core, source, "T-current");
        core.selection_attachments.insert(
            instance.clone(),
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );

        assert!(
            core.follow_view_set_selection(instance.clone(), source)
                .is_empty()
        );
        let effects = settle_subject(&mut core);
        assert!(!effects.is_empty());
        assert_eq!(
            core.selection_attachments.get(&instance),
            Some(&crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: source,
            })
        );
        set_selection(&mut core, source, "T-later");
        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work"),
            Some(serde_json::json!({"thread":"T-later"}))
        );
    }

    #[test]
    fn pin_keeps_exact_value_and_has_no_write_owner() {
        let (mut core, instance) = core_with_selection_view();
        let owner = core.active_view_set().unwrap().id;
        set_selection(&mut core, owner, "T-one");
        settle_subject(&mut core);

        let prepared_effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::PinViewSelection {
                    instance_key: instance.clone(),
                },
            },
        });
        assert!(prepared_effects.is_empty());
        assert!(
            core.followed_selection_view_set(&instance).is_some(),
            "pin does not publish before ACK"
        );
        let effects = settle_subject(&mut core);
        assert_eq!(effects.len(), 2, "initial and on-demand channels refresh");
        set_selection(&mut core, owner, "T-two");

        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work.thread"),
            Some(serde_json::json!("T-one"))
        );
        assert!(
            core.followed_selection_view_set(&instance).is_none(),
            "a pinned snapshot is not a mutable selection owner"
        );
    }

    #[test]
    fn follow_rebind_fences_old_sources_and_refreshes_every_channel() {
        let (mut core, instance) = core_with_selection_view();
        let source_id = core.active_view_set().unwrap().id;
        let source = core
            .active_surface_view_sets()
            .unwrap()
            .iter()
            .position(|set| set.id == source_id)
            .unwrap();
        set_selection(&mut core, source_id, "T-source");
        core.new_view_set();
        settle_subject(&mut core);
        let target_id = core.active_view_set().unwrap().id;
        set_selection(&mut core, target_id, "T-target");
        core.switch_view_set_tab(source);
        settle_subject(&mut core);

        let old_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "initial")
                .encode();
        core.data
            .sources
            .insert(old_key.clone(), serde_json::json!({"old": true}));

        let prepared_effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::FollowViewSetSelection {
                    instance_key: instance.clone(),
                    view_set_id: target_id,
                },
            },
        });

        assert!(prepared_effects.is_empty());
        let effects = settle_subject(&mut core);
        assert_eq!(effects.len(), 2, "on-demand channels are refreshed too");
        assert!(!core.data.sources.contains_key(&old_key));
        assert!(core.data.source_floor.contains_key(&old_key));
        for effect in effects {
            let RyeOsEffectKind::FetchSource { request, .. } = effect.kind else {
                panic!("attachment refresh must emit source fetches");
            };
            let UiBindingPayload::SourceParameters { params } = request.payload else {
                panic!("source fetch must carry resolved parameters");
            };
            assert_eq!(params["thread"], "T-target");
        }
    }

    #[test]
    fn open_alongside_mounts_a_new_instance_pinned_before_its_initial_fetch() {
        let (mut core, original) = core_with_selection_view();
        let owner = core.active_view_set().unwrap().id;
        set_selection(&mut core, owner, "T-one");
        settle_subject(&mut core);
        let draft_key = crate::ui::model::InputBufferKey::new(
            original.clone(),
            "view:test/selection",
            "query",
            core.binding_attachment_for_instance(&original).unwrap(),
        )
        .storage_key();
        core.input_buffers.insert(
            draft_key.clone(),
            crate::ui::model::RyeOsInputState {
                text: "original draft".into(),
                ..Default::default()
            },
        );
        let frontier = core
            .acknowledged_composition_replay()
            .unwrap()
            .next_mount_id;

        let preparation = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenPinnedViewAlongside {
                    instance_key: original.clone(),
                },
            },
        });
        assert!(preparation.is_empty());
        assert_eq!(core.active_view_set().unwrap().tiles.len(), 1);
        assert_eq!(
            core.acknowledged_composition_replay()
                .unwrap()
                .next_mount_id,
            frontier
        );
        let producer = core.presentation_operations.producer.clone().unwrap();
        let prepared = pending_prefix(&core);
        assert_eq!(prepared.iter().filter(|event| matches!(&event.kind, crate::ui::seat::SeatEventKind::PresentationOperationPrepared(operation) if operation.composition().is_some_and(|request| matches!(request.mutation, crate::ui::composition_operation::CompositionMutation::InsertPinnedView { .. })))).count(), 1);
        assert!(core.acknowledge_seat_prefix(&producer, prepared).is_empty());
        assert_eq!(
            core.active_view_set().unwrap().tiles.len(),
            1,
            "prepared ACK cannot publish a provisional pin or tile"
        );
        assert_eq!(
            core.acknowledged_composition_replay()
                .unwrap()
                .next_mount_id,
            frontier
        );
        let effects = settle_subject(&mut core);

        assert_eq!(core.active_view_set().unwrap().tiles.len(), 2);
        let new_instance = core.focused_view_instance_key().unwrap();
        assert_ne!(new_instance, original);
        assert_eq!(core.input_buffers[&draft_key].text, "original draft");
        assert!(core.input_buffers.keys().all(|key| {
            !crate::ui::model::InputBufferKey::storage_key_belongs_to(key, &new_instance)
        }));
        assert!(matches!(
            core.selection_attachments.get(&new_instance),
            Some(crate::ui::attachment::SelectionAttachment::Pinned { .. })
        ));
        assert_eq!(
            effects.len(),
            2,
            "both visible mounts refresh their initial channel"
        );
        let new_effects = effects
            .iter()
            .filter(|effect| {
                matches!(&effect.kind, RyeOsEffectKind::FetchSource { tile_id, .. }
                if crate::ui::source_key::RyeOsSourceInstanceKey::decode(tile_id)
                    .is_some_and(|key| key.belongs_to(&new_instance)))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            new_effects.len(),
            1,
            "the new pinned mount opens only its initial channel"
        );
        let RyeOsEffectKind::FetchSource {
            tile_id, request, ..
        } = &new_effects[0].kind
        else {
            panic!("pinned open must fetch its initial source");
        };
        let UiBindingPayload::SourceParameters { params } = &request.payload else {
            panic!("source fetch must carry resolved parameters");
        };
        assert_eq!(params["thread"], "T-one");
        // A newly allocated source key has no prior subject to fence. Its
        // initial request is tracked by the epoch; floors fence replacements.
        assert_eq!(core.data.source_floor.get(tile_id), None);
        assert_eq!(
            core.data.source_epoch.get(tile_id),
            Some(&new_effects[0].id)
        );
        assert_eq!(
            core.data.source_subject_fingerprint.get(tile_id),
            Some(&crate::ui::field::source_subject_fingerprint(
                "service:test/initial",
                params
            ))
        );
        assert!(matches!(
            core.pending_effects.get(&new_effects[0].id),
            Some(RyeOsEffectKind::FetchSource { tile_id: pending_key, request: pending_request, .. })
                if pending_key == tile_id && pending_request == request
        ));

        set_selection(&mut core, owner, "T-two");
        assert_eq!(
            core.facet_value_for_instance(&original, "selection.work"),
            Some(serde_json::json!({"thread": "T-two"}))
        );
        assert_eq!(
            core.facet_value_for_instance(&new_instance, "selection.work.thread"),
            Some(serde_json::json!("T-one"))
        );
    }

    #[test]
    fn readable_selection_view_exposes_open_pinned_alongside() {
        let (mut core, instance) = core_with_selection_view();
        let actions = super::super::view_model::available_command_items(&core);
        assert!(actions.iter().any(|action| matches!(
            &action.intent,
            RyeOsUiIntent::OpenPinnedViewAlongside { instance_key }
                if instance_key == &instance
        )));

        core.active_view_set_mut().unwrap().tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;
        let actions = super::super::view_model::available_command_items(&core);
        assert!(!actions.iter().any(|action| matches!(
            &action.intent,
            RyeOsUiIntent::OpenPinnedViewAlongside { .. }
        )));
    }

    #[test]
    fn attachment_intents_reject_inactive_or_missing_coordinates() {
        let (mut core, instance) = core_with_selection_view();
        core.new_view_set();
        settle_subject(&mut core);
        let active_id = core.active_view_set().unwrap().id;

        assert!(
            core.pin_view_selection(instance.clone()).is_empty(),
            "origin is mounted but not in the active set"
        );
        assert!(
            core.open_pinned_view_alongside(instance.clone()).is_empty(),
            "inactive origins cannot duplicate into the active set"
        );
        assert!(
            core.follow_view_set_selection(instance, ViewSetId::new(u64::MAX))
                .is_empty()
        );
        assert_eq!(core.active_view_set().unwrap().id, active_id);
    }
}
