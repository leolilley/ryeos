//! Terminal guard — RAII raw mode, alt screen, mouse capture.
//!
//! Wraps crossterm setup/teardown so the terminal is always restored.

use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, terminal,
};
use std::io;

pub struct TerminalGuard {
    width: u16,
    height: u16,
    keyboard_enhancement_pushed: bool,
}

impl TerminalGuard {
    pub fn init() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // Own restoration before any further fallible setup, including a
        // partially written bracketed-paste/screen command sequence.
        let mut guard = Self {
            width: 0,
            height: 0,
            keyboard_enhancement_pushed: false,
        };
        let mut stdout = io::stdout();
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide,
        )?;

        // Try Kitty keyboard protocol (best-effort)
        guard.keyboard_enhancement_pushed = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        )
        .is_ok();

        (guard.width, guard.height) = terminal::size()?;

        Ok(guard)
    }

    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn update_size(&mut self) -> io::Result<(u16, u16)> {
        let (w, h) = terminal::size()?;
        self.width = w;
        self.height = h;
        Ok((w, h))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        // Balance only this guard's successful push, including setup failures
        // after it. Never pop a host application's keyboard protocol frame.
        if self.keyboard_enhancement_pushed {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            Show,
            DisableMouseCapture,
            DisableBracketedPaste,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}
