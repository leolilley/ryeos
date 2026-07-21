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
        let tty = match override_mode.as_str() {
            "always" if !force_plain && !term_dumb => true,
            "never" => false,
            _ => {
                !force_plain
                    && !term_dumb
                    && io::stdout().is_terminal()
                    && io::stderr().is_terminal()
            }
        };
        let mode = if tty {
            HumanOutputMode::Tty
        } else {
            HumanOutputMode::Plain
        };
        Self {
            mode,
            color: tty && std::env::var_os("NO_COLOR").is_none(),
            unicode: tty,
            width: terminal_width(),
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
                .filter(|width| *width >= 20)
        })
        .unwrap_or(DEFAULT_TERMINAL_WIDTH)
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
    (status == 0 && size.ws_col >= 20).then_some(size.ws_col as usize)
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
        let _lock = crate::test_env::lock();
        let previous = std::env::var_os("RYEOS_TTY");
        std::env::set_var("RYEOS_TTY", "never");
        let caps = TerminalCapabilities::detect(false);
        match previous {
            Some(value) => std::env::set_var("RYEOS_TTY", value),
            None => std::env::remove_var("RYEOS_TTY"),
        }
        assert_eq!(caps.mode, HumanOutputMode::Plain);
        assert!(!caps.color);
        assert!(!caps.unicode);
    }

    #[test]
    fn machine_mode_wins_over_always() {
        let _lock = crate::test_env::lock();
        let previous = std::env::var_os("RYEOS_TTY");
        std::env::set_var("RYEOS_TTY", "always");
        let caps = TerminalCapabilities::detect(true);
        match previous {
            Some(value) => std::env::set_var("RYEOS_TTY", value),
            None => std::env::remove_var("RYEOS_TTY"),
        }
        assert_eq!(caps.mode, HumanOutputMode::Plain);
        assert!(!caps.color);
    }

    #[test]
    fn dumb_terminal_wins_over_always() {
        let _lock = crate::test_env::lock();
        let previous_override = std::env::var_os("RYEOS_TTY");
        let previous_term = std::env::var_os("TERM");
        std::env::set_var("RYEOS_TTY", "always");
        std::env::set_var("TERM", "dumb");
        let caps = TerminalCapabilities::detect(false);
        match previous_override {
            Some(value) => std::env::set_var("RYEOS_TTY", value),
            None => std::env::remove_var("RYEOS_TTY"),
        }
        match previous_term {
            Some(value) => std::env::set_var("TERM", value),
            None => std::env::remove_var("TERM"),
        }
        assert_eq!(caps.mode, HumanOutputMode::Plain);
        assert!(!caps.unicode);
    }
}
