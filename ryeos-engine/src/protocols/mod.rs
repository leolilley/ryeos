//! Protocol descriptors + registry — the `protocol` kind.
//!
//! Protocol descriptors are bundle-shipped, signed YAML items declaring
//! a wire contract a subprocess terminator must speak. Loaded by
//! `ProtocolRegistry::load_base` via the Layer-1 raw signed-YAML loader;
//! never goes through the protocol-aware dispatch path.
//!
//! Composes only from the closed vocabulary in `protocol_vocabulary`.
//! Adding a new vocabulary primitive is a daemon code change, not a
//! protocol-descriptor change.

pub mod descriptor;
pub mod registry;

pub use descriptor::ProtocolDescriptor;
pub use registry::{ProtocolError, ProtocolRegistry, VerifiedProtocol};

/// Protocol ABI version this engine supports.
/// Distinct from handler and runtime ABI versions.
pub const SUPPORTED_PROTOCOL_ABI_VERSION: &str = "v1";
