//! `claude-code-adapter`: the workload the structured-session bridge spawns
//! for the claude-code bundle.
//!
//! It speaks the bridge's stdio wire (one JSON object per line) and nothing
//! else. Sessions are served by the Claude Code driver, which runs the
//! configured executable once per turn; a build with the `stub-driver`
//! feature can also serve scripted stub sessions (see `select`).

mod claude;
mod driver;
mod invocation;
mod select;
mod sessions;
mod stream;
#[cfg(feature = "stub-driver")]
mod stub;
mod wire;

use std::io::{self, Write as _};
use std::process::ExitCode;
use std::sync::Arc;

use driver::{Driver, Server};
use select::Adapter;
use wire::Emitter;

fn main() -> ExitCode {
    let adapter = Arc::new(Adapter::from_environment());
    // On SIGTERM, stop every Claude Code process group before exiting; on
    // input EOF the server does the same through `shutdown`.
    let on_signal = Arc::clone(&adapter);
    if let Err(error) = lillux::child_group::exit_on_terminate(move || on_signal.shutdown()) {
        let _ = writeln!(
            io::stderr(),
            "claude-code-adapter: install signal handler: {error}"
        );
        return ExitCode::FAILURE;
    }
    let mut server = Server::new(adapter, Emitter::new(io::stdout()));
    match server.run(io::stdin().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Never echo input: a request can carry task data.
            let _ = writeln!(io::stderr(), "claude-code-adapter: {error}");
            ExitCode::FAILURE
        }
    }
}
