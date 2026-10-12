//! Shared RyeOs key map.
//!
//! This mirrors the browser RyeOs key handling so renderers do not grow
//! divergent product behavior. Platform adapters convert local key events into
//! `RyeOsKeyEvent`; this module returns semantic RyeOs commands/events.

use serde::{Deserialize, Serialize};

use super::event::{FieldStepDirection, RyeOsStackMoveDirection, RyeOsUiEvent, RyeOsUiIntent};
use super::model::RyeOsDockEdge;
use crate::view_set::FocusDirection;

/// Descriptive help for this module's semantic matcher, not another keyboard registry.
pub fn shortcut_entries() -> Vec<super::view_model::RyeOsShortcutEntryVm> {
    let entry =
        |category: &str, keys: &str, description: &str| super::view_model::RyeOsShortcutEntryVm {
            category: category.to_string(),
            keys: keys.to_string(),
            description: description.to_string(),
        };
    vec![
        entry(
            "Move",
            "↑ / ↓",
            "Move the point through rows; else move focus",
        ),
        entry(
            "Move",
            "← / →",
            "Navigate field hierarchy, expand row/feed details, or move focus",
        ),
        entry(
            "Act",
            "Enter",
            "Activate the selected row; submit a non-filter input with text",
        ),
        entry(
            "Act",
            "Alt+Enter",
            "Interrupt-submit a focused non-filter input with text",
        ),
        entry(
            "Act",
            "Tab / ⇧Tab",
            "Cycle declared filter fields, accept completion, or cycle a declared route target",
        ),
        entry(
            "Act",
            "Esc",
            "Close the focused dialog; otherwise end field query, blur input, cancel a running thread, or close the view",
        ),
        entry("Views", "Alt+←", "Return from a drill-in view"),
        entry(
            "Input",
            "type",
            "Edit only the focused View's declared input",
        ),
        entry(
            "Lenses",
            "Ctrl+K",
            "Open the configured presentation launcher",
        ),
        entry("Backdrop", "Ctrl+S", "Toggle backdrop break"),
        entry("Lenses", "Ctrl+← / →", "Switch view-set tab"),
        entry(
            "Move",
            "Ctrl+U / Ctrl+D",
            "Jump the focused rows by half a screen",
        ),
        entry("Layout", "Ctrl+↑ / ↓", "Move the focused tile in the stack"),
        entry("Layout", "Ctrl+⇧+arrows", "Resize the focused tile"),
        entry(
            "Layout",
            "Alt+M",
            "Promote the focused tile in a master-and-stack layout",
        ),
        entry(
            "Layout",
            "Alt+T / Alt+B",
            "Toggle the top / bottom presentation bar",
        ),
        entry(
            "App",
            "Alt+Q",
            "Close the focused view; its running work is unaffected",
        ),
        entry("App", "Ctrl+C", "Quit"),
    ]
}

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
    pub dialog_open: bool,
    pub input_visible: bool,
    /// The focused view instance owns an input buffer.
    pub input_focused: bool,
    /// The focused input was reached as a dock/input focus and can return to
    /// the prior view with Esc.
    pub input_blurrable: bool,
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
    /// The selected table row owns one or more hierarchy children.
    pub focused_tree_collapsible: bool,
    /// The selected hierarchy row currently hides those descendants.
    pub focused_tree_collapsed: bool,
    /// Capabilities of the focused semantic field, if the focused instance is
    /// one. The keymap consumes capabilities rather than widget names or
    /// project roles, so every renderer gets the same actions.
    pub field: Option<RyeOsFieldKeyContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RyeOsFieldKeyContext {
    pub query_editing: bool,
    pub query_has_text: bool,
    pub group_collapsible: bool,
    pub group_collapsed: bool,
    pub has_parent: bool,
    pub has_child: bool,
    pub replay_previous: bool,
    pub replay_next: bool,
    pub replay_live: bool,
    pub replay_playing: bool,
    pub search_matches: bool,
    pub compare: bool,
    pub expand: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsKeyCommand {
    CloseFocusedDialog,
    Ui {
        event: RyeOsUiEvent,
    },
    MoveFocusedRowOrFocus {
        delta: i32,
        fallback_direction: FocusDirection,
    },
    MoveFocusedRowPage {
        direction: i32,
    },
    SetFocusedFieldGroupCollapsed {
        collapsed: bool,
    },
    MoveFocusedFieldHierarchy {
        parent: bool,
    },
    StepFocusedFieldCursor {
        direction: FieldStepDirection,
    },
    ToggleFocusedFieldPlayback,
    BeginFocusedFieldQuery,
    EndFocusedFieldQuery,
    InsertFocusedFieldQueryChar {
        ch: char,
    },
    DeleteFocusedFieldQueryChar,
    MoveFocusedFieldSearchMatch {
        delta: i32,
    },
    ToggleFocusedFieldCompare,
    ExpandFocusedField,
    Quit,
    Ignore,
}

pub fn ryeos_key_command(event: RyeOsKeyEvent, context: RyeOsKeyContext) -> RyeOsKeyCommand {
    if context.dialog_open && event.key == RyeOsKey::Escape && event.modifiers.none() {
        return RyeOsKeyCommand::CloseFocusedDialog;
    }
    // Shell navigation remains available while a focused field edits its
    // query; that field owns ordinary characters, not the launcher chord.
    if let RyeOsKey::Char(ch) = event.key {
        if ch.eq_ignore_ascii_case(&'k')
            && (event.modifiers.ctrl_only() || event.modifiers.alt_only())
        {
            return ui(RyeOsUiEvent::OpenLauncher);
        }
    }
    if let Some(field) = context.field.filter(|field| field.query_editing) {
        return match event.key {
            RyeOsKey::Escape if event.modifiers.none() => RyeOsKeyCommand::EndFocusedFieldQuery,
            RyeOsKey::Enter if event.modifiers.none_or_shift() => {
                RyeOsKeyCommand::EndFocusedFieldQuery
            }
            RyeOsKey::ArrowUp if event.modifiers.none() => {
                RyeOsKeyCommand::MoveFocusedFieldSearchMatch { delta: -1 }
            }
            RyeOsKey::ArrowDown if event.modifiers.none() => {
                RyeOsKeyCommand::MoveFocusedFieldSearchMatch { delta: 1 }
            }
            RyeOsKey::Backspace if event.modifiers.none() && field.query_has_text => {
                RyeOsKeyCommand::DeleteFocusedFieldQueryChar
            }
            RyeOsKey::Backspace if event.modifiers.none() => RyeOsKeyCommand::EndFocusedFieldQuery,
            RyeOsKey::Char(ch)
                if !event.modifiers.ctrl && !event.modifiers.alt && !event.modifiers.meta =>
            {
                RyeOsKeyCommand::InsertFocusedFieldQueryChar { ch }
            }
            RyeOsKey::Char('c') if event.modifiers.ctrl_only() => RyeOsKeyCommand::Quit,
            _ => RyeOsKeyCommand::Ignore,
        };
    }

    match event.key {
        RyeOsKey::Char('n') if event.modifiers.alt_only() => intent(RyeOsUiIntent::NewViewSet),
        // Ctrl+K is the reliable view-overlay binding: a control char that
        // terminals and tmux pass straight through. Alt+K is kept for
        // environments that deliver it, but Alt/ESC combos are eaten by
        // tmux, so Ctrl+K is what we advertise.
        RyeOsKey::Char(c)
            if (event.modifiers.ctrl_only() || event.modifiers.alt_only())
                && c.eq_ignore_ascii_case(&'k') =>
        {
            ui(RyeOsUiEvent::OpenLauncher)
        }
        RyeOsKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'q') => {
            intent(RyeOsUiIntent::CloseFocused)
        }
        RyeOsKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'m') => {
            intent(RyeOsUiIntent::ToggleFocusedMaster)
        }
        RyeOsKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'t') => {
            intent(RyeOsUiIntent::ToggleTopStatusBar)
        }
        RyeOsKey::Char(c) if event.modifiers.alt_only() && c.eq_ignore_ascii_case(&'b') => {
            intent(RyeOsUiIntent::ToggleBottomStatusBar)
        }
        RyeOsKey::Char(c)
            if event.modifiers.none()
                && c.eq_ignore_ascii_case(&'i')
                && context.input_visible
                && !context.input_focused =>
        {
            ui(RyeOsUiEvent::FocusInput)
        }
        RyeOsKey::Char(c)
            if event.modifiers.none() && c.eq_ignore_ascii_case(&'s') && !context.input_focused =>
        {
            ui(RyeOsUiEvent::FocusDock {
                edge: RyeOsDockEdge::Top,
            })
        }
        RyeOsKey::Char(c)
            if (event.modifiers.ctrl_only() || event.modifiers.alt_only())
                && c.eq_ignore_ascii_case(&'s') =>
        {
            intent(RyeOsUiIntent::ToggleBackdropBreak)
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
        RyeOsKey::Char('[') if event.modifiers.alt_only() => intent(RyeOsUiIntent::CycleViewTab {
            direction: RyeOsStackMoveDirection::Up,
        }),
        RyeOsKey::Char(']') if event.modifiers.alt_only() => intent(RyeOsUiIntent::CycleViewTab {
            direction: RyeOsStackMoveDirection::Down,
        }),
        RyeOsKey::ArrowLeft if event.modifiers.alt_only() => ui(RyeOsUiEvent::PopLens),
        RyeOsKey::Escape if event.modifiers.none() && context.input_blurrable => {
            ui(RyeOsUiEvent::BlurInput)
        }
        // Esc interrupts a running head thread (chat convention); otherwise it
        // closes the focused tile.
        RyeOsKey::Escape if event.modifiers.none() && context.head_thread_running => {
            ui(RyeOsUiEvent::InterruptHead)
        }
        RyeOsKey::Escape if event.modifiers.none() => intent(RyeOsUiIntent::CloseFocused),
        // Alt+Enter submits as a forceful INTERRUPT (cut the running thread's
        // in-flight cognition and redirect); plain Enter steers. Checked first so
        // the modifier wins over the plain-submit arm below.
        RyeOsKey::Enter
            if context.input_focused
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
            if context.input_focused
                && context.input_has_text
                && !context.input_is_live_filter
                && (event.modifiers.none() || event.modifiers.shift_only()) =>
        {
            ui(RyeOsUiEvent::SubmitInput)
        }
        RyeOsKey::Enter
            if context.input_focused
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
                && context
                    .field
                    .is_some_and(|field| field.group_collapsible && field.group_collapsed) =>
        {
            RyeOsKeyCommand::SetFocusedFieldGroupCollapsed { collapsed: false }
        }
        RyeOsKey::ArrowLeft
            if event.modifiers.none()
                && context
                    .field
                    .is_some_and(|field| field.group_collapsible && !field.group_collapsed) =>
        {
            RyeOsKeyCommand::SetFocusedFieldGroupCollapsed { collapsed: true }
        }
        RyeOsKey::ArrowLeft
            if event.modifiers.none() && context.field.is_some_and(|field| field.has_parent) =>
        {
            RyeOsKeyCommand::MoveFocusedFieldHierarchy { parent: true }
        }
        RyeOsKey::ArrowRight
            if event.modifiers.none() && context.field.is_some_and(|field| field.has_child) =>
        {
            RyeOsKeyCommand::MoveFocusedFieldHierarchy { parent: false }
        }
        RyeOsKey::Char('[')
            if event.modifiers.none()
                && context.field.is_some_and(|field| field.replay_previous) =>
        {
            RyeOsKeyCommand::StepFocusedFieldCursor {
                direction: FieldStepDirection::Previous,
            }
        }
        RyeOsKey::Char(']')
            if event.modifiers.none() && context.field.is_some_and(|field| field.replay_next) =>
        {
            RyeOsKeyCommand::StepFocusedFieldCursor {
                direction: FieldStepDirection::Next,
            }
        }
        RyeOsKey::Char(c)
            if event.modifiers.none()
                && c.eq_ignore_ascii_case(&'l')
                && context.field.is_some_and(|field| field.replay_live) =>
        {
            RyeOsKeyCommand::StepFocusedFieldCursor {
                direction: FieldStepDirection::Live,
            }
        }
        RyeOsKey::Char(c)
            if event.modifiers.none()
                && c.eq_ignore_ascii_case(&'p')
                && context
                    .field
                    .is_some_and(|field| field.replay_playing || field.replay_next) =>
        {
            RyeOsKeyCommand::ToggleFocusedFieldPlayback
        }
        RyeOsKey::Char('/') if event.modifiers.none() && context.field.is_some() => {
            RyeOsKeyCommand::BeginFocusedFieldQuery
        }
        RyeOsKey::Char(c)
            if event.modifiers.none_or_shift()
                && c.eq_ignore_ascii_case(&'n')
                && context.field.is_some_and(|field| field.search_matches) =>
        {
            RyeOsKeyCommand::MoveFocusedFieldSearchMatch {
                delta: if event.modifiers.shift { -1 } else { 1 },
            }
        }
        RyeOsKey::Char(' ')
            if event.modifiers.none() && context.field.is_some_and(|field| field.compare) =>
        {
            RyeOsKeyCommand::ToggleFocusedFieldCompare
        }
        RyeOsKey::Char(c)
            if event.modifiers.none()
                && c.eq_ignore_ascii_case(&'e')
                && context.field.is_some_and(|field| field.expand) =>
        {
            RyeOsKeyCommand::ExpandFocusedField
        }
        RyeOsKey::Char(c) if event.modifiers.ctrl_only() && c.eq_ignore_ascii_case(&'u') => {
            RyeOsKeyCommand::MoveFocusedRowPage { direction: -1 }
        }
        RyeOsKey::Char(c) if event.modifiers.ctrl_only() && c.eq_ignore_ascii_case(&'d') => {
            RyeOsKeyCommand::MoveFocusedRowPage { direction: 1 }
        }
        RyeOsKey::ArrowRight
            if event.modifiers.none()
                && context.focused_tree_collapsible
                && context.focused_tree_collapsed =>
        {
            ui(RyeOsUiEvent::SetTreeRowCollapsed { collapsed: false })
        }
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
        RyeOsKey::ArrowLeft
            if event.modifiers.none()
                && context.focused_tree_collapsible
                && !context.focused_tree_collapsed =>
        {
            ui(RyeOsUiEvent::SetTreeRowCollapsed { collapsed: true })
        }
        RyeOsKey::ArrowLeft if event.modifiers.none() => focus(FocusDirection::Left),
        RyeOsKey::ArrowRight if event.modifiers.none() => focus(FocusDirection::Right),
        RyeOsKey::Backspace
            if context.input_focused && context.input_has_text && event.modifiers.none() =>
        {
            ui(RyeOsUiEvent::DeleteInputChar)
        }
        // Backspace with no text to edit steps back up the execution-drill
        // stack — the "return" from a step-in. A no-op at the top of the tree.
        RyeOsKey::Backspace if event.modifiers.none() => ui(RyeOsUiEvent::PopLens),
        // A live filter with multiple target fields owns Tab to cycle the field
        // (it has no completion or route-target, so Tab is otherwise free here).
        RyeOsKey::Tab
            if context.input_focused && context.input_filter_fields && event.modifiers.none() =>
        {
            ui(RyeOsUiEvent::CycleFilterField { forward: true })
        }
        RyeOsKey::Tab
            if context.input_focused
                && context.input_filter_fields
                && event.modifiers.shift_only() =>
        {
            ui(RyeOsUiEvent::CycleFilterField { forward: false })
        }
        // Tab precedence (central interaction policy, capability + buffer
        // state — never a per-view keybinding):
        //   1. completion that can accept now (`/foo⇥`) wins;
        //   2. else route-chain targeting cycles (Tab fwd, Shift+Tab back);
        //   3. else a declared-but-not-acceptable completion is the fallback.
        RyeOsKey::Tab
            if context.input_focused
                && context.input_can_accept_completion
                && event.modifiers.none() =>
        {
            ui(RyeOsUiEvent::CompleteInput)
        }
        RyeOsKey::Tab
            if context.input_focused
                && context.input_target_cycle.is_some()
                && event.modifiers.none() =>
        {
            ui(RyeOsUiEvent::CycleInputTarget { forward: true })
        }
        RyeOsKey::Tab
            if context.input_focused
                && context.input_target_cycle.is_some()
                && event.modifiers.shift_only() =>
        {
            ui(RyeOsUiEvent::CycleInputTarget { forward: false })
        }
        RyeOsKey::Tab
            if context.input_focused && context.input_has_completion && event.modifiers.none() =>
        {
            ui(RyeOsUiEvent::CompleteInput)
        }
        RyeOsKey::Char('c') if event.modifiers.ctrl_only() => RyeOsKeyCommand::Quit,
        RyeOsKey::Char(ch)
            if context.input_focused
                && !event.modifiers.ctrl
                && !event.modifiers.alt
                && !event.modifiers.meta =>
        {
            ui(RyeOsUiEvent::InsertInputChar { ch })
        }
        _ => RyeOsKeyCommand::Ignore,
    }
}

