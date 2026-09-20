use super::effect::RyeOsEffect;
use super::model::RyeOsCore;
use crate::ids::{RyeOsViewInstanceKey, ViewSetId};

impl RyeOsCore {
    fn active_attachment_view(&self, instance: &RyeOsViewInstanceKey) -> Option<String> {
        (self.view_set_index_for_instance(instance) == Some(self.active_view_set))
            .then(|| self.mounted_view_ref(instance).map(str::to_string))
            .flatten()
    }

    fn refresh_attachment_subject(
        &mut self,
        instance: &RyeOsViewInstanceKey,
        view_ref: &str,
    ) -> Vec<RyeOsEffect> {
        let channels = self
            .binding_for_instance(instance, view_ref)
            .map(|binding| binding.sources.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        self.invalidate_view_sources(instance);
        self.reset_field_replay_for_subject(instance);
        let effects = channels
            .into_iter()
            .flat_map(|channel| {
                self.clear_field_expansions_for_channel(instance, &channel);
                self.refresh_source_channel(instance.clone(), view_ref, &channel)
            })
            .collect();
        self.bump_generation();
        effects
    }

    pub(crate) fn pin_view_selection(
        &mut self,
        instance: RyeOsViewInstanceKey,
    ) -> Vec<RyeOsEffect> {
        let Some(view_ref) = self.active_attachment_view(&instance) else {
            return Vec::new();
        };
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
        self.selection_attachments.insert(instance.clone(), pinned);
        self.refresh_attachment_subject(&instance, &view_ref)
    }

    pub(crate) fn open_pinned_view_alongside(
        &mut self,
        instance: RyeOsViewInstanceKey,
    ) -> Vec<RyeOsEffect> {
        let Some(view_ref) = self.active_attachment_view(&instance) else {
            return Vec::new();
        };
        let Some(binding_id) = self
            .binding_attachment_for_instance(&instance)
            .map(|attachment| attachment.binding_attachment_id.clone())
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
        let Some(tile_id) = self.add_tile_motions_under_binding(
            crate::view_set::ViewSpec::bound(&view_ref),
            &binding_id,
        ) else {
            self.notice(
                "The layout cannot open another view alongside this one.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        };
        let Some(new_instance) = self.view_sets[self.active_view_set]
            .tiles
            .get(&tile_id)
            .map(|tile| tile.instance_key.clone())
        else {
            return Vec::new();
        };
        self.stamp_instance_binding(new_instance.clone(), &binding_id);
        self.selection_attachments
            .insert(new_instance.clone(), pinned);
        self.normalize_field_local_states();
        let effects = self.emit_fetch_source_for_instance(new_instance, &view_ref);
        self.floor_source_fetches(&effects, true);
        self.bump_generation();
        effects
    }

    pub(crate) fn follow_view_set_selection(
        &mut self,
        instance: RyeOsViewInstanceKey,
        view_set_id: ViewSetId,
    ) -> Vec<RyeOsEffect> {
        let Some(view_ref) = self.active_attachment_view(&instance) else {
            return Vec::new();
        };
        if !self
            .view_sets
            .iter()
            .any(|view_set| view_set.id == view_set_id)
        {
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
                && self.view_sets[self.active_view_set].id == view_set_id)
        {
            return Vec::new();
        }
        self.selection_attachments.insert(instance.clone(), next);
        self.refresh_attachment_subject(&instance, &view_ref)
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
        self.selection_attachments.insert(instance.clone(), pinned);
        self.invalidate_view_sources(&instance);
        self.reset_field_replay_for_subject(&instance);
        let effects = self.emit_fetch_source_for_instance(instance, &view_ref);
        self.floor_source_fetches(&effects, true);
        self.notice(
            format!("Subject supplied from the current selection for {input}."),
            super::view_model::RyeOsTone::Good,
        );
        self.bump_generation();
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::binding::UiBindingPayload;
    use crate::ui::effect::RyeOsEffectKind;
    use crate::ui::event::{RyeOsEvent, RyeOsUiEvent, RyeOsUiIntent};
    use crate::ui::reducer::test_support::*;

    fn core_with_selection_view() -> (RyeOsCore, RyeOsViewInstanceKey) {
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/selection",
            serde_json::json!({
                "widget": "rows",
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
            }),
        );
        core.add_center_tile(ViewSpec::bound("view:test/selection"));
        let tile = core.view_sets[core.active_view_set].focused_tile;
        let instance = core.view_sets[core.active_view_set].tiles[&tile]
            .instance_key
            .clone();
        (core, instance)
    }

    fn set_selection(core: &mut RyeOsCore, view_set_id: ViewSetId, thread: &str) {
        let key = crate::ui::seat::selection_storage_key(view_set_id, "selection.work").unwrap();
        core.seat
            .append_facet(key, serde_json::json!({"thread": thread}));
    }

    #[test]
    fn unresolved_subject_is_a_source_and_attachment_action_fence() {
        let (mut core, instance) = core_with_selection_view();
        let owner = core.view_sets[core.active_view_set].id;
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
        let mut core = RyeOsCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/source-fence",
            serde_json::json!({
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
            }),
        );
        core.add_center_tile(ViewSpec::bound("view:test/source-fence"));
        let tile = core.view_sets[core.active_view_set].focused_tile;
        let instance = core.view_sets[core.active_view_set].tiles[&tile]
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
        let source = core.view_sets[core.active_view_set].id;
        set_selection(&mut core, source, "T-current");
        core.selection_attachments.insert(
            instance.clone(),
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );

        let effects = core.supply_required_subject(instance.clone(), source);
        assert_eq!(effects.len(), 1, "only the initial source is activated");
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
        let source = core.view_sets[core.active_view_set].id;
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
        let source = core.view_sets[core.active_view_set].id;
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

        let tile_id = instance.view_set_tile_id().unwrap();
        core.view_sets[core.active_view_set].tiles.remove(&tile_id);
        assert!(core.supply_required_subject(instance, source).is_empty());
    }

    #[test]
    fn supply_required_subject_enforces_the_exact_target_byte_bound() {
        let (mut core, instance) = core_with_selection_view();
        let source = core.view_sets[core.active_view_set].id;
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
        assert!(!core.supply_required_subject(instance, source).is_empty());
    }

    #[test]
    fn compatible_follow_replaces_required_subject_without_pinning_values() {
        let (mut core, instance) = core_with_selection_view();
        let source = core.view_sets[core.active_view_set].id;
        set_selection(&mut core, source, "T-current");
        core.selection_attachments.insert(
            instance.clone(),
            crate::ui::attachment::SelectionAttachment::RequiredSubject {
                input: "subject_tile_0".into(),
                facets: vec!["selection.work.thread".into()],
            },
        );

        let effects = core.follow_view_set_selection(instance.clone(), source);
        assert!(!effects.is_empty());
        assert_eq!(
            core.selection_attachments.get(&instance),
            Some(&crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: source,
            })
        );
        set_selection(&mut core, source, "T-later");
        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work.thread"),
            Some(serde_json::json!("T-later"))
        );
    }

