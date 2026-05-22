//! Terminal guard — RAII raw mode, alt screen, mouse capture.
//!
//! Wraps crossterm setup/teardown so the terminal is always restored.

use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute, terminal,
};
use std::io::{self, Write};

pub struct TerminalGuard {
    width: u16,
    height: u16,
}

impl TerminalGuard {
    pub fn init() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            EnableMouseCapture,
            Hide,
        )?;

        // Try Kitty keyboard protocol (best-effort)
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        );

        let (width, height) = terminal::size()?;

        Ok(Self { width, height })
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
        let _ = execute!(stdout, Show, DisableMouseCapture, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}
