//! Shared node foundations, independent of App's domain service container.
//!
//! The complete foundation extraction is being assembled. Registry primitives
//! already have one owner shared by composition readers and App mutations.

pub mod registry;

pub mod calibration_policy;

pub mod bundle_bootstrap;
pub mod node_config_source;
pub mod node_document;

pub mod config;

pub mod engine_init;

pub mod identity;

pub mod node_policy;

pub mod state_lock;
