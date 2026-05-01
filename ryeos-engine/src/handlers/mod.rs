//! Handler descriptors + registry — the new `handler` kind from
//! Phase 3. See `.tmp/STRATEGY-AND-GAPS/implementation/03-PHASE-3-DESIGN.md`.

pub mod descriptor;
pub mod registry;
pub(crate) mod subprocess;

pub use descriptor::{HandlerDescriptor, HandlerServes};
pub use registry::{HandlerError, HandlerRegistry, VerifiedHandler};

/// Protocol ABI version this engine speaks for handler binaries.
/// Distinct from SUPPORTED_RUNTIME_ABI_VERSION.
pub const SUPPORTED_HANDLER_ABI_VERSION: &str = "v1";
