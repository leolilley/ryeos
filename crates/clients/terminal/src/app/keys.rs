//! Terminal key translation: crossterm events → shared studio key
//! commands → core events. The bindings themselves live in
//! `ryeos-client-base` (`studio_key_command`) so terminal and web agree;
//! this is only the crossterm adapter and the row-cursor fallback.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ryeos_client_base::studio::view_model::{StudioLayoutNodeVm, StudioViewVm};
use ryeos_client_base::studio::{
    studio_key_command, StudioCore, StudioEffect, StudioEvent, StudioKey, StudioKeyCommand,
    StudioKeyContext, StudioKeyEvent, StudioKeyModifiers, StudioUiEvent,
};
use ryeos_client_base::workspace::{FocusDirection, ViewLocalState};

pub fn handle_key(core: &mut StudioCore, key: KeyEvent) -> Vec<StudioEffect> {
    let Some(event) = terminal_studio_key_event(key) else {
        return Vec::new();
    };
    // Feed folding: plain ←/→ fold/unfold the turn under the point when the
    // focused lens is a feed sitting on a foldable section. The state change
    // is the shared `SetFold` event; only this key binding is terminal-local
    // (the feed is a cell-grid lens), so web keeps ←/→ for tile focus.
    let no_mods = !event.modifiers.ctrl
        && !event.modifiers.alt
        && !event.modifiers.meta
        && !event.modifiers.shift;
    if no_mods && matches!(event.key, StudioKey::ArrowLeft | StudioKey::ArrowRight) {
        if let Some((tile_id, section)) = focused_fold_section(core) {
            let collapsed = matches!(event.key, StudioKey::ArrowLeft);
            return core.dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SetFold {
                    tile_id,
                    section,
                    collapsed,
                },
            });
        }
    }
    match studio_key_command(event, key_context(core)) {
        StudioKeyCommand::Ui { event } => core.dispatch(StudioEvent::Ui { event }),
        StudioKeyCommand::MoveFocusedRowOrFocus {
            delta,
            fallback_direction,
        } => move_focused_row_or_focus(core, delta, fallback_direction),
        StudioKeyCommand::InsertLauncherChar { ch } => {
            let mut query = core.ui.launcher.query.clone();
            query.push(ch);
            core.dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SetLauncherQuery { query },
            })
        }
        StudioKeyCommand::DeleteLauncherChar => {
            let mut query = core.ui.launcher.query.clone();
            query.pop();
            core.dispatch(StudioEvent::Ui {
                event: StudioUiEvent::SetLauncherQuery { query },
            })
        }
        StudioKeyCommand::Quit => Vec::new(),
        StudioKeyCommand::Ignore => Vec::new(),
    }
}

pub fn should_quit(core: &StudioCore, key: &KeyEvent) -> bool {
    terminal_studio_key_event(*key).is_some_and(|event| {
        matches!(
            studio_key_command(event, key_context(core)),
            StudioKeyCommand::Quit
        )
    })
}

fn terminal_studio_key_event(key: KeyEvent) -> Option<StudioKeyEvent> {
    let studio_key = match key.code {
        KeyCode::Char(ch) => StudioKey::Char(ch),
        KeyCode::Up => StudioKey::ArrowUp,
        KeyCode::Down => StudioKey::ArrowDown,
        KeyCode::Left => StudioKey::ArrowLeft,
        KeyCode::Right => StudioKey::ArrowRight,
        KeyCode::Enter => StudioKey::Enter,
        KeyCode::Esc => StudioKey::Escape,
        KeyCode::Backspace => StudioKey::Backspace,
        KeyCode::Tab => StudioKey::Tab,
        // Shift+Tab arrives as a distinct BackTab code; normalize it to a
        // shifted Tab so the shared keymap sees one key with a modifier.
        KeyCode::BackTab => StudioKey::Tab,
        _ => return None,
    };
    let force_shift = matches!(key.code, KeyCode::BackTab);
    Some(StudioKeyEvent {
        key: studio_key,
        modifiers: StudioKeyModifiers {
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            shift: force_shift || key.modifiers.contains(KeyModifiers::SHIFT),
            meta: key.modifiers.contains(KeyModifiers::SUPER),
        },
    })
}

