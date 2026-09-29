//! Protected external-candidate runtime.
//!
//! This crate owns guest launcher, supervisor and transport behavior. It has
//! no node application, executor, API, provider-account or cloud dependency.

pub mod backends;
pub mod guest_content;
pub mod guest_import_authorization;
pub mod guest_inputs;
pub mod guest_installation;
pub mod guest_package_producer;
pub mod guest_runtime_product;
pub mod guest_staging;
pub mod launcher;
pub mod launcher_protocol;
pub mod lifecycle_adapter;
pub mod restoration_verifier_delivery;
pub mod supervisor;
pub mod transport;
