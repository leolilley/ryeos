use serde_json::Value;

use super::effect::RyeOsEffect;
use super::event::{FieldStepDirection, RyeOsUiEvent};
use super::model::RyeOsCore;
use crate::ids::RyeOsViewInstanceKey;
use crate::ui::field::{self, FieldCursor, FieldEventRef};
use crate::ui::source_key::{RyeOsSourceChannel, RyeOsSourceInstanceKey};
use crate::workspace::{
    FieldCursorState, FieldEventRefState, FieldExpansionState, FieldLocalState, ViewLocalState,
};

impl RyeOsCore {
    pub(crate) fn dispatch_field_event(&mut self, event: RyeOsUiEvent) -> Vec<RyeOsEffect> {
        match event {
            RyeOsUiEvent::SetFieldSelection {
                instance_key,
                entity_id,
            } => self.set_field_selection(instance_key, entity_id),
            RyeOsUiEvent::MoveFieldSelection {
                instance_key,
                delta,
            } => {
                let Some((_view_ref, field)) =
                    super::view_model::field_vm_for_instance(self, &instance_key)
                else {
                    return Vec::new();
                };
                if field.traversal.is_empty() {
                    return Vec::new();
                }
                let current = field
                    .selected
                    .as_ref()
                    .and_then(|selected| field.traversal.iter().position(|id| id == selected))
                    .unwrap_or_default();
                let next = (current as i32 + delta)
                    .clamp(0, field.traversal.len().saturating_sub(1) as i32)
                    as usize;
                self.set_field_selection(instance_key, Some(field.traversal[next].clone()))
            }
            RyeOsUiEvent::SetFieldGroupCollapsed {
                instance_key,
                group_id,
                collapsed,
            } => {
                let valid = super::view_model::field_vm_for_instance(self, &instance_key)
                    .is_some_and(|(_, field)| {
                        field.groups.iter().any(|group| group.id == group_id)
                    });
                if !valid {
                    return Vec::new();
                }
                let Some(local) = self.field_local_mut(&instance_key) else {
                    return Vec::new();
                };
                let changed = if collapsed {
                    local.collapsed_groups.insert(group_id)
                } else {
                    local.collapsed_groups.remove(&group_id)
                };
                if changed {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetFieldLayerVisible {
                instance_key,
                layer_id,
                visible,
            } => {
                let valid = super::view_model::field_vm_for_instance(self, &instance_key)
                    .is_some_and(|(_, field)| {
                        field.layers.iter().any(|layer| layer.id == layer_id)
                    });
                if !valid {
                    return Vec::new();
                }
                let Some(local) = self.field_local_mut(&instance_key) else {
                    return Vec::new();
                };
                let changed = if visible {
                    local.hidden_layers.remove(&layer_id)
                } else {
                    local.hidden_layers.insert(layer_id)
                };
                if changed {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetFieldCursor {
                instance_key,
                cursor,
            } => self.set_field_cursor(instance_key, cursor, true),
            RyeOsUiEvent::StepFieldCursor {
                instance_key,
                direction,
            } => self.step_field_cursor(instance_key, direction),
            RyeOsUiEvent::SetFieldPlayback {
                instance_key,
                playing,
            } => {
                let can_play = super::view_model::field_vm_for_instance(self, &instance_key)
                    .is_some_and(|(_, field)| field.replay.next.is_some());
                let Some(local) = self.field_local_mut(&instance_key) else {
                    return Vec::new();
                };
                let next = playing && can_play;
                if local.playback.playing != next {
                    local.playback.playing = next;
                    local.playback.awaiting = None;
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsUiEvent::SetFieldQuery {
                instance_key,
                query,
            } => {
                let Some(local) = self.field_local_mut(&instance_key) else {
                    return Vec::new();
                };
                if local.query == query {
                    return Vec::new();
                }
                local.query = query;
                local.search_match = None;
                self.bump_generation();
                let active = super::view_model::field_vm_for_instance(self, &instance_key)
                    .and_then(|(_, field)| field.search.active_match);
                active.map_or_else(Vec::new, |id| {
                    self.set_field_selection(instance_key, Some(id))
                })
            }
            RyeOsUiEvent::MoveFieldSearchMatch {
                instance_key,
                delta,
            } => {
                let Some((_view_ref, field)) =
                    super::view_model::field_vm_for_instance(self, &instance_key)
                else {
                    return Vec::new();
                };
                if field.search.match_ids.is_empty() {
                    return Vec::new();
                }
                let current = field
                    .search
                    .active_match
                    .as_ref()
                    .and_then(|active| field.search.match_ids.iter().position(|id| id == active))
                    .unwrap_or_default();
                let next = (current as i32 + delta).rem_euclid(field.search.match_ids.len() as i32)
                    as usize;
                let entity_id = field.search.match_ids[next].clone();
                if let Some(local) = self.field_local_mut(&instance_key) {
                    local.search_match = Some(entity_id.clone());
                }
                self.set_field_selection(instance_key, Some(entity_id))
            }
            RyeOsUiEvent::ToggleFieldCompare {
                instance_key,
                entity_id,
            } => {
                let Some((_view_ref, field)) =
                    super::view_model::field_vm_for_instance(self, &instance_key)
                else {
                    return Vec::new();
                };
                if !field::entity_has_comparable_preview(&field, &entity_id) {
                    return Vec::new();
                }
                let Some(local) = self.field_local_mut(&instance_key) else {
                    return Vec::new();
                };
                if let Some(index) = local.compare.iter().position(|id| id == &entity_id) {
                    local.compare.remove(index);
                } else {
                    if let Some(left) = local.compare.first()
                        && !field::entities_have_compatible_previews(&field, left, &entity_id)
                    {
                        return Vec::new();
                    }
                    local.compare.push(entity_id);
                    if local.compare.len() > 2 {
                        local.compare.remove(0);
                    }
                }
                self.bump_generation();
                Vec::new()
            }
            RyeOsUiEvent::RequestFieldExpansion {
                instance_key,
                source,
                root_id,
            } => self.set_field_expansion(instance_key, source, root_id, false, false),
            RyeOsUiEvent::ContinueFieldExpansion {
                instance_key,
                source,
                root_id,
            } => self.set_field_expansion(instance_key, source, root_id, true, false),
            RyeOsUiEvent::ClearFieldExpansion {
                instance_key,
                source,
                root_id,
            } => self.set_field_expansion(instance_key, source, root_id, false, true),
            _ => Vec::new(),
        }
    }

    fn set_field_selection(
        &mut self,
        instance_key: RyeOsViewInstanceKey,
        entity_id: Option<String>,
    ) -> Vec<RyeOsEffect> {
        let Some((_view_ref, field)) =
            super::view_model::field_vm_for_instance(self, &instance_key)
        else {
            return Vec::new();
        };
        let select_intent = match entity_id.as_deref() {
            Some(entity_id) => {
                let Some(entity) = field.entities.iter().find(|entity| entity.id == entity_id)
                else {
                    return Vec::new();
                };
                entity.select_intent.clone()
            }
            None => None,
        };
        let Some(local) = self.field_local_mut(&instance_key) else {
            return Vec::new();
        };
        if local.selected == entity_id {
            return Vec::new();
        }
        local.selected = entity_id;
        self.bump_generation();
        select_intent.map_or_else(Vec::new, |intent| self.dispatch_intent(intent))
    }

    fn step_field_cursor(
        &mut self,
        instance_key: RyeOsViewInstanceKey,
        direction: FieldStepDirection,
    ) -> Vec<RyeOsEffect> {
        let Some((_view_ref, field)) =
            super::view_model::field_vm_for_instance(self, &instance_key)
        else {
            return Vec::new();
        };
        let cursor = match direction {
            FieldStepDirection::Previous => field.replay.previous,
            FieldStepDirection::Next => field.replay.next,
            FieldStepDirection::Live => {
                return self.set_field_cursor(instance_key, FieldCursorState::Live, false);
            }
        };
        let Some(cursor) = cursor else {
            if let Some(local) = self.field_local_mut(&instance_key) {
                local.playback.playing = false;
            }
            return Vec::new();
        };
        self.set_field_cursor(
            instance_key,
            FieldCursorState::BraidCut {
                anchor: event_ref_state(&cursor),
            },
            false,
        )
    }

    fn set_field_cursor(
        &mut self,
        instance_key: RyeOsViewInstanceKey,
        cursor: FieldCursorState,
        validate_rail: bool,
    ) -> Vec<RyeOsEffect> {
        let Some((view_ref, field)) = super::view_model::field_vm_for_instance(self, &instance_key)
        else {
            return Vec::new();
        };
        if validate_rail
            && let FieldCursorState::BraidCut { anchor } = &cursor
            && !field
                .replay
                .rail
                .iter()
                .any(|entry| event_ref_matches(anchor, &entry.event))
        {
            return Vec::new();
        }
        let channels = cursor_channels(self.views.get(&view_ref));
        if channels.is_empty() {
            return Vec::new();
        }
        let awaiting = match &cursor {
            FieldCursorState::Live => None,
            FieldCursorState::BraidCut { anchor } => Some(anchor.clone()),
        };
        let Some(local) = self.field_local_mut(&instance_key) else {
            return Vec::new();
        };
        if local.cursor == cursor && local.playback.awaiting == awaiting {
            return Vec::new();
        }
        local.cursor = cursor;
        local.playback.awaiting = awaiting;
        self.bump_generation();
        channels
            .into_iter()
            .flat_map(|channel| {
                self.refresh_source_channel(instance_key.clone(), &view_ref, &channel)
            })
            .collect()
    }

    fn set_field_expansion(
        &mut self,
        instance_key: RyeOsViewInstanceKey,
        source: String,
        root_id: String,
        continue_existing: bool,
        clear: bool,
    ) -> Vec<RyeOsEffect> {
        let Some((view_ref, field)) = super::view_model::field_vm_for_instance(self, &instance_key)
        else {
            return Vec::new();
        };
        if !field.sources.iter().any(|status| status.name == source)
            || !field.entities.iter().any(|entity| entity.id == root_id)
        {
            return Vec::new();
        }
        let key = format!("{source}\0{root_id}");
        let Some(local) = self.field_local_mut(&instance_key) else {
            return Vec::new();
        };
        if clear {
            if local.expansions.remove(&key).is_none() {
                return Vec::new();
            }
        } else if continue_existing {
            let Some(expansion) = local.expansions.get_mut(&key) else {
                return Vec::new();
            };
            if expansion.continuation_token.is_none() {
                return Vec::new();
            }
        } else {
            local.expansions.entry(key).or_insert(FieldExpansionState {
                max_depth: 2,
                max_entities: 250,
                continuation_token: None,
            });
        }
        self.bump_generation();
        self.refresh_source_channel(instance_key, &view_ref, &source)
    }

    pub(crate) fn advance_field_playback(&mut self) -> Vec<RyeOsEffect> {
        let keys = self
            .workspace
            .tiles
            .values()
            .filter_map(|tile| match &tile.local {
                ViewLocalState::Field(local)
                    if local.playback.playing && local.playback.awaiting.is_none() =>
                {
                    Some(tile.instance_key.clone())
                }
                _ => None,
            })
            .chain(
                self.ui
                    .dock_local
                    .iter()
                    .filter_map(|(key, local)| match local {
                        ViewLocalState::Field(local)
                            if local.playback.playing && local.playback.awaiting.is_none() =>
                        {
                            Some(key.clone())
                        }
                        _ => None,
                    }),
            )
            .collect::<Vec<_>>();
        keys.into_iter()
            .flat_map(|key| self.step_field_cursor(key, FieldStepDirection::Next))
            .collect()
    }

    pub(crate) fn settle_field_source(&mut self, source_key: &str, response: &Value) {
        let Some(key) = RyeOsSourceInstanceKey::decode(source_key) else {
            return;
        };
        let RyeOsSourceChannel::Named(channel) = key.channel else {
            return;
        };
        let Ok(document) = field::parse_field_document(response) else {
            return;
        };
        let Some(local) = self.field_local_mut(&key.view_instance) else {
            return;
        };
        let settled = match (&local.cursor, &document.cursor) {
            (FieldCursorState::Live, FieldCursor::Live) => true,
            (
                FieldCursorState::BraidCut { anchor },
                FieldCursor::BraidCut {
                    anchor: document_anchor,
                    ..
                },
            ) => event_ref_matches(anchor, document_anchor),
            _ => false,
        };
        if settled {
            local.playback.awaiting = None;
        }
        for expansion in &document.expansions {
            let Some(root_id) = expansion.get("root_id").and_then(Value::as_str) else {
                continue;
            };
            let key = format!("{channel}\0{root_id}");
            if let Some(state) = local.expansions.get_mut(&key) {
                state.continuation_token = expansion
                    .get("continuation_token")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
    }

    /// Diff the complete projected field after one named source is accepted.
    /// The snapshot covers every still-mounted channel, so refreshing one
    /// source cannot manufacture exits for facts owned by another source.
    pub(crate) fn note_field_semantic_changes(&mut self, source_key: &str) {
        let Some(key) = RyeOsSourceInstanceKey::decode(source_key) else {
            return;
        };
        if !matches!(key.channel, RyeOsSourceChannel::Named(_)) {
            return;
        }
        // Invalid field payloads degrade through the VM warning path but must
        // not erase the last good semantic snapshot or invent mass exits.
        if self
            .data
            .sources
            .get(source_key)
            .is_none_or(|value| field::parse_field_document(value).is_err())
        {
            return;
        }
        let Some((_view_ref, vm)) =
            super::view_model::field_vm_for_instance(self, &key.view_instance)
        else {
            return;
        };
        if vm.structural_revision.is_empty() {
            return;
        }
        let current = field::semantic_fingerprints(&vm);
        let now_ms = self.runtime.now_ms;
        let Some(local) = self.field_local_mut(&key.view_instance) else {
            return;
        };
        let prior = std::mem::replace(&mut local.change_fingerprints, current.clone());
        let keys = prior
            .keys()
            .chain(current.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut changed = false;
        for key in keys {
            let before = prior.get(&key);
            let after = current.get(&key);
            let kind = match (before, after) {
                (None, Some(after)) if after.fact_kind == "relation" => "relation_added",
                (None, Some(_)) => "entered",
                (Some(before), None) if before.fact_kind == "relation" => "relation_removed",
                (Some(_), None) => "exited",
                (Some(before), Some(after)) if before.status != after.status => "status_changed",
                (Some(before), Some(after)) if before.fingerprint != after.fingerprint => "updated",
                _ => continue,
            };
            let target = after.or(before).expect("one side exists");
            let removed_entity = after.is_none() && target.fact_kind == "entity";
            local.changes.insert(
                key,
                crate::workspace::FieldChangeState {
                    id: target.id.clone(),
                    kind: kind.to_string(),
                    at_ms: now_ms,
                    tone: after.or(before).and_then(|fact| fact.tone.clone()),
                    prior_fingerprint: before.map(|fact| fact.fingerprint.clone()),
                    fingerprint: after.map(|fact| fact.fingerprint.clone()),
                    tombstone_label: removed_entity.then(|| target.label.clone()).flatten(),
                    tombstone_traits: removed_entity.then(|| target.traits.clone()),
                },
            );
            changed = true;
        }
        if local.changes.len() > 512 {
            let mut oldest = local
                .changes
                .iter()
                .map(|(key, change)| (change.at_ms, key.clone()))
                .collect::<Vec<_>>();
            oldest.sort();
            for (_, key) in oldest.into_iter().take(local.changes.len() - 512) {
                local.changes.remove(&key);
            }
        }
        if changed {
            self.bump_activity_pulse(0.35);
        }
    }

    pub(crate) fn expire_field_changes(&mut self, now_ms: u64) {
        fn expire(local: &mut ViewLocalState, now_ms: u64) {
            let ViewLocalState::Field(field) = local else {
                return;
            };
            field
                .changes
                .retain(|_, change| now_ms.saturating_sub(change.at_ms) <= 2_000);
        }
        for tile in self.workspace.tiles.values_mut() {
            expire(&mut tile.local, now_ms);
        }
        for local in self.ui.dock_local.values_mut() {
            expire(local, now_ms);
        }
    }

    fn field_local_mut(
        &mut self,
        instance_key: &RyeOsViewInstanceKey,
    ) -> Option<&mut FieldLocalState> {
        let local = if let Some(tile_id) = instance_key.workspace_tile_id() {
            &mut self.workspace.tiles.get_mut(&tile_id)?.local
        } else {
            self.ui.dock_local.get_mut(instance_key)?
        };
        match local {
            ViewLocalState::Field(local) => Some(local),
            _ => None,
        }
    }
}

fn cursor_channels(binding: Option<&super::content::ViewBinding>) -> Vec<String> {
    binding
        .into_iter()
        .flat_map(|binding| binding.sources.iter())
        .filter(|(_, source)| contains_string(&source.params, "@field:cursor"))
        .map(|(channel, _)| channel.clone())
        .collect()
}

fn contains_string(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(value) => value == needle,
        Value::Array(values) => values.iter().any(|value| contains_string(value, needle)),
        Value::Object(values) => values.values().any(|value| contains_string(value, needle)),
        _ => false,
    }
}

fn event_ref_state(event: &FieldEventRef) -> FieldEventRefState {
    FieldEventRefState {
        chain_root_id: event.chain_root_id.clone(),
        chain_seq: event.chain_seq,
        event_hash: event.event_hash.clone(),
    }
}

fn event_ref_matches(state: &FieldEventRefState, event: &FieldEventRef) -> bool {
    state.chain_root_id == event.chain_root_id
        && state.chain_seq == event.chain_seq
        && state.event_hash == event.event_hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::effect::{RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
    use crate::ui::event::RyeOsEvent;
    use crate::ui::model::{BrowserSession, BrowserViewport};

    fn facts(source: &str) -> Value {
        serde_json::json!({
            "schema_version": crate::ui::field::FIELD_FACTS_SCHEMA,
            "source": source,
            "subject": { "kind": "project", "id": "project:test" },
            "revision": format!("revision:{source}"),
            "cursor": { "mode": "live" },
            "truncated": false,
            "entities": [
                {
                    "id": "run:one", "kind": "run", "label": "one", "status": "running",
                    "attributes": { "thread": { "id": "T-one", "facets": {} } },
                    "provenance": { "source_ref": "service:project", "source_revision": "revision:project", "evidence": [] }
                },
                {
                    "id": "run:two", "kind": "run", "label": "two", "status": "completed",
                    "attributes": { "thread": { "id": "T-two", "facets": {} } },
                    "provenance": { "source_ref": "service:project", "source_revision": "revision:project", "evidence": [] }
                }
            ],
            "relations": [], "previews": [], "metrics": [], "expansions": [], "warnings": []
        })
    }

    fn core() -> RyeOsCore {
        RyeOsCore::new(
            BrowserSession {
                effective_surface: Some(serde_json::json!({
                    "name": "field-test",
                    "tiles": ["view:test/field"],
                    "views": {
                        "view:test/field": {
                            "widget": "field",
                            "sources": {
                                "project": { "ref": "service:project" },
                                "execution": {
                                    "ref": "service:execution",
                                    "params": {
                                        "thread_id": "@facet:selection.thread_id",
                                        "cursor": "@field:cursor"
                                    }
                                }
                            },
                            "projections": {
                                "schema_version": "ryeos.ui.field.projection.v1",
                                "groups": [{ "id": "runs", "label": "Runs" }],
                                "layers": [{ "id": "live", "label": "Live" }],
                                "entity_rules": [{
                                    "match": { "kind": "run" },
                                    "set": { "group": "runs", "layer": "live" }
                                }]
                            },
                            "selection": { "change": "select", "activate": "select" },
                            "affordances": [{
                                "id": "select",
                                "invoke": {
                                    "plane": "ui",
                                    "facet": "selection",
                                    "value": {
                                        "thread_id": "{record.attributes.thread.id}",
                                        "entity_id": "{record.id}"
                                    }
                                }
                            }]
                        }
                    }
                })),
                read_only: false,
                ..Default::default()
            },
            BrowserViewport::default(),
            0,
        )
    }

    #[test]
    fn field_selection_writes_declared_facet_and_refreshes_only_dependent_channel() {
        let mut core = core();
        let effects = core.initial_effects();
        let project = effects
            .iter()
            .find(|effect| {
                matches!(&effect.kind, RyeOsEffectKind::FetchSource { source_ref, .. } if source_ref == "service:project")
            })
            .expect("project source fetches without a selection")
            .clone();
        assert!(!effects.iter().any(|effect| {
            matches!(&effect.kind, RyeOsEffectKind::FetchSource { source_ref, .. } if source_ref == "service:execution")
        }));
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: project.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(facts("project")),
                error: None,
            },
        });
        let instance_key = core
            .workspace
            .tiles
            .values()
            .next()
            .expect("field tile")
            .instance_key
            .clone();
        let project_key =
            crate::ui::source_key::RyeOsSourceInstanceKey::named(instance_key.clone(), "project")
                .encode();
        assert_eq!(
            core.data
                .field_sources
                .get(&project_key)
                .and_then(|parsed| parsed.as_ref().ok())
                .map(|document| document.revision.as_str()),
            Some("revision:project")
        );

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetFieldSelection {
                instance_key: instance_key.clone(),
                entity_id: Some("run:two".to_string()),
            },
        });
        let selection = core.seat.fold().get("selection").cloned().unwrap();
        assert_eq!(selection["thread_id"], "T-two");
        assert_eq!(selection["entity_id"], "run:two");
        assert!(effects.iter().any(|effect| {
            matches!(&effect.kind, RyeOsEffectKind::FetchSource { source_ref, params, .. }
                if source_ref == "service:execution"
                && params["thread_id"] == "T-two"
                && params["cursor"]["mode"] == "live")
        }));
        assert!(!effects.iter().any(|effect| {
            matches!(&effect.kind, RyeOsEffectKind::FetchSource { source_ref, .. } if source_ref == "service:project")
        }));
        let (_, selected_vm) =
            super::super::view_model::field_vm_for_instance(&core, &instance_key).unwrap();
        let project_subject = selected_vm
            .sources
            .iter()
            .find(|source| source.name == "project")
            .and_then(|source| source.subject_fingerprint.clone())
            .unwrap();
        let first_execution_subject = selected_vm
            .sources
            .iter()
            .find(|source| source.name == "execution")
            .and_then(|source| source.subject_fingerprint.clone())
            .unwrap();

        let effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::MoveFieldSelection {
                instance_key: instance_key.clone(),
                delta: -1,
            },
        });
        assert_eq!(
            core.seat.fold().get("selection").unwrap()["thread_id"],
            "T-one"
        );
        assert!(effects.iter().any(|effect| {
            matches!(&effect.kind, RyeOsEffectKind::FetchSource { params, .. } if params["thread_id"] == "T-one")
        }));
        let (_, moved_vm) =
            super::super::view_model::field_vm_for_instance(&core, &instance_key).unwrap();
        assert_eq!(
            moved_vm
                .sources
                .iter()
                .find(|source| source.name == "project")
                .and_then(|source| source.subject_fingerprint.as_ref()),
            Some(&project_subject)
        );
        assert_ne!(
            moved_vm
                .sources
                .iter()
                .find(|source| source.name == "execution")
                .and_then(|source| source.subject_fingerprint.as_ref()),
            Some(&first_execution_subject)
        );
    }

    #[test]
    fn accepted_refresh_records_projected_changes_without_exiting_other_sources() {
        let mut core = core();
        let initial = core.initial_effects();
        let project = initial
            .into_iter()
            .find(|effect| {
                matches!(&effect.kind, RyeOsEffectKind::FetchSource { source_ref, .. } if source_ref == "service:project")
            })
            .unwrap();
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: project.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(facts("project")),
                error: None,
            },
        });
        let instance_key = core
            .workspace
            .tiles
            .values()
            .next()
            .unwrap()
            .instance_key
            .clone();

