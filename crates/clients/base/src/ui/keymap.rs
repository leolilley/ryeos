//! Shared RyeOs key map.
//!
//! This mirrors the browser RyeOs key handling so renderers do not grow
//! divergent product behavior. Platform adapters convert local key events into
//! `RyeOsKeyEvent`; this module returns semantic RyeOs commands/events.

use serde::{Deserialize, Serialize};

use super::event::{RyeOsAction, RyeOsStackMoveDirection, RyeOsUiEvent};
use crate::workspace::FocusDirection;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsKey {
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
pub struct RyeOsKeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsKeyEvent {
    pub key: RyeOsKey,
    pub modifiers: RyeOsKeyModifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RyeOsKeyContext {
    pub overlay_open: bool,
    pub input_visible: bool,
    /// The focused input has a non-empty buffer. Plain Enter then submits
    /// (the chat convention) rather than activating a row — terminals
    /// can't reliably send Shift+Enter, so it can't be the only submit.
    pub input_has_text: bool,
    /// The focused input is a LIVE FILTER (feeds a source param, no submit
    /// target). Its buffer applies live, so Enter must still activate the
    /// focused row rather than submit — typing narrows, Enter opens.
    pub input_is_live_filter: bool,
    /// The focused live filter declares more than one target field, so Tab
    /// cycles which field it filters (status → kind → source).
    pub input_filter_fields: bool,
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
    /// The focused rows/table/timeline lens has an expandable selected point.
    pub focused_row_expandable: bool,
    /// The focused expandable point is currently open.
    pub focused_row_expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsKeyCommand {
    Ui {
        event: RyeOsUiEvent,
    },
    MoveFocusedRowOrFocus {
        delta: i32,
        fallback_direction: FocusDirection,
    },
    InsertOverlayChar {
        ch: char,
    },
    DeleteOverlayChar,
    Quit,
    Ignore,
}

pub fn ryeos_key_command(event: RyeOsKeyEvent, context: RyeOsKeyContext) -> RyeOsKeyCommand {
    if context.overlay_open {
        return overlay_key_command(event);
    }

    match event.key {
        // Ctrl+K is the reliable view-overlay binding: a control char that
        // terminals and tmux pass straight through. Alt+K is kept for
        // environments that deliver it, but Alt/ESC combos are eaten by
        // tmux, so Ctrl+K is what we advertise.
        RyeOsKey::Char(c)
            if (event.modifiers.ctrl_only() || event.modifiers.alt_only())
                && c.eq_ignore_ascii_case(&'k') =>
        {
            ui(RyeOsUiEvent::OpenOverlay {
                overlay_id: "views".to_string(),
            })
        }
        // Most terminals cannot distinguish Ctrl+Shift+P from Ctrl+P, so the
        // reliable command-overlay binding is Ctrl+P.
        RyeOsKey::Char(c)
            if (event.modifiers.ctrl_only() || event.modifiers.ctrl_shift())
                && c.eq_ignore_ascii_case(&'p') =>
        {
            ui(RyeOsUiEvent::OpenOverlay {
                overlay_id: "commands".to_string(),
            })
        }
        RyeOsKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'q') => {
            action(RyeOsAction::CloseFocused)
        }
        RyeOsKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'m') => {
            action(RyeOsAction::ToggleFocusedMaster)
        }
        RyeOsKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'t') => {
            action(RyeOsAction::ToggleTopStatusBar)
        }
        RyeOsKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'b') => {
            action(RyeOsAction::ToggleBottomStatusBar)
        }
        RyeOsKey::Char(c)
            if (event.modifiers.ctrl_only() || event.modifiers.alt_only())
                && c.eq_ignore_ascii_case(&'s') =>
        {
            action(RyeOsAction::ToggleBackdropBreak)
        }
        RyeOsKey::ArrowUp if event.modifiers.ctrl_shift() => resize(FocusDirection::Up),
        RyeOsKey::ArrowDown if event.modifiers.ctrl_shift() => resize(FocusDirection::Down),
        RyeOsKey::ArrowLeft if event.modifiers.ctrl_shift() => resize(FocusDirection::Left),
        RyeOsKey::ArrowRight if event.modifiers.ctrl_shift() => resize(FocusDirection::Right),
        RyeOsKey::ArrowUp if event.modifiers.ctrl_only() => {
            move_focused_tile(RyeOsStackMoveDirection::Up)
        }
        RyeOsKey::ArrowDown if event.modifiers.ctrl_only() => {
            move_focused_tile(RyeOsStackMoveDirection::Down)
        }
        RyeOsKey::ArrowLeft if event.modifiers.ctrl_only() => {
            cycle_tab(RyeOsStackMoveDirection::Up)
        }
        RyeOsKey::ArrowRight if event.modifiers.ctrl_only() => {
            cycle_tab(RyeOsStackMoveDirection::Down)
        }
        RyeOsKey::ArrowLeft if event.modifiers.alt_only() => ui(RyeOsUiEvent::PopLens),
        // Esc interrupts a running head thread (chat convention); otherwise it
        // closes the focused tile.
        RyeOsKey::Escape if event.modifiers.none() && context.head_thread_running => {
            ui(RyeOsUiEvent::InterruptHead)
        }
        RyeOsKey::Escape if event.modifiers.none() => action(RyeOsAction::CloseFocused),
        // Alt+Enter submits as a forceful INTERRUPT (cut the running thread's
        // in-flight cognition and redirect); plain Enter steers. Checked first so
        // the modifier wins over the plain-submit arm below.
        RyeOsKey::Enter
            if context.input_visible
                && context.input_has_text
                && !context.input_is_live_filter
                && event.modifiers.alt_only() =>
        {
            ui(RyeOsUiEvent::SubmitInputInterrupt)
        }
        // Plain Enter submits when you're typing in the input (chat
        // convention); Shift+Enter also submits where the terminal can
        // send it. An empty input lets plain Enter activate a row — as does a
        // live filter (it applies live; Enter opens the narrowed selection).
        RyeOsKey::Enter
            if context.input_visible
                && context.input_has_text
                && !context.input_is_live_filter
                && (event.modifiers.none() || event.modifiers.shift_only()) =>
        {
            ui(RyeOsUiEvent::SubmitInput)
        }
        RyeOsKey::Enter
            if context.input_visible
                && !context.input_is_live_filter
                && event.modifiers.shift_only() =>
        {
            ui(RyeOsUiEvent::SubmitInput)
        }
        RyeOsKey::Enter if event.modifiers.none() => ui(RyeOsUiEvent::ActivateFocused),
        RyeOsKey::ArrowUp if event.modifiers.none() => RyeOsKeyCommand::MoveFocusedRowOrFocus {
            delta: -1,
            fallback_direction: FocusDirection::Up,
        },
        RyeOsKey::ArrowDown if event.modifiers.none() => RyeOsKeyCommand::MoveFocusedRowOrFocus {
            delta: 1,
            fallback_direction: FocusDirection::Down,
        },
        RyeOsKey::ArrowRight
            if event.modifiers.none()
                && context.focused_row_expandable
                && !context.focused_row_expanded =>
        {
            ui(RyeOsUiEvent::ExpandSelectedRow { expand: true })
        }
        RyeOsKey::ArrowLeft
            if event.modifiers.none()
                && context.focused_row_expandable
                && context.focused_row_expanded =>
        {
            ui(RyeOsUiEvent::ExpandSelectedRow { expand: false })
        }
        RyeOsKey::ArrowLeft if event.modifiers.none() => focus(FocusDirection::Left),
        RyeOsKey::ArrowRight if event.modifiers.none() => focus(FocusDirection::Right),
        RyeOsKey::Backspace
            if context.input_visible && context.input_has_text && event.modifiers.none() =>
        {
            ui(RyeOsUiEvent::DeleteInputChar)
        }
        // Backspace with no text to edit steps back up the execution-drill
        // stack — the "return" from a step-in. A no-op at the top of the tree.
        RyeOsKey::Backspace if event.modifiers.none() => ui(RyeOsUiEvent::PopLens),
        // A live filter with multiple target fields owns Tab to cycle the field
        // (it has no completion or route-target, so Tab is otherwise free here).
        RyeOsKey::Tab if context.input_filter_fields && event.modifiers.none() => {
            ui(RyeOsUiEvent::CycleFilterField { forward: true })
        }
        RyeOsKey::Tab if context.input_filter_fields && event.modifiers.shift_only() => {
            ui(RyeOsUiEvent::CycleFilterField { forward: false })
        }
        // Tab precedence (central interaction policy, capability + buffer
        // state — never a per-view keybinding):
        //   1. completion that can accept now (`/foo⇥`) wins;
        //   2. else route-chain targeting cycles (Tab fwd, Shift+Tab back);
        //   3. else a declared-but-not-acceptable completion is the fallback.
        RyeOsKey::Tab if context.input_can_accept_completion && event.modifiers.none() => {
            ui(RyeOsUiEvent::CompleteInput)
        }
        RyeOsKey::Tab if context.input_target_cycle.is_some() && event.modifiers.none() => {
            ui(RyeOsUiEvent::CycleInputTarget { forward: true })
        }
        RyeOsKey::Tab if context.input_target_cycle.is_some() && event.modifiers.shift_only() => {
            ui(RyeOsUiEvent::CycleInputTarget { forward: false })
        }
        RyeOsKey::Tab
            if context.input_visible && context.input_has_completion && event.modifiers.none() =>
        {
            ui(RyeOsUiEvent::CompleteInput)
        }
        RyeOsKey::Char('c') if event.modifiers.ctrl_only() => RyeOsKeyCommand::Quit,
        // `?` opens the keys overlay — but only with an empty input, so it
        // still types into a message you're composing (the chat convention;
        // the always-present foot input owns plain chars otherwise).
        RyeOsKey::Char('?') if event.modifiers.none() && !context.input_has_text => {
            ui(RyeOsUiEvent::OpenOverlay {
                overlay_id: "shortcuts".to_string(),
            })
        }
        RyeOsKey::Char(ch)
            if context.input_visible
                && !event.modifiers.ctrl
                && !event.modifiers.alt
                && !event.modifiers.meta =>
        {
            ui(RyeOsUiEvent::InsertInputChar { ch })
        }
        _ => RyeOsKeyCommand::Ignore,
    }
}

