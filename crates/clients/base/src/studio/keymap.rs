//! Shared Studio key map.
//!
//! This mirrors the browser Studio key handling so renderers do not grow
//! divergent product behavior. Platform adapters convert local key events into
//! `StudioKeyEvent`; this module returns semantic Studio commands/events.

use serde::{Deserialize, Serialize};

use super::event::{StudioAction, StudioStackMoveDirection, StudioUiEvent};
use crate::workspace::FocusDirection;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioKey {
    Char(char),
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Enter,
    Escape,
    Backspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StudioKeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudioKeyEvent {
    pub key: StudioKey,
    pub modifiers: StudioKeyModifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StudioKeyContext {
    pub launcher_open: bool,
    pub input_visible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioKeyCommand {
    Ui {
        event: StudioUiEvent,
    },
    MoveFocusedRowOrFocus {
        delta: i32,
        fallback_direction: FocusDirection,
    },
    InsertLauncherChar {
        ch: char,
    },
    DeleteLauncherChar,
    Quit,
    Ignore,
}

pub fn studio_key_command(event: StudioKeyEvent, context: StudioKeyContext) -> StudioKeyCommand {
    if context.launcher_open {
        return launcher_key_command(event);
    }

    match event.key {
        StudioKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'k') => {
            ui(StudioUiEvent::OpenLauncher)
        }
        StudioKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'q') => {
            action(StudioAction::CloseFocused)
        }
        StudioKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'m') => {
            action(StudioAction::ToggleFocusedMaster)
        }
        StudioKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'t') => {
            action(StudioAction::ToggleTopStatusBar)
        }
        StudioKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'b') => {
            action(StudioAction::ToggleBottomStatusBar)
        }
        StudioKey::ArrowUp if event.modifiers.ctrl_shift() => resize(FocusDirection::Up),
        StudioKey::ArrowDown if event.modifiers.ctrl_shift() => resize(FocusDirection::Down),
        StudioKey::ArrowLeft if event.modifiers.ctrl_shift() => resize(FocusDirection::Left),
        StudioKey::ArrowRight if event.modifiers.ctrl_shift() => resize(FocusDirection::Right),
        StudioKey::ArrowUp if event.modifiers.ctrl_only() => {
            move_focused_tile(StudioStackMoveDirection::Up)
        }
        StudioKey::ArrowDown if event.modifiers.ctrl_only() => {
            move_focused_tile(StudioStackMoveDirection::Down)
        }
        StudioKey::ArrowLeft if event.modifiers.ctrl_only() => {
            cycle_tab(StudioStackMoveDirection::Up)
        }
        StudioKey::ArrowRight if event.modifiers.ctrl_only() => {
            cycle_tab(StudioStackMoveDirection::Down)
        }
        StudioKey::Escape if event.modifiers.none() => action(StudioAction::CloseFocused),
        StudioKey::Enter if context.input_visible && event.modifiers.shift_only() => {
            ui(StudioUiEvent::SubmitInput)
        }
        StudioKey::Enter if event.modifiers.none() => ui(StudioUiEvent::ActivateFocused),
        StudioKey::ArrowUp if event.modifiers.none() => StudioKeyCommand::MoveFocusedRowOrFocus {
            delta: -1,
            fallback_direction: FocusDirection::Up,
        },
        StudioKey::ArrowDown if event.modifiers.none() => StudioKeyCommand::MoveFocusedRowOrFocus {
            delta: 1,
            fallback_direction: FocusDirection::Down,
        },
        StudioKey::ArrowLeft if event.modifiers.none() => focus(FocusDirection::Left),
        StudioKey::ArrowRight if event.modifiers.none() => focus(FocusDirection::Right),
        StudioKey::Backspace if context.input_visible && event.modifiers.none() => {
            ui(StudioUiEvent::DeleteInputChar)
        }
        StudioKey::Char('c') if event.modifiers.ctrl_only() => StudioKeyCommand::Quit,
        StudioKey::Char(ch)
            if context.input_visible
                && !event.modifiers.ctrl
                && !event.modifiers.alt
                && !event.modifiers.meta =>
        {
            ui(StudioUiEvent::InsertInputChar { ch })
        }
        _ => StudioKeyCommand::Ignore,
    }
}

fn launcher_key_command(event: StudioKeyEvent) -> StudioKeyCommand {
    match event.key {
        StudioKey::Escape if event.modifiers.none() => ui(StudioUiEvent::CloseLauncher),
        StudioKey::Enter if event.modifiers.none_or_shift() => ui(StudioUiEvent::ChooseLauncher {
            secondary: event.modifiers.shift,
        }),
        StudioKey::ArrowUp if event.modifiers.none() => {
            ui(StudioUiEvent::MoveLauncherSelection { delta: -1 })
        }
        StudioKey::ArrowDown if event.modifiers.none() => {
            ui(StudioUiEvent::MoveLauncherSelection { delta: 1 })
        }
        StudioKey::Backspace if event.modifiers.none() => StudioKeyCommand::DeleteLauncherChar,
        StudioKey::Char(ch)
            if !event.modifiers.ctrl && !event.modifiers.alt && !event.modifiers.meta =>
        {
            StudioKeyCommand::InsertLauncherChar { ch }
        }
        StudioKey::Char('c') if event.modifiers.ctrl_only() => StudioKeyCommand::Quit,
        _ => StudioKeyCommand::Ignore,
    }
}