fn ui(event: RyeOsUiEvent) -> RyeOsKeyCommand {
    RyeOsKeyCommand::Ui { event }
}

fn intent(intent: RyeOsUiIntent) -> RyeOsKeyCommand {
    ui(RyeOsUiEvent::Activate { intent })
}

fn focus(direction: FocusDirection) -> RyeOsKeyCommand {
    ui(RyeOsUiEvent::FocusDirection { direction })
}

fn resize(direction: FocusDirection) -> RyeOsKeyCommand {
    intent(RyeOsUiIntent::ResizeFocused { direction })
}

fn move_focused_tile(direction: RyeOsStackMoveDirection) -> RyeOsKeyCommand {
    intent(RyeOsUiIntent::MoveFocusedTile { direction })
}

fn cycle_tab(direction: RyeOsStackMoveDirection) -> RyeOsKeyCommand {
    intent(RyeOsUiIntent::CycleTab { direction })
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
            RyeOsKeyCommand::CloseFocusedDialog => {
                let Some(instance_key) = self.focused_view_instance_key() else {
                    return Vec::new();
                };
                self.close_dialog(&instance_key)
            }
            RyeOsKeyCommand::Ui { event } => self.dispatch(RyeOsEvent::Ui { event }),
            RyeOsKeyCommand::MoveFocusedRowOrFocus {
                delta,
                fallback_direction,
            } => self.move_focused_row_or_focus(delta, fallback_direction),
            RyeOsKeyCommand::MoveFocusedRowPage { direction } => {
                let page = (self.runtime.viewport.height as i32 / 2).max(6);
                self.move_focused_row(direction.signum() * page).1
            }
            RyeOsKeyCommand::SetFocusedFieldGroupCollapsed { collapsed } => {
                let Some((instance_key, field)) = focused_field(self) else {
                    return Vec::new();
                };
                let Some(group_id) =
                    selected_field_entity(&field).and_then(|entity| entity.group_id.clone())
                else {
                    return Vec::new();
                };
                self.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetFieldGroupCollapsed {
                        instance_key,
                        group_id,
                        collapsed,
                    },
                })
            }
            RyeOsKeyCommand::MoveFocusedFieldHierarchy { parent } => {
                let Some((instance_key, field)) = focused_field(self) else {
                    return Vec::new();
                };
                let Some(selected) = selected_field_entity(&field) else {
                    return Vec::new();
                };
                let entity_id = if parent {
                    selected.parent_id.clone()
                } else {
                    field
                        .entities
                        .iter()
                        .find(|entity| entity.parent_id.as_deref() == Some(selected.id.as_str()))
                        .map(|entity| entity.id.clone())
                };
                let Some(entity_id) = entity_id else {
                    return Vec::new();
                };
                self.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetFieldSelection {
                        instance_key,
                        entity_id: Some(entity_id),
                    },
                })
            }
            RyeOsKeyCommand::StepFocusedFieldCursor { direction } => {
                let Some(instance_key) = focused_field_instance(self) else {
                    return Vec::new();
                };
                self.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::StepFieldCursor {
                        instance_key,
                        direction,
                    },
                })
            }
            RyeOsKeyCommand::ToggleFocusedFieldPlayback => {
                let Some((instance_key, field)) = focused_field(self) else {
                    return Vec::new();
                };
                self.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::SetFieldPlayback {
                        instance_key,
                        playing: !field.replay.playing,
                    },
                })
            }
            RyeOsKeyCommand::BeginFocusedFieldQuery => {
                let Some(instance_key) = focused_field_instance(self) else {
                    return Vec::new();
                };
                if self.field_query_editing.as_ref() != Some(&instance_key) {
                    self.field_query_editing = Some(instance_key);
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsKeyCommand::EndFocusedFieldQuery => {
                if self.field_query_editing.take().is_some() {
                    self.bump_generation();
                }
                Vec::new()
            }
            RyeOsKeyCommand::InsertFocusedFieldQueryChar { ch } => {
                self.edit_focused_field_query(|query| query.push(ch))
            }
            RyeOsKeyCommand::DeleteFocusedFieldQueryChar => {
                self.edit_focused_field_query(|query| {
                    query.pop();
                })
            }
            RyeOsKeyCommand::MoveFocusedFieldSearchMatch { delta } => {
                let Some(instance_key) = focused_field_instance(self) else {
                    return Vec::new();
                };
                self.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::MoveFieldSearchMatch {
                        instance_key,
                        delta,
                    },
                })
            }
            RyeOsKeyCommand::ToggleFocusedFieldCompare => {
                let Some((instance_key, field)) = focused_field(self) else {
                    return Vec::new();
                };
                let Some(entity_id) = field.selected else {
                    return Vec::new();
                };
                self.dispatch(RyeOsEvent::Ui {
                    event: RyeOsUiEvent::ToggleFieldCompare {
                        instance_key,
                        entity_id,
                    },
                })
            }
            RyeOsKeyCommand::ExpandFocusedField => {
                let Some((instance_key, field)) = focused_field(self) else {
                    return Vec::new();
                };
                let Some(selected) = selected_field_entity(&field) else {
                    return Vec::new();
                };
                let source = selected.source.clone();
                let root_id = selected.id.clone();
                let expansion = field
                    .expansions
                    .iter()
                    .find(|item| item.source == source && item.root_id == root_id);
                let event = match expansion {
                    Some(expansion) if expansion.can_continue => {
                        RyeOsUiEvent::ContinueFieldExpansion {
                            instance_key,
                            source,
                            root_id,
                        }
                    }
                    Some(_) => return Vec::new(),
                    None => RyeOsUiEvent::RequestFieldExpansion {
                        instance_key,
                        source,
                        root_id,
                    },
                };
                self.dispatch(RyeOsEvent::Ui { event })
            }
            RyeOsKeyCommand::Quit | RyeOsKeyCommand::Ignore => Vec::new(),
        }
    }

    fn edit_focused_field_query(
        &mut self,
        edit: impl FnOnce(&mut String),
    ) -> Vec<super::RyeOsEffect> {
        let Some((instance_key, field)) = focused_field(self) else {
            return Vec::new();
        };
        if self.field_query_editing.as_ref() != Some(&instance_key) {
            return Vec::new();
        }
        let mut query = field.search.query;
        edit(&mut query);
        self.dispatch(super::RyeOsEvent::Ui {
            event: super::RyeOsUiEvent::SetFieldQuery {
                instance_key,
                query,
            },
        })
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

    pub(crate) fn move_focused_row(&mut self, delta: i32) -> (bool, Vec<super::RyeOsEffect>) {
        let Some(instance_key) = self.focused_view_instance_key() else {
            return (false, Vec::new());
        };
        let Some(view) = super::view_model::view_vm_for_instance(self, &instance_key) else {
            return (false, Vec::new());
        };
        let target = FocusedRowsTarget {
            instance_key,
            count_and_feed: selectable_of(&view),
        };
        let (count, is_feed) = target.count_and_feed;
        if count == 0 {
            return (false, Vec::new());
        }
        if matches!(
            view_local_for_instance(self, &target.instance_key),
            Some(crate::view_set::ViewLocalState::Field(_))
        ) {
            let effects = self.dispatch(super::RyeOsEvent::Ui {
                event: super::RyeOsUiEvent::MoveFieldSelection {
                    instance_key: target.instance_key,
                    delta,
                },
            });
            return (true, effects);
        }
        let current = self
            .stored_cursor_for(&target.instance_key)
            .min(count.saturating_sub(1));
        // The feed cursor is distance-from-bottom (0 = newest), so arrow-up
        // walks back into history — the opposite sense from a top-down row
        // list.
        let step = if is_feed { -delta } else { delta };
        let magnitude = step.unsigned_abs() as usize;
        let next = if step < 0 {
            current.saturating_sub(magnitude)
        } else {
            current
                .saturating_add(magnitude)
                .min(count.saturating_sub(1))
        };
        if next == current {
            return (false, Vec::new());
        }
        self.set_cursor_for(&target.instance_key, next);
        (true, Vec::new())
    }

    /// The focused view-instance's stored list cursor (row index, or feed
    /// distance-from-bottom). Both renderers store it the same way; the meaning
    /// is per-widget.
    fn stored_cursor_for(&self, instance_key: &crate::ids::RyeOsViewInstanceKey) -> usize {
        match view_local_for_instance(self, instance_key) {
            Some(crate::view_set::ViewLocalState::GenericList { cursor, .. }) => *cursor,
            _ => 0,
        }
    }

    fn set_cursor_for(&mut self, instance_key: &crate::ids::RyeOsViewInstanceKey, index: usize) {
        let Some(local) = view_local_for_instance_mut(self, instance_key) else {
            return;
        };
        let crate::view_set::ViewLocalState::GenericList { cursor, .. } = local else {
            return;
        };
        if *cursor != index {
            *cursor = index;
            self.bump_generation();
        }
    }
}