fn overlay_key_command(event: RyeOsKeyEvent) -> RyeOsKeyCommand {
    match event.key {
        RyeOsKey::Escape if event.modifiers.none() => ui(RyeOsUiEvent::CloseOverlay),
        RyeOsKey::Enter if event.modifiers.none_or_shift() => ui(RyeOsUiEvent::ChooseOverlay {
            secondary: event.modifiers.shift,
        }),
        RyeOsKey::ArrowUp if event.modifiers.none() => {
            ui(RyeOsUiEvent::MoveOverlaySelection { delta: -1 })
        }
        RyeOsKey::ArrowDown if event.modifiers.none() => {
            ui(RyeOsUiEvent::MoveOverlaySelection { delta: 1 })
        }
        RyeOsKey::Backspace if event.modifiers.none() => RyeOsKeyCommand::DeleteOverlayChar,
        RyeOsKey::Char(ch)
            if !event.modifiers.ctrl && !event.modifiers.alt && !event.modifiers.meta =>
        {
            RyeOsKeyCommand::InsertOverlayChar { ch }
        }
        RyeOsKey::Char('c') if event.modifiers.ctrl_only() => RyeOsKeyCommand::Quit,
        _ => RyeOsKeyCommand::Ignore,
    }
}

fn ui(event: RyeOsUiEvent) -> RyeOsKeyCommand {
    RyeOsKeyCommand::Ui { event }
}

