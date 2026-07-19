use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::{execute, terminal};

static INTERACTION_ACTIVE: AtomicBool = AtomicBool::new(false);
static INSTALL_PANIC_RESTORE: Once = Once::new();

/// Restores terminal attributes for a command-local raw-mode interaction.
///
/// Unlike the persistent TUI guard, this does not enter the alternate screen
/// or enable mouse capture. The command owns only the viewport it paints in
/// normal shell scrollback.
pub(crate) struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    pub(crate) fn enter() -> io::Result<Self> {
        install_panic_restore();
        terminal::enable_raw_mode()?;
        let mut output = io::stdout();
        if let Err(error) = execute!(output, Hide, EnableBracketedPaste) {
            let _ = restore_terminal();
            return Err(error);
        }
        INTERACTION_ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self { active: true })
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        restore_terminal()?;
        self.active = false;
        INTERACTION_ACTIVE.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn install_panic_restore() {
    INSTALL_PANIC_RESTORE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if INTERACTION_ACTIVE.swap(false, Ordering::SeqCst) {
                let _ = restore_terminal();
            }
            previous(info);
        }));
    });
}

fn restore_terminal() -> io::Result<()> {
    let mut output = io::stdout();
    let presentation = execute!(output, DisableBracketedPaste, Show);
    let raw_mode = terminal::disable_raw_mode();
    let _ = write!(output, "\r");
    let _ = output.flush();
    presentation.and(raw_mode)
}
