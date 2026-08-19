use std::io::{self, IsTerminal};
#[cfg(unix)]
use std::os::fd::AsRawFd;

const DEFAULT_TERMINAL_WIDTH: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HumanOutputMode {
    Tty,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub mode: HumanOutputMode,
    pub color: bool,
    pub unicode: bool,
    pub width: usize,
}

impl TerminalCapabilities {
    #[cfg(test)]
    pub const fn plain(width: usize) -> Self {
        Self {
            mode: HumanOutputMode::Plain,
            color: false,
            unicode: false,
            width,
        }
    }

    pub fn detect(force_plain: bool) -> Self {
        let override_mode = std::env::var("RYEOS_TTY").unwrap_or_else(|_| "auto".into());
        let term_dumb = std::env::var("TERM").is_ok_and(|term| term == "dumb");
        let streams_are_tty = io::stdout().is_terminal() && io::stderr().is_terminal();
        Self::detect_with(
            force_plain,
            &override_mode,
            term_dumb,
            streams_are_tty,
            std::env::var_os("NO_COLOR").is_none(),
            terminal_width(),
        )
    }

    fn detect_with(
        force_plain: bool,
        override_mode: &str,
        term_dumb: bool,
        streams_are_tty: bool,
        color_allowed: bool,
        width: usize,
    ) -> Self {
        let tty = match override_mode {
            "always" if !force_plain && !term_dumb => true,
            "never" => false,
            _ => !force_plain && !term_dumb && streams_are_tty,
        };
        let mode = if tty {
            HumanOutputMode::Tty
        } else {
            HumanOutputMode::Plain
        };
        Self {
            mode,
            color: tty && color_allowed,
            unicode: tty,
            width,
        }
    }

    pub fn tty(self) -> bool {
        self.mode == HumanOutputMode::Tty
    }

    /// Whether a foreground command may safely take over terminal input.
    /// Presentation overrides never turn pipes or `/dev/null` into an
    /// interactive input source.
    pub fn interactive(self) -> bool {
        self.tty()
            && io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && io::stderr().is_terminal()
    }
}

fn terminal_width() -> usize {
    tty_width()
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|width| *width >= 2)
        })
        .unwrap_or(DEFAULT_TERMINAL_WIDTH)
}

pub(super) fn live_terminal_width(fallback: usize) -> usize {
    tty_width().unwrap_or(fallback.max(2))
}

#[cfg(unix)]
fn tty_width() -> Option<usize> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let status = unsafe { libc::ioctl(io::stderr().as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    (status == 0 && size.ws_col >= 2).then_some(size.ws_col as usize)
}

#[cfg(not(unix))]
fn tty_width() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_never_is_plain_and_ascii() {
        let caps = TerminalCapabilities::detect_with(false, "never", false, true, true, 80);
        assert_eq!(caps.mode, HumanOutputMode::Plain);
        assert!(!caps.color);
        assert!(!caps.unicode);
    }

    #[test]
    fn machine_mode_wins_over_always() {
        let caps = TerminalCapabilities::detect_with(true, "always", false, true, true, 80);
        assert_eq!(caps.mode, HumanOutputMode::Plain);
        assert!(!caps.color);
    }

    #[test]
    fn dumb_terminal_wins_over_always() {
        let caps = TerminalCapabilities::detect_with(false, "always", true, true, true, 80);
        assert_eq!(caps.mode, HumanOutputMode::Plain);
        assert!(!caps.unicode);
    }
}
