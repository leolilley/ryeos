//! Built-in runtime handlers.
//!
//! Each handler claims one top-level YAML key on tool/runtime items
//! and owns deserialization + processing of its own block.

pub mod config_resolve;
pub mod env_config;
pub mod execution_params;
pub mod native_async;
pub mod native_resume;
pub mod runtime_config;
pub mod verify_deps;
