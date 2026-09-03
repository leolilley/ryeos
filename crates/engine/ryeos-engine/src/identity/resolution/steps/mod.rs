//! Built-in resolution step implementations.
//!
//! Each module exposes a `run(ctx, field, max_depth)` entry point that
//! `ResolutionContext::run_step` dispatches into based on the tagged
//! enum variant declared in the kind schema.

pub mod extends;
pub mod references;
