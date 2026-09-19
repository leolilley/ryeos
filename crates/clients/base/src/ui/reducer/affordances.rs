use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::model::RyeOsCore;
use crate::view_set::ViewSpec;

impl RyeOsCore {
    pub(crate) fn effects_for_focused_feeds(&mut self) -> Vec<RyeOsEffect> {
        let Some((key, view_ref)) = self.focused_input_instance() else {
            return Vec::new();
        };
        let feeds = self
            .binding_for_instance(&key.view_instance_key, &view_ref)
            .and_then(|binding| binding.input.as_ref())
            .and_then(|input| input.feeds.as_ref())
            .is_some();
        if !feeds {
            return Vec::new();
        }
        // A live filter narrows the list, so a table cursor the operator moved
        // down may now point past the shortened rows — which would make Enter
        // (activate row) a no-op. Reset the owning tile's cursor to the top so
        // the first narrowed row is selected and openable.
        if let Some(tile_id) = self.view_sets[self.active_view_set]
            .tile_ids()
            .into_iter()
            .find(|id| {
                self.view_sets[self.active_view_set]
                    .tiles
                    .get(id)
                    .is_some_and(|tile| tile.instance_key == key.view_instance_key)
            })
        {
            self.set_tile_cursor(tile_id, 0);
        }
        self.emit_fetch_source_for_instance(key.view_instance_key.clone(), &view_ref)
            .into_iter()
            .collect()
    }

