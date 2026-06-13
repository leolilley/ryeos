//! Terminal key translation: crossterm events → shared studio key
//! commands → core events. The bindings themselves live in
//! `ryeos-client-base` (`studio_key_command`) so terminal and web agree;
//! this is only the crossterm adapter and the row-cursor fallback.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ryeos_client_base::studio::view_model::{StudioLayoutNodeVm, StudioRowVm, StudioViewVm};
use ryeos_client_base::studio::{
    studio_key_command, StudioCore, StudioEffect, StudioEvent, StudioKey, StudioKeyCommand,
    StudioKeyContext, StudioKeyEvent, StudioKeyModifiers, StudioUiEvent,
};
use ryeos_client_base::workspace::FocusDirection;

pub fn handle_key(core: &mut StudioCore, key: KeyEvent) -> Vec<StudioEffect> {
    let Some(event) = terminal_studio_key_event(key) else {
        return Vec::new();
    };
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
        _ => return None,
    };
    Some(StudioKeyEvent {
        key: studio_key,
        modifiers: StudioKeyModifiers {
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            shift: key.modifiers.contains(KeyModifiers::SHIFT),
            meta: key.modifiers.contains(KeyModifiers::SUPER),
        },
    })
}

fn key_context(core: &StudioCore) -> StudioKeyContext {
    StudioKeyContext {
        launcher_open: core.ui.launcher.open,
        // Input follows the focused view instance: printable keys edit a
        // buffer only when the focused instance declares `input`.
        input_visible: core.has_focused_input(),
    }
}

fn move_focused_row(core: &mut StudioCore, delta: i32) -> (bool, Vec<StudioEffect>) {
    let vm = core.envelope(Vec::new()).view_model;
    let focused = vm.workspace.focused_tile;
    let Some(root) = vm.workspace.root.as_ref() else {
        return (false, Vec::new());
    };
    let row_count = focused_row_count(root, &focused).unwrap_or(0);
    if row_count == 0 {
        return (false, Vec::new());
    }
    let current = focused_selected_index(root, &focused).unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(1)
    } else {
        (current + 1).min(row_count.saturating_sub(1))
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

fn focused_row_count(node: &StudioLayoutNodeVm, focused: &str) -> Option<usize> {
    match node {
        StudioLayoutNodeVm::Tile { tile_id, view, .. } if tile_id == focused => {
            Some(rows_in_view(view).len())
        }
        StudioLayoutNodeVm::Tile { .. } => None,
        StudioLayoutNodeVm::Split { first, second, .. } => {
            focused_row_count(first, focused).or_else(|| focused_row_count(second, focused))
        }
    }
}

fn focused_selected_index(node: &StudioLayoutNodeVm, focused: &str) -> Option<usize> {
    match node {
        StudioLayoutNodeVm::Tile { tile_id, view, .. } if tile_id == focused => {
            rows_in_view(view).iter().position(|row| row.selected)
        }
        StudioLayoutNodeVm::Tile { .. } => None,
        StudioLayoutNodeVm::Split { first, second, .. } => focused_selected_index(first, focused)
            .or_else(|| focused_selected_index(second, focused)),
    }
}

fn rows_in_view(view: &StudioViewVm) -> &[StudioRowVm] {
    match view {
        StudioViewVm::Rows { rows, .. } => rows,
        _ => &[],
    }
}
