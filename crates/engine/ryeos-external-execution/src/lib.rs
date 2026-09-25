//! Protected external-candidate runtime.
//!
//! This crate owns guest launcher, supervisor and transport behavior. It has
//! no node application, executor, API, provider-account or cloud dependency.

pub mod backends;
pub mod guest_inputs;
pub mod launcher;
pub mod launcher_protocol;
pub mod lifecycle_adapter;
pub mod supervisor;
pub mod transport;
