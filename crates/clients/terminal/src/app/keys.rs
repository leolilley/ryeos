//! Terminal key translation: crossterm events → shared ryeos key
//! commands → core events. Both the bindings (`ryeos_key_command`) and the
//! command interpretation (`RyeOsCore::apply_key_command`) live in
//! `ryeos-client-base` so terminal and web agree; this is only the
//! crossterm adapter plus the one deliberate terminal-local binding
//! (plain ←/→ feed folding).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ryeos_client_base::ids::RyeOsViewInstanceKey;
use ryeos_client_base::ui::model::RyeOsFocusTarget;
use ryeos_client_base::ui::view_model::{RyeOsLayoutNodeVm, RyeOsViewSetVm, RyeOsViewVm};
use ryeos_client_base::ui::{
    RyeOsCore, RyeOsEffect, RyeOsEvent, RyeOsKey, RyeOsKeyCommand, RyeOsKeyContext, RyeOsKeyEvent,
    RyeOsKeyModifiers, RyeOsUiEvent, ryeos_key_command,
};

pub fn handle_key(core: &mut RyeOsCore, key: KeyEvent) -> Vec<RyeOsEffect> {
    let Some(event) = terminal_ryeos_key_event(key) else {
        return Vec::new();
    };
    // Feed folding: plain ←/→ fold/unfold the turn under the point when the
    // focused lens is a feed sitting on a foldable section. The state change
    // is the exact-instance `SetViewFold` event; only this key binding is
    // terminal-local (the feed is a cell-grid lens), so web keeps ←/→ for tile focus.
    let no_mods = !event.modifiers.ctrl
        && !event.modifiers.alt
        && !event.modifiers.meta
        && !event.modifiers.shift;
    let context = key_context(core);
    if no_mods && matches!(event.key, RyeOsKey::ArrowLeft | RyeOsKey::ArrowRight) {
        let expansion_wins = match event.key {
            RyeOsKey::ArrowRight => {
                (context.focused_tree_collapsible && context.focused_tree_collapsed)
                    || (context.focused_row_expandable && !context.focused_row_expanded)
            }
            RyeOsKey::ArrowLeft => {
                (context.focused_row_expandable && context.focused_row_expanded)
                    || (context.focused_tree_collapsible && !context.focused_tree_collapsed)
            }
            _ => false,
        };
        if !expansion_wins && let Some((instance_key, section)) = focused_fold_section(core) {
            let collapsed = matches!(event.key, RyeOsKey::ArrowLeft);
            return core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SetViewFold {
                    instance_key,
                    section,
                    collapsed,
                },
            });
        }
    }
    let command = ryeos_key_command(event, context);
    core.apply_key_command(command)
}

pub fn should_quit(core: &RyeOsCore, key: &KeyEvent) -> bool {
    terminal_ryeos_key_event(*key).is_some_and(|event| {
        matches!(
            ryeos_key_command(event, key_context(core)),
            RyeOsKeyCommand::Quit
        )
    })
}

fn terminal_ryeos_key_event(key: KeyEvent) -> Option<RyeOsKeyEvent> {
    let ryeos_key = match key.code {
        KeyCode::Char(ch) => RyeOsKey::Char(ch),
        KeyCode::Up => RyeOsKey::ArrowUp,
        KeyCode::Down => RyeOsKey::ArrowDown,
        KeyCode::Left => RyeOsKey::ArrowLeft,
        KeyCode::Right => RyeOsKey::ArrowRight,
        KeyCode::Enter => RyeOsKey::Enter,
        KeyCode::Esc => RyeOsKey::Escape,
        KeyCode::Backspace => RyeOsKey::Backspace,
        KeyCode::Tab => RyeOsKey::Tab,
        // Shift+Tab arrives as a distinct BackTab code; normalize it to a
        // shifted Tab so the shared keymap sees one key with a modifier.
        KeyCode::BackTab => RyeOsKey::Tab,
        _ => return None,
    };
    let force_shift = matches!(key.code, KeyCode::BackTab);
    Some(RyeOsKeyEvent {
        key: ryeos_key,
        modifiers: RyeOsKeyModifiers {
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            shift: force_shift || key.modifiers.contains(KeyModifiers::SHIFT),
            meta: key.modifiers.contains(KeyModifiers::SUPER),
        },
    })
}

