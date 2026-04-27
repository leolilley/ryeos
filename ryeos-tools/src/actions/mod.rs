//! Action library — direct-mode logic shared by the `rye` CLI binary
//! (in `rye-cli/`) and (transitively) the daemon's spawned-subprocess workers.
//!
//! Each submodule exposes a small `run_*` API used by `rye-cli/src/exec.rs`.
//! No `Command::new("rye-*")` shelling — every direct subcommand runs
//! the logic in-process.

pub mod build_bundle;
pub mod sign;
