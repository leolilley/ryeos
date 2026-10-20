//! Process-level output for the client binaries.
//!
//! Pre-launch diagnostics and help text leave a client process through these
//! functions, so one module owns the stdout and stderr handles. Diagnostics use
//! the same plain `ryeos: ` line form as the CLI's non-terminal output.

use std::io::{self, Write};

/// Write one diagnostic line to stderr.
///
/// A closed stderr has nowhere left to report to, so a failed write is dropped.
fn write_line(line: &str) {
    let mut err = io::stderr().lock();
    let _ = writeln!(err, "ryeos: {line}");
}

pub fn info(message: &str) {
    write_line(&format!("info: {message}"));
}

pub fn warning(message: &str) {
    write_line(&format!("warning: {message}"));
}

/// Report a fatal condition and its optional hint, then exit with status 1.
pub fn fatal(message: &str, hint: Option<&str>) -> ! {
    write_line(message);
    if let Some(hint) = hint {
        write_line(&format!("hint: {hint}"));
    }
    std::process::exit(1);
}

/// Write text to stdout followed by a newline.
pub fn stdout_line(text: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{text}");
}

/// Write a prompt to stderr without a line ending and flush it.
pub fn stderr_prompt(text: &str) {
    let mut err = io::stderr().lock();
    let _ = write!(err, "{text}");
    let _ = err.flush();
}

/// End a prompt line on stderr.
pub fn stderr_newline() {
    let mut err = io::stderr().lock();
    let _ = writeln!(err);
}
