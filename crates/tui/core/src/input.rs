//! Platform-neutral input events.
//!
//! Terminal and web adapters convert their platform-specific events
//! to these types before passing to the update reducer.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    Key(Key),
    Mouse(Mouse),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Key {
    Char(char),
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Backspace,
    Delete,
    Tab,
    Enter,
    Escape,
    F(u8),
    Ctrl(char),
    Alt(char),
    CtrlEnter,
    ShiftEnter,
    ShiftTab,
    AltEnter,
    AltBackspace,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mouse {
    pub x: u16,
    pub y: u16,
    pub action: MouseAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    Scroll(ScrollDirection),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScrollDirection {
    Up,
    Down,
}
