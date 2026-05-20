//! Engine initialization (Model B).
//!
//! Thin re-export shim. The implementation now lives in
//! [`ryeos_app::engine_init`] so the per-request engine cache in
//! `ryeos-executor` can call `build_engine_for_roots` without a
//! cyclic dependency on `ryeosd` (the daemon binary).
//!
//! See `ryeos_app::engine_init` for the full constructor docs.

pub use ryeos_app::engine_init::{build_engine, build_engine_for_roots};
