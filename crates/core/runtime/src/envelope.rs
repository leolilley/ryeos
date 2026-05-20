//! Launch envelope types — re-exported from `ryeos_engine::launch_envelope_types`.
//!
//! Single source of truth lives in the engine crate; the daemon mints
//! the envelope and runtimes deserialise the same struct.
//!
//! `EnvelopeTarget` is gone — the runtime gets the root path / digest /
//! kind / id from `LaunchEnvelope.resolution.root` directly.

pub use ryeos_engine::launch_envelope_types::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, HardLimits,
    ItemDescriptor, LaunchEnvelope, LaunchEnvelopeBuilder, RuntimeCost, RuntimeResult,
};
