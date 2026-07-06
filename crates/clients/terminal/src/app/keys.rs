//! Terminal key translation: crossterm events → shared studio key
//! commands → core events. Both the bindings (`studio_key_command`) and the
//! command interpretation (`StudioCore::apply_key_command`) live in
//! `ryeos-client-base` so terminal and web agree; this is only the
//! crossterm adapter plus the one deliberate terminal-local binding
//! (plain ←/→ feed folding).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ryeos_client_base::studio::view_model::{StudioLayoutNodeVm, StudioViewVm};
use ryeos_client_base::studio::{
    studio_key_command, StudioCore, StudioEffect, StudioEvent, StudioKey, StudioKeyCommand,
    StudioKeyContext, StudioKeyEvent, StudioKeyModifiers, StudioUiEvent,
};

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
    let context = key_context(core);
    if no_mods && matches!(event.key, StudioKey::ArrowLeft | StudioKey::ArrowRight) {
        let expansion_wins = match event.key {
            StudioKey::ArrowRight => context.focused_row_expandable && !context.focused_row_expanded,
            StudioKey::ArrowLeft => context.focused_row_expandable && context.focused_row_expanded,
            _ => false,
        };
        if !expansion_wins {
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
    }
    let command = studio_key_command(event, context);
    core.apply_key_command(command)
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
            view:
                StudioViewVm::Timeline { fold_section, .. }
                | StudioViewVm::Sections { fold_section, .. },
            ..
        } if tile_id == focused => *fold_section,
        StudioLayoutNodeVm::Tile { .. } => None,
        StudioLayoutNodeVm::Split { first, second, .. } => {
            find_fold_section(first, focused).or_else(|| find_fold_section(second, focused))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backtab_normalizes_to_shifted_tab() {
        // crossterm sends Shift+Tab as a distinct BackTab code; the adapter
        // must present it to the shared keymap as Tab + shift so Shift+Tab
        // drives the backward target cycle.
        let ev = terminal_studio_key_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE))
            .expect("BackTab maps to a studio key");
        assert_eq!(ev.key, StudioKey::Tab);
        assert!(ev.modifiers.shift, "BackTab carries shift");
    }

    #[test]
    fn plain_tab_is_unshifted() {
        let ev = terminal_studio_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .expect("Tab maps to a studio key");
        assert_eq!(ev.key, StudioKey::Tab);
        assert!(!ev.modifiers.shift);
    }
}
