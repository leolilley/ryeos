//! Shared runtime primitives for bounded interactive CLI flows.
//!
//! These modules deliberately do not depend on the persistent terminal client:
//! an interaction owns only the viewport printed by one foreground command and
//! always returns control to the invoking shell.

mod event;
#[allow(dead_code)]
mod focus;
mod frame;
mod list;
mod pager;
mod terminal_guard;
mod text_input;

pub(crate) use event::{Event, EventReader, Key, KeyEvent};
pub(crate) use frame::Frame;
pub(crate) use list::{ListItem, ListState};
pub(crate) use pager::Pager;
pub(crate) use terminal_guard::TerminalGuard;
pub(crate) use text_input::{InputAction, SecretInput, TextInput};