        let execution_effects = core.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetFieldSelection {
                instance_key: instance_key.clone(),
                entity_id: Some("run:one".to_string()),
            },
        });
        let execution = execution_effects
            .into_iter()
            .find(|effect| {
                matches!(&effect.kind, RyeOsEffectKind::FetchSource { source_ref, .. } if source_ref == "service:execution")
            })
            .unwrap();
        let mut execution_facts = facts("execution");
        execution_facts["entities"] = serde_json::json!([{
            "id": "occurrence:kept", "kind": "occurrence", "label": "kept",
            "attributes": {},
            "provenance": { "source_ref": "service:execution", "source_revision": "revision:execution", "evidence": [] }
        }]);
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: execution.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(execution_facts),
                error: None,
            },
        });
        core.dispatch(RyeOsEvent::Tick { now_ms: 3_001 });

        let project_refresh = core
            .refresh_source_channel(instance_key.clone(), "view:test/field", "project")
            .pop()
            .unwrap();
        let mut updated = facts("project");
        updated["revision"] = serde_json::json!("revision:project:2");
        updated["entities"] = serde_json::json!([
            {
                "id": "run:one", "kind": "run", "label": "one", "status": "completed",
                "attributes": { "thread": { "id": "T-one", "facets": {} } },
                "provenance": { "source_ref": "service:project", "source_revision": "revision:project:2", "evidence": [] }
            },
            {
                "id": "run:three", "kind": "run", "label": "three", "status": "running",
                "attributes": { "thread": { "id": "T-three", "facets": {} } },
                "provenance": { "source_ref": "service:project", "source_revision": "revision:project:2", "evidence": [] }
            }
        ]);
        updated["relations"] = serde_json::json!([{
            "id": "relation:one-three", "kind": "precedes",
            "source_id": "run:one", "target_id": "run:three", "directed": true,
            "attributes": {},
            "provenance": { "source_ref": "service:project", "source_revision": "revision:project:2", "evidence": [] }
        }]);
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: project_refresh.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(updated),
                error: None,
            },
        });

        let (_, vm) =
            super::super::view_model::field_vm_for_instance(&core, &instance_key).unwrap();
        let changes = vm
            .changes
            .iter()
            .map(|change| (change.id.as_str(), change.kind))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            changes["run:one"],
            field::RyeOsFieldChangeKind::StatusChanged
        );
        assert_eq!(changes["run:two"], field::RyeOsFieldChangeKind::Exited);
        assert_eq!(changes["run:three"], field::RyeOsFieldChangeKind::Entered);
        assert_eq!(
            changes["relation:one-three"],
            field::RyeOsFieldChangeKind::RelationAdded
        );
        assert!(!changes.contains_key("occurrence:kept"));
        assert_eq!(
            vm.changes
                .iter()
                .find(|change| change.id == "run:two")
                .and_then(|change| change.tombstone.as_ref())
                .map(|tombstone| tombstone.label.as_str()),
            Some("two")
        );

        core.dispatch(RyeOsEvent::Tick { now_ms: 6_002 });
        let (_, vm) =
            super::super::view_model::field_vm_for_instance(&core, &instance_key).unwrap();
        assert!(vm.changes.is_empty());
    }

    #[test]
    fn shared_arrow_and_enter_commands_navigate_and_activate_field_entities() {
        use crate::ui::keymap::{RyeOsKey, RyeOsKeyEvent, RyeOsKeyModifiers, ryeos_key_command};
        use crate::ui::model::RyeOsFocusTarget;

        let mut core = core();
        let project = core
            .initial_effects()
            .into_iter()
            .find(|effect| {
                matches!(&effect.kind, RyeOsEffectKind::FetchSource { source_ref, .. } if source_ref == "service:project")
            })
            .unwrap();
        core.dispatch(RyeOsEvent::EffectResult {
            result: RyeOsEffectResult {
                id: project.id,
                ok: true,
                kind: RyeOsEffectResultKind::SourceData,
                data: Some(facts("project")),
                error: None,
            },
        });
        let tile_id = core.workspace.tiles.keys().next().copied().unwrap();
        core.workspace.focused_tile = tile_id;
        core.ui.focus_target = Some(RyeOsFocusTarget::WorkspaceTile {
            tile_id: tile_id.0.to_string(),
        });

        let down = ryeos_key_command(
            RyeOsKeyEvent {
                key: RyeOsKey::ArrowDown,
                modifiers: RyeOsKeyModifiers::default(),
            },
            core.key_context(),
        );
        core.apply_key_command(down);
        assert_eq!(
            core.seat.fold().get("selection").unwrap()["entity_id"],
            "run:two"
        );

        let enter = ryeos_key_command(
            RyeOsKeyEvent {
                key: RyeOsKey::Enter,
                modifiers: RyeOsKeyModifiers::default(),
            },
            core.key_context(),
        );
        core.apply_key_command(enter);
        assert_eq!(
            core.seat.fold().get("selection").unwrap()["thread_id"],
            "T-two"
        );
    }
}
