use super::effect::{RyeOsEffect, RyeOsEffectKind};
use super::model::RyeOsCore;
use crate::view_set::ViewSpec;

impl RyeOsCore {
    pub(crate) fn effects_for_focused_feeds(&mut self) -> Vec<RyeOsEffect> {
        let Some((key, view_ref)) = self.focused_input_instance() else {
            return Vec::new();
        };
        if self.move_freeze_restricts_instance(&key.view_instance_key) {
            return Vec::new();
        }
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
        self.set_view_cursor(&key.view_instance_key, 0);
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
        if self.move_freeze_restricts_instance(instance_key) {
            return Vec::new();
        }
        // Browser events carry the mounted instance that projected the row.
        // Revalidate it against retained composition so a delayed or forged
        // event cannot apply a valid affordance to another view set.
        if !self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.mount(instance_key))
            .is_some_and(|mount| mount.visible())
            || self.mounted_view_ref(instance_key) != Some(view_ref)
        {
            return Vec::new();
        }
        if self.instance_has_unresolved_required_subject(instance_key) {
            self.notice(
                "This view requires a subject before it can act.",
                super::view_model::RyeOsTone::Warn,
            );
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
        // The authored placeholder requests the current exact composition
        // destination. It is never accepted from a browser row or inferred by
        // the daemon from a subsequently focused Surface.
        let enriched;
        let record = if affordance
            .pointer("/invoke/args/placement")
            .and_then(serde_json::Value::as_str)
            == Some("{record.opening_placement}")
        {
            if record.get("opening_placement").is_some() {
                return Vec::new();
            }
            let Ok(destination) = self.composition_destination(instance_key) else {
                return Vec::new();
            };
            let Ok(replay) = self.acknowledged_composition_replay() else {
                return Vec::new();
            };
            let Some(surface) = replay
                .composition
                .surfaces()
                .get(&destination.surface_instance_id)
            else {
                return Vec::new();
            };
            let Some(mut object) = record.as_object().cloned() else {
                return Vec::new();
            };
            let placement = crate::surface::opening::SurfaceOpenPlacement::AppendTabs {
                destination_surface_instance_id: destination.surface_instance_id,
                destination_binding: surface.binding().clone(),
                expected_composition_revision: replay.revision,
                expected_next_mount_id: replay.next_mount_id,
                insertion_index: surface.view_sets().len(),
                activate_imported_tab: true,
            };
            let Ok(value) = serde_json::to_value(placement) else {
                return Vec::new();
            };
            object.insert("opening_placement".into(), value);
            enriched = serde_json::Value::Object(object);
            &enriched
        } else {
            record
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
            Some(super::content::AffordanceInvoke::SaveParticularViewSet {
                context,
                persist_affordance,
            }) => self.save_particular_view_set_record(
                instance_key,
                view_ref,
                &persist_affordance,
                context,
            ),
            Some(super::content::AffordanceInvoke::SaveActiveSurface {
                context,
                persist_affordance,
            }) => self.save_surface_record(instance_key, view_ref, &persist_affordance, context),
            Some(super::content::AffordanceInvoke::SaveParticularSurface {
                context,
                persist_affordance,
            }) => self.save_particular_surface_record(
                instance_key,
                view_ref,
                &persist_affordance,
                context,
            ),
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
            Some(
                resolved @ (super::content::AffordanceInvoke::Rye { .. }
                | super::content::AffordanceInvoke::Service { .. }),
            ) => {
                let (notice, service_args) = match resolved {
                    super::content::AffordanceInvoke::Rye { notice, .. } => (notice, None),
                    super::content::AffordanceInvoke::Service { notice, args, .. } => {
                        (notice, Some(args))
                    }
                    _ => return Vec::new(),
                };
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
                if let Some(purpose) = affordance.get("presentation_operation") {
                    let Some(service_args) = service_args.as_ref() else {
                        return Vec::new();
                    };
                    let Ok((purpose, surface_open_target)) =
                        crate::ui::admission_operation::resolve_presentation_operation(
                            purpose,
                            service_args,
                        )
                    else {
                        return Vec::new();
                    };
                    let Ok(particular_selection) = crate::ui::admission_operation::UiParticularAdmissionSelection::from_resolved_params(
                        purpose, service_args,
                    ) else {
                        return Vec::new();
                    };
                    return self.prepare_presentation_operation(
                        request,
                        request_bounds,
                        purpose,
                        instance_key.clone(),
                        notice,
                        surface_open_target,
                        particular_selection,
                    );
                }
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
        if origin.is_some_and(|instance| self.move_freeze_restricts_instance(instance)) {
            return Vec::new();
        }
        if origin.is_some_and(|instance| {
            !self
                .presentation
                .as_ref()
                .and_then(|presentation| presentation.mount(instance))
                .is_some_and(|mount| mount.visible())
        }) {
            return Vec::new();
        }
        // Shared mounts have no implicit Surface insertion target. An open
        // destination requires the originating active Surface-local tab.
        if open_view.is_some()
            && !origin
                .and_then(|instance| self.view_set_for_instance(instance))
                .is_some_and(|set| {
                    self.active_view_set()
                        .is_some_and(|active| active.id == set.id)
                })
        {
            return Vec::new();
        }
        let route_subject = facet == super::seat::KEY_INPUT_ROUTE;
        if route_subject
            && origin.is_some_and(|instance| self.route_write_fenced_by_composition(instance))
        {
            return Vec::new();
        }
        let selection_subject =
            facet == super::seat::KEY_SELECTION || facet.starts_with("selection.");
        if drill
            && selection_subject
            && origin
                .and_then(|instance| self.view_set_for_instance(instance))
                .is_some_and(|set| set.tiling.mode == crate::surface::TilingModeSpec::SingleLens)
        {
            let Some(origin) = origin else {
                return Vec::new();
            };
            let destination = open_view
                .as_deref()
                .or_else(|| self.mounted_view_ref(origin));
            let Some(destination) = destination else {
                return Vec::new();
            };
            // The destination is not mounted yet. Resolve it within the
            // origin's exact admitted closure, never via a current-mount lookup
            // or the active tab's potentially different insertion authority.
            let Some(binding) = self
                .binding_context_for_instance(origin)
                .and_then(|context| context.views.get(destination))
            else {
                return Vec::new();
            };
            let dependencies = crate::ui::attachment::selection_dependencies(binding);
            let mut patch = value.unwrap_or(serde_json::Value::Null);
            if let Some(merge) = merge {
                patch = self
                    .facet_value_for_instance(origin, &facet)
                    .unwrap_or_else(|| serde_json::json!({}));
                let (Some(fields), Some(changes)) = (patch.as_object_mut(), merge.as_object())
                else {
                    return Vec::new();
                };
                fields.extend(
                    changes
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
            }
            let mut values = std::collections::BTreeMap::new();
            for dependency in dependencies {
                let next = if dependency == facet {
                    Some(patch.clone())
                } else if let Some(relative) = dependency.strip_prefix(&format!("{facet}.")) {
                    crate::ui::content::field_path(&patch, relative).cloned()
                } else if let Some(relative) = facet.strip_prefix(&format!("{dependency}.")) {
                    let mut previous = self
                        .facet_value_for_instance(origin, &dependency)
                        .unwrap_or_else(|| serde_json::json!({}));
                    let parts = relative.split('.').collect::<Vec<_>>();
                    let mut current = &mut previous;
                    for part in &parts[..parts.len() - 1] {
                        let Some(fields) = current.as_object_mut() else {
                            return Vec::new();
                        };
                        current = fields
                            .entry((*part).to_string())
                            .or_insert_with(|| serde_json::json!({}));
                    }
                    let Some(fields) = current.as_object_mut() else {
                        return Vec::new();
                    };
                    fields.insert(parts[parts.len() - 1].into(), patch.clone());
                    Some(previous)
                } else {
                    self.facet_value_for_instance(origin, &dependency)
                };
                let Some(next) = next.filter(|value| !value.is_null()) else {
                    return Vec::new();
                };
                values.insert(dependency, next);
            }
            use sha2::{Digest, Sha256};
            let fingerprint = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&values).expect("subject values serialize"))
            );
            let attachment = crate::ui::attachment::SelectionAttachment::Pinned {
                values,
                fingerprint,
            };
            let route = Some(
                serde_json::to_value(self.route_for_instance(origin))
                    .expect("exact route serializes"),
            );
            return self.prepare_lens_drill(origin, open_view, attachment, route, None);
        }
        if drill
            && route_subject
            && origin
                .and_then(|instance| self.view_set_for_instance(instance))
                .is_some_and(|set| set.tiling.mode == crate::surface::TilingModeSpec::SingleLens)
        {
            let Some(origin) = origin else {
                return Vec::new();
            };
            let route = if let Some(patch) = merge {
                let mut route = serde_json::to_value(self.route_for_instance(origin))
                    .expect("InputRoute serializes");
                let (Some(fields), Some(patch)) = (route.as_object_mut(), patch.as_object()) else {
                    return Vec::new();
                };
                fields.extend(
                    patch
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
                Some(route)
            } else {
                value.filter(|value| !value.is_null())
            };
            let label = route
                .as_ref()
                .and_then(|route| route.get("thread"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            return self.prepare_lens_route_drill(origin, open_view, route, label);
        }
        if route_subject && let Some(open_view) = open_view {
            let Some(origin) = origin else {
                return Vec::new();
            };
            let Some(binding_id) = self
                .binding_attachment_for_instance(origin)
                .map(|binding| binding.binding_attachment_id.clone())
            else {
                return Vec::new();
            };
            let route = if let Some(patch) = merge {
                let mut route = serde_json::to_value(self.route_for_instance(origin))
                    .expect("InputRoute serializes");
                let (Some(fields), Some(patch)) = (route.as_object_mut(), patch.as_object()) else {
                    return Vec::new();
                };
                fields.extend(
                    patch
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
                Some(route)
            } else {
                value.filter(|value| !value.is_null())
            };
            return self.open_view_under_binding_with_route(
                ViewSpec::bound(open_view),
                &binding_id,
                crate::ui::composition_operation::UiCompositionRouteAssignment::Override {
                    input_route: route,
                },
            );
        }
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
        let origin_selection_attachment = selection_subject
            .then(|| origin.and_then(|instance| self.selection_attachment_for_instance(instance)))
            .flatten();
        let selection_view_set = if selection_subject {
            match origin_selection_attachment.as_ref() {
                Some(crate::ui::attachment::SelectionAttachment::FollowViewSet { view_set_id }) => {
                    origin
                        .filter(|instance| self.same_surface(instance, *view_set_id))
                        .and_then(|_| self.view_set(*view_set_id))
                        .map(|set| set.id)
                }
                Some(crate::ui::attachment::SelectionAttachment::Pinned { .. }) => {
                    self.notice("This view's selection is pinned. Follow a view set before changing its selection.", super::view_model::RyeOsTone::Warn);
                    return Vec::new();
                }
                Some(crate::ui::attachment::SelectionAttachment::RequiredSubject { .. }) => {
                    self.notice(
                        "This view requires a subject before its selection can change.",
                        super::view_model::RyeOsTone::Warn,
                    );
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
        if selection_view_set.is_some_and(|set| self.move_freeze_restricts_view_set(set)) {
            return Vec::new();
        }
        let mut effects = Vec::new();
        let route_instance = if route_subject { origin.cloned() } else { None };
        if route_subject && route_instance.is_none() {
            self.notice(
                "The route destination is not mounted.",
                super::view_model::RyeOsTone::Warn,
            );
            return effects;
        }
        let storage_facet = if let Some(index) = selection_view_set {
            super::seat::selection_storage_key(index, &facet)
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
            if let Some(attachment) = origin_selection_attachment.clone() {
                let attachment_id = opening_binding.as_deref().expect("open attachment checked");
                let Some(descriptor) = self.binding_attachment(attachment_id) else {
                    return effects;
                };
                let binding = crate::ui::binding::UiBindingAttachmentCoordinate {
                    binding_attachment_id: descriptor.binding_attachment_id.clone(),
                    binding_generation: descriptor.binding_generation,
                    binding_digest: descriptor.binding_digest.clone(),
                };
                let Some(set) = self.active_view_set() else {
                    return effects;
                };
                use crate::ui::composition_operation::{
                    CompositionMutation, CompositionPlacement, UiCompositionRouteAssignment,
                };
                let mutation = if set.tiling.mode == crate::surface::TilingModeSpec::SingleLens
                    && !set.center_is_empty()
                {
                    CompositionMutation::ReplaceFollowingView {
                        tab_id: set.id,
                        tile_id: set.focused_tile,
                        view_ref,
                        binding,
                        attachment,
                        route_assignment: UiCompositionRouteAssignment::Authored,
                    }
                } else {
                    let placement = if let Some(target) = set.tile_ids().last().copied() {
                        CompositionPlacement::Split {
                            target,
                            axis: match set.tiling.stack.arrange {
                                crate::surface::ArrangeSpec::Horizontal => {
                                    crate::layout::SplitAxis::Horizontal
                                }
                                crate::surface::ArrangeSpec::Vertical => {
                                    crate::layout::SplitAxis::Vertical
                                }
                            },
                            before: false,
                            ratio: 0.5,
                        }
                    } else {
                        CompositionPlacement::First
                    };
                    CompositionMutation::InsertFollowingView {
                        tab_id: set.id,
                        placement,
                        view_ref,
                        binding,
                        attachment,
                        route_assignment: UiCompositionRouteAssignment::Authored,
                    }
                };
                effects.extend(self.prepare_composition_mutation(mutation));
                return effects;
            }
            effects.extend(self.open_view_under_binding(
                ViewSpec::bound(view_ref),
                opening_binding.as_deref().expect("open attachment checked"),
            ));
        }
        effects
    }

    /// Facet write arrived: refetch every bound tile or visible dock whose binding
    /// declares `refresh.on_facet: <key>` or whose source params
    /// reference the facet explicitly.
    pub fn effects_for_facet(&mut self, facet: &str) -> Vec<RyeOsEffect> {
        let selection = facet == super::seat::KEY_SELECTION || facet.starts_with("selection.");
        if selection {
            let Some(owner) = self.active_view_set().map(|set| set.id) else {
                return Vec::new();
            };
            return self.effects_for_facet_in_view_set(facet, owner);
        }
        self.effects_for_facet_mounts(facet, None)
    }

    pub(crate) fn effects_for_facet_in_view_set(
        &mut self,
        facet: &str,
        view_set_id: crate::ids::ViewSetId,
    ) -> Vec<RyeOsEffect> {
        if self.view_set(view_set_id).is_none() {
            return Vec::new();
        }
        self.effects_for_facet_mounts(facet, Some(view_set_id))
    }

    fn effects_for_facet_mounts(
        &mut self,
        facet: &str,
        owner: Option<crate::ids::ViewSetId>,
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
        let mounted = self
            .mounted_instances()
            .filter_map(|instance| {
                let mount = self.presentation.as_ref()?.mount(&instance)?;
                if !selection_change
                    && owner.is_some_and(|id| {
                        self.view_set_for_instance(&instance)
                            .is_none_or(|set| set.id != id)
                    })
                {
                    return None;
                }
                Some((instance.clone(), mount.view_ref().to_string()))
            })
            .collect::<Vec<_>>();
        let targets = mounted.into_iter().filter_map(|(instance_key, view_ref)| {
            if selection_change && !matches!(self.selection_attachment_for_instance(&instance_key),
                Some(crate::ui::attachment::SelectionAttachment::FollowViewSet { view_set_id }) if Some(view_set_id) == owner) {
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
        let target = self
            .presentation
            .as_ref()
            .and_then(|presentation| presentation.mount(instance))
            .filter(|mount| mount.visible())
            .map(|mount| (instance.clone(), mount.view_ref().to_string()));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::reducer::test_support::core as test_support_core;
    use crate::ui::reducer::test_support::*;

    fn transcript_lens_core() -> RyeOsCore {
        let mut core = test_support_core(session_with_surface(
            serde_json::json!({"name":"transcript-lens","tiling":{"mode":"single_lens"},"tiles":["view:ryeos/thread/transcript"],"views":{"view:ryeos/thread/transcript":{"widget":"timeline","input":{"id":"line","submit":"route"},"sources":{"default":{"ref":"service:events/chain_replay","params":{"chain_root_id":"@facet:input.route.chain_root"},"collection":"events"}}}}}),
        ));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core
    }
    fn authored_affordance_core(views: &[(&str, serde_json::Value)]) -> RyeOsCore {
        let declared = views
            .iter()
            .map(|(reference, value)| (reference.to_string(), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        let mut session = session_with_surface(serde_json::json!({
            "name":"Affordance fixture", "tiles":[views[0].0], "views":declared
        }));
        for attachment in &mut session.binding_attachments {
            attachment.posture = crate::ui::binding::UiEffectivePosture::Interactive;
        }
        let mut core = test_support_core(session);
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core
    }
    fn settle_lens(core: &mut RyeOsCore) -> Vec<RyeOsEffect> {
        let producer = core
            .presentation_operations
            .producer
            .clone()
            .expect("lens fixture claimed");
        settle_pending_prefix(core, &producer)
    }
    fn lens_depth(core: &RyeOsCore) -> usize {
        core.focused_view_instance_key()
            .and_then(|key| core.retained_lens_frames.get(&key))
            .map_or(0, Vec::len)
    }

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
        if let Some(instance_key) = core
            .active_view_set()
            .unwrap()
            .tiles
            .values()
            .find(|tile| tile.view.view_ref == view_ref)
            .map(|tile| tile.instance_key.clone())
        {
            return instance_key;
        }
        assert!(
            core.binding_for_insertion(core.active_view_set().unwrap().id, view_ref)
                .is_some(),
            "fixture must explicitly admit its View before boot"
        );
        if core.presentation_operations.producer.is_none() {
            let producer = claim_fixture_producer(core);
            settle_pending_prefix(core, &producer);
        }
        core.add_center_tile(ViewSpec::bound(view_ref));
        settle_lens(core);
        core.active_view_set()
            .unwrap()
            .tiles
            .values()
            .find(|tile| tile.view.view_ref == view_ref)
            .expect("fixture insertion was acknowledged and mounted")
            .instance_key
            .clone()
    }

    #[test]
    fn route_write_without_open_destination_uses_origin_not_keyboard_focus() {
        let mut core = authored_affordance_core(&[
            ("view:test/origin", serde_json::json!({"widget":"rows"})),
            ("view:test/other", serde_json::json!({"widget":"rows"})),
        ]);
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
        let mut core = authored_affordance_core(&[(
            "view:test/pinned",
            serde_json::json!({
                "widget":"rows", "sources":{"default":{
                    "ref":"service:test/read", "params":{"thread":"@facet:selection.work.thread"}
                }}
            }),
        )]);

        let origin = mount_affordance_view(&mut core, "view:test/pinned");
        let owner = core.active_view_set().unwrap().id;
        let key = crate::ui::seat::selection_storage_key(
            core.view_set(owner).unwrap().id,
            "selection.work",
        )
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
            core.facet_value_for_instance(&origin, "selection.work.thread"),
            Some(serde_json::json!("T-one"))
        );
    }

    fn cross_set_follower_fixture() -> (
        RyeOsCore,
        crate::ids::ViewSetId,
        crate::ids::ViewSetId,
        crate::ids::RyeOsViewInstanceKey,
        String,
    ) {
        let mut session = session_with_surface(serde_json::json!({
            "name":"Cross-set followers",
            "view_sets":[
                {"schema":2,"id":"owner","title":"Owner","root":{"type":"group","views":["view:test/owner"],"active":0}},
                {"schema":2,"id":"follower","title":"Follower","root":{"type":"group","views":["view:test/follower"],"active":0}}
            ],
            "views":{
                "view:test/owner":{"widget":"rows"},
                "view:test/follower":{
                    "widget":"rows",
                    "sources":{"default":{"ref":"service:test/follower","params":{"thread":"@facet:selection.work.thread"}}},
                    "affordances":[
                        {"id":"select-work","invoke":{"plane":"ui","facet":"selection.work","value":{"thread":"{record.thread}"}}},
                        {"id":"open-work","invoke":{"plane":"ui","facet":"selection.work","value":{"thread":"{record.thread}"},"open_view":"view:test/detail"}}
                    ]
                },
                "view:test/detail":{"widget":"text","sources":{"default":{"ref":"service:test/detail","params":{"thread":"@facet:selection.work.thread"}}}},
                "view:test/unrelated":{"widget":"text"}
            }
        }));
        for attachment in &mut session.binding_attachments {
            attachment.posture = crate::ui::binding::UiEffectivePosture::Interactive;
        }
        let mut core = test_support_core(session);
        let owner = core.active_surface().unwrap().view_sets()[0].id;
        let owner_id = owner;
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        core.switch_view_set_tab(1);
        settle_pending_prefix(&mut core, &producer);
        let follower_tile = core.active_view_set().unwrap().focused_tile;
        let follower = core.active_view_set().unwrap().tiles[&follower_tile]
            .instance_key
            .clone();
        let source_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(follower.clone(), "default")
                .encode();
        core.prepare_subject_operation(
            follower.clone(),
            crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: owner_id,
            },
        );
        settle_pending_prefix(&mut core, &producer);
        (core, owner, owner_id, follower, source_key)
    }

    #[test]
    fn owner_selection_change_refreshes_cross_set_follower_from_owner_value() {
        let (mut core, owner, owner_id, follower, source_key) = cross_set_follower_fixture();
        core.active_surface_mut()
            .unwrap()
            .select_view_set(owner)
            .unwrap();
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
        let follower_set = core.active_view_set().unwrap().id;
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
            core.view_set(follower_set).unwrap().id,
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
        let mut core = test_support_core(session_with_surface(
            serde_json::json!({"name":"followed-lens","tiling":{"mode":"single_lens"},"tiles":["view:test/follower"],"views":{"view:test/follower":{"widget":"rows","sources":{"default":{"ref":"service:test/follower","params":{"thread":"@facet:selection.work.thread"}}}}}}),
        ));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let owner_id = core.active_view_set().unwrap().id;
        core.new_view_set();
        settle_lens(&mut core);
        core.open_view(ViewSpec::bound("view:test/follower"));
        settle_lens(&mut core);
        let follower = core.focused_view_instance_key().unwrap();
        core.prepare_subject_operation(
            follower.clone(),
            crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: owner_id,
            },
        );
        settle_lens(&mut core);
        let containing_id = core.active_view_set().unwrap().id;
        let owner_key = crate::ui::seat::selection_storage_key(owner_id, "selection.work").unwrap();
        let containing_key =
            crate::ui::seat::selection_storage_key(containing_id, "selection.work").unwrap();
        core.seat
            .append_facet(owner_key.clone(), serde_json::json!({"thread":"T-owner"}));
        core.seat.append_facet(
            containing_key.clone(),
            serde_json::json!({"thread":"T-containing"}),
        );
        settle_lens(&mut core);

        let route_before = core.route_for_instance(&follower);
        assert!(
            core.apply_ui_affordance_from(
                Some(&follower),
                "selection.work".into(),
                Some(serde_json::json!({"thread":"T-child"})),
                None,
                None,
                true
            )
            .is_empty()
        );
        assert_eq!(
            core.seat.fold().get(&owner_key).unwrap()["thread"],
            "T-owner"
        );
        settle_lens(&mut core);
        assert_eq!(
            core.facet_value_for_instance(&follower, "selection.work.thread"),
            Some(serde_json::json!("T-child"))
        );
        assert_eq!(core.route_for_instance(&follower), route_before);
        let refused_before = core.seat.events().to_vec();
        for patch in [
            serde_json::Value::Null,
            serde_json::json!({"other":"missing dependency"}),
        ] {
            assert!(
                core.apply_ui_affordance_from(
                    Some(&follower),
                    "selection.work".into(),
                    Some(patch),
                    None,
                    None,
                    true
                )
                .is_empty()
            );
            assert_eq!(
                core.seat.events(),
                refused_before.as_slice(),
                "invalid selection drill cannot retain provisional lens work"
            );
        }
        core.apply_ui_affordance_from(
            Some(&follower),
            "selection".into(),
            Some(serde_json::json!({"work":{"thread":"T-whole"}})),
            None,
            None,
            true,
        );
        settle_lens(&mut core);
        assert_eq!(
            core.facet_value_for_instance(&follower, "selection.work.thread"),
            Some(serde_json::json!("T-whole"))
        );
        core.pop_view();
        settle_lens(&mut core);
        core.apply_ui_affordance_from(
            Some(&follower),
            "selection.work.thread".into(),
            Some(serde_json::json!("T-subpath")),
            None,
            None,
            true,
        );
        settle_lens(&mut core);
        assert_eq!(
            core.facet_value_for_instance(&follower, "selection.work.thread"),
            Some(serde_json::json!("T-subpath"))
        );
        core.pop_view();
        settle_lens(&mut core);
        assert_eq!(
            core.seat.fold().get(&containing_key).unwrap()["thread"],
            "T-containing"
        );

        let frame = core
            .retained_lens_frames
            .get(&follower)
            .expect("exact followed lens frame")
            .last()
            .expect("drill pushes a return frame");
        assert_eq!(
            frame.attachment,
            crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: owner_id
            }
        );
        assert_ne!(
            frame.attachment,
            crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: containing_id
            }
        );
        core.pop_view();
        settle_lens(&mut core);
        assert_eq!(
            core.selection_attachment_for_instance(&follower),
            Some(crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: owner_id
            })
        );
        assert_eq!(core.route_for_instance(&follower), route_before);
    }

    #[test]
    fn selection_open_transfers_origin_attachment_and_discards_default_fetch() {
        let (mut core, _owner, owner_id, follower, _) = cross_set_follower_fixture();
        let containing_id = core.active_view_set().unwrap().id;
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
        assert!(
            effects
                .iter()
                .filter_map(source_request)
                .all(|(_, view_ref, _, _)| view_ref != "view:test/detail"),
            "destination fetch must wait for composition acknowledgement"
        );
        let effects = settle_lens(&mut core);

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

        core.apply_ui_affordance_from(
            Some(&follower),
            "selection.work".into(),
            Some(serde_json::json!({"thread": "T-owner-open"})),
            None,
            Some("view:test/unrelated".into()),
            false,
        );
        settle_lens(&mut core);

        let destination = core
            .focused_view_instance_key()
            .expect("opened destination is focused");
        assert_eq!(
            core.mounted_view_ref(&destination),
            Some("view:test/unrelated")
        );
        assert_ne!(
            core.selection_attachment_for_instance(&destination),
            Some(crate::ui::attachment::SelectionAttachment::FollowViewSet {
                view_set_id: _owner_id
            })
        );
        assert_eq!(
            core.followed_selection_view_set(&destination),
            Some(core.active_view_set().unwrap().id)
        );
    }

    #[test]
    fn explicit_empty_source_refresh_disables_inherited_facet_liveness() {
        let mut core = test_support_core(writable_session());
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
        {
            let mount_presentation = core.presentation.as_ref().unwrap().id().clone();
            let mount_surface = core.active_surface().unwrap().id().clone();
            core.active_view_set_mut().unwrap().add_tile(
                ViewSpec {
                    view_ref: "view:test/static".to_string(),
                },
                &mount_presentation,
                &mount_surface,
            )
        }
        .expect("fixture layout accepts view");

        assert!(
            core.effects_for_facet("selection").is_empty(),
            "an explicit per-source empty policy is not an inheritance fallback"
        );
    }

    #[test]
    fn invoke_affordance_ui_plane_writes_facet_and_refetches_subscribers() {
        let mut core = authored_affordance_core(&[
            (
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
            ),
            (
                "view:test/inspector",
                serde_json::json!({
                    "widget": "key_value",
                    "sources": { "default": {
                        "ref": "service:test/inspect",
                        "params": { "canonical_ref": "@facet:selection.item" }
                    } }
                }),
            ),
        ]);

        let inspector = mount_affordance_view(&mut core, "view:test/inspector");
        let source_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(inspector, "default").encode();

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
        let mut core = authored_affordance_core(&[(
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
        )]);

        let origin = mount_affordance_view(&mut core, "view:test/list");
        let origin_view_set_id = core.active_view_set().unwrap().id;
        core.new_view_set();
        settle_lens(&mut core);
        let active_view_set_id = core.active_view_set().unwrap().id;

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
        let mut core = authored_affordance_core(&[
            (
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
            ),
            (
                "view:ryeos/thread/transcript",
                serde_json::json!({
                    "widget": "timeline",
                    "sources": { "default": {
                        "ref": "service:events/chain_replay",
                        "params": { "chain_root_id": "@facet:input.route.chain_root" },
                        "collection": "events"
                    } }
                }),
            ),
        ]);

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

        assert!(
            effects.is_empty(),
            "new View is not published before acknowledgement"
        );
        let effects = settle_lens(&mut core);
        // The route is owned by the exact destination mount, not a global input.
        let route = focused_route_value(&core);
        assert_eq!(route["thread"], "T-9");
        assert_eq!(route["chain_root"], "T-root");

        // 2. The braid lens was opened as a tile.
        assert!(
            core.active_view_set()
                .unwrap()
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
        let mut core = authored_affordance_core(&[(
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
        )]);

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
        let mut core = authored_affordance_core(&[(
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
        )]);

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
    fn actual_threads_list_watch_affordance_inspects_exact_execution() {
        let binding: serde_json::Value = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml"
        ))
        .unwrap();
        let session = session_with_surface(serde_json::json!({
            "name":"Execution inspection", "tiling":{"mode":"single_lens"},
            "tiles":["view:ryeos/threads/list"],
            "views":{
                "view:ryeos/threads/list":binding,
                "view:ryeos/threads/detail":{
                    "widget":"text",
                    "sources":{"default":{"ref":"service:test/detail",
                        "params":{"thread_id":"@facet:selection.thread_id"}}}
                }
            }
        }));
        let mut core = test_support_core(session);
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        let source = core.focused_view_instance_key().unwrap();
        let effects = core.invoke_affordance(
            &source,
            "view:ryeos/threads/list",
            "watch",
            &serde_json::json!({"thread_id":"T-9","chain_root_id":"T-root"}),
        );
        assert!(
            effects.is_empty(),
            "inspection waits for retained composition acknowledgement"
        );
        assert_eq!(
            core.mounted_view_ref(&source),
            Some("view:ryeos/threads/list")
        );
        let effects = settle_pending_prefix(&mut core, &producer);
        let destination = core.focused_view_instance_key().unwrap();
        assert_eq!(
            core.mounted_view_ref(&destination),
            Some("view:ryeos/threads/detail")
        );
        assert_eq!(
            core.facet_value_for_instance(&destination, "selection.thread_id"),
            Some(serde_json::json!("T-9"))
        );
        assert!(
            effects
                .iter()
                .filter_map(source_request)
                .any(
                    |(_, view_ref, _, params)| view_ref == "view:ryeos/threads/detail"
                        && params["thread_id"] == "T-9"
                )
        );
        assert!(
            core.route_for_instance(&destination).thread.is_none(),
            "generic execution inspection does not invent a conversation route"
        );
    }

    #[test]
    fn shipped_comparison_actions_merge_operands_and_fetch_only_when_complete() {
        let history: serde_json::Value = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/threads/history.yaml"
        ))
        .unwrap();
        let comparison: serde_json::Value = serde_yaml::from_str(include_str!(
            "../../../../../../bundles/ryeos-ui/.ai/views/ryeos/runs/comparison.yaml"
        ))
        .unwrap();
        let mut core = authored_affordance_core(&[
            ("view:ryeos/threads/history", history),
            ("view:ryeos/runs/comparison", comparison),
        ]);
        mount_affordance_view(&mut core, "view:ryeos/runs/comparison");

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
            core.facet_value_for_instance(&source_instance, "comparison")
                .unwrap()["left_thread_id"],
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
                    instance_key: source_instance.clone(),
                    view_ref: "view:ryeos/threads/history".to_string(),
                    affordance_id: "compare-right".to_string(),
                    record: serde_json::json!({ "thread_id": "T-right" }),
                },
            },
        });
        let facet = core
            .facet_value_for_instance(&source_instance, "comparison")
            .unwrap();
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
        let mut core = transcript_lens_core();
        // On the game braid (chain_root A).
        set_focused_route_value(&mut core, serde_json::json!({ "chain_root": "A" }));
        settle_lens(&mut core);

        // Step into the child braid (chain_root B) with drill = true.
        core.apply_ui_affordance(
            crate::ui::seat::KEY_INPUT_ROUTE.to_string(),
            None,
            Some(serde_json::json!({ "chain_root": "B" })),
            Some("view:ryeos/thread/transcript".to_string()),
            true,
        );
        let producer = core.presentation_operations.producer.clone().unwrap();
        core.acknowledge_seat_prefix(&producer, pending_prefix(&core));
        let lens = core.focused_view_instance_key().unwrap();
        assert!(
            core.route_write_fenced_by_composition(&lens),
            "local completion is not a released lens fence"
        );
        let prefix_before = core.seat.events().to_vec();
        assert!(
            core.apply_ui_affordance_from(
                Some(&lens),
                crate::ui::seat::KEY_INPUT_ROUTE.into(),
                Some(serde_json::json!({"chain_root":"old-overwrite"})),
                None,
                None,
                false
            )
            .is_empty()
        );
        assert_eq!(
            core.seat.events(),
            prefix_before.as_slice(),
            "old route write cannot append behind completion"
        );
        if let Some(shared) = core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Bottom)
        {
            assert!(
                !core.route_write_fenced_by_composition(&shared),
                "unrelated shared mount is not globally frozen"
            );
        }
        settle_lens(&mut core);
        assert!(!core.route_write_fenced_by_composition(&lens));

