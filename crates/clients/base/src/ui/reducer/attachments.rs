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
            .views
            .get(view_ref)
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
        let Some(tile_id) = self.add_tile_motions(crate::view_set::ViewSpec::bound(&view_ref))
        else {
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