    /// Execute a content-declared affordance: resolve the binding,
    /// substitute the row, apply the plane. UI-plane writes append seat
    /// facets (braided when the seat thread is attached) and refetch
    /// every binding subscribed to that facet; rye-plane dispatches
    /// tokens through the one daemon path.
    pub(crate) fn invoke_affordance(
        &mut self,
        instance_key: &crate::ids::RyeOsViewInstanceKey,
        view_ref: &str,
        affordance_id: &str,
        record: &serde_json::Value,
    ) -> Vec<RyeOsEffect> {
        // Browser events carry the mounted instance that projected the row.
        // Revalidate it against retained composition so a delayed or forged
        // event cannot apply a valid affordance to another view set.
        let Some(origin_view_set) = self.view_set_index_for_instance(instance_key) else {
            return Vec::new();
        };
        if origin_view_set != self.active_view_set
            || self.mounted_view_ref(instance_key) != Some(view_ref)
        {
            return Vec::new();
        }
        let Some(binding) = self.binding_for_instance(instance_key, view_ref) else {
            return Vec::new();
        };
        let Some(affordance) = binding
            .affordances
            .iter()
            .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(affordance_id))
            .cloned()
        else {
            return Vec::new();
        };
        let eligibility = super::content::affordance_eligibility(&affordance, record);
        if !eligibility.visible || !eligibility.enabled {
            return Vec::new();
        }
        // Row activation is the `selection` producer: affordances read
        // `{record.<field>}`. Validation is binding-time (fails closed).
        let payload = super::content::Payload::Selection(record);
        match super::content::resolve_affordance_invoke(
            &affordance,
            super::content::Producer::Selection,
            &payload,
        ) {
            Some(super::content::AffordanceInvoke::OpenSavedViewSet { template }) => {
                self.open_view_set_record(instance_key, template)
            }
            Some(super::content::AffordanceInvoke::SaveActiveViewSet {
                context,
                persist_affordance,
            }) => self.save_view_set_record(instance_key, view_ref, &persist_affordance, context),
            Some(super::content::AffordanceInvoke::Ui {
                facet,
                value,
                merge,
                open_view,
                drill,
            }) => self.apply_ui_affordance_from(
                Some(instance_key),
                facet,
                value,
                merge,
                open_view,
                drill,
            ),
            Some(super::content::AffordanceInvoke::Rye { notice, .. })
            | Some(super::content::AffordanceInvoke::Service { notice, .. }) => {
                if self.refuse_blocked_mutation_for_instance(instance_key) {
                    return Vec::new();
                }
                let Some((request, request_bounds)) = self.compiled_binding_operation(
                    instance_key,
                    crate::ui::binding::UiBindingCoordinate::Affordance {
                        view_ref: view_ref.to_string(),
                        affordance_id: affordance_id.to_string(),
                    },
                    crate::ui::binding::UiBindingPayload::Selection {
                        record: record.clone(),
                    },
                ) else {
                    return Vec::new();
                };
                vec![self.emit(RyeOsEffectKind::InvokeBinding {
                    request,
                    request_bounds,
                    intent: super::effect::InvokeIntent::Service,
                    success_notice: notice,
                    invocation_origin: Some(instance_key.clone()),
                    input_origin: None,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            None => Vec::new(),
        }
    }

    /// Apply a resolved Ui-plane affordance: write the seat facet (value
    /// replaces; merge folds into the existing value) and refetch every
    /// binding subscribed to that facet.
    pub(crate) fn apply_ui_affordance(
        &mut self,
        facet: String,
        value: Option<serde_json::Value>,
        merge: Option<serde_json::Value>,
        open_view: Option<String>,
        drill: bool,
    ) -> Vec<RyeOsEffect> {
        let origin = self.focused_view_instance_key();
        self.apply_ui_affordance_from(origin.as_ref(), facet, value, merge, open_view, drill)
    }

    pub(crate) fn apply_ui_affordance_from(
        &mut self,
        origin: Option<&crate::ids::RyeOsViewInstanceKey>,
        facet: String,
        value: Option<serde_json::Value>,
        merge: Option<serde_json::Value>,
        open_view: Option<String>,
        drill: bool,
    ) -> Vec<RyeOsEffect> {
        let route_subject = facet == super::seat::KEY_INPUT_ROUTE;
        let opening_binding = origin
            .and_then(|instance| self.binding_attachment_for_instance(instance))
            .map(|attachment| attachment.binding_attachment_id.clone());
        if open_view.is_some() && opening_binding.is_none() {
            self.notice(
                "The originating view's admitted binding is unavailable.",
                super::view_model::RyeOsTone::Warn,
            );
            return Vec::new();
        }
        let selection_subject =
            facet == super::seat::KEY_SELECTION || facet.starts_with("selection.");
        let origin_selection_attachment = selection_subject
            .then(|| origin.and_then(|instance| self.selection_attachment_for_instance(instance)))
            .flatten();
        let selection_view_set = if selection_subject {
            match origin_selection_attachment.as_ref() {
                Some(crate::ui::attachment::SelectionAttachment::FollowViewSet { view_set_id }) => {
                    self.view_sets.iter().position(|set| set.id == *view_set_id)
                }
                Some(crate::ui::attachment::SelectionAttachment::Pinned { .. }) => {
                    self.notice("This view's selection is pinned. Follow a view set before changing its selection.", super::view_model::RyeOsTone::Warn);
                    return Vec::new();
                }
                None => None,
            }
        } else {
            None
        };
        if selection_subject && selection_view_set.is_none() {
            return Vec::new();
        }
        // Step-in: before the drill writes its facet (and possibly swaps the
        // center), record a return frame — the view being left plus the facet
        // context it was reading — so a later pop restores them. Only on
        // single-lens surfaces (where the drill is otherwise irreversible) and
        // only when there is a center to leave. Captured BEFORE the facet write,
        // so the frame holds the pre-drill values. Note: a drill need NOT carry
        // `open_view` — a same-lens route retarget (stepping the braid timeline
        // onto a child chain) is still a returnable step-in.
        if drill
            && self.view_sets[self.active_view_set].tiling.mode
                == crate::surface::TilingModeSpec::SingleLens
            && !self.view_sets[self.active_view_set].center_is_empty()
            && let Some(view) = self.view_sets[self.active_view_set].focused_view().cloned()
        {
            let focused_instance = self.focused_view_instance_key();
            let attachment = focused_instance
                .as_ref()
                .and_then(|instance| self.selection_attachment_for_instance(instance));
            let selection_view_set_id = match &attachment {
                Some(super::super::attachment::SelectionAttachment::FollowViewSet {
                    view_set_id,
                }) => Some(*view_set_id),
                Some(super::super::attachment::SelectionAttachment::Pinned { .. }) => None,
                None => Some(self.view_sets[self.active_view_set].id),
            };
            let folded = self.seat.fold();
            let dependencies = self
                .focused_view_instance_key()
                .and_then(|instance| self.binding_for_instance(&instance, &view.view_ref))
                .map(super::super::attachment::facet_dependencies)
                .unwrap_or_default();
            let facets = dependencies
                .into_iter()
                .filter_map(|facet| {
                    let key = if facet == super::super::seat::KEY_SELECTION
                        || facet.starts_with("selection.")
                    {
                        super::super::seat::selection_storage_key(selection_view_set_id?, &facet)?
                    } else if facet == super::super::seat::KEY_INPUT_ROUTE {
                        super::super::seat::input_route_facet_key(focused_instance.as_ref()?)
                    } else {
                        facet
                    };
                    let value = folded.get(&key).filter(|value| !value.is_null()).cloned();
                    Some((key, value))
                })
                .collect();
            // The frame carries the label of the level being left (the
            // current lens label), so the breadcrumb reads the ancestor
            // cognitions, not repeated view titles.
            let label = self.view_sets[self.active_view_set].lens_label.clone();
            let Some(binding_attachment_id) = self
                .focused_view_instance_key()
                .and_then(|instance| self.instance_binding_attachments.get(&instance).cloned())
            else {
                return Vec::new();
            };
            self.view_sets[self.active_view_set].push_lens_frame(
                binding_attachment_id,
                view,
                facets,
                label,
                attachment,
            );
        }
        // A route carried by an affordance belongs to the view it opens. Mount
        // that view first so its durable instance key—not current focus—is the
        // subject coordinate. The initial fetch is fenced below and refreshed
        // against the newly written route.
        let mut effects = if route_subject {
            open_view
                .as_ref()
                .map(|view_ref| {
                    self.open_view_under_binding(
                        ViewSpec::bound(view_ref.clone()),
                        opening_binding.as_deref().expect("open attachment checked"),
                    )
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let route_instance = if route_subject {
            if let Some(view_ref) = open_view.as_ref() {
                // An explicitly opened destination may own the route only if
                // it actually mounted. Refused opens must not retarget the
                // previously focused view.
                self.focused_view_instance_key().filter(|instance| {
                    self.mounted_view_ref(instance) == Some(view_ref.as_str())
                        && self
                            .binding_attachment_for_instance(instance)
                            .map(|binding| binding.binding_attachment_id.as_str())
                            == opening_binding.as_deref()
                })
            } else {
                origin.cloned()
            }
        } else {
            None
        };
        if route_subject && route_instance.is_none() {
            self.notice(
                "The route destination is not mounted.",
                super::view_model::RyeOsTone::Warn,
            );
            return effects;
        }
        let storage_facet = if let Some(index) = selection_view_set {
            super::seat::selection_storage_key(self.view_sets[index].id, &facet)
                .expect("selection subject was classified above")
        } else {
            route_instance
                .as_ref()
                .map(super::seat::input_route_facet_key)
                .unwrap_or_else(|| facet.clone())
        };
        let next = if let Some(merge) = merge {
            let mut current = if route_subject {
                route_instance
                    .as_ref()
                    .map(|instance| {
                        serde_json::to_value(self.route_for_instance(instance))
                            .expect("InputRoute serializes")
                    })
                    .unwrap_or_else(|| serde_json::json!({}))
            } else {
                self.seat
                    .fold()
                    .get(&storage_facet)
                    .cloned()
                    .unwrap_or(serde_json::json!({}))
            };
            if let (Some(target), Some(patch)) = (current.as_object_mut(), merge.as_object()) {
                for (key, val) in patch {
                    target.insert(key.clone(), val.clone());
                }
            }
            current
        } else {
            value.unwrap_or(serde_json::Value::Null)
        };
        self.seat.append_facet(storage_facet, next);
        // Only the input-route contract gives the shared client a typed thread
        // label. Other signed facets (including inspection selection) are
        // intentionally opaque: deriving their breadcrumb from input.route
        // would falsely label inspection as a composer retarget, while
        // interpreting their authored value would hard-code product schemas.
        if drill {
            self.view_sets[self.active_view_set].lens_label = (facet
                == super::seat::KEY_INPUT_ROUTE)
                .then(|| self.focused_input_route().thread)
                .flatten();
        }
        self.bump_generation();
        let refreshed = if let Some(instance) = route_instance.as_ref() {
            self.effects_for_view_instance(instance)
        } else if let Some(index) = selection_view_set {
            self.effects_for_facet_in_view_set(&facet, index)
        } else {
            self.effects_for_facet(&facet)
        };
        if route_subject {
            self.floor_source_fetches(&refreshed, true);
        }
        effects.extend(refreshed);
        // Open the view AFTER the facet write, so the opened view's fetch
        // resolves its `@facet:` params against the value just written (e.g. a
        // row drill-in sets input.route.chain_root, then the braid lens fetches
        // that chain). Single-lens surfaces replace the center in place.
        if !route_subject && let Some(view_ref) = open_view {
            let active = self.active_view_set;
            let mounted_before = self.view_sets[active]
                .tiles
                .values()
                .map(|tile| tile.instance_key.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let existing_destination =
                self.view_sets[active]
                    .tile_ids()
                    .into_iter()
                    .find_map(|tile_id| {
                        let tile = self.view_sets[active].tiles.get(&tile_id)?;
                        (tile.view.view_ref == view_ref
                            && self
                                .binding_attachment_for_instance(&tile.instance_key)
                                .map(|binding| binding.binding_attachment_id.as_str())
                                == opening_binding.as_deref())
                        .then(|| tile.instance_key.clone())
                    });
            let replaced_destination = (existing_destination.is_none()
                && self.view_sets[active].tiling.mode
                    == crate::surface::TilingModeSpec::SingleLens
                && !self.view_sets[active].center_is_empty())
            .then(|| {
                self.view_sets[active]
                    .tiles
                    .get(&self.view_sets[active].focused_tile)
                    .map(|tile| tile.instance_key.clone())
            })
            .flatten();
            let premature = self.open_view_under_binding(
                ViewSpec::bound(view_ref.clone()),
                opening_binding.as_deref().expect("open attachment checked"),
            );
            // Resolve the result of this exact layout operation. Current
            // keyboard focus is not a subject-routing input.
            let destination = existing_destination
                .or(replaced_destination)
                .or_else(|| {
                    self.view_sets[active].tiles.values().find_map(|tile| {
                        (tile.view.view_ref == view_ref
                            && !mounted_before.contains(&tile.instance_key))
                        .then(|| tile.instance_key.clone())
                    })
                })
                .filter(|instance| {
                    self.mounted_view_ref(instance) == Some(view_ref.as_str())
                        && self
                            .binding_attachment_for_instance(instance)
                            .map(|binding| binding.binding_attachment_id.as_str())
                            == opening_binding.as_deref()
                });
            let destination_participates = destination
                .as_ref()
                .and_then(|instance| self.binding_for_instance(instance, &view_ref))
                .is_some_and(super::super::attachment::participates_in_selection);
            if selection_subject
                && destination_participates
                && let (Some(destination), Some(attachment)) =
                    (destination, origin_selection_attachment)
            {
                // `open_view` necessarily resolves initial sources before this
                // relationship can be installed. Cancel those default-context
                // requests and evict their coordinates, then issue the exact
                // same view through the origin's retained attachment.
                self.invalidate_view_sources(&destination);
                self.selection_attachments
                    .insert(destination.clone(), attachment);
                effects.retain(|effect| self.pending_effects.contains_key(&effect.id));
                effects.extend(self.emit_fetch_source_for_instance(destination, &view_ref));
            } else {
                effects.extend(premature);
            }
        }
        effects
    }

    /// Facet write arrived: refetch every bound tile or visible dock whose binding
    /// declares `refresh.on_facet: <key>` or whose source params
    /// reference the facet explicitly.
    pub fn effects_for_facet(&mut self, facet: &str) -> Vec<RyeOsEffect> {
        self.effects_for_facet_in_view_set(facet, self.active_view_set)
    }

    pub(crate) fn effects_for_facet_in_view_set(
        &mut self,
        facet: &str,
        view_set_index: usize,
    ) -> Vec<RyeOsEffect> {
        let subscribed_channels = |binding: &super::content::ViewBinding| {
            let cursor_scope_depends_on_facet = binding.field_state.as_ref().is_some_and(|state| {
                state.cursor_scope.subject.iter().any(|subject| {
                    subject == &format!("@facet:{facet}")
                        || subject.starts_with(&format!("@facet:{facet}."))
                })
            });
            binding
                .sources
                .iter()
                .filter(|(_, source)| {
                    // Absent/null inherits the view policy; an explicit empty
                    // object is a declaration of no per-source liveness.
                    let refresh = if source.refresh.is_null() {
                        &binding.refresh
                    } else {
                        &source.refresh
                    };
                    refresh.get("on_facet").and_then(|v| v.as_str()) == Some(facet)
                        || serde_json::to_string(&source.params)
                            .unwrap_or_default()
                            .contains(&format!("@facet:{facet}"))
                        || (cursor_scope_depends_on_facet
                            && serde_json::to_string(&source.params)
                                .unwrap_or_default()
                                .contains("@field:cursor"))
                })
                .map(|(channel, _)| channel.clone())
                .collect::<Vec<_>>()
        };
        let selection_change =
            facet == super::seat::KEY_SELECTION || facet.starts_with("selection.");
        let owner = self.view_sets[view_set_index].id;
        let mut mounted = Vec::new();
        for (index, set) in self.view_sets.iter().enumerate() {
            if !selection_change && index != view_set_index {
                continue;
            }
            mounted.extend(
                set.tiles
                    .values()
                    .map(|tile| (tile.instance_key.clone(), tile.view.view_ref.clone())),
            );
            for edge in [
                super::model::RyeOsDockEdge::Top,
                super::model::RyeOsDockEdge::Bottom,
                super::model::RyeOsDockEdge::Left,
                super::model::RyeOsDockEdge::Right,
            ] {
                if let Some(slot) = set.docks.slot(edge) {
                    let super::model::RyeOsDockContent::View { view_ref } = &slot.content;
                    mounted.push((
                        super::model::dock_view_instance_key(set.id, edge),
                        view_ref.clone(),
                    ));
                }
            }
        }
        let targets = mounted.into_iter().filter_map(|(instance_key, view_ref)| {
            if selection_change && !matches!(self.selection_attachment_for_instance(&instance_key),
                Some(crate::ui::attachment::SelectionAttachment::FollowViewSet { view_set_id }) if view_set_id == owner) {
                return None;
            }
            let channels = self
                .binding_for_instance(&instance_key, &view_ref)
                .map(&subscribed_channels)
                .unwrap_or_default();
            (!channels.is_empty()).then_some((instance_key, view_ref, channels))
        }).collect::<Vec<_>>();
        targets
            .into_iter()
            .flat_map(|(instance_key, view_ref, channels)| {
                // A facet write means the SUBJECT changed (a new selection,
                // a new route). Fence only subscribed named channels before
                // resolving their new parameters; unrelated evidence stays
                // mounted and keeps its accepted revision.
                self.reset_field_replay_for_subject(&instance_key);
                channels
                    .into_iter()
                    .flat_map(|channel| {
                        self.clear_field_expansions_for_channel(&instance_key, &channel);
                        self.refresh_source_channel(instance_key.clone(), &view_ref, &channel)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Refresh route-dependent sources for one mounted view. `input.route`
    /// is a logical name in signed view data, but its live value is owned by
    /// the mounted view instance; route changes must never refetch or retarget
    /// sibling views.
    pub(crate) fn effects_for_view_instance(
        &mut self,
        instance: &crate::ids::RyeOsViewInstanceKey,
    ) -> Vec<RyeOsEffect> {
        let target = self.view_sets.iter().find_map(|view_set| {
            view_set
                .tiles
                .values()
                .find_map(|tile| {
                    (&tile.instance_key == instance)
                        .then(|| (tile.instance_key.clone(), tile.view.view_ref.clone()))
                })
                .or_else(|| {
                    view_set
                        .docks
                        .visible_slot_views()
                        .into_iter()
                        .find_map(|(edge, view_ref)| {
                            (super::model::dock_view_instance_key(view_set.id, edge) == *instance)
                                .then(|| (instance.clone(), view_ref))
                        })
                })
        });
        let Some((instance, view_ref)) = target else {
            return Vec::new();
        };
        let Some(binding) = self.binding_for_instance(&instance, &view_ref) else {
            return Vec::new();
        };
        let channels = binding
            .sources
            .iter()
            .filter_map(|(channel, source)| {
                let refresh = if source.refresh.is_null() {
                    &binding.refresh
                } else {
                    &source.refresh
                };
                let references_route = serde_json::to_string(&source.params)
                    .unwrap_or_default()
                    .contains("@facet:input.route");
                (references_route
                    || refresh.get("on_facet").and_then(serde_json::Value::as_str)
                        == Some(super::seat::KEY_INPUT_ROUTE))
                .then(|| channel.clone())
            })
            .collect::<Vec<_>>();
        self.reset_field_replay_for_subject(&instance);
        channels
            .into_iter()
            .flat_map(|channel| {
                self.clear_field_expansions_for_channel(&instance, &channel);
                self.refresh_source_channel(instance.clone(), &view_ref, &channel)
            })
            .collect()
    }

    /// Resolve the atlas arrangement a `SetAtlas*` event targets: `Some(tile)`
    /// → that tile's per-tile arrangement, created from the default on first
    /// touch; `None` → the ambient backdrop atlas.
    pub(crate) fn atlas_target_mut(
        &mut self,
        tile_id: &Option<String>,
    ) -> &mut crate::atlas::AtlasUiStateVm {
        match tile_id {
            Some(id) => self.ui.tile_atlas.entry(id.clone()).or_default(),
            None => &mut self.ui.atlas,
        }
    }

    pub(crate) fn atlas_target(&self, tile_id: &Option<String>) -> &crate::atlas::AtlasUiStateVm {
        match tile_id {
            Some(id) => self.ui.tile_atlas.get(id).unwrap_or(&self.ui.atlas),
            None => &self.ui.atlas,
        }
    }

    pub(crate) fn effects_for_view(&mut self, view: &ViewSpec) -> Vec<RyeOsEffect> {
        let view_ref = view.view_ref.clone();
        let tile_id = self.view_sets[self.active_view_set].focused_tile;
        self.emit_fetch_source(tile_id, &view_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::*;

    fn source_request(effect: &RyeOsEffect) -> Option<(&str, &str, &str, &serde_json::Value)> {
        let RyeOsEffectKind::FetchSource {
            tile_id, request, ..
        } = &effect.kind
        else {
            return None;
        };
        let crate::ui::binding::UiBindingCoordinate::Source { view_ref, channel } =
            &request.coordinate
        else {
            return None;
        };
        let crate::ui::binding::UiBindingPayload::SourceParameters { params } = &request.payload
        else {
            return None;
        };
        Some((
            tile_id.as_str(),
            view_ref.as_str(),
            channel.as_str(),
            params,
        ))
    }

    /// Mount the view that actually originated a synthetic affordance event.
    ///
    /// Production events carry a mounted instance identity and the reducer
    /// rejects a view ref borrowed from any other tile. Tests must preserve
    /// that same boundary instead of borrowing whichever fixture tile happens
    /// to be focused.
    fn mount_affordance_view(
        core: &mut RyeOsCore,
        view_ref: &str,
    ) -> crate::ids::RyeOsViewInstanceKey {
        if let Some(instance_key) = core.view_sets[core.active_view_set]
            .tiles
            .values()
            .find(|tile| tile.view.view_ref == view_ref)
            .map(|tile| tile.instance_key.clone())
        {
            return instance_key;
        }
        let tile_id = core.view_sets[core.active_view_set]
            .add_tile(ViewSpec {
                view_ref: view_ref.to_string(),
            })
            .expect("fixture layout accepts affordance source");
        core.view_sets[core.active_view_set].tiles[&tile_id]
            .instance_key
            .clone()
    }

    #[test]
    fn route_write_without_open_destination_uses_origin_not_keyboard_focus() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let origin = mount_affordance_view(&mut core, "view:test/origin");
        let other = mount_affordance_view(&mut core, "view:test/other");
        assert_eq!(core.focused_view_instance_key(), Some(other.clone()));
        core.apply_ui_affordance_from(
            Some(&origin),
            crate::ui::seat::KEY_INPUT_ROUTE.into(),
            Some(serde_json::json!({"thread": "T-origin"})),
            None,
            None,
            false,
        );
        let fold = core.seat.fold();
        assert_eq!(
            fold.get(&crate::ui::seat::input_route_facet_key(&origin))
                .unwrap()["thread"],
            "T-origin"
        );
        assert!(
            fold.get(&crate::ui::seat::input_route_facet_key(&other))
                .is_none()
        );
        assert!(fold.get(crate::ui::seat::KEY_INPUT_ROUTE).is_none());
    }

    #[test]
    fn pinned_selection_neither_refreshes_nor_writes_its_former_owner() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/pinned",
            serde_json::json!({
                "widget":"rows", "sources":{"default":{
                    "ref":"service:test/read", "params":{"thread":"@facet:selection.work.thread"}
                }}
            }),
        );
        let origin = mount_affordance_view(&mut core, "view:test/pinned");
        let owner = core.active_view_set;
        let key =
            crate::ui::seat::selection_storage_key(core.view_sets[owner].id, "selection.work")
                .unwrap();
        core.seat
            .append_facet(key.clone(), serde_json::json!({"thread":"T-one"}));
        let pin = core.capture_pinned_selection(&origin).unwrap();
        core.selection_attachments.insert(origin.clone(), pin);
        core.seat
            .append_facet(key.clone(), serde_json::json!({"thread":"T-two"}));
        assert!(
            core.effects_for_facet_in_view_set("selection.work", owner)
                .is_empty()
        );
        assert!(
            core.apply_ui_affordance_from(
                Some(&origin),
                "selection.work".into(),
                Some(serde_json::json!({"thread":"T-three"})),
                None,
                None,
                false
            )
            .is_empty()
        );
        assert_eq!(core.seat.fold().get(&key).unwrap()["thread"], "T-two");
        assert_eq!(
            core.facet_value_for_instance(&origin, "selection.work")
                .unwrap()["thread"],
            "T-one"
        );
    }

    fn cross_set_follower_fixture() -> (
        RyeOsCore,
        usize,
        crate::ids::ViewSetId,
        crate::ids::RyeOsViewInstanceKey,
        String,
    ) {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/follower",
            serde_json::json!({
                "widget": "rows",
                "sources": {"default": {
                    "ref": "service:test/follower",
                    "params": {"thread": "@facet:selection.work.thread"}
                }},
                "affordances": [
                    {
                        "id": "select-work",
                        "invoke": {
                            "plane": "ui",
                            "facet": "selection.work",
                            "value": {"thread": "{record.thread}"}
                        }
                    },
                    {
                        "id": "open-work",
                        "invoke": {
                            "plane": "ui",
                            "facet": "selection.work",
                            "value": {"thread": "{record.thread}"},
                            "open_view": "view:test/detail"
                        }
                    }
                ]
            }),
        );
        seed_view_value(
            &mut core,
            "view:test/detail",
            serde_json::json!({
                "widget": "text",
                "sources": {"default": {
                    "ref": "service:test/detail",
                    "params": {"thread": "@facet:selection.work.thread"}
                }}
            }),
        );
        let owner = core.active_view_set;
        let owner_id = core.view_sets[owner].id;
        core.view_sets[owner]
            .add_tile(ViewSpec::bound("view:test/owner"))
            .expect("owner set accepts an origin view");
        core.new_view_set();
        let follower_tile = core.view_sets[core.active_view_set]
            .add_tile(ViewSpec::bound("view:test/follower"))
            .unwrap();
        let follower = core.view_sets[core.active_view_set].tiles[&follower_tile]
            .instance_key
            .clone();
        let source_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(follower.clone(), "default")
                .encode();
        core.selection_attachments.insert(
            follower.clone(),
            crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: owner_id,
            },
        );
        (core, owner, owner_id, follower, source_key)
    }

    #[test]
    fn owner_selection_change_refreshes_cross_set_follower_from_owner_value() {
        let (mut core, owner, owner_id, follower, source_key) = cross_set_follower_fixture();
        core.switch_view_set_tab(owner);
        core.data
            .sources
            .insert(source_key.clone(), serde_json::json!({"old": true}));
        let origin = core
            .focused_view_instance_key()
            .expect("owner set has a mounted focused view");

        let effects = core.apply_ui_affordance_from(
            Some(&origin),
            "selection.work".into(),
            Some(serde_json::json!({"thread": "T-owner"})),
            None,
            None,
            false,
        );

        assert_eq!(
            core.seat
                .fold()
                .get(&crate::ui::seat::selection_storage_key(owner_id, "selection.work").unwrap())
                .unwrap()["thread"],
            "T-owner"
        );
        assert!(!core.data.sources.contains_key(&source_key));
        assert!(matches!(
            effects.first().and_then(source_request),
            Some((fetched, "view:test/follower", "default", params))
                if fetched == source_key && params["thread"] == "T-owner"
        ));
        assert_eq!(
            core.facet_value_for_instance(&follower, "selection.work")
                .unwrap()["thread"],
            "T-owner"
        );
    }

    #[test]
    fn cross_set_follower_write_targets_followed_owner_and_refreshes_itself() {
        let (mut core, owner, owner_id, follower, source_key) = cross_set_follower_fixture();
        let follower_set = core.active_view_set;
        assert_ne!(follower_set, owner);

        let effects = core.invoke_affordance(
            &follower,
            "view:test/follower",
            "select-work",
            &serde_json::json!({"thread": "T-written"}),
        );

        let fold = core.seat.fold();
        let owner_key = crate::ui::seat::selection_storage_key(owner_id, "selection.work").unwrap();
        let containing_key = crate::ui::seat::selection_storage_key(
            core.view_sets[follower_set].id,
            "selection.work",
        )
        .unwrap();
        assert_eq!(fold.get(&owner_key).unwrap()["thread"], "T-written");
        assert!(fold.get(&containing_key).is_none());
        assert!(matches!(
            effects.first().and_then(source_request),
            Some((fetched, "view:test/follower", "default", params))
                if fetched == source_key && params["thread"] == "T-written"
        ));
    }

    #[test]
    fn drill_captures_selection_from_followed_owner_not_containing_set() {
        let (mut core, _owner, owner_id, follower, _) = cross_set_follower_fixture();
        core.view_sets[core.active_view_set].tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;
        let containing_id = core.view_sets[core.active_view_set].id;
        let owner_key = crate::ui::seat::selection_storage_key(owner_id, "selection.work").unwrap();
        let containing_key =
            crate::ui::seat::selection_storage_key(containing_id, "selection.work").unwrap();
        core.seat
            .append_facet(owner_key.clone(), serde_json::json!({"thread":"T-owner"}));
        core.seat.append_facet(
            containing_key.clone(),
            serde_json::json!({"thread":"T-containing"}),
        );

        core.apply_ui_affordance_from(
            Some(&follower),
            "selection.work".into(),
            Some(serde_json::json!({"thread":"T-child"})),
            None,
            None,
            true,
        );

        let frame = core.view_sets[core.active_view_set]
            .lens_stack
            .last()
            .expect("drill pushes a return frame");
        assert_eq!(
            frame.facets.get(&owner_key),
            Some(&Some(serde_json::json!({"thread":"T-owner"})))
        );
        assert!(!frame.facets.contains_key(&containing_key));
    }

    #[test]
    fn selection_open_transfers_origin_attachment_and_discards_default_fetch() {
        let (mut core, _owner, owner_id, follower, _) = cross_set_follower_fixture();
        let containing_id = core.view_sets[core.active_view_set].id;
        core.seat.append_facet(
            crate::ui::seat::selection_storage_key(containing_id, "selection.work").unwrap(),
            serde_json::json!({"thread": "T-wrong-default"}),
        );

        let effects = core.invoke_affordance(
            &follower,
            "view:test/follower",
            "open-work",
            &serde_json::json!({"thread": "T-owner-open"}),
        );

        let destination = core
            .focused_view_instance_key()
            .expect("opened destination is focused");
        assert_eq!(
            core.mounted_view_ref(&destination),
            Some("view:test/detail")
        );
        assert_eq!(
            core.selection_attachment_for_instance(&destination),
            Some(crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: owner_id
            })
        );
        let detail_fetches = effects
            .iter()
            .filter_map(source_request)
            .filter(|(_, view_ref, _, _)| *view_ref == "view:test/detail")
            .collect::<Vec<_>>();
        assert_eq!(detail_fetches.len(), 1);
        assert_eq!(detail_fetches[0].3["thread"], "T-owner-open");
        assert!(
            effects
                .iter()
                .all(|effect| core.pending_effects.contains_key(&effect.id)),
            "discarded default-context effects must not escape the reducer"
        );
    }

    #[test]
    fn selection_open_does_not_attach_an_unrelated_destination() {
        let (mut core, _owner, _owner_id, follower, _) = cross_set_follower_fixture();
        seed_view(&mut core, "view:test/unrelated");

        core.apply_ui_affordance_from(
            Some(&follower),
            "selection.work".into(),
            Some(serde_json::json!({"thread": "T-owner-open"})),
            None,
            Some("view:test/unrelated".into()),
            false,
        );

        let destination = core
            .focused_view_instance_key()
            .expect("opened destination is focused");
        assert_eq!(
            core.mounted_view_ref(&destination),
            Some("view:test/unrelated")
        );
        assert!(!core.selection_attachments.contains_key(&destination));
        assert_eq!(
            core.followed_selection_view_set(&destination),
            Some(core.view_sets[core.active_view_set].id)
        );
    }

    #[test]
    fn explicit_empty_source_refresh_disables_inherited_facet_liveness() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/static",
            serde_json::json!({
                "widget": "text",
                "sources": {
                    "default": {
                        "ref": "service:test/static",
                        "params": {},
                        "refresh": {}
                    }
                },
                "refresh": {"on_facet": "selection"}
            }),
        );
        core.view_sets[core.active_view_set]
            .add_tile(ViewSpec {
                view_ref: "view:test/static".to_string(),
            })
            .expect("fixture layout accepts view");

        assert!(
            core.effects_for_facet("selection").is_empty(),
            "an explicit per-source empty policy is not an inheritance fallback"
        );
    }

    #[test]
    fn invoke_affordance_ui_plane_writes_facet_and_refetches_subscribers() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/list",
            serde_json::json!({
                "widget": "rows",
                "sources": { "default": { "ref": "service:test/list", "params": {}, "collection": "rows" } },
                "affordances": [{
                    "id": "select-item",
                    "invoke": {
                        "plane": "ui",
                        "facet": "selection",
                        "value": { "item": "{record.canonical_ref}" }
                    }
                }]
            }),
        );
        seed_view_value(
            &mut core,
            "view:test/inspector",
            serde_json::json!({
                "widget": "key_value",
                "sources": { "default": {
                    "ref": "service:test/inspect",
                    "params": { "canonical_ref": "@facet:selection.item" }
                } }
            }),
        );
        let tile_id = core.view_sets[core.active_view_set]
            .add_tile(ViewSpec {
                view_ref: "view:test/inspector".to_string(),
            })
            .expect("fixture layout accepts view");
        let source_key = crate::ui::source_key::RyeOsSourceInstanceKey::named(
            core.view_sets[core.active_view_set].tiles[&tile_id]
                .instance_key
                .clone(),
            "default",
        )
        .encode();

        let source_instance = mount_affordance_view(&mut core, "view:test/list");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:test/list".to_string(),
                    affordance_id: "select-item".to_string(),
                    record: serde_json::json!({ "canonical_ref": "tool:demo/run" }),
                },
            },
        });

        assert_eq!(active_selection(&core)["item"], "tool:demo/run");
        assert!(matches!(
            effects.first().and_then(source_request),
            Some((fetched_tile, "view:test/inspector", "default", params))
                if fetched_tile == source_key && params["canonical_ref"] == "tool:demo/run"
        ));
    }

