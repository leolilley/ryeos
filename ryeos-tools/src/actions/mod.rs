//! Action library — direct-mode logic shared by maintainer-only binaries
//! (e.g. `ryeos-cli`'s `rye-bundle-tool`, which calls
//! [`build_bundle::rebuild_bundle_manifest`]) and (transitively) the
//! daemon's spawned-subprocess workers.
//!
//! Each submodule exposes a small `run_*` / `rebuild_*` API. No
//! `Command::new("rye-*")` shelling — every direct subcommand runs the
//! logic in-process so callers get typed errors and a single audit path.

pub mod build_bundle;
pub mod sign;
pub mod sign_bundle;