fn action(action: RyeOsAction) -> RyeOsKeyCommand {
    ui(RyeOsUiEvent::Activate { action })
}

fn focus(direction: FocusDirection) -> RyeOsKeyCommand {
    ui(RyeOsUiEvent::FocusDirection { direction })
}

fn resize(direction: FocusDirection) -> RyeOsKeyCommand {
    action(RyeOsAction::ResizeFocused { direction })
}

fn move_focused_tile(direction: RyeOsStackMoveDirection) -> RyeOsKeyCommand {
    action(RyeOsAction::MoveFocusedTile { direction })
}

fn cycle_tab(direction: RyeOsStackMoveDirection) -> RyeOsKeyCommand {
    action(RyeOsAction::CycleTab { direction })
}

impl RyeOsKeyModifiers {
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

impl super::model::RyeOsCore {
    /// Apply a resolved shared-keymap command to the core. This is the ONE
    /// interpretation of `RyeOsKeyCommand` — the row-cursor walk, the
    /// focus fallback, and the overlay query edits live here so renderers
    /// cannot drift on what a command does. Platform adapters translate
    /// their native key events into `RyeOsKeyEvent`, call
    /// [`ryeos_key_command`], and hand the command straight in; `Quit` is
    /// interpreted by the platform (terminal exits, browser leaves the key
    /// native) and is a no-op here, like `Ignore`.
    pub fn apply_key_command(&mut self, command: RyeOsKeyCommand) -> Vec<super::RyeOsEffect> {
        use super::{RyeOsEvent, RyeOsUiEvent};
        match command {
            RyeOsKeyCommand::Ui { event } => self.dispatch(RyeOsEvent::Ui { event }),
            RyeOsKeyCommand::MoveFocusedRowOrFocus {
                delta,
                fallback_direction,
            } => self.move_focused_row_or_focus(delta, fallback_direction),
            RyeOsKeyCommand::InsertOverlayChar { ch } => {
                let mut query = self.ui.overlay.query.clone();
                query.push(ch);
                self.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetOverlayQuery { query },
                })
            }
            RyeOsKeyCommand::DeleteOverlayChar => {
                let mut query = self.ui.overlay.query.clone();
                query.pop();
                self.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetOverlayQuery { query },
                })
            }
            RyeOsKeyCommand::Quit | RyeOsKeyCommand::Ignore => Vec::new(),
        }
    }

    /// Move the point within the focused list, falling back to a directional
    /// focus change when the focused lens has no selectable rows.
    fn move_focused_row_or_focus(
        &mut self,
        delta: i32,
        fallback_direction: FocusDirection,
    ) -> Vec<super::RyeOsEffect> {
        use super::{RyeOsEvent, RyeOsUiEvent};
        let (handled, effects) = self.move_focused_row(delta);
        if handled {
            effects
        } else {
            self.dispatch(RyeOsEvent::Ui {
                event: RyeOsUiEvent::FocusDirection {
                    direction: fallback_direction,
                },
            })
        }
    }

    fn move_focused_row(&mut self, delta: i32) -> (bool, Vec<super::RyeOsEffect>) {
        use super::{RyeOsEvent, RyeOsUiEvent};
        let vm = self.envelope(Vec::new()).view_model;
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
        let current = self.stored_cursor().min(count.saturating_sub(1));
        // The feed cursor is distance-from-bottom (0 = newest), so arrow-up
        // walks back into history — the opposite sense from a top-down row
        // list.
        let step = if is_feed { -delta } else { delta };
        let next = if step < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(count.saturating_sub(1))
        };
        if next == current {
            return (false, Vec::new());
        }
        let effects = self.dispatch(RyeOsEvent::Ui {
            event: RyeOsUiEvent::SetTileCursor {
                tile_id: focused,
                index: next,
            },
        });
        (true, effects)
    }

    /// The focused tile's stored list cursor (row index, or feed distance-
    /// from-bottom). Both renderers store it the same way; the meaning is
    /// per-widget.
    fn stored_cursor(&self) -> usize {
        match self
            .workspace
            .tiles
            .get(&self.workspace.focused_tile)
            .map(|tile| &tile.local)
        {
            Some(crate::workspace::ViewLocalState::GenericList { cursor, .. }) => *cursor,
            _ => 0,
        }
    }
}

