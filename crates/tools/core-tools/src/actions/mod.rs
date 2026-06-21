//! Action library — direct-mode logic shared by maintainer-only binaries
//! (e.g. `ryeos-cli`'s `ryeos publish`, which calls
//! [`build_bundle::rebuild_bundle_manifest`]) and (transitively) the
//! daemon's spawned-subprocess workers.
//!
//! Each submodule exposes a small `run_*` / `rebuild_*` API. No
//! `Command::new("ryeos-*")` shelling — every direct subcommand runs the
//! logic in-process so callers get typed errors and a single audit path.

pub mod authorize;
pub mod build_bundle;
pub mod hosted_policy;
pub mod inspect;
pub mod install;
pub mod manifest_sign;
pub mod node_logs;
pub mod publish;
pub mod remote_descriptor;
pub mod sign;
pub mod sign_bundle;
pub mod snapshot;
pub mod trust;
pub mod vault;