fn key_context(core: &StudioCore) -> StudioKeyContext {
    // Capability resolution is shared (base) so terminal and web agree;
    // this adapter only translates crossterm events.
    core.key_context()
}

fn move_focused_row(core: &mut StudioCore, delta: i32) -> (bool, Vec<StudioEffect>) {
    let vm = core.envelope(Vec::new()).view_model;
    let focused = vm.workspace.focused_tile;
    let Some(root) = vm.workspace.root.as_ref() else {
        return (false, Vec::new());
    };
    let Some((count, is_feed)) = focused_selectable(root, &focused) else {
        return (false, Vec::new());
    };
    if count == 0 {
        return (false, Vec::new());
    }
    let current = stored_cursor(core).min(count.saturating_sub(1));
    // The feed cursor is distance-from-bottom (0 = newest), so arrow-up
    // walks back into history — the opposite sense from a top-down row list.
    let step = if is_feed { -delta } else { delta };
    let next = if step < 0 {
        current.saturating_sub(1)
    } else {
        (current + 1).min(count.saturating_sub(1))
    };
    if next == current {
        return (false, Vec::new());
    }
    let effects = core.dispatch(StudioEvent::Ui {
        event: StudioUiEvent::SetTileCursor {
            tile_id: focused,
            index: next,
        },
    });
    (true, effects)
}

fn move_focused_row_or_focus(
    core: &mut StudioCore,
    delta: i32,
    fallback_direction: FocusDirection,
) -> Vec<StudioEffect> {
    let (handled, effects) = move_focused_row(core, delta);
    if handled {
        effects
    } else {
        core.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::FocusDirection {
                direction: fallback_direction,
            },
        })
    }
}

/// The focused tile's selectable count and whether it is a feed (timeline).
/// Feeds count their coalesced entries and address them as distance-from-
/// bottom; row lists count rows addressed top-down.
fn focused_selectable(node: &StudioLayoutNodeVm, focused: &str) -> Option<(usize, bool)> {
    match node {
        StudioLayoutNodeVm::Tile { tile_id, view, .. } if tile_id == focused => {
            Some(selectable_of(view))
        }
        StudioLayoutNodeVm::Tile { .. } => None,
        StudioLayoutNodeVm::Split { first, second, .. } => {
            focused_selectable(first, focused).or_else(|| focused_selectable(second, focused))
        }
    }
}

/// The focused tile id and the foldable turn-section under its point, when
/// the focused lens is a feed positioned on one. Drives plain ←/→ folding.
fn focused_fold_section(core: &mut StudioCore) -> Option<(String, usize)> {
    let vm = core.envelope(Vec::new()).view_model;
    let focused = vm.workspace.focused_tile;
    let root = vm.workspace.root.as_ref()?;
    find_fold_section(root, &focused).map(|section| (focused.clone(), section))
}

fn find_fold_section(node: &StudioLayoutNodeVm, focused: &str) -> Option<usize> {
    match node {
        StudioLayoutNodeVm::Tile {
            tile_id,
            view: StudioViewVm::Timeline { fold_section, .. },
            ..
        } if tile_id == focused => *fold_section,
        StudioLayoutNodeVm::Tile { .. } => None,
        StudioLayoutNodeVm::Split { first, second, .. } => {
            find_fold_section(first, focused).or_else(|| find_fold_section(second, focused))
        }
    }
}

fn selectable_of(view: &StudioViewVm) -> (usize, bool) {
    match view {
        StudioViewVm::Rows { rows, .. } => (rows.len(), false),
        StudioViewVm::Timeline { entries, .. } => (entries.len(), true),
        _ => (0, false),
    }
}

/// The focused tile's stored list cursor (row index, or feed distance-from-
/// bottom). Both renderers store it the same way; the meaning is per-widget.
fn stored_cursor(core: &StudioCore) -> usize {
    match core
        .workspace
        .tiles
        .get(&core.workspace.focused_tile)
        .map(|tile| &tile.local)
    {
        Some(ViewLocalState::GenericList { cursor, .. }) => *cursor,
        _ => 0,
    }
}
