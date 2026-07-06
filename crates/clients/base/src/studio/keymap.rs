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
        StudioKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'s') => {
            action(StudioAction::ToggleBackdropShards)
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
        StudioKey::ArrowLeft if event.modifiers.alt_only() => ui(StudioUiEvent::PopLens),
        // Esc interrupts a running head thread (chat convention); otherwise it
        // closes the focused tile.
        StudioKey::Escape if event.modifiers.none() && context.head_thread_running => {
            ui(StudioUiEvent::InterruptHead)
        }
        StudioKey::Escape if event.modifiers.none() => action(StudioAction::CloseFocused),
        // Alt+Enter submits as a forceful INTERRUPT (cut the running thread's
        // in-flight cognition and redirect); plain Enter steers. Checked first so
        // the modifier wins over the plain-submit arm below.
        StudioKey::Enter
            if context.input_visible
                && context.input_has_text
                && !context.input_is_live_filter
                && event.modifiers.alt_only() =>
        {
            ui(StudioUiEvent::SubmitInputInterrupt)
        }
        // Plain Enter submits when you're typing in the input (chat
        // convention); Shift+Enter also submits where the terminal can
        // send it. An empty input lets plain Enter activate a row — as does a
        // live filter (it applies live; Enter opens the narrowed selection).
        StudioKey::Enter
            if context.input_visible
                && context.input_has_text
                && !context.input_is_live_filter
                && (event.modifiers.none() || event.modifiers.shift_only()) =>
        {
            ui(StudioUiEvent::SubmitInput)
        }
        StudioKey::Enter
            if context.input_visible
                && !context.input_is_live_filter
                && event.modifiers.shift_only() =>
        {
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
        StudioKey::ArrowRight
            if event.modifiers.none()
                && context.focused_row_expandable
                && !context.focused_row_expanded =>
        {
            ui(StudioUiEvent::ExpandSelectedRow { expand: true })
        }
        StudioKey::ArrowLeft
            if event.modifiers.none()
                && context.focused_row_expandable
                && context.focused_row_expanded =>
        {
            ui(StudioUiEvent::ExpandSelectedRow { expand: false })
        }
        StudioKey::ArrowLeft if event.modifiers.none() => focus(FocusDirection::Left),
        StudioKey::ArrowRight if event.modifiers.none() => focus(FocusDirection::Right),
        StudioKey::Backspace
            if context.input_visible && context.input_has_text && event.modifiers.none() =>
        {
            ui(StudioUiEvent::DeleteInputChar)
        }
        // Backspace with no text to edit steps back up the execution-drill
        // stack — the "return" from a step-in. A no-op at the top of the tree.
        StudioKey::Backspace if event.modifiers.none() => ui(StudioUiEvent::PopLens),
        // A live filter with multiple target fields owns Tab to cycle the field
        // (it has no completion or route-target, so Tab is otherwise free here).
        StudioKey::Tab if context.input_filter_fields && event.modifiers.none() => {
            ui(StudioUiEvent::CycleFilterField { forward: true })
        }
        StudioKey::Tab if context.input_filter_fields && event.modifiers.shift_only() => {
            ui(StudioUiEvent::CycleFilterField { forward: false })
        }
        // Tab precedence (central interaction policy, capability + buffer
        // state — never a per-view keybinding):
        //   1. completion that can accept now (`/foo⇥`) wins;
        //   2. else route-chain targeting cycles (Tab fwd, Shift+Tab back);
        //   3. else a declared-but-not-acceptable completion is the fallback.
        StudioKey::Tab if context.input_can_accept_completion && event.modifiers.none() => {
            ui(StudioUiEvent::CompleteInput)
        }
        StudioKey::Tab if context.input_target_cycle.is_some() && event.modifiers.none() => {
            ui(StudioUiEvent::CycleInputTarget { forward: true })
        }
        StudioKey::Tab if context.input_target_cycle.is_some() && event.modifiers.shift_only() => {
            ui(StudioUiEvent::CycleInputTarget { forward: false })
        }
        StudioKey::Tab
            if context.input_visible && context.input_has_completion && event.modifiers.none() =>
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

impl super::model::StudioCore {
    /// Apply a resolved shared-keymap command to the core. This is the ONE
    /// interpretation of `StudioKeyCommand` — the row-cursor walk, the
    /// focus fallback, and the launcher query edits live here so renderers
    /// cannot drift on what a command does. Platform adapters translate
    /// their native key events into `StudioKeyEvent`, call
    /// [`studio_key_command`], and hand the command straight in; `Quit` is
    /// interpreted by the platform (terminal exits, browser leaves the key
    /// native) and is a no-op here, like `Ignore`.
    pub fn apply_key_command(&mut self, command: StudioKeyCommand) -> Vec<super::StudioEffect> {
        use super::{StudioEvent, StudioUiEvent};
        match command {
            StudioKeyCommand::Ui { event } => self.dispatch(StudioEvent::Ui { event }),
            StudioKeyCommand::MoveFocusedRowOrFocus {
                delta,
                fallback_direction,
            } => self.move_focused_row_or_focus(delta, fallback_direction),
            StudioKeyCommand::InsertLauncherChar { ch } => {
                let mut query = self.ui.launcher.query.clone();
                query.push(ch);
                self.dispatch(StudioEvent::Ui {
                    event: StudioUiEvent::SetLauncherQuery { query },
                })
            }
            StudioKeyCommand::DeleteLauncherChar => {
                let mut query = self.ui.launcher.query.clone();
                query.pop();
                self.dispatch(StudioEvent::Ui {
                    event: StudioUiEvent::SetLauncherQuery { query },
                })
            }
            StudioKeyCommand::Quit | StudioKeyCommand::Ignore => Vec::new(),
        }
    }

    /// Move the point within the focused list, falling back to a directional
    /// focus change when the focused lens has no selectable rows.
    fn move_focused_row_or_focus(
        &mut self,
        delta: i32,
        fallback_direction: FocusDirection,
    ) -> Vec<super::StudioEffect> {
        use super::{StudioEvent, StudioUiEvent};
        let (handled, effects) = self.move_focused_row(delta);
        if handled {
            effects
        } else {
            self.dispatch(StudioEvent::Ui {
                event: StudioUiEvent::FocusDirection {
                    direction: fallback_direction,
                },
            })
        }
    }

    fn move_focused_row(&mut self, delta: i32) -> (bool, Vec<super::StudioEffect>) {
        use super::{StudioEvent, StudioUiEvent};
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
        let effects = self.dispatch(StudioEvent::Ui {
            event: StudioUiEvent::SetTileCursor {
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
    node: &super::view_model::StudioLayoutNodeVm,
    focused: &str,
) -> Option<(usize, bool)> {
    use super::view_model::StudioLayoutNodeVm;
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

fn selectable_of(view: &super::view_model::StudioViewVm) -> (usize, bool) {
    use super::view_model::StudioViewVm;
    match view {
        StudioViewVm::Rows { rows, .. } => (rows.len(), false),
        StudioViewVm::Table { rows, .. } => (rows.len(), false),
        StudioViewVm::Timeline { entries, .. } => (entries.len(), true),
        // The point walks a flat top-down list: an expanded section's rows,
        // or a collapsed section's single header (so it stays re-expandable)
        // — matching the flat cursor the resolver projects selection from.
        StudioViewVm::Sections { sections, .. } => {
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

    fn alt_arrow(key: StudioKey) -> StudioKeyEvent {
        StudioKeyEvent {
            key,
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
            input_is_live_filter: false,
            input_filter_fields: false,
            input_has_completion: false,
            input_can_accept_completion: false,
            input_target_cycle: None,
            head_thread_running: false,
            focused_row_expandable: false,
            focused_row_expanded: false,
        }
    }

    fn context_typing() -> StudioKeyContext {
        StudioKeyContext {
            launcher_open: false,
            help_open: false,
            input_visible: true,
            input_has_text: true,
            input_is_live_filter: false,
            input_filter_fields: false,
            input_has_completion: false,
            input_can_accept_completion: false,
            input_target_cycle: None,
            head_thread_running: false,
            focused_row_expandable: false,
            focused_row_expanded: false,
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
            studio_key_command(key(StudioKey::Backspace), context_typing()),
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
    fn alt_enter_submits_as_interrupt_plain_enter_steers() {
        let alt_enter = StudioKeyEvent {
            key: StudioKey::Enter,
            modifiers: StudioKeyModifiers {
                alt: true,
                ..Default::default()
            },
        };
        assert!(matches!(
            studio_key_command(alt_enter, context_typing()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::SubmitInputInterrupt
            }
        ));
        // Plain Enter with text steers (default intent).
        assert!(matches!(
            studio_key_command(key(StudioKey::Enter), context_typing()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::SubmitInput
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
    fn live_filter_enter_activates_the_row_while_typing_still_filters() {
        // A live-filter input (feeds, no submit) applies live: even with text,
        // Enter activates the focused row rather than submitting the buffer.
        let filtering = StudioKeyContext {
            input_is_live_filter: true,
            ..context_typing()
        };
        assert!(matches!(
            studio_key_command(key(StudioKey::Enter), filtering),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::ActivateFocused
            }
        ));
        // Alt+Enter is not an interrupt for a live filter either.
        let alt_enter = StudioKeyEvent {
            key: StudioKey::Enter,
            modifiers: StudioKeyModifiers {
                alt: true,
                ..Default::default()
            },
        };
        assert!(!matches!(
            studio_key_command(
                alt_enter,
                StudioKeyContext {
                    input_is_live_filter: true,
                    ..context_typing()
                }
            ),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::SubmitInputInterrupt
            }
        ));
        // Typing still edits the filter buffer.
        assert!(matches!(
            studio_key_command(
                key(StudioKey::Char('r')),
                StudioKeyContext {
                    input_is_live_filter: true,
                    ..context_typing()
                }
            ),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::InsertInputChar { ch: 'r' }
            }
        ));
    }

    #[test]
    fn tab_cycles_a_multi_field_live_filter() {
        let ctx = StudioKeyContext {
            input_is_live_filter: true,
            input_filter_fields: true,
            ..context_typing()
        };
        assert!(matches!(
            studio_key_command(key(StudioKey::Tab), ctx),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CycleFilterField { forward: true }
            }
        ));
        assert!(matches!(
            studio_key_command(
                shift_tab(),
                StudioKeyContext {
                    input_is_live_filter: true,
                    input_filter_fields: true,
                    ..context_typing()
                }
            ),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::CycleFilterField { forward: false }
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
            StudioKeyCommand::Ui {
                event: StudioUiEvent::PopLens
            },
        );
        assert_eq!(
            studio_key_command(shift_enter(), context(false, false)),
            StudioKeyCommand::Ignore,
        );
    }

    #[test]
    fn backspace_pops_empty_input_but_edits_non_empty_input() {
        assert_eq!(
            studio_key_command(key(StudioKey::Backspace), context(false, true)),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::PopLens
            },
        );
        assert_eq!(
            studio_key_command(key(StudioKey::Backspace), context_typing()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::DeleteInputChar
            },
        );
    }

    #[test]
    fn alt_left_pops_lens_even_while_typing() {
        assert_eq!(
            studio_key_command(alt_arrow(StudioKey::ArrowLeft), context_typing()),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::PopLens
            },
        );
    }

    #[test]
    fn arrows_expand_and_collapse_selected_expandable_row_before_focus() {
        let collapsed = StudioKeyContext {
            focused_row_expandable: true,
            focused_row_expanded: false,
            ..context(false, true)
        };
        assert_eq!(
            studio_key_command(key(StudioKey::ArrowRight), collapsed),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::ExpandSelectedRow { expand: true }
            }
        );

        let expanded = StudioKeyContext {
            focused_row_expandable: true,
            focused_row_expanded: true,
            ..context(false, true)
        };
        assert_eq!(
            studio_key_command(key(StudioKey::ArrowLeft), expanded),
            StudioKeyCommand::Ui {
                event: StudioUiEvent::ExpandSelectedRow { expand: false }
            }
        );
    }
}