    #[test]
    fn pin_keeps_exact_value_and_has_no_write_owner() {
        let (mut core, instance) = core_with_selection_view();
        let owner = core.view_sets[core.active_view_set].id;
        set_selection(&mut core, owner, "T-one");

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::PinViewSelection {
                    instance_key: instance.clone(),
                },
            },
        });
        assert_eq!(effects.len(), 2, "initial and on-demand channels refresh");
        set_selection(&mut core, owner, "T-two");

        assert_eq!(
            core.facet_value_for_instance(&instance, "selection.work"),
            Some(serde_json::json!({"thread": "T-one"}))
        );
        assert!(
            core.facet_storage_key_for_instance(&instance, "selection.work")
                .is_none(),
            "a pinned snapshot is not a mutable selection owner"
        );
    }

    #[test]
    fn follow_rebind_fences_old_sources_and_refreshes_every_channel() {
        let (mut core, instance) = core_with_selection_view();
        let source = core.active_view_set;
        let source_id = core.view_sets[source].id;
        set_selection(&mut core, source_id, "T-source");
        core.new_view_set();
        let target_id = core.view_sets[core.active_view_set].id;
        set_selection(&mut core, target_id, "T-target");
        core.switch_view_set_tab(source);

        let old_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance.clone(), "initial")
                .encode();
        core.data
            .sources
            .insert(old_key.clone(), serde_json::json!({"old": true}));

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::FollowViewSetSelection {
                    instance_key: instance.clone(),
                    view_set_id: target_id,
                },
            },
        });

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
        let owner = core.view_sets[core.active_view_set].id;
        set_selection(&mut core, owner, "T-one");

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::OpenPinnedViewAlongside {
                    instance_key: original.clone(),
                },
            },
        });

        assert_eq!(core.view_sets[core.active_view_set].tiles.len(), 2);
        let new_instance = core.focused_view_instance_key().unwrap();
        assert_ne!(new_instance, original);
        assert!(matches!(
            core.selection_attachments.get(&new_instance),
            Some(crate::ui::attachment::SelectionAttachment::Pinned { .. })
        ));
        assert_eq!(effects.len(), 1, "only the initial channel opens eagerly");
        let RyeOsEffectKind::FetchSource {
            tile_id, request, ..
        } = &effects[0].kind
        else {
            panic!("pinned open must fetch its initial source");
        };
        let UiBindingPayload::SourceParameters { params } = &request.payload else {
            panic!("source fetch must carry resolved parameters");
        };
        assert_eq!(params["thread"], "T-one");
        assert_eq!(core.data.source_floor.get(tile_id), Some(&effects[0].id));

        set_selection(&mut core, owner, "T-two");
        assert_eq!(
            core.facet_value_for_instance(&original, "selection.work"),
            Some(serde_json::json!({"thread": "T-two"}))
        );
        assert_eq!(
            core.facet_value_for_instance(&new_instance, "selection.work"),
            Some(serde_json::json!({"thread": "T-one"}))
        );
    }

    #[test]
    fn readable_selection_view_exposes_open_pinned_alongside() {
        let (mut core, instance) = core_with_selection_view();
        let actions = super::super::view_model::command_overlay_items_for(&core);
        assert!(actions.iter().any(|action| matches!(
            &action.intent,
            RyeOsUiIntent::OpenPinnedViewAlongside { instance_key }
                if instance_key == &instance
        )));

        core.view_sets[core.active_view_set].tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;
        let actions = super::super::view_model::command_overlay_items_for(&core);
        assert!(!actions.iter().any(|action| matches!(
            &action.intent,
            RyeOsUiIntent::OpenPinnedViewAlongside { .. }
        )));
    }

    #[test]
    fn attachment_intents_reject_inactive_or_missing_coordinates() {
        let (mut core, instance) = core_with_selection_view();
        core.new_view_set();
        let active_id = core.view_sets[core.active_view_set].id;

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
        assert_eq!(core.view_sets[core.active_view_set].id, active_id);
    }
}