pub(crate) fn focused_field_key_context(
    core: &super::model::RyeOsCore,
) -> Option<RyeOsFieldKeyContext> {
    let (instance_key, field) = focused_field(core)?;
    let selected = selected_field_entity(&field);
    let group = selected
        .and_then(|entity| entity.group_id.as_deref())
        .and_then(|group_id| field.groups.iter().find(|group| group.id == group_id));
    let selected_id = selected.map(|entity| entity.id.as_str());
    let has_child = selected_id.is_some_and(|selected_id| {
        field
            .entities
            .iter()
            .any(|entity| entity.parent_id.as_deref() == Some(selected_id))
    });
    let compare = selected.is_some_and(|entity| {
        field.compare.contains(&entity.id)
            || entity.preview_ids.iter().any(|preview_id| {
                field
                    .previews
                    .iter()
                    .any(|preview| preview.id == *preview_id && preview.comparison_key.is_some())
            })
    });
    let expand = selected.is_some_and(|entity| {
        field
            .expansions
            .iter()
            .find(|item| item.source == entity.source && item.root_id == entity.id)
            .is_none_or(|item| item.can_continue)
    });
    Some(RyeOsFieldKeyContext {
        query_editing: core.field_query_editing.as_ref() == Some(&instance_key),
        query_has_text: !field.search.query.is_empty(),
        group_collapsible: group.is_some(),
        group_collapsed: group.is_some_and(|group| group.collapsed),
        has_parent: selected.is_some_and(|entity| entity.parent_id.is_some()),
        has_child,
        replay_previous: field.replay.previous.is_some(),
        replay_next: field.replay.next.is_some(),
        replay_live: field.replay.mode != "live" && field.replay.live_head.is_some(),
        replay_playing: field.replay.playing,
        search_matches: !field.search.match_ids.is_empty(),
        compare,
        expand,
    })
}

