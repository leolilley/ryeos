//! Terminal key translation: crossterm events → shared ryeos key
//! commands → core events. Both the bindings (`ryeos_key_command`) and the
//! command interpretation (`RyeOsCore::apply_key_command`) live in
//! `ryeos-client-base` so terminal and web agree; this is only the
//! crossterm adapter plus the one deliberate terminal-local binding
//! (plain ←/→ feed folding).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ryeos_client_base::ui::view_model::{RyeOsLayoutNodeVm, RyeOsViewVm};
use ryeos_client_base::ui::{
    ryeos_key_command, RyeOsCore, RyeOsEffect, RyeOsEvent, RyeOsKey, RyeOsKeyCommand,
    RyeOsKeyContext, RyeOsKeyEvent, RyeOsKeyModifiers, RyeOsUiEvent,
};

pub fn handle_key(core: &mut RyeOsCore, key: KeyEvent) -> Vec<RyeOsEffect> {
    let Some(event) = terminal_ryeos_key_event(key) else {
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
    if no_mods && matches!(event.key, RyeOsKey::ArrowLeft | RyeOsKey::ArrowRight) {
        let expansion_wins = match event.key {
            RyeOsKey::ArrowRight => context.focused_row_expandable && !context.focused_row_expanded,
            RyeOsKey::ArrowLeft => context.focused_row_expandable && context.focused_row_expanded,
            _ => false,
        };
        if !expansion_wins {
            if let Some((tile_id, section)) = focused_fold_section(core) {
                let collapsed = matches!(event.key, RyeOsKey::ArrowLeft);
                return core.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetFold {
                        tile_id,
                        section,
                        collapsed,
                    },
                });
            }
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

/// The focused tile id and the foldable turn-section under its point, when
/// the focused lens is a feed positioned on one. Drives plain ←/→ folding.
fn focused_fold_section(core: &mut RyeOsCore) -> Option<(String, usize)> {
    let vm = core.envelope(Vec::new()).view_model;
    let focused = vm.workspace.focused_tile;
    let root = vm.workspace.root.as_ref()?;
    find_fold_section(root, &focused).map(|section| (focused.clone(), section))
}

fn find_fold_section(node: &RyeOsLayoutNodeVm, focused: &str) -> Option<usize> {
    match node {
        RyeOsLayoutNodeVm::Tile {
            tile_id,
            view:
                RyeOsViewVm::Timeline { fold_section, .. } | RyeOsViewVm::Sections { fold_section, .. },
            ..
        } if tile_id == focused => *fold_section,
        RyeOsLayoutNodeVm::Tile { .. } => None,
        RyeOsLayoutNodeVm::Split { first, second, .. } => {
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
