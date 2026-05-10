//! Launch envelope — re-exports from `ryeos_runtime::envelope`.
//!
//! Single source of truth lives in the runtime crate; the daemon mints
//! the envelope and runtimes deserialise the same struct.
//!
//! `EnvelopeTarget` is gone — the runtime gets the root path / digest /
//! kind / id from `LaunchEnvelope.resolution.root` directly.

pub use ryeos_runtime::envelope::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, HardLimits,
    LaunchEnvelope, LaunchEnvelopeBuilder, RuntimeResult,
};