fn selected_field_entity(
    field: &super::field::RyeOsFieldVm,
) -> Option<&super::field::RyeOsFieldEntityVm> {
    field
        .selected
        .as_deref()
        .and_then(|id| field.entities.iter().find(|entity| entity.id == id))
}

fn focused_field(
    core: &super::model::RyeOsCore,
) -> Option<(crate::ids::RyeOsViewInstanceKey, super::field::RyeOsFieldVm)> {
    let instance_key = focused_field_instance(core)?;
    let (_, field) = super::view_model::field_vm_for_instance(core, &instance_key)?;
    Some((instance_key, field))
}

fn focused_field_instance(
    core: &super::model::RyeOsCore,
) -> Option<crate::ids::RyeOsViewInstanceKey> {
    let instance_key = core.focused_view_instance_key()?;
    matches!(
        view_local_for_instance(core, &instance_key),
        Some(crate::view_set::ViewLocalState::Field(_))
    )
    .then_some(instance_key)
}

struct FocusedRowsTarget {
    instance_key: crate::ids::RyeOsViewInstanceKey,
    count_and_feed: (usize, bool),
}

fn view_local_for_instance<'a>(
    core: &'a super::model::RyeOsCore,
    instance_key: &crate::ids::RyeOsViewInstanceKey,
) -> Option<&'a crate::view_set::ViewLocalState> {
    core.mounted_local_state(instance_key)
}
fn view_local_for_instance_mut<'a>(
    core: &'a mut super::model::RyeOsCore,
    instance_key: &crate::ids::RyeOsViewInstanceKey,
) -> Option<&'a mut crate::view_set::ViewLocalState> {
    let local = core.mounted_local_state_mut(instance_key)?;
    if matches!(local, crate::view_set::ViewLocalState::None) {
        *local = initial_list_local_state();
    }
    Some(local)
}

