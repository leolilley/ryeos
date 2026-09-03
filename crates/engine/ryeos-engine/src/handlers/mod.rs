//! Handler descriptors + registry for the `handler` kind.

pub mod descriptor;
pub mod registry;
pub(crate) mod subprocess;

pub use descriptor::{HandlerDescriptor, HandlerServes};
pub use registry::{HandlerError, HandlerRegistry, VerifiedHandler};

/// Protocol ABI version this engine speaks for handler binaries.
/// Distinct from SUPPORTED_RUNTIME_ABI_VERSION.
pub const SUPPORTED_HANDLER_ABI_VERSION: &str = "v3";