        // A return frame captured the pre-drill braid; the fold now reads B.
        assert_eq!(lens_depth(&core), 1);
        assert_eq!(
            focused_route_value(&core)["chain_root"],
            serde_json::json!("B")
        );

        // Return: PopLens restores chain_root A and refetches that braid.
        assert!(
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::PopLens,
            })
            .is_empty()
        );
        let effects = settle_lens(&mut core);
        assert_eq!(lens_depth(&core), 0);
        assert_eq!(
            focused_route_value(&core)["chain_root"],
            serde_json::json!("A")
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
        let mut core = transcript_lens_core();
        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::PopLens,
        });
        assert_eq!(lens_depth(&core), 0);
        assert!(effects.is_empty());
    }

    #[test]
    fn drill_thread_steps_into_child_braid_and_pop_returns() {
        // The cross-thread step-in (the deepest debugger drill, ready for the
        // run-stability child_thread_spawned edge): DrillThread retargets the
        // route at the child AND pushes a return frame — no open_view, the braid
        // lens re-projects via the route facet. Backspace returns to the parent.
        let mut core = transcript_lens_core();
        // On the parent braid (chain_root P).
        set_focused_route_value(&mut core, serde_json::json!({ "chain_root": "P" }));
        settle_lens(&mut core);

        // Step into child C (a fresh root: both coords = C).
        assert!(
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::Activate {
                    intent: RyeOsUiIntent::DrillThread {
                        thread_id: "C".to_string(),
                        chain_root_id: "C".to_string(),
                        label: Some("study".to_string()),
                    },
                },
            })
            .is_empty()
        );
        let effects = settle_lens(&mut core);
        assert_eq!(lens_depth(&core), 1);
        // The breadcrumb tail reads the node stepped into, not the child id.
        assert_eq!(
            core.focused_view_instance_key()
                .and_then(|instance| core.retained_lens_labels.get(&instance))
                .and_then(|label| label.as_deref()),
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
        settle_lens(&mut core);
        assert_eq!(lens_depth(&core), 0);
        assert_eq!(
            core.focused_view_instance_key()
                .and_then(|instance| core.retained_lens_labels.get(&instance))
                .cloned()
                .flatten(),
            None,
            "pop restores the top-of-tree label"
        );
        assert_eq!(
            focused_route_value(&core)["chain_root"],
            serde_json::json!("P"),
            "pop restores the pre-drill route (parent braid, no child thread)"
        );
        assert!(
            focused_route_value(&core)["thread"].is_null(),
            "return cannot retain the child's thread coordinate"
        );
    }

    #[test]
    fn invoke_affordance_rye_plane_emits_bound_selection_coordinate() {
        let mut core = authored_affordance_core(&[(
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
        )]);

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
        let mut core = authored_affordance_core(&[(
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
        )]);
        set_focused_route_value(
            &mut core,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "params": { "directive": "directive:demo/base" }
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
        let mut core = test_support_core(session_with_surface(
            serde_json::json!({"name":"inspection","tiles":["view:test/work"],"views":{"view:test/work":{
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
            }}}),
        ));
        let producer = claim_fixture_producer(&mut core);
        settle_pending_prefix(&mut core, &producer);
        set_focused_route_value(&mut core, serde_json::json!({"thread":"T-composer"}));
        settle_lens(&mut core);
        let source_instance = core.focused_view_instance_key().unwrap();
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
        settle_lens(&mut core);

        assert_eq!(active_selection(&core)["work"]["thread"], "T-inspected");
        assert_eq!(
            core.focused_view_instance_key()
                .and_then(|instance| core.retained_lens_labels.get(&instance))
                .cloned()
                .flatten(),
            None
        );
    }

    #[test]
    fn directive_threads_dock_renders_bound_view_rows() {
        let mut core = test_support_core(writable_session());
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("left")
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
                core.shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Left)
                    .unwrap(),
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
        let dock = vm.docks.left.expect("left dock");
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
        let first_view = "view:test/selection".to_string();
        let mut core = authored_affordance_core(&[(
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
        )]);
        let first_id = core.active_surface_view_sets().unwrap()[0].id;
        core.duplicate_view_set(first_id);
        settle_lens(&mut core);
        let first_instance = core.active_surface_view_sets().unwrap()[0].tiles
            [&core.active_surface_view_sets().unwrap()[0].focused_tile]
            .instance_key
            .clone();
        let second_instance = core.active_surface_view_sets().unwrap()[1].tiles
            [&core.active_surface_view_sets().unwrap()[1].focused_tile]
            .instance_key
            .clone();

        core.switch_view_set_tab(0);
        settle_lens(&mut core);
        core.invoke_affordance(
            &first_instance,
            &first_view,
            "select",
            &serde_json::json!({ "item": "first" }),
        );
        core.switch_view_set_tab(1);
        settle_lens(&mut core);
        core.invoke_affordance(
            &second_instance,
            &first_view,
            "select",
            &serde_json::json!({ "item": "second" }),
        );

        let fold = core.seat.fold();
        let first_key =
            super::super::seat::selection_facet_key(core.active_surface_view_sets().unwrap()[0].id);
        let second_key =
            super::super::seat::selection_facet_key(core.active_surface_view_sets().unwrap()[1].id);
        assert_eq!(fold.get(&first_key).unwrap()["item"], "first");
        assert_eq!(fold.get(&second_key).unwrap()["item"], "second");
        assert_ne!(first_key, second_key);
    }

    #[test]
    fn sections_view_assembles_one_group_per_section_from_its_own_source() {
        let mut core = test_support_core(writable_session());
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("left")
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
        let instance_key = core
            .shared_dock_instance_key(crate::ui::model::RyeOsDockEdge::Left)
            .unwrap();
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
        let dock = vm.docks.left.expect("left dock");
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
        let mut core = test_support_core(writable_session());
        core.presentation
            .as_mut()
            .unwrap()
            .dock_mut("left")
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
        match vm.docks.left.expect("left dock").view {
            crate::ui::view_model::RyeOsViewVm::Sections { sections, .. } => {
                assert_eq!(sections.len(), 1);
                assert_eq!(sections[0].count, 0);
                assert!(sections[0].rows.is_empty());
            }
            other => panic!("expected sections dock view, got {other:?}"),
        }
    }
}