fn initial_list_local_state() -> crate::view_set::ViewLocalState {
    crate::view_set::ViewLocalState::GenericList {
        cursor: 0,
        scroll: 0,
        collapsed: std::collections::BTreeSet::new(),
        expanded_rows: std::collections::BTreeSet::new(),
        collapsed_tree_rows: std::collections::BTreeSet::new(),
        changed_rows: std::collections::BTreeMap::new(),
    }
}

fn selectable_of(view: &super::view_model::RyeOsViewVm) -> (usize, bool) {
    use super::view_model::RyeOsViewVm;
    match view {
        RyeOsViewVm::Rows {
            rows, total_rows, ..
        } => ((*total_rows).max(rows.len()), false),
        RyeOsViewVm::Table {
            rows, total_rows, ..
        } => ((*total_rows).max(rows.len()), false),
        RyeOsViewVm::Timeline { entries, .. } => (entries.len(), true),
        RyeOsViewVm::Field { field } => (field.traversal.len(), false),
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

    fn context(dialog_open: bool, input_visible: bool) -> RyeOsKeyContext {
        RyeOsKeyContext {
            dialog_open,
            input_visible,
            ..Default::default()
        }
    }

    #[test]
    fn ctrl_s_toggles_backdrop_break() {
        assert!(matches!(
            ryeos_key_command(ctrl('s'), context(false, true)),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::Activate {
                    intent: RyeOsUiIntent::ToggleBackdropBreak
                }
            }
        ));
    }

    #[test]
    fn ctrl_u_and_ctrl_d_jump_rows_by_page() {
        assert_eq!(
            ryeos_key_command(ctrl('u'), context(false, true)),
            RyeOsKeyCommand::MoveFocusedRowPage { direction: -1 }
        );
        assert_eq!(
            ryeos_key_command(ctrl('d'), context(false, true)),
            RyeOsKeyCommand::MoveFocusedRowPage { direction: 1 }
        );
    }

    #[test]
    fn horizontal_arrows_fold_the_focused_tree_row_before_moving_focus() {
        let mut collapsed = context(false, false);
        collapsed.focused_tree_collapsible = true;
        collapsed.focused_tree_collapsed = true;
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::ArrowRight), collapsed),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::SetTreeRowCollapsed { collapsed: false }
            }
        ));

        let mut expanded = context(false, false);
        expanded.focused_tree_collapsible = true;
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::ArrowLeft), expanded),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::SetTreeRowCollapsed { collapsed: true }
            }
        ));
    }

    #[test]
    fn field_keys_resolve_to_renderer_neutral_semantic_commands() {
        let mut ctx = context(false, false);
        ctx.field = Some(RyeOsFieldKeyContext {
            group_collapsible: true,
            replay_previous: true,
            replay_next: true,
            replay_live: true,
            search_matches: true,
            compare: true,
            expand: true,
            ..Default::default()
        });
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::ArrowLeft), ctx),
            RyeOsKeyCommand::SetFocusedFieldGroupCollapsed { collapsed: true }
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Char('[')), ctx),
            RyeOsKeyCommand::StepFocusedFieldCursor {
                direction: FieldStepDirection::Previous
            }
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Char(']')), ctx),
            RyeOsKeyCommand::StepFocusedFieldCursor {
                direction: FieldStepDirection::Next
            }
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Char('l')), ctx),
            RyeOsKeyCommand::StepFocusedFieldCursor {
                direction: FieldStepDirection::Live
            }
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Char('/')), ctx),
            RyeOsKeyCommand::BeginFocusedFieldQuery
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Char(' ')), ctx),
            RyeOsKeyCommand::ToggleFocusedFieldCompare
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Char('e')), ctx),
            RyeOsKeyCommand::ExpandFocusedField
        );
    }

    #[test]
    fn field_query_editor_owns_text_and_match_navigation_until_closed() {
        let mut ctx = context(false, false);
        ctx.field = Some(RyeOsFieldKeyContext {
            query_editing: true,
            query_has_text: true,
            search_matches: true,
            ..Default::default()
        });
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Char('x')), ctx),
            RyeOsKeyCommand::InsertFocusedFieldQueryChar { ch: 'x' }
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Backspace), ctx),
            RyeOsKeyCommand::DeleteFocusedFieldQueryChar
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::ArrowDown), ctx),
            RyeOsKeyCommand::MoveFocusedFieldSearchMatch { delta: 1 }
        );
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Escape), ctx),
            RyeOsKeyCommand::EndFocusedFieldQuery
        );
    }

    #[test]
    fn left_closes_row_details_before_folding_its_tree_branch() {
        let mut ctx = context(false, false);
        ctx.focused_tree_collapsible = true;
        ctx.focused_row_expandable = true;
        ctx.focused_row_expanded = true;
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::ArrowLeft), ctx),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::ExpandSelectedRow { expand: false }
            }
        ));
    }

    #[test]
    fn i_focuses_input_from_view_mode() {
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Char('i')), context(false, true)),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::FocusInput
            }
        ));
    }

    #[test]
    fn escape_blurs_focused_input() {
        let mut ctx = context(false, true);
        ctx.input_focused = true;
        ctx.input_blurrable = true;
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Escape), ctx),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::BlurInput
            }
        ));
    }

    #[test]
    fn question_mark_types_into_focused_input() {
        let mut ctx = context(false, true);
        ctx.input_focused = true;
        assert!(matches!(
            ryeos_key_command(key(RyeOsKey::Char('?')), ctx),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::InsertInputChar { ch: '?' }
            }
        ));
    }

    #[test]
    fn question_mark_does_not_type_when_input_is_not_focused() {
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Char('?')), context(false, true)),
            RyeOsKeyCommand::Ignore
        );
    }

    #[test]
    fn configured_launcher_has_one_semantic_shortcut() {
        assert!(matches!(
            ryeos_key_command(ctrl('k'), context(false, false)),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::OpenLauncher
            }
        ));
        assert_eq!(
            ryeos_key_command(ctrl('p'), context(false, false)),
            RyeOsKeyCommand::Ignore
        );
        let mut editing = context(false, false);
        editing.field = Some(RyeOsFieldKeyContext {
            query_editing: true,
            query_has_text: true,
            ..Default::default()
        });
        assert!(matches!(
            ryeos_key_command(ctrl('k'), editing),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::OpenLauncher
            }
        ));
    }

    #[test]
    fn help_describes_declared_input_and_contextual_escape_from_the_same_owner() {
        let help = shortcut_entries();
        assert!(
            help.iter()
                .any(|entry| entry.keys == "Ctrl+K" && entry.description.contains("configured"))
        );
        assert!(
            !help
                .iter()
                .any(|entry| entry.description.contains("foot input")
                    || entry.description.contains("launcher groups"))
        );
        assert_eq!(
            ryeos_key_command(
                key(RyeOsKey::Escape),
                RyeOsKeyContext {
                    dialog_open: true,
                    ..Default::default()
                }
            ),
            RyeOsKeyCommand::CloseFocusedDialog
        );
        assert!(matches!(
            ryeos_key_command(
                key(RyeOsKey::Escape),
                RyeOsKeyContext {
                    input_blurrable: true,
                    ..Default::default()
                }
            ),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::BlurInput
            }
        ));
        assert!(matches!(
            ryeos_key_command(
                key(RyeOsKey::Escape),
                RyeOsKeyContext {
                    head_thread_running: true,
                    ..Default::default()
                }
            ),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::InterruptHead
            }
        ));
        assert!(matches!(
            ryeos_key_command(
                key(RyeOsKey::Enter),
                RyeOsKeyContext {
                    input_focused: true,
                    input_has_text: true,
                    ..Default::default()
                }
            ),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::SubmitInput
            }
        ));
        assert!(matches!(
            ryeos_key_command(
                key(RyeOsKey::Enter),
                RyeOsKeyContext {
                    input_focused: true,
                    input_has_text: true,
                    input_is_live_filter: true,
                    ..Default::default()
                }
            ),
            RyeOsKeyCommand::Ui {
                event: RyeOsUiEvent::ActivateFocused
            }
        ));
    }

    #[test]
    fn dialog_escape_is_exact_core_close_not_a_catalogue_command() {
        assert_eq!(
            ryeos_key_command(key(RyeOsKey::Escape), context(true, false)),
            RyeOsKeyCommand::CloseFocusedDialog
        );
        assert!(!matches!(
            ryeos_key_command(key(RyeOsKey::ArrowDown), context(true, false)),
            RyeOsKeyCommand::CloseFocusedDialog
        ));
    }
}