fn ui(event: StudioUiEvent) -> StudioKeyCommand {
    StudioKeyCommand::Ui { event }
}

fn action(action: StudioAction) -> StudioKeyCommand {
    ui(StudioUiEvent::Activate { action })
}

fn focus(direction: FocusDirection) -> StudioKeyCommand {
    ui(StudioUiEvent::FocusDirection { direction })
}

fn resize(direction: FocusDirection) -> StudioKeyCommand {
    action(StudioAction::ResizeFocused { direction })
}

fn move_focused_tile(direction: StudioStackMoveDirection) -> StudioKeyCommand {
    action(StudioAction::MoveFocusedTile { direction })
}

fn cycle_tab(direction: StudioStackMoveDirection) -> StudioKeyCommand {
    action(StudioAction::CycleTab { direction })
}

impl StudioKeyModifiers {
    fn none(self) -> bool {
        !self.ctrl && !self.alt && !self.meta && !self.shift
    }

    fn none_or_shift(self) -> bool {
        !self.ctrl && !self.alt && !self.meta
    }

    fn shift_only(self) -> bool {
        self.shift && !self.ctrl && !self.alt && !self.meta
    }

    fn alt_only(self) -> bool {
        self.alt && !self.ctrl && !self.meta
    }

    fn ctrl_only(self) -> bool {
        self.ctrl && !self.shift && !self.alt && !self.meta
    }

    fn ctrl_shift(self) -> bool {
        self.ctrl && self.shift && !self.alt && !self.meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: StudioKey) -> StudioKeyEvent {
        StudioKeyEvent {
            key,
            modifiers: StudioKeyModifiers::default(),
        }
    }

    fn alt(ch: char) -> StudioKeyEvent {
        StudioKeyEvent {
            key: StudioKey::Char(ch),
            modifiers: StudioKeyModifiers {
                alt: true,
                ..Default::default()
            },
        }
    }

    fn ctrl_arrow(key: StudioKey) -> StudioKeyEvent {
        StudioKeyEvent {
            key,
            modifiers: StudioKeyModifiers {
                ctrl: true,
                ..Default::default()
            },
        }
    }

    fn context(launcher_open: bool, input_visible: bool) -> StudioKeyContext {
        StudioKeyContext {
            launcher_open,
            input_visible,
        }
    }

    fn shift_enter() -> StudioKeyEvent {
        StudioKeyEvent {
            key: StudioKey::Enter,
            modifiers: StudioKeyModifiers {
                shift: true,
                ..Default::default()
            },
        }
    }

    #[test]
    fn maps_global_web_studio_keys() {
        assert!(matches!(
            studio_key_command(alt('k'), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::OpenLauncher
            }
        ));
        assert!(matches!(
            studio_key_command(alt('q'), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::Activate {
                    action: StudioAction::CloseFocused
                }
            }
        ));
        assert!(matches!(
            studio_key_command(ctrl_arrow(StudioKey::ArrowRight), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::Activate {
                    action: StudioAction::CycleTab {
                        direction: StudioStackMoveDirection::Down
                    }
                }
            }
        ));
    }

    #[test]
    fn maps_rows_before_focus_like_web() {
        assert_eq!(
            studio_key_command(key(StudioKey::ArrowDown), context(false, true)),
            StudioKeyCommand::MoveFocusedRowOrFocus {
                delta: 1,
                fallback_direction: FocusDirection::Down,
            }
        );
    }

    #[test]
    fn launcher_captures_text_and_selection() {
        assert_eq!(
            studio_key_command(key(StudioKey::Char('a')), context(true, false)),
            StudioKeyCommand::InsertLauncherChar { ch: 'a' }
        );
        assert!(matches!(
            studio_key_command(key(StudioKey::ArrowUp), context(true, false)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::MoveLauncherSelection { delta: -1 }
            }
        ));
    }

    #[test]
    fn maps_studio_input_text_and_submit() {
        assert!(matches!(
            studio_key_command(key(StudioKey::Char('x')), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::InsertInputChar { ch: 'x' }
            }
        ));
        assert!(matches!(
            studio_key_command(key(StudioKey::Backspace), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::DeleteInputChar
            }
        ));
        assert!(matches!(
            studio_key_command(shift_enter(), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::SubmitInput
            }
        ));
    }

    #[test]
    fn does_not_edit_hidden_input() {
        assert_eq!(
            studio_key_command(key(StudioKey::Char('x')), context(false, false)),
            StudioKeyCommand::Ignore,
        );
        assert_eq!(
            studio_key_command(key(StudioKey::Backspace), context(false, false)),
            StudioKeyCommand::Ignore,
        );
        assert_eq!(
            studio_key_command(shift_enter(), context(false, false)),
            StudioKeyCommand::Ignore,
        );
    }
}