/// The focused tile's selectable count and whether it is a feed (timeline).
/// Feeds count their coalesced entries and address them as distance-from-
/// bottom; row lists count rows addressed top-down.
fn focused_selectable(
    node: &super::view_model::RyeOsLayoutNodeVm,
    focused: &str,
) -> Option<(usize, bool)> {
    use super::view_model::RyeOsLayoutNodeVm;
    match node {
        RyeOsLayoutNodeVm::Tile { tile_id, view, .. } if tile_id == focused => {
            Some(selectable_of(view))
        }
        RyeOsLayoutNodeVm::Tile { .. } => None,
        RyeOsLayoutNodeVm::Split { first, second, .. } => {
            focused_selectable(first, focused).or_else(|| focused_selectable(second, focused))
        }
    }
}

fn selectable_of(view: &super::view_model::RyeOsViewVm) -> (usize, bool) {
    use super::view_model::RyeOsViewVm;
    match view {
        RyeOsViewVm::Rows { rows, .. } => (rows.len(), false),
        RyeOsViewVm::Table { rows, .. } => (rows.len(), false),
        RyeOsViewVm::Timeline { entries, .. } => (entries.len(), true),
        // The point walks a flat top-down list: an expanded section's rows,
        // or a collapsed section's single header (so it stays re-expandable)
        // — matching the flat cursor the resolver projects selection from.
        RyeOsViewVm::Sections { sections, .. } => {
            let points = sections
                .iter()
                .map(|section| {
                    if section.collapsed {
                        1
                    } else {
                        section.rows.len()
                    }
                })
                .sum();
            (points, false)
        }
        _ => (0, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: RyeOsKey) -> RyeOsKeyEvent {
        RyeOsKeyEvent {
            key,
            modifiers: RyeOsKeyModifiers::default(),
        }
    }

    fn ctrl(ch: char) -> RyeOsKeyEvent {
        RyeOsKeyEvent {
            key: RyeOsKey::Char(ch),
            modifiers: RyeOsKeyModifiers {
                ctrl: true,
                ..Default::default()
            },
        }
    }

    fn ctrl_shift(ch: char) -> RyeOsKeyEvent {
        RyeOsKeyEvent {
            key: RyeOsKey::Char(ch),
            modifiers: RyeOsKeyModifiers {
                ctrl: true,
                shift: true,
                ..Default::default()
            },
        }
    }

    fn context(overlay_open: bool, input_visible: bool) -> RyeOsKeyContext {
        RyeOsKeyContext {
            overlay_open,
            input_visible,
            ..Default::default()
        }
    }

    #[test]
    fn ctrl_k_opens_the_views_overlay() {
        assert!(matches!(
            ryeos_key_command(ctrl('k'), context(false, true)),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::OpenOverlay { overlay_id }
            } if overlay_id == "views"
        ));
    }

    #[test]
    fn ctrl_p_opens_the_commands_overlay() {
        assert!(matches!(
            ryeos_key_command(ctrl('p'), context(false, true)),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::OpenOverlay { overlay_id }
            } if overlay_id == "commands"
        ));
    }

    #[test]
    fn ctrl_shift_p_also_opens_the_commands_overlay_when_reported() {
        assert!(matches!(
            ryeos_key_command(ctrl_shift('p'), context(false, true)),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::OpenOverlay { overlay_id }
            } if overlay_id == "commands"
        ));
    }

    #[test]
    fn ctrl_s_toggles_backdrop_break() {
        assert!(matches!(
            ryeos_key_command(ctrl('s'), context(false, true)),
            RyeOsKeyCommand::Action {
                action: RyeOsAction::ToggleBackdropBreak
            }
        ));
    }

    #[test]
    fn question_mark_opens_shortcuts_only_when_input_is_empty() {
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Char('?')), context(false, true)),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::OpenOverlay { overlay_id }
            } if overlay_id == "shortcuts"
        ));

        let mut composing = context(false, true);
        composing.input_has_text = true;
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Char('?')), composing),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::InsertInputChar { ch: '?' }
            }
        ));
    }

    #[test]
    fn overlay_keys_edit_query_and_choose_selection() {
        let overlay = context(true, true);
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Char('a')), overlay),
            RyeOsKeyCommand::InsertOverlayChar { ch: 'a' }
        ));
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Backspace), overlay),
            RyeOsKeyCommand::DeleteOverlayChar
        ));
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::ArrowDown), overlay),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::MoveOverlaySelection { delta: 1 }
            }
        ));
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Enter), overlay),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::ChooseOverlay { secondary: false }
            }
        ));
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Escape), overlay),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::CloseOverlay
            }
        ));
    }
}