    #[test]
    fn delayed_affordance_from_inactive_view_set_is_rejected() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/list",
            serde_json::json!({
                "widget": "rows",
                "sources": { "default": { "ref": "service:test/list", "params": {}, "collection": "rows" } },
                "affordances": [{
                    "id": "select-item",
                    "invoke": {
                        "plane": "ui",
                        "facet": "selection",
                        "value": { "item": "{record.canonical_ref}" }
                    }
                }]
            }),
        );
        let origin = mount_affordance_view(&mut core, "view:test/list");
        let origin_view_set_id = core.view_sets[core.active_view_set].id;
        core.new_view_set();
        let active_view_set_id = core.view_sets[core.active_view_set].id;

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: origin,
                    view_ref: "view:test/list".to_string(),
                    affordance_id: "select-item".to_string(),
                    record: serde_json::json!({ "canonical_ref": "tool:delayed/run" }),
                },
            },
        });

        assert!(effects.is_empty());
        let facets = core.seat.fold();
        for view_set_id in [origin_view_set_id, active_view_set_id] {
            assert!(
                facets
                    .get(&super::super::seat::selection_facet_key(view_set_id))
                    .is_none(),
                "delayed affordance must not write selection in either view set"
            );
        }
    }

    #[test]
    fn ui_affordance_open_view_writes_route_then_opens_view_with_new_facet() {
        // The watch drill-in (P1): a row activation merges route {thread, chain_root}
        // AND opens the braid lens, whose fetch must resolve the just-written chain_root.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "table",
                "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "params": {}, "collection": "threads" } },
                "affordances": [{
                    "id": "watch",
                    "invoke": {
                        "plane": "ui",
                        "facet": "input.route",
                        "merge": { "thread": "{record.thread_id}", "chain_root": "{record.chain_root_id}" },
                        "open_view": "view:ryeos/thread/transcript"
                    }
                }]
            }),
        );
        seed_view_value(
            &mut core,
            "view:ryeos/thread/transcript",
            serde_json::json!({
                "widget": "timeline",
                "sources": { "default": {
                    "ref": "service:events/chain_replay",
                    "params": { "chain_root_id": "@facet:input.route.chain_root" },
                    "collection": "events"
                } }
            }),
        );

        let source_instance = mount_affordance_view(&mut core, "view:ryeos/threads/list");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:ryeos/threads/list".to_string(),
                    affordance_id: "watch".to_string(),
                    record: serde_json::json!({ "thread_id": "T-9", "chain_root_id": "T-root" }),
                },
            },
        });

        // 1. Route facet carries BOTH thread and chain_root.
        let fold = core.seat.fold();
        let route = fold.get("input.route").expect("route facet written");
        assert_eq!(route["thread"], "T-9");
        assert_eq!(route["chain_root"], "T-root");

        // 2. The braid lens was opened as a tile.
        assert!(
            core.view_sets[core.active_view_set]
                .tiles
                .values()
                .any(|t| t.view.view_ref == "view:ryeos/thread/transcript"),
            "drill-in opens the braid lens"
        );

        // 3. Its fetch resolved the just-written chain_root (write-then-open order).
        assert!(
            effects.iter().filter_map(source_request).any(
                |(_, view_ref, channel, params)| view_ref == "view:ryeos/thread/transcript"
                    && channel == "default"
                    && params["chain_root_id"] == "T-root"
            ),
            "timeline fetch must use the selected chain_root; got {effects:?}"
        );
    }

    #[test]
    fn service_ref_affordance_emits_bound_selection_coordinate() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "table",
                "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "params": {}, "collection": "threads" } },
                "affordances": [{
                    "id": "cancel",
                    "invoke": {
                        "plane": "rye",
                        "ref": "service:commands/submit",
                        "args": { "thread_id": "{record.thread_id}", "command_type": "cancel" }
                    }
                }]
            }),
        );

        let source_instance = mount_affordance_view(&mut core, "view:ryeos/threads/list");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:ryeos/threads/list".to_string(),
                    affordance_id: "cancel".to_string(),
                    record: serde_json::json!({ "thread_id": "T-7" }),
                },
            },
        });

        assert!(
            matches!(effects.first().map(|e| &e.kind),
                Some(RyeOsEffectKind::InvokeBinding {
                    request: crate::ui::binding::UiBindingRequest {
                        coordinate: crate::ui::binding::UiBindingCoordinate::Affordance { view_ref, affordance_id },
                        payload: crate::ui::binding::UiBindingPayload::Selection { record },
                        ..
                    },
                    ..
                }) if view_ref == "view:ryeos/threads/list"
                    && affordance_id == "cancel"
                    && record["thread_id"] == "T-7"),
            "service affordance must send only its compiled coordinate and selection; got {effects:?}"
        );
    }

    #[test]
    fn service_affordance_does_not_infer_navigation_from_target_ref() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/projects",
            serde_json::json!({
                "widget": "table",
                "sources": { "default": {
                    "ref": "service:test/projects",
                    "params": {},
                    "collection": "projects"
                } },
                "affordances": [{
                    "id": "open",
                    "invoke": {
                        "plane": "rye",
                        "ref": "service:projects/open",
                        "args": { "local_id": "{record.local_id}" }
                    }
                }]
            }),
        );

        let source_instance = mount_affordance_view(&mut core, "view:test/projects");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:test/projects".to_string(),
                    affordance_id: "open".to_string(),
                    record: serde_json::json!({ "local_id": "project-7" }),
                },
            },
        });

        assert!(matches!(
            effects.as_slice(),
            [RyeOsEffect {
                kind: RyeOsEffectKind::InvokeBinding {
                    request: crate::ui::binding::UiBindingRequest {
                        coordinate: crate::ui::binding::UiBindingCoordinate::Affordance { view_ref, affordance_id },
                        payload: crate::ui::binding::UiBindingPayload::Selection { record },
                        ..
                    },
                    ..
                },
                ..
            }] if view_ref == "view:test/projects"
                && affordance_id == "open"
                && record == &serde_json::json!({ "local_id": "project-7" })
        ));
    }

    #[test]
    fn actual_threads_list_watch_affordance_drills_into_braid() {
        // The shipped product contract: the real threads/list.yaml `watch`
        // affordance drills a row into its braid.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let binding: crate::ui::content::ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml"
        ))
        .unwrap();
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .views
            .insert("view:ryeos/threads/list".to_string(), binding);
        seed_view_value(
            &mut core,
            "view:ryeos/thread/transcript",
            serde_json::json!({
                "widget": "timeline",
                "sources": { "default": {
                    "ref": "service:events/chain_replay",
                    "params": { "chain_root_id": "@facet:input.route.chain_root" },
                    "collection": "events"
                } }
            }),
        );
        // The signed initial route is the template for a newly opened view;
        // its fields must survive the instance-local subject merge.
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .initial_input_route = serde_json::from_value(serde_json::json!({
            "params": { "directive": "directive:ryeos/ops/base" }
        }))
        .unwrap();

        let source_instance = mount_affordance_view(&mut core, "view:ryeos/threads/list");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:ryeos/threads/list".to_string(),
                    affordance_id: "watch".to_string(),
                    record: serde_json::json!({ "thread_id": "T-9", "chain_root_id": "T-root" }),
                },
            },
        });

        let route = focused_route_value(&core);
        assert_eq!(route["thread"], "T-9");
        assert_eq!(route["chain_root"], "T-root");
        assert_eq!(
            route["params"]["directive"], "directive:ryeos/ops/base",
            "merge preserves existing route fields"
        );
        assert!(
            core.view_sets[core.active_view_set]
                .tiles
                .values()
                .any(|t| t.view.view_ref == "view:ryeos/thread/transcript"),
            "watch opens the braid lens"
        );
        assert!(
            effects.iter().filter_map(source_request).any(
                |(_, view_ref, channel, params)| view_ref == "view:ryeos/thread/transcript"
                    && channel == "default"
                    && params["chain_root_id"] == "T-root"
            ),
            "timeline fetch uses the row's chain_root; got {effects:?}"
        );
    }

    #[test]
    fn shipped_comparison_actions_merge_operands_and_fetch_only_when_complete() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let history: crate::ui::content::ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/history.yaml"
        ))
        .unwrap();
        let comparison: crate::ui::content::ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/runs/comparison.yaml"
        ))
        .unwrap();
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .views
            .insert("view:ryeos/threads/history".to_string(), history);
        core.binding_attachments
            .get_mut("fixture-attachment")
            .unwrap()
            .views
            .insert("view:ryeos/runs/comparison".to_string(), comparison);
        core.view_sets[core.active_view_set]
            .add_tile(ViewSpec {
                view_ref: "view:ryeos/runs/comparison".to_string(),
            })
            .expect("fixture layout accepts view");

        let source_instance = mount_affordance_view(&mut core, "view:ryeos/threads/history");
        let left_effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance.clone(),
                    view_ref: "view:ryeos/threads/history".to_string(),
                    affordance_id: "compare-left".to_string(),
                    record: serde_json::json!({ "thread_id": "T-left" }),
                },
            },
        });
        assert_eq!(
            core.seat.fold().get("comparison").unwrap()["left_thread_id"],
            "T-left"
        );
        assert!(
            !left_effects
                .iter()
                .filter_map(source_request)
                .any(|(_, view_ref, _, _)| view_ref == "view:ryeos/runs/comparison"),
            "a missing right operand must suppress comparison fetch"
        );

        let right_effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:ryeos/threads/history".to_string(),
                    affordance_id: "compare-right".to_string(),
                    record: serde_json::json!({ "thread_id": "T-right" }),
                },
            },
        });
        let fold = core.seat.fold();
        let facet = fold.get("comparison").unwrap();
        assert_eq!(facet["left_thread_id"], "T-left");
        assert_eq!(facet["right_thread_id"], "T-right");
        assert!(
            right_effects.iter().filter_map(source_request).any(
                |(_, view_ref, _, params)| view_ref == "view:ryeos/runs/comparison"
                    && params["left_thread_id"] == "T-left"
                    && params["right_thread_id"] == "T-right"
            )
        );
    }

    #[test]
    fn shipped_threads_list_cancel_uses_the_single_command_submit_path() {
        // Steering guard (05c §3): the ryeos has exactly ONE cancel path — the
        // audited command channel `service:commands/submit` with
        // `command_type: cancel`. The row Cancel affordance in the shipped
        // list.yaml must target that, and the killed raw-cancel service refs
        // (`service:ui/ryeos-ui/thread/cancel`, `service:threads/cancel`) must be
        // gone from every affordance in the view.
        let binding: crate::ui::content::ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml"
        ))
        .unwrap();

        let cancel = binding
            .affordances
            .iter()
            .find(|a| a.get("id").and_then(|v| v.as_str()) == Some("cancel"))
            .expect("shipped threads/list must offer a `cancel` affordance");
        let invoke = cancel.get("invoke").expect("cancel affordance has invoke");
        assert_eq!(
            invoke.get("ref").and_then(|v| v.as_str()),
            Some("service:commands/submit"),
            "cancel must route through the single audited command path"
        );
        assert_eq!(
            invoke
                .get("args")
                .and_then(|a| a.get("command_type"))
                .and_then(|v| v.as_str()),
            Some("cancel"),
            "cancel affordance submits command_type: cancel"
        );

        // The killed cancel forms appear nowhere in the view's affordances.
        let all = serde_json::to_string(&binding.affordances).unwrap();
        assert!(
            !all.contains("service:ui/ryeos-ui/thread/cancel"),
            "the ui/ryeos/thread/cancel affordance route is gone"
        );
        assert!(
            !all.contains("service:threads/cancel"),
            "the raw threads/cancel affordance route is gone"
        );

        // Resolving it identifies the signed service form and bound row data.
        // The emitted effect itself carries only the compiled affordance
        // coordinate (no bespoke cancel effect and no executable target).
        let record = serde_json::json!({ "thread_id": "T-42" });
        let resolved = crate::ui::content::resolve_affordance_invoke(
            cancel,
            crate::ui::content::Producer::Selection,
            &crate::ui::content::Payload::Selection(&record),
        )
        .expect("cancel affordance resolves for a row");
        match resolved {
            crate::ui::content::AffordanceInvoke::Service { item_ref, args, .. } => {
                assert_eq!(item_ref, "service:commands/submit");
                assert_eq!(args["thread_id"], "T-42");
                assert_eq!(args["command_type"], "cancel");
            }
            other => panic!("cancel must resolve to a Service invoke; got {other:?}"),
        }
    }

    #[test]
    fn drill_pushes_return_frame_and_pop_restores_braid_and_facets() {
        // Step-in / return over the single-lens braid — the debugger drill.
        // Stepping from the game braid (chain_root A) into a child braid
        // (chain_root B) pushes a return frame; PopLens restores A and refetches
        // it. This is the vertical-drill primitive the execution tracer hangs on.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set].tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;
        core.view_sets[core.active_view_set]
            .add_tile(ViewSpec::bound("view:ryeos/thread/transcript"))
            .expect("fixture layout accepts view");
        seed_view_value(
            &mut core,
            "view:ryeos/thread/transcript",
            serde_json::json!({
                "widget": "timeline",
                "sources": { "default": {
                    "ref": "service:events/chain_replay",
                    "params": { "chain_root_id": "@facet:input.route.chain_root" },
                    "collection": "events"
                } }
            }),
        );
        // On the game braid (chain_root A).
        set_focused_route_value(&mut core, serde_json::json!({ "chain_root": "A" }));

        // Step into the child braid (chain_root B) with drill = true.
        core.apply_ui_affordance(
            crate::ui::seat::KEY_INPUT_ROUTE.to_string(),
            None,
            Some(serde_json::json!({ "chain_root": "B" })),
            Some("view:ryeos/thread/transcript".to_string()),
            true,
        );

        // A return frame captured the pre-drill braid; the fold now reads B.
        assert_eq!(core.view_sets[core.active_view_set].lens_depth(), 1);
        assert_eq!(
            focused_route_value(&core),
            serde_json::json!({ "chain_root": "B" })
        );

        // Return: PopLens restores chain_root A and refetches that braid.
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::PopLens,
        });
        assert_eq!(core.view_sets[core.active_view_set].lens_depth(), 0);
        assert_eq!(
            focused_route_value(&core),
            serde_json::json!({ "chain_root": "A" })
        );
        assert!(
            effects
                .iter()
                .filter_map(source_request)
                .any(
                    |(_, view_ref, _, params)| view_ref == "view:ryeos/thread/transcript"
                        && params["chain_root_id"] == "A"
                ),
            "pop refetches the restored braid at chain_root A; got {effects:?}"
        );
    }

    #[test]
    fn pop_lens_at_top_of_tree_is_a_noop() {
        // No return frame → PopLens does nothing (no panic, no effects).
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::PopLens,
        });
        assert_eq!(core.view_sets[core.active_view_set].lens_depth(), 0);
        assert!(effects.is_empty());
    }

    #[test]
    fn drill_thread_steps_into_child_braid_and_pop_returns() {
        // The cross-thread step-in (the deepest debugger drill, ready for the
        // run-stability child_thread_spawned edge): DrillThread retargets the
        // route at the child AND pushes a return frame — no open_view, the braid
        // lens re-projects via the route facet. Backspace returns to the parent.
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set].tiling.mode =
            crate::surface::TilingModeSpec::SingleLens;
        core.view_sets[core.active_view_set]
            .add_tile(ViewSpec::bound("view:ryeos/thread/transcript"))
            .expect("fixture layout accepts view");
        seed_view_value(
            &mut core,
            "view:ryeos/thread/transcript",
            serde_json::json!({
                "widget": "timeline",
                "sources": { "default": {
                    "ref": "service:events/chain_replay",
                    "params": { "chain_root_id": "@facet:input.route.chain_root" },
                    "collection": "events"
                } }
            }),
        );
        // On the parent braid (chain_root P).
        set_focused_route_value(&mut core, serde_json::json!({ "chain_root": "P" }));

        // Step into child C (a fresh root: both coords = C).
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::DrillThread {
                    thread_id: "C".to_string(),
                    chain_root_id: "C".to_string(),
                    label: Some("study".to_string()),
                },
            },
        });
        assert_eq!(core.view_sets[core.active_view_set].lens_depth(), 1);
        // The breadcrumb tail reads the node stepped into, not the child id.
        assert_eq!(
            core.view_sets[core.active_view_set].lens_label.as_deref(),
            Some("study")
        );
        let route = focused_route_value(&core);
        assert_eq!(route["thread"], "C");
        assert_eq!(route["chain_root"], "C");
        // The braid lens refetched onto the child chain via the route facet.
        assert!(
            effects
                .iter()
                .filter_map(source_request)
                .any(
                    |(_, view_ref, _, params)| view_ref == "view:ryeos/thread/transcript"
                        && params["chain_root_id"] == "C"
                ),
            "drill refetches the child braid; got {effects:?}"
        );

        // Return to the parent braid.
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::PopLens,
        });
        assert_eq!(core.view_sets[core.active_view_set].lens_depth(), 0);
        assert_eq!(
            core.view_sets[core.active_view_set].lens_label, None,
            "pop restores the top-of-tree label"
        );
        assert_eq!(
            focused_route_value(&core),
            serde_json::json!({ "chain_root": "P" }),
            "pop restores the pre-drill route (parent braid, no child thread)"
        );
    }

    #[test]
    fn invoke_affordance_rye_plane_emits_bound_selection_coordinate() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/threads",
            serde_json::json!({
                "widget": "rows",
                "sources": { "default": { "ref": "service:test/threads", "params": {}, "collection": "rows" } },
                "affordances": [{
                    "id": "cancel",
                    "invoke": {
                        "plane": "rye",
                        "ref": "service:commands/dispatch",
                        "tokens": ["thread", "cancel"],
                        "args": { "thread_id": "{record.thread_id}" }
                    }
                }]
            }),
        );

        let source_instance = mount_affordance_view(&mut core, "view:test/threads");
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:test/threads".to_string(),
                    affordance_id: "cancel".to_string(),
                    record: serde_json::json!({ "thread_id": "T-demo" }),
                },
            },
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(RyeOsEffectKind::InvokeBinding {
                request: crate::ui::binding::UiBindingRequest {
                    coordinate: crate::ui::binding::UiBindingCoordinate::Affordance { view_ref, affordance_id },
                    payload: crate::ui::binding::UiBindingPayload::Selection { record },
                    ..
                },
                route_seq: None,
                ..
            }) if view_ref == "view:test/threads"
                && affordance_id == "cancel"
                && record["thread_id"] == "T-demo"
        ));
    }

    #[test]
    fn invoke_affordance_ui_merge_folds_into_existing_facet() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        set_focused_route_value(
            &mut core,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "params": { "directive": "directive:demo/base" }
            }),
        );
        seed_view_value(
            &mut core,
            "view:test/threads",
            serde_json::json!({
                "widget": "rows",
                "sources": { "default": { "ref": "service:test/threads", "params": {}, "collection": "rows" } },
                "affordances": [{
                    "id": "aim-input",
                    "invoke": {
                        "plane": "ui",
                        "facet": "input.route",
                        "merge": { "thread": "{record.thread_id}" }
                    }
                }]
            }),
        );

        let source_instance = mount_affordance_view(&mut core, "view:test/threads");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:test/threads".to_string(),
                    affordance_id: "aim-input".to_string(),
                    record: serde_json::json!({ "thread_id": "T-route" }),
                },
            },
        });

        let route = focused_route_value(&core);
        assert_eq!(route["params"]["directive"], "directive:demo/base");
        assert_eq!(route["thread"], "T-route");
    }

    #[test]
    fn inspection_drill_does_not_borrow_the_composer_route_label() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        set_focused_route_value(&mut core, serde_json::json!({ "thread": "T-composer" }));
        seed_view_value(
            &mut core,
            "view:test/work",
            serde_json::json!({
                "widget": "rows",
                "sources": { "default": { "ref": "service:test/work", "params": {}, "collection": "rows" } },
                "affordances": [{
                    "id": "inspect",
                    "invoke": {
                        "plane": "ui",
                        "facet": "selection",
                        "merge": { "work": { "thread": "{record.thread_id}" } },
                        "drill": true
                    }
                }]
            }),
        );

        let source_instance = mount_affordance_view(&mut core, "view:test/work");
        core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::Activate {
                intent: RyeOsUiIntent::InvokeAffordance {
                    instance_key: source_instance,
                    view_ref: "view:test/work".to_string(),
                    affordance_id: "inspect".to_string(),
                    record: serde_json::json!({ "thread_id": "T-inspected" }),
                },
            },
        });

        assert_eq!(active_selection(&core)["work"]["thread"], "T-inspected");
        assert_eq!(core.view_sets[core.active_view_set].lens_label, None);
    }

    #[test]
    fn directive_threads_dock_renders_bound_view_rows() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set]
            .docks
            .left
            .as_mut()
            .unwrap()
            .visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "rows",
                "sources": { "default": { "ref": "service:ui/ryeos-ui/threads/list", "params": {}, "collection": "rows" } },
                "projections": { "primary": "thread_id", "meta": "item_ref" }
            }),
        );
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::named(
                crate::ui::model::dock_view_instance_key(
                    core.view_sets[core.active_view_set].id,
                    crate::ui::model::RyeOsDockEdge::Left,
                ),
                "default",
            )
            .encode(),
            serde_json::json!({
                "rows": [{
                    "thread_id": "T-running",
                    "item_ref": "directive:demo/chat",
                    "status": "running"
                }]
            }),
        );

        let vm = build_view_model(&core);
        let dock = vm.view_set.docks.left.expect("left dock");
        assert!(dock.input.is_none(), "a rows view declares no input");
        match dock.view {
            crate::ui::view_model::RyeOsViewVm::Rows { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].primary, "T-running");
                assert_eq!(rows[0].meta.as_deref(), Some("directive:demo/chat"));
            }
            other => panic!("expected bound rows dock view, got {other:?}"),
        }
    }

    #[test]
    fn identical_views_in_two_sets_keep_selection_independent() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        let first_view = core.view_sets[0]
            .focused_view()
            .expect("fixture has a center view")
            .view_ref
            .clone();
        seed_view_value(
            &mut core,
            &first_view,
            serde_json::json!({
                "widget": "rows",
                "sources": {},
                "affordances": [{
                    "id": "select",
                    "invoke": {
                        "plane": "ui",
                        "facet": "selection",
                        "value": { "item": "{record.item}" }
                    }
                }]
            }),
        );
        core.view_sets
            .push(core.view_sets[0].duplicate_composition());
        let first_instance = core.view_sets[0].tiles[&core.view_sets[0].focused_tile]
            .instance_key
            .clone();
        let second_instance = core.view_sets[1].tiles[&core.view_sets[1].focused_tile]
            .instance_key
            .clone();

        core.invoke_affordance(
            &first_instance,
            &first_view,
            "select",
            &serde_json::json!({ "item": "first" }),
        );
        core.invoke_affordance(
            &second_instance,
            &first_view,
            "select",
            &serde_json::json!({ "item": "second" }),
        );

        let fold = core.seat.fold();
        let first_key = super::super::seat::selection_facet_key(core.view_sets[0].id);
        let second_key = super::super::seat::selection_facet_key(core.view_sets[1].id);
        assert_eq!(fold.get(&first_key).unwrap()["item"], "first");
        assert_eq!(fold.get(&second_key).unwrap()["item"], "second");
        assert_ne!(first_key, second_key);
    }

    #[test]
    fn sections_view_assembles_one_group_per_section_from_its_own_source() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set]
            .docks
            .left
            .as_mut()
            .unwrap()
            .visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "sections",
                "sources": {
                    "threads": { "ref": "service:ui/ryeos-ui/threads/list" },
                    "bundles": { "ref": "service:ui/ryeos-ui/items/list" }
                },
                "sections": [
                    {
                        "title": "Threads",
                        "source_channel": "threads",
                        "collection": "rows",
                        "projection": { "primary": "thread_id", "meta": "status" }
                    },
                    {
                        "title": "Bundles",
                        "source_channel": "bundles",
                        "collection": "rows",
                        "projection": { "primary": "name", "meta": "version" }
                    }
                ]
            }),
        );
        // Each section's response lands under its own per-section key.
        let instance_key = crate::ui::model::dock_view_instance_key(
            core.view_sets[core.active_view_set].id,
            crate::ui::model::RyeOsDockEdge::Left,
        );
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "threads")
                .encode(),
            serde_json::json!({ "rows": [
                { "thread_id": "T-ab", "status": "running" },
                { "thread_id": "T-cd", "status": "done" }
            ]}),
        );
        core.data.sources.insert(
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key, "bundles").encode(),
            serde_json::json!({ "rows": [ { "name": "ryeos", "version": "v1.0.0" } ]}),
        );

        let vm = build_view_model(&core);
        let dock = vm.view_set.docks.left.expect("left dock");
        match dock.view {
            crate::ui::view_model::RyeOsViewVm::Sections { sections, .. } => {
                assert_eq!(sections.len(), 2);
                assert_eq!(sections[0].title, "Threads");
                assert_eq!(sections[0].count, 2);
                assert_eq!(sections[0].rows[0].primary, "T-ab");
                assert_eq!(sections[0].rows[0].meta.as_deref(), Some("running"));
                assert_eq!(sections[1].title, "Bundles");
                assert_eq!(sections[1].count, 1);
                assert_eq!(sections[1].rows[0].primary, "ryeos");
                assert_eq!(sections[1].rows[0].meta.as_deref(), Some("v1.0.0"));
            }
            other => panic!("expected bound sections dock view, got {other:?}"),
        }
    }

    #[test]
    fn sections_view_without_a_loaded_source_shows_an_empty_group() {
        let mut core = RyeOsCore::new(writable_session(), BrowserViewport::default(), 0);
        core.view_sets[core.active_view_set]
            .docks
            .left
            .as_mut()
            .unwrap()
            .visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "sections",
                "sources": {
                    "threads": {
                        "ref": "service:ui/ryeos-ui/threads/list",
                        "collection": "rows"
                    }
                },
                "sections": [{
                    "title": "Threads",
                    "source_channel": "threads",
                    "collection": "rows",
                    "projection": { "primary": "thread_id" }
                }]
            }),
        );
        // No source seeded → the section is present but empty (count 0), not a
        // placeholder: the surface is up, the data just hasn't arrived.
        let vm = build_view_model(&core);
        match vm.view_set.docks.left.expect("left dock").view {
            crate::ui::view_model::RyeOsViewVm::Sections { sections, .. } => {
                assert_eq!(sections.len(), 1);
                assert_eq!(sections[0].count, 0);
                assert!(sections[0].rows.is_empty());
            }
            other => panic!("expected sections dock view, got {other:?}"),
        }
    }
}
