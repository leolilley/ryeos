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
    Tab,
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
    /// The keys/help overlay is open — any dismiss key closes it; everything
    /// else is swallowed so the overlay is modal.
    pub help_open: bool,
    pub input_visible: bool,
    /// The focused input has a non-empty buffer. Plain Enter then submits
    /// (the chat convention) rather than activating a row — terminals
    /// can't reliably send Shift+Enter, so it can't be the only submit.
    pub input_has_text: bool,
    /// The focused input declares a completion source.
    pub input_has_completion: bool,
    /// Completion would actually accept something right now for the focused
    /// buffer (cursor at end, leading single `/`, and a matching record).
    /// Cursor-aware so Tab never dispatches a no-op completion when it could
    /// instead cycle the route target.
    pub input_can_accept_completion: bool,
    /// The focused input's declared targeting capability, if any.
    pub input_target_cycle: Option<super::content::InputTargetCycle>,
    /// The head thread is mid-execution — esc interrupts it instead of
    /// closing the focused tile.
    pub head_thread_running: bool,
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
    if context.help_open {
        return help_key_command(event);
    }

    match event.key {
        // Ctrl+K is the reliable launcher binding: a control char that
        // terminals and tmux pass straight through. Alt+K is kept for
        // environments that deliver it, but Alt/ESC combos are eaten by
        // tmux, so Ctrl+K is what we advertise.
        StudioKey::Char(c)
            if (event.modifiers.ctrl_only() || event.modifiers.alt_only())
                && c.eq_ignore_ascii_case(&'k') =>
        {
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
        // Esc interrupts a running head thread (chat convention); otherwise it
        // closes the focused tile.
        StudioKey::Escape if event.modifiers.none() && context.head_thread_running => {
            ui(StudioUiEvent::InterruptHead)
        }
        StudioKey::Escape if event.modifiers.none() => action(StudioAction::CloseFocused),
        // Plain Enter submits when you're typing in the input (chat
        // convention); Shift+Enter also submits where the terminal can
        // send it. An empty input lets plain Enter activate a row.
        StudioKey::Enter
            if context.input_visible
                && context.input_has_text
                && (event.modifiers.none() || event.modifiers.shift_only()) =>
        {
            ui(StudioUiEvent::SubmitInput)
        }
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
        // Tab precedence (central interaction policy, capability + buffer
        // state — never a per-view keybinding):
        //   1. completion that can accept now (`/foo⇥`) wins;
        //   2. else route-chain targeting cycles (Tab fwd, Shift+Tab back);
        //   3. else a declared-but-not-acceptable completion is the fallback.
        StudioKey::Tab if context.input_can_accept_completion && event.modifiers.none() => {
            ui(StudioUiEvent::CompleteInput)
        }
        StudioKey::Tab
            if context.input_target_cycle.is_some() && event.modifiers.none() =>
        {
            ui(StudioUiEvent::CycleInputTarget { forward: true })
        }
        StudioKey::Tab
            if context.input_target_cycle.is_some() && event.modifiers.shift_only() =>
        {
            ui(StudioUiEvent::CycleInputTarget { forward: false })
        }
        StudioKey::Tab
            if context.input_visible
                && context.input_has_completion
                && event.modifiers.none() =>
        {
            ui(StudioUiEvent::CompleteInput)
        }
        StudioKey::Char('c') if event.modifiers.ctrl_only() => StudioKeyCommand::Quit,
        // `?` opens the keys overlay — but only with an empty input, so it
        // still types into a message you're composing (the chat convention;
        // the always-present foot input owns plain chars otherwise).
        StudioKey::Char('?') if event.modifiers.none() && !context.input_has_text => {
            ui(StudioUiEvent::OpenHelp)
        }
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

/// The keys overlay is modal: any dismiss key (Esc, `?`, `q`, Enter) closes
/// it, Ctrl+C still quits, and every other key is swallowed.
fn help_key_command(event: StudioKeyEvent) -> StudioKeyCommand {
    match event.key {
        StudioKey::Char('c') if event.modifiers.ctrl_only() => StudioKeyCommand::Quit,
        StudioKey::Escape | StudioKey::Enter => ui(StudioUiEvent::CloseHelp),
        StudioKey::Char(c) if event.modifiers.none() && (c == '?' || c == 'q') => {
            ui(StudioUiEvent::CloseHelp)
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

    fn ctrl(ch: char) -> StudioKeyEvent {
        StudioKeyEvent {
            key: StudioKey::Char(ch),
            modifiers: StudioKeyModifiers {
                ctrl: true,
                ..Default::default()
            },
        }
    }

    #[test]
    fn ctrl_k_opens_the_launcher() {
        // The reliable launcher binding — Alt/ESC combos are eaten by tmux.
        assert!(matches!(
            studio_key_command(ctrl('k'), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::OpenLauncher
            }
        ));
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
            help_open: false,
            input_visible,
            input_has_text: false,
            input_has_completion: false,
            input_can_accept_completion: false,
            input_target_cycle: None,
            head_thread_running: false,
        }
    }

    fn context_typing() -> StudioKeyContext {
        StudioKeyContext {
            launcher_open: false,
            help_open: false,
            input_visible: true,
            input_has_text: true,
            input_has_completion: false,
            input_can_accept_completion: false,
            input_target_cycle: None,
            head_thread_running: false,
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
            studio_key_command(key(StudioKey::Tab), context_target()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CycleInputTarget { forward: true }
            }
        ));
        assert!(matches!(
            studio_key_command(shift_tab(), context_target()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CycleInputTarget { forward: false }
            }
        ));
        assert!(matches!(
            studio_key_command(shift_enter(), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::SubmitInput
            }
        ));
    }

    /// A context whose focused input declares route-chain targeting.
    fn context_target() -> StudioKeyContext {
        StudioKeyContext {
            input_target_cycle: Some(crate::studio::content::InputTargetCycle::RouteChains),
            ..context(false, true)
        }
    }

    fn shift_tab() -> StudioKeyEvent {
        StudioKeyEvent {
            key: StudioKey::Tab,
            modifiers: StudioKeyModifiers {
                shift: true,
                ..Default::default()
            },
        }
    }

    #[test]
    fn esc_interrupts_running_head_else_falls_through() {
        // Head thread running → esc interrupts it.
        let running = StudioKeyContext {
            head_thread_running: true,
            ..context(false, true)
        };
        assert!(matches!(
            studio_key_command(key(StudioKey::Escape), running),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::InterruptHead
            }
        ));
        // Idle (no running head) → esc does NOT interrupt (existing behavior).
        assert!(!matches!(
            studio_key_command(key(StudioKey::Escape), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::InterruptHead
            }
        ));
    }

    #[test]
    fn tab_precedence_completion_vs_targeting() {
        // No target, has completion that can accept → Tab completes (1 & 4).
        let complete_ctx = StudioKeyContext {
            input_has_completion: true,
            input_can_accept_completion: true,
            ..context(false, true)
        };
        assert!(matches!(
            studio_key_command(key(StudioKey::Tab), complete_ctx),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CompleteInput
            }
        ));

        // BOTH completion and targeting, completion can accept (`/foo⇥`) →
        // completion wins (4a).
        let both_accept = StudioKeyContext {
            input_has_completion: true,
            input_can_accept_completion: true,
            input_target_cycle: Some(crate::studio::content::InputTargetCycle::RouteChains),
            ..context(false, true)
        };
        assert!(matches!(
            studio_key_command(key(StudioKey::Tab), both_accept),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CompleteInput
            }
        ));

        // BOTH, but completion can't accept now (prose / cursor mid-line) →
        // targeting cycles (4b & 4c).
        let both_no_accept = StudioKeyContext {
            input_has_completion: true,
            input_can_accept_completion: false,
            input_target_cycle: Some(crate::studio::content::InputTargetCycle::RouteChains),
            ..context(false, true)
        };
        assert!(matches!(
            studio_key_command(key(StudioKey::Tab), both_no_accept.clone()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CycleInputTarget { forward: true }
            }
        ));
        assert!(matches!(
            studio_key_command(shift_tab(), both_no_accept),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CycleInputTarget { forward: false }
            }
        ));

        // Launcher open beats everything (test 4).
        let launcher_ctx = StudioKeyContext {
            launcher_open: true,
            input_target_cycle: Some(crate::studio::content::InputTargetCycle::RouteChains),
            ..context(true, true)
        };
        assert!(!matches!(
            studio_key_command(key(StudioKey::Tab), launcher_ctx),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CycleInputTarget { .. }
            }
        ));
    }

    #[test]
    fn plain_enter_submits_when_input_has_text() {
        // The core chat loop: typing then plain Enter submits (terminals
        // can't be relied on for Shift+Enter).
        assert!(matches!(
            studio_key_command(key(StudioKey::Enter), context_typing()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::SubmitInput
            }
        ));
        // Empty input: plain Enter activates the focused row instead.
        assert!(matches!(
            studio_key_command(key(StudioKey::Enter), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::ActivateFocused
            }
        ));
    }

    #[test]
    fn question_mark_opens_help_only_with_an_empty_input() {
        // Empty input → `?` opens the keys overlay.
        assert!(matches!(
            studio_key_command(key(StudioKey::Char('?')), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::OpenHelp
            }
        ));
        // Mid-message → `?` is just a character (the foot input owns it).
        assert!(matches!(
            studio_key_command(key(StudioKey::Char('?')), context_typing()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::InsertInputChar { ch: '?' }
            }
        ));
    }

    #[test]
    fn help_overlay_is_modal_and_dismissible() {
        let help_ctx = StudioKeyContext {
            help_open: true,
            ..context(false, true)
        };
        for k in [
            StudioKey::Escape,
            StudioKey::Enter,
            StudioKey::Char('?'),
            StudioKey::Char('q'),
        ] {
            assert!(
                matches!(
                    studio_key_command(key(k), help_ctx),
                    StudioKeyCommand::Ui {
                        event: StudioUiEvent::CloseHelp
                    }
                ),
                "{k:?} should close the help overlay"
            );
        }
        // Any other key is swallowed (the overlay is modal)…
        assert_eq!(
            studio_key_command(key(StudioKey::Char('x')), help_ctx),
            StudioKeyCommand::Ignore
        );
        // …but Ctrl+C still quits.
        assert_eq!(
            studio_key_command(ctrl('c'), help_ctx),
            StudioKeyCommand::Quit
        );
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