fn key_context(core: &RyeOsCore) -> RyeOsKeyContext {
    // Capability resolution is shared (base) so terminal and web agree;
    // this adapter only translates crossterm events.
    core.key_context()
}

/// The focused mounted View and the foldable turn-section under its point, when
/// the focused lens is a feed positioned on one. Drives plain ←/→ folding.
fn focused_fold_section(core: &mut RyeOsCore) -> Option<(RyeOsViewInstanceKey, usize)> {
    let focus = core.focus_target()?;
    let vm = core.envelope(Vec::new()).view_model;
    fold_section_for_focus(vm.view_set.as_ref(), &focus)
}

fn fold_section_for_focus(
    view_set: Option<&RyeOsViewSetVm>,
    focus: &RyeOsFocusTarget,
) -> Option<(RyeOsViewInstanceKey, usize)> {
    find_fold_section(view_set?.root.as_ref()?, focus)
}

fn find_fold_section(
    node: &RyeOsLayoutNodeVm,
    focus: &RyeOsFocusTarget,
) -> Option<(RyeOsViewInstanceKey, usize)> {
    match node {
        RyeOsLayoutNodeVm::Tile {
            tile_id,
            instance_key,
            view,
            ..
        } => {
            // Surface focused_tile remains populated while an ordinary dialog
            // or dock owns focus. Resolve only the actual shared focus target.
            let focused = match focus {
                RyeOsFocusTarget::MountedView {
                    instance_key: focused,
                } => focused == instance_key,
                RyeOsFocusTarget::ViewSetTile { tile_id: focused } => focused == tile_id,
                RyeOsFocusTarget::Dock { .. } => false,
            };
            if !focused {
                return None;
            }
            let section = match view.as_ref() {
                RyeOsViewVm::Timeline { fold_section, .. }
                | RyeOsViewVm::Sections { fold_section, .. } => *fold_section,
                _ => None,
            }?;
            Some((instance_key.clone(), section))
        }
        RyeOsLayoutNodeVm::Split { first, second, .. } => {
            find_fold_section(first, focus).or_else(|| find_fold_section(second, focus))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_client_base::ids::{PresentationId, SurfaceInstanceId, TileId, ViewSetId};
    use ryeos_client_base::ui::model::RyeOsDockEdge;

    fn feed_core() -> (RyeOsCore, Vec<RyeOsViewInstanceKey>) {
        use ryeos_client_base::ui::binding::{
            UiBindingAttachment, UiBindingAttachmentCoordinate, UiPresentationInitialization,
            UiPresentationMembership, UiSurfaceMembership,
        };
        use ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest;
        use ryeos_client_base::ui::model::{BrowserSession, BrowserViewport};
        use ryeos_client_base::ui::presentation_initialization::UiPresentationInitializationRecord;
        use ryeos_client_base::ui::seat::{SeatEvent, SeatEventKind};
        let presentation_id = "11111111-1111-4111-8111-111111111111";
        let surface_id = "22222222-2222-4222-8222-222222222222";
        let bounds = serde_json::json!({"max_request_bytes":65536,"max_input_bytes":4096,"max_seat_append_bytes":65536});
        let shared: UiBindingAttachment = serde_json::from_value(serde_json::json!({
            "binding_attachment_id":"shared","binding_generation":1,"binding_digest":"ab".repeat(32),
            "root_ref":"config:test/presentation","root_generation":"cd".repeat(32),
            "project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only",
            "binding_request_bounds":bounds,
            "presentation":{"definition":{"kind":"shared_presentation","config":{
                "category":"ui","shared_presentation":{"schema":1,"title":"Keys fixture","launcher":"launcher",
                    "dialogs":[{"id":"launcher","title":"Launcher","view":"view:test/launcher"},
                        {"id":"help","title":"Help","view":"view:test/help"}],
                    "docks":{},"capabilities":{"sources":[],"affordances":[]}}}},
                "views":{"view:test/launcher":{"widget":"text","body":"Launcher"},
                    "view:test/help":{"widget":"text","body":"Help"}}}
        })).unwrap();
        let surface: UiBindingAttachment = serde_json::from_value(serde_json::json!({
            "binding_attachment_id":"surface","binding_generation":1,"binding_digest":"ef".repeat(32),
            "root_ref":"surface:test/feed","root_generation":"12".repeat(32),
            "project_path":null,"subject":{"kind":"projectless"},"posture":"observation_only",
            "binding_request_bounds":bounds,
            "presentation":ryeos_client_base::presentation::UiCompositionPresentation::from_effective_surface(
                serde_json::json!({"name":"Feeds","tiles":["view:test/feed","view:test/feed"],
                    "views":{"view:test/feed":{"widget":"sections",
                        "sources":{"threads":{"ref":"service:threads/list"}},
                        "sections":[{"title":"Threads","source_channel":"threads","collection":"threads",
                            "projection":{"primary":"thread_id"}}]}}})
            ).unwrap()
        })).unwrap();
        let coordinate = |binding: &UiBindingAttachment| UiBindingAttachmentCoordinate {
            binding_attachment_id: binding.binding_attachment_id.clone(),
            binding_generation: binding.binding_generation,
            binding_digest: binding.binding_digest.clone(),
        };
        let seeds = surface
            .presentation
            .definition
            .surface()
            .unwrap()
            .to_mount_seeds()
            .unwrap();
        let active = seeds.first().unwrap().id.clone();
        let manifest = UiSurfaceMountManifest::allocate(
            PresentationId::try_new(presentation_id).unwrap(),
            SurfaceInstanceId::try_new(surface_id).unwrap(),
            1,
            seeds
                .into_iter()
                .map(|seed| (seed, coordinate(&surface)))
                .collect(),
            Some(&active),
        )
        .unwrap();
        let initial = UiPresentationInitialization {
            schema: 2,
            selection: ryeos_client_base::ui::presentation_initialization::UiPresentationInitialSelection::ConfiguredDefaults,
            presentation_id: presentation_id.into(),
            creation_request_id: "33333333-3333-4333-8333-333333333333".into(),
            root_binding: coordinate(&shared),
            next_mount_id: manifest.next_mount_id,
            surfaces: vec![UiSurfaceMembership {
                surface_instance_id: surface_id.into(),
                ordinal: 1,
                title: "Feeds".into(),
                binding: coordinate(&surface),
                mount_manifest: manifest,
                saved_surface: None,
            }],
            binding_attachments: vec![shared, surface],
        };
        initial.validate().unwrap();
        let session = BrowserSession {
            ui_binding_contract_revision: ryeos_client_base::UI_BINDING_CONTRACT_REVISION.into(),
            session_id: "terminal-keys".into(),
            root_attachment_id: "shared".into(),
            binding_attachments: initial.binding_attachments.clone(),
            presentations: vec![UiPresentationMembership {
                presentation_id: presentation_id.into(),
                root_binding: initial.root_binding.clone(),
                thread_id: None,
                chain_root_id: None,
                next_mount_id: initial.next_mount_id,
                initialization_digest: initial.digest().unwrap(),
                surfaces: initial.surfaces.clone(),
            }],
            ..Default::default()
        };
        let mut core = RyeOsCore::new(session, presentation_id, BrowserViewport::default(), 0);
        core.replay_seat_events([SeatEvent {
            seq: 0,
            producer_engine_seq: None,
            kind: SeatEventKind::PresentationInitialized(UiPresentationInitializationRecord {
                operation_id: initial.creation_request_id.clone(),
                initialization: initial,
            }),
        }]);
        core.require_replayed_initialization().unwrap();
        let set = core.active_view_set().unwrap();
        let instances = std::iter::once(set.focused_tile)
            .chain(
                set.root
                    .as_ref()
                    .unwrap()
                    .tile_ids()
                    .into_iter()
                    .filter(|tile| *tile != set.focused_tile),
            )
            .map(|tile| set.tiles[&tile].instance_key.clone())
            .collect::<Vec<_>>();
        assert_eq!(instances.len(), 2);
        for instance in &instances {
            core.data.sources.insert(
                ryeos_client_base::ui::source_key::RyeOsSourceInstanceKey::named(
                    instance.clone(),
                    "threads",
                )
                .encode(),
                serde_json::json!({"threads":[{"thread_id":"T-fixture"}]}),
            );
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SetViewCursor {
                    instance_key: instance.clone(),
                    index: 0,
                },
            });
            core.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::SetViewFold {
                    instance_key: instance.clone(),
                    section: 0,
                    collapsed: true,
                },
            });
        }
        (core, instances)
    }

    fn section_is_collapsed(core: &RyeOsCore, instance: &RyeOsViewInstanceKey) -> bool {
        match core.mounted_local_state(instance).unwrap() {
            ryeos_client_base::view_set::ViewLocalState::GenericList { collapsed, .. } => {
                collapsed.contains(&0)
            }
            other => panic!("expected feed local state, got {other:?}"),
        }
    }

    #[test]
    fn control_k_enters_the_exact_shared_launcher_without_mutating_surface_views() {
        let (mut core, instances) = feed_core();
        let presentation = PresentationId::try_new("11111111-1111-4111-8111-111111111111").unwrap();
        let launcher = RyeOsViewInstanceKey::shared_dialog(&presentation, "launcher").unwrap();
        assert!(
            !matches!(core.focus_target(), Some(RyeOsFocusTarget::MountedView { instance_key }) if instance_key == launcher)
        );

        // Exercise the actual crossterm entry, not direct OpenLauncher dispatch.
        // Projected Surface and required-subject actions use this shared dialog
        // owner; the terminal must not substitute a background tile origin.
        handle_key(
            &mut core,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert!(
            matches!(core.focus_target(), Some(RyeOsFocusTarget::MountedView { instance_key }) if instance_key == launcher)
        );
        for instance in &instances {
            assert!(section_is_collapsed(&core, instance));
        }
    }

    #[test]
    fn plain_arrow_with_launcher_or_dialog_focus_leaves_background_feed_folded() {
        for event in [
            RyeOsUiEvent::OpenLauncher,
            RyeOsUiEvent::OpenDialog {
                dialog_id: "help".into(),
            },
        ] {
            let (mut core, instances) = feed_core();
            core.dispatch(RyeOsEvent::Ui { event });
            let focus = core.focus_target().unwrap();
            assert!(
                matches!(focus, RyeOsFocusTarget::MountedView { instance_key } if !instances.contains(&instance_key))
            );
            handle_key(&mut core, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
            for instance in &instances {
                assert!(section_is_collapsed(&core, instance));
            }
        }
    }

    #[test]
    fn plain_arrows_fold_only_the_exact_focused_feed_instance() {
        let (mut core, instances) = feed_core();
        let set = core.active_view_set().unwrap();
        assert_ne!(set.tiles[&set.focused_tile].instance_key, instances[1]);
        // Model the exact mounted focus reported by the shared presentation.
        // No producer or structural mutation authority is needed for local folds.
        core.active_view_set_mut().unwrap().focus_target = Some(RyeOsFocusTarget::MountedView {
            instance_key: instances[1].clone(),
        });
        assert_eq!(
            focused_fold_section(&mut core),
            Some((instances[1].clone(), 0))
        );
        handle_key(&mut core, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(section_is_collapsed(&core, &instances[0]));
        assert!(!section_is_collapsed(&core, &instances[1]));
        handle_key(&mut core, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(section_is_collapsed(&core, &instances[0]));
        assert!(section_is_collapsed(&core, &instances[1]));
    }

    fn feed_set(surface: &str, section: Option<usize>) -> RyeOsViewSetVm {
        let presentation = PresentationId::try_new("terminal-test").unwrap();
        let surface = SurfaceInstanceId::try_new(surface).unwrap();
        RyeOsViewSetVm {
            view_set_id: ViewSetId::new(1),
            layout_guard: String::new(),
            split_min_ratio: 0.1,
            split_max_ratio: 0.9,
            root: Some(RyeOsLayoutNodeVm::Tile {
                group_id: "1".into(),
                group_label: None,
                tabs: Vec::new(),
                instance_key: RyeOsViewInstanceKey::surface_tile(
                    &presentation,
                    &surface,
                    TileId::new(1),
                ),
                tile_id: "1".into(),
                focused: true,
                maximized: false,
                title: "Feed".into(),
                attachment_label: None,
                heading: None,
                supplement: None,
                intents: Vec::new(),
                view: Box::new(RyeOsViewVm::Sections {
                    title: "Feed".into(),
                    sections: Vec::new(),
                    fold_section: section,
                }),
                chrome_hidden: false,
                background_transparent: false,
                input: None,
            }),
            focused_tile: "1".into(),
            center_is_empty: false,
            tile_count: 1,
            lens_trail: Vec::new(),
            lens_label: None,
        }
    }

    fn feed_instance(set: &RyeOsViewSetVm) -> RyeOsViewInstanceKey {
        let Some(RyeOsLayoutNodeVm::Tile { instance_key, .. }) = set.root.as_ref() else {
            panic!("fixture must be a tile");
        };
        instance_key.clone()
    }

    #[test]
    fn absent_surface_and_empty_layout_have_no_terminal_fold_target() {
        let focus = RyeOsFocusTarget::ViewSetTile {
            tile_id: "1".into(),
        };
        assert_eq!(fold_section_for_focus(None, &focus), None);
        let mut set = feed_set("surface-a", Some(2));
        set.root = None;
        assert_eq!(fold_section_for_focus(Some(&set), &focus), None);
    }

    #[test]
    fn launcher_and_dock_focus_do_not_fold_the_background_surface() {
        let set = feed_set("surface-a", Some(2));
        let presentation = PresentationId::try_new("terminal-test").unwrap();
        for focus in [
            RyeOsFocusTarget::MountedView {
                instance_key: RyeOsViewInstanceKey::shared_dialog(&presentation, "launcher")
                    .unwrap(),
            },
            RyeOsFocusTarget::Dock {
                edge: RyeOsDockEdge::Bottom,
            },
            RyeOsFocusTarget::MountedView {
                instance_key: RyeOsViewInstanceKey::shared_dock(&presentation, "bottom").unwrap(),
            },
        ] {
            assert_eq!(fold_section_for_focus(Some(&set), &focus), None);
        }
    }

    #[test]
    fn focused_feed_returns_exact_instance_not_a_bare_tile_id() {
        let set = feed_set("surface-a", Some(2));
        let instance = feed_instance(&set);
        for focus in [
            RyeOsFocusTarget::ViewSetTile {
                tile_id: "1".into(),
            },
            RyeOsFocusTarget::MountedView {
                instance_key: instance.clone(),
            },
        ] {
            assert_eq!(
                fold_section_for_focus(Some(&set), &focus),
                Some((instance.clone(), 2))
            );
        }
        let other_surface = feed_set("surface-b", Some(2));
        let focus = RyeOsFocusTarget::MountedView {
            instance_key: feed_instance(&other_surface),
        };
        assert_eq!(fold_section_for_focus(Some(&set), &focus), None);
    }

    #[test]
    fn focused_feed_without_a_foldable_section_does_not_intercept_arrows() {
        let set = feed_set("surface-a", None);
        let focus = RyeOsFocusTarget::MountedView {
            instance_key: feed_instance(&set),
        };
        assert_eq!(fold_section_for_focus(Some(&set), &focus), None);
    }

    #[test]
    fn backtab_normalizes_to_shifted_tab() {
        // crossterm sends Shift+Tab as a distinct BackTab code; the adapter
        // must present it to the shared keymap as Tab + shift so Shift+Tab
        // drives the backward target cycle.
        let ev = terminal_ryeos_key_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE))
            .expect("BackTab maps to a ryeos key");
        assert_eq!(ev.key, RyeOsKey::Tab);
        assert!(ev.modifiers.shift, "BackTab carries shift");
    }

    #[test]
    fn plain_tab_is_unshifted() {
        let ev = terminal_ryeos_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .expect("Tab maps to a ryeos key");
        assert_eq!(ev.key, RyeOsKey::Tab);
        assert!(!ev.modifiers.shift);
    }
}
