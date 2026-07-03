use super::effect::{StudioEffect, StudioEffectKind};
use super::model::StudioCore;
use crate::ids::TileId;
use crate::workspace::ViewSpec;

impl StudioCore {
    pub(crate) fn effects_for_focused_feeds(&mut self) -> Vec<StudioEffect> {
        let Some((key, view_ref)) = self.focused_input_instance() else {
            return Vec::new();
        };
        let feeds = self
            .views
            .get(&view_ref)
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
        if let Some(tile_id) = self
            .workspace
            .tile_ids()
            .into_iter()
            .find(|id| id.0.to_string() == key.view_instance_id)
        {
            self.set_tile_cursor(tile_id, 0);
        }
        self.emit_fetch_source_keyed(key.view_instance_id.clone(), &view_ref)
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
        view_ref: &str,
        affordance_id: &str,
        record: &serde_json::Value,
    ) -> Vec<StudioEffect> {
        let Some(binding) = self.views.get(view_ref) else {
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
        // Row activation is the `selection` producer: affordances read
        // `{record.<field>}`. Validation is binding-time (fails closed).
        let payload = super::content::Payload::Selection(record);
        match super::content::resolve_affordance_invoke(
            &affordance,
            super::content::Producer::Selection,
            &payload,
        ) {
            Some(super::content::AffordanceInvoke::Ui {
                facet,
                value,
                merge,
                open_view,
            }) => self.apply_ui_affordance(facet, value, merge, open_view),
            Some(super::content::AffordanceInvoke::Rye {
                tokens,
                args,
                notice,
            }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Tokens { tokens },
                    params: args,
                    intent: super::effect::InvokeIntent::Service,
                    success_notice: notice,
                    route_seq: None,
                    ratchet_on_thread_id: false,
                })]
            }
            Some(super::content::AffordanceInvoke::Service {
                item_ref,
                args,
                notice,
            }) => {
                vec![self.emit(StudioEffectKind::Invoke {
                    target: super::effect::InvokeRef::Ref { item_ref },
                    params: args,
                    intent: super::effect::InvokeIntent::Service,
                    success_notice: notice,
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
    ) -> Vec<StudioEffect> {
        let next = if let Some(merge) = merge {
            let mut current = self
                .seat
                .fold()
                .get(&facet)
                .cloned()
                .unwrap_or(serde_json::json!({}));
            if let (Some(target), Some(patch)) = (current.as_object_mut(), merge.as_object()) {
                for (key, val) in patch {
                    target.insert(key.clone(), val.clone());
                }
            }
            current
        } else {
            value.unwrap_or(serde_json::Value::Null)
        };
        self.seat.append_facet(facet.clone(), next);
        self.bump_generation();
        let mut effects = self.effects_for_facet(&facet);
        // Open the view AFTER the facet write, so the opened view's fetch
        // resolves its `@facet:` params against the value just written (e.g. a
        // row drill-in sets input.route.chain_root, then the braid lens fetches
        // that chain). Single-lens surfaces replace the center in place.
        if let Some(view_ref) = open_view {
            effects.extend(self.open_view(ViewSpec::bound(view_ref)));
        }
        effects
    }

    /// Facet write arrived: refetch every bound tile whose binding
    /// declares `refresh.on_facet: <key>` or whose source params
    /// reference the facet explicitly.
    pub fn effects_for_facet(&mut self, facet: &str) -> Vec<StudioEffect> {
        let targets: Vec<(crate::ids::TileId, String)> = self
            .workspace
            .tile_ids()
            .into_iter()
            .filter_map(|tile_id| {
                let tile = self.workspace.tiles.get(&tile_id)?;
                let view_ref = &tile.view.view_ref;
                let binding = self.views.get(view_ref)?;
                let subscribed = binding.refresh.get("on_facet").and_then(|v| v.as_str())
                    == Some(facet)
                    || binding
                        .source
                        .as_ref()
                        .map(|source| {
                            serde_json::to_string(&source.params)
                                .unwrap_or_default()
                                .contains(&format!("@facet:{facet}"))
                        })
                        .unwrap_or(false);
                subscribed.then(|| (tile_id, view_ref.clone()))
            })
            .collect();
        targets
            .into_iter()
            .flat_map(|(tile_id, view_ref)| self.emit_fetch_source(tile_id, &view_ref))
            .collect()
    }

    /// Resolve the atlas arrangement a `SetAtlas*` event targets: `Some(tile)`
    /// → that tile's per-tile arrangement, created from the default on first
    /// touch; `None` → the ambient backdrop atlas.
    pub(crate) fn atlas_target_mut(&mut self, tile_id: &Option<String>) -> &mut crate::atlas::AtlasUiStateVm {
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

    pub(crate) fn effects_for_view(&mut self, view: &ViewSpec) -> Vec<StudioEffect> {
        let view_ref = view.view_ref.clone();
        // Scene widgets pull engine data the generic source path doesn't
        // carry; everything else fetches its declared source.
        let widget = self
            .views
            .get(&view_ref)
            .map(|binding| binding.widget.clone());
        match widget.as_deref() {
            Some("atlas") => vec![
                self.emit(StudioEffectKind::FetchDimension),
                self.emit(StudioEffectKind::FetchTopology),
                self.emit(StudioEffectKind::FetchItems {
                    tile_id: None,
                    query: None,
                    kind: None,
                    limit: 1000,
                }),
            ],
            Some("graph") => vec![
                self.emit(StudioEffectKind::FetchDimension),
                self.emit(StudioEffectKind::FetchTopology),
            ],
            _ => {
                let tile_id = self.workspace.focused_tile;
                self.emit_fetch_source(tile_id, &view_ref)
                    .into_iter()
                    .collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::reducer::test_support::*;

    #[test]
    fn invoke_affordance_ui_plane_writes_facet_and_refetches_subscribers() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/list",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/list", "params": {}, "collection": "rows" },
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
                "source": {
                    "ref": "service:test/inspect",
                    "params": { "canonical_ref": "@facet:selection.item" }
                }
            }),
        );
        let tile_id = core
            .workspace
            .add_tile(ViewSpec {
                view_ref: "view:test/inspector".to_string(),
            })
            .0
            .to_string();

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:test/list".to_string(),
                    affordance_id: "select-item".to_string(),
                    record: serde_json::json!({ "canonical_ref": "tool:demo/run" }),
                },
            },
        });

        let fold = core.seat.fold();
        assert_eq!(fold.get("selection").unwrap()["item"], "tool:demo/run");
        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::FetchSource { tile_id: fetched_tile, source_ref, params })
                if fetched_tile == &tile_id
                    && source_ref == "service:test/inspect"
                    && params["canonical_ref"] == "tool:demo/run"
        ));
    }

    #[test]
    fn ui_affordance_open_view_writes_route_then_opens_view_with_new_facet() {
        // The watch drill-in (P1): a row activation merges route {thread, chain_root}
        // AND opens the braid lens, whose fetch must resolve the just-written chain_root.
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "table",
                "source": { "ref": "service:ui/studio/threads/list", "params": {}, "collection": "threads" },
                "affordances": [{
                    "id": "watch",
                    "invoke": {
                        "plane": "ui",
                        "facet": "input.route",
                        "merge": { "thread": "{record.thread_id}", "chain_root": "{record.chain_root_id}" },
                        "open_view": "view:ryeos/chain/timeline"
                    }
                }]
            }),
        );
        seed_view_value(
            &mut core,
            "view:ryeos/chain/timeline",
            serde_json::json!({
                "widget": "timeline",
                "source": {
                    "ref": "service:events/chain_replay",
                    "params": { "chain_root_id": "@facet:input.route.chain_root" },
                    "collection": "events"
                }
            }),
        );

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
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
            core.workspace
                .tiles
                .values()
                .any(|t| t.view.view_ref == "view:ryeos/chain/timeline"),
            "drill-in opens the braid lens"
        );

        // 3. Its fetch resolved the just-written chain_root (write-then-open order).
        assert!(
            effects.iter().any(|e| matches!(&e.kind,
                StudioEffectKind::FetchSource { source_ref, params, .. }
                    if source_ref == "service:events/chain_replay"
                        && params["chain_root_id"] == "T-root")),
            "timeline fetch must use the selected chain_root; got {effects:?}"
        );
    }

    #[test]
    fn service_ref_affordance_emits_execute_invoke_with_row_args() {
        // Row management (P2): a service-ref affordance emits an /execute Invoke
        // carrying the row's args — so cancel/kill/continue target that row, not
        // the route head (the token path would drop the args).
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "table",
                "source": { "ref": "service:ui/studio/threads/list", "params": {}, "collection": "threads" },
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

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:ryeos/threads/list".to_string(),
                    affordance_id: "cancel".to_string(),
                    record: serde_json::json!({ "thread_id": "T-7" }),
                },
            },
        });

        assert!(
            matches!(effects.first().map(|e| &e.kind),
                Some(StudioEffectKind::Invoke {
                    target: crate::studio::effect::InvokeRef::Ref { item_ref },
                    params,
                    ..
                }) if item_ref == "service:commands/submit"
                    && params["thread_id"] == "T-7"
                    && params["command_type"] == "cancel"),
            "service-ref affordance must /execute with the row's args; got {effects:?}"
        );
    }

    #[test]
    fn actual_threads_list_watch_affordance_drills_into_braid() {
        // The shipped product contract: the real threads/list.yaml `watch`
        // affordance drills a row into its braid.
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        let binding: crate::studio::content::ViewBinding = serde_yaml::from_str(include_str!(
            "../../../../../bundles/studio/.ai/views/ryeos/threads/list.yaml"
        ))
        .unwrap();
        core.views
            .insert("view:ryeos/threads/list".to_string(), binding);
        seed_view_value(
            &mut core,
            "view:ryeos/chain/timeline",
            serde_json::json!({
                "widget": "timeline",
                "source": {
                    "ref": "service:events/chain_replay",
                    "params": { "chain_root_id": "@facet:input.route.chain_root" },
                    "collection": "events"
                }
            }),
        );
        // A pre-existing route field must survive the merge.
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({ "directive": "directive:ryeos/ops/base" }),
        );

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:ryeos/threads/list".to_string(),
                    affordance_id: "watch".to_string(),
                    record: serde_json::json!({ "thread_id": "T-9", "chain_root_id": "T-root" }),
                },
            },
        });

        let fold = core.seat.fold();
        let route = fold.get("input.route").expect("route facet");
        assert_eq!(route["thread"], "T-9");
        assert_eq!(route["chain_root"], "T-root");
        assert_eq!(
            route["directive"], "directive:ryeos/ops/base",
            "merge preserves existing route fields"
        );
        assert!(
            core.workspace
                .tiles
                .values()
                .any(|t| t.view.view_ref == "view:ryeos/chain/timeline"),
            "watch opens the braid lens"
        );
        assert!(
            effects.iter().any(|e| matches!(&e.kind,
                StudioEffectKind::FetchSource { source_ref, params, .. }
                    if source_ref == "service:events/chain_replay"
                        && params["chain_root_id"] == "T-root")),
            "timeline fetch uses the row's chain_root; got {effects:?}"
        );
    }

    #[test]
    fn invoke_affordance_rye_plane_emits_token_invoke_with_args() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:test/threads",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/threads", "params": {}, "collection": "rows" },
                "affordances": [{
                    "id": "cancel",
                    "invoke": {
                        "plane": "rye",
                        "tokens": ["thread", "cancel"],
                        "args": { "thread_id": "{record.thread_id}" }
                    }
                }]
            }),
        );

        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:test/threads".to_string(),
                    affordance_id: "cancel".to_string(),
                    record: serde_json::json!({ "thread_id": "T-demo" }),
                },
            },
        });

        assert!(matches!(
            effects.first().map(|effect| &effect.kind),
            Some(StudioEffectKind::Invoke {
                target: super::super::effect::InvokeRef::Tokens { tokens },
                params,
                route_seq: None,
                ..
            }) if tokens == &vec!["thread".to_string(), "cancel".to_string()]
                && params["thread_id"] == "T-demo"
        ));
    }

    #[test]
    fn invoke_affordance_ui_merge_folds_into_existing_facet() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.seat.append_facet(
            crate::studio::seat::KEY_INPUT_ROUTE,
            serde_json::json!({
                "invoke": { "type": "service", "ref": "service:threads/input" },
                "directive": "directive:demo/base"
            }),
        );
        seed_view_value(
            &mut core,
            "view:test/threads",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:test/threads", "params": {}, "collection": "rows" },
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

        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::InvokeAffordance {
                    view_ref: "view:test/threads".to_string(),
                    affordance_id: "aim-input".to_string(),
                    record: serde_json::json!({ "thread_id": "T-route" }),
                },
            },
        });

        let fold = core.seat.fold();
        let route = fold.get(crate::studio::seat::KEY_INPUT_ROUTE).unwrap();
        assert_eq!(route["directive"], "directive:demo/base");
        assert_eq!(route["thread"], "T-route");
    }

    #[test]
    fn graph_view_effects_fetch_topology() {
        let mut core = StudioCore::new(session(), BrowserViewport::default(), 0);
        seed_view_value(
            &mut core,
            "view:ryeos/graph/topology",
            serde_json::json!({ "widget": "graph" }),
        );
        let effects = core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::Activate {
                action: StudioAction::OpenView {
                    view: ViewSpec::bound("view:ryeos/graph/topology"),
                },
            },
        });

        assert!(effects
            .iter()
            .any(|effect| matches!(effect.kind, StudioEffectKind::FetchTopology)));
    }

    #[test]
    fn atlas_surface_fetches_items_and_builds_scene_atlas() {
        let mut core = StudioCore::new(atlas_session(), BrowserViewport::default(), 0);
        let effects = core.initial_effects();
        let items_id = effects
            .iter()
            .find(|effect| {
                matches!(
                    effect.kind,
                    StudioEffectKind::FetchItems {
                        tile_id: None,
                        query: None,
                        kind: None,
                        ..
                    }
                )
            })
            .map(|effect| effect.id)
            .expect("atlas surface should fetch atlas items");

        core.dispatch(StudioEvent::EffectResult {
            result: StudioEffectResult {
                id: items_id,
                ok: true,
                kind: StudioEffectResultKind::Items,
                data: Some(serde_json::json!({
                    "schema_version": "studio.items.v1",
                    "counts": { "by_kind": {}, "by_space": {} },
                    "items": [{
                        "canonical_ref": "tool:demo/run",
                        "item_kind": "tool",
                        "bare_id": "demo/run",
                        "label": "run",
                        "namespace": "demo",
                        "source_path": "/tmp/.ai/tools/demo/run.yaml",
                        "space": "project",
                        "executable": true
                    }]
                })),
                error: None,
            },
        });

        let scene =
            crate::studio::scene_model::build_scene_model(&core, &core.ui.atlas, None, None);
        let atlas = scene.atlas.expect("atlas surface should build scene atlas");
        assert_eq!(atlas.root_label, ".ai");
        assert!(atlas
            .nodes
            .iter()
            .flat_map(|node| &node.stack)
            .any(|item| item.canonical_ref == "tool:demo/run"));
    }

    #[test]
    fn directive_threads_dock_renders_bound_view_rows() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.ui.docks.left.as_mut().unwrap().visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "rows",
                "source": { "ref": "service:ui/studio/threads", "params": {}, "collection": "rows" },
                "projections": { "primary": "thread_id", "meta": "item_ref" }
            }),
        );
        core.data.sources.insert(
            "dock:left".to_string(),
            serde_json::json!({
                "rows": [{
                    "thread_id": "T-running",
                    "item_ref": "directive:demo/chat",
                    "status": "running"
                }]
            }),
        );

        let vm = build_view_model(&core);
        let dock = vm.workspace.docks.left.expect("left dock");
        assert!(dock.input.is_none(), "a rows view declares no input");
        match dock.view {
            crate::studio::view_model::StudioViewVm::Rows { rows, .. } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].primary, "T-running");
                assert_eq!(rows[0].meta.as_deref(), Some("directive:demo/chat"));
            }
            other => panic!("expected bound rows dock view, got {other:?}"),
        }
    }

    #[test]
    fn sections_view_assembles_one_group_per_section_from_its_own_source() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.ui.docks.left.as_mut().unwrap().visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "sections",
                "sections": [
                    {
                        "title": "Threads",
                        "source": { "ref": "service:ui/studio/threads", "collection": "rows" },
                        "projection": { "primary": "thread_id", "meta": "status" }
                    },
                    {
                        "title": "Bundles",
                        "source": { "ref": "service:ui/studio/bundles", "collection": "rows" },
                        "projection": { "primary": "name", "meta": "version" }
                    }
                ]
            }),
        );
        // Each section's response lands under its own per-section key.
        core.data.sources.insert(
            crate::studio::content::section_source_key("dock:left", 0),
            serde_json::json!({ "rows": [
                { "thread_id": "T-ab", "status": "running" },
                { "thread_id": "T-cd", "status": "done" }
            ]}),
        );
        core.data.sources.insert(
            crate::studio::content::section_source_key("dock:left", 1),
            serde_json::json!({ "rows": [ { "name": "studio", "version": "v1.0.0" } ]}),
        );

        let vm = build_view_model(&core);
        let dock = vm.workspace.docks.left.expect("left dock");
        match dock.view {
            crate::studio::view_model::StudioViewVm::Sections { sections, .. } => {
                assert_eq!(sections.len(), 2);
                assert_eq!(sections[0].title, "Threads");
                assert_eq!(sections[0].count, 2);
                assert_eq!(sections[0].rows[0].primary, "T-ab");
                assert_eq!(sections[0].rows[0].meta.as_deref(), Some("running"));
                assert_eq!(sections[1].title, "Bundles");
                assert_eq!(sections[1].count, 1);
                assert_eq!(sections[1].rows[0].primary, "studio");
                assert_eq!(sections[1].rows[0].meta.as_deref(), Some("v1.0.0"));
            }
            other => panic!("expected bound sections dock view, got {other:?}"),
        }
    }

    #[test]
    fn sections_view_without_a_loaded_source_shows_an_empty_group() {
        let mut core = StudioCore::new(writable_session(), BrowserViewport::default(), 0);
        core.ui.docks.left.as_mut().unwrap().visible = true;
        seed_view_value(
            &mut core,
            "view:ryeos/threads/list",
            serde_json::json!({
                "widget": "sections",
                "sections": [{
                    "title": "Threads",
                    "source": { "ref": "service:ui/studio/threads", "collection": "rows" },
                    "projection": { "primary": "thread_id" }
                }]
            }),
        );
        // No source seeded → the section is present but empty (count 0), not a
        // placeholder: the surface is up, the data just hasn't arrived.
        let vm = build_view_model(&core);
        match vm.workspace.docks.left.expect("left dock").view {
            crate::studio::view_model::StudioViewVm::Sections { sections, .. } => {
                assert_eq!(sections.len(), 1);
                assert_eq!(sections[0].count, 0);
                assert!(sections[0].rows.is_empty());
            }
            other => panic!("expected sections dock view, got {other:?}"),
        }
    }
}
