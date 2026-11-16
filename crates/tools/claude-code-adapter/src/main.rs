//! `claude-code-adapter`: the workload the structured-session bridge spawns
//! for the claude-code bundle.
//!
//! It speaks the bridge's stdio wire (one JSON object per line) and nothing
//! else. In this slice the only driver is the stub, which needs no model, no
//! network and no Claude Code; a driver that runs the real executable later
//! implements the same `Driver` trait and replaces the one constructed below.

mod driver;
mod stub;
mod wire;

use std::io::{self, Write as _};
use std::process::ExitCode;
use std::sync::Arc;

use driver::Server;
use stub::StubDriver;
use wire::Emitter;

fn main() -> ExitCode {
    let mut server = Server::new(
        Arc::new(StubDriver::from_environment()),
        Emitter::new(io::stdout()),
    );
    match server.run(io::stdin().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Never echo input: a request can carry task data.
            let _ = writeln!(io::stderr(), "claude-code-adapter: {error}");
            ExitCode::FAILURE
        }
    }
}
