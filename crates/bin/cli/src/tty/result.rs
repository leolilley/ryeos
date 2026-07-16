use std::io::{self, Write};

use serde::Serialize;

/// Exact machine serializer boundary. Human presentation must never call this
/// and this function must never add headings, hints, or diagnostics to stdout.
pub fn write_json(value: &impl Serialize) -> io::Result<()> {
    let rendered = serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut out = io::stdout().lock();
    writeln!(out, "{rendered}")?;
    out.flush()
}

pub fn write_machine_diagnostics(lines: &[String]) -> io::Result<()> {
    let mut out = io::stderr().lock();
    for line in lines {
        writeln!(out, "{line}")?;
    }
    out.flush()
}

pub fn write_raw(value: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{value}")?;
    out.flush()
}
