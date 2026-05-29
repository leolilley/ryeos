//! Shared tracing infrastructure for the Rye OS workspace.
//!
//! Provides:
//! - [`init_subscriber`] — unified subscriber initialization for all binaries
//! - [`test`] — trace-capture harness for asserting spans in tests (enable `test-harness` feature)

pub mod subscriber;

#[cfg(any(test, feature = "test-harness"))]
pub mod test;

pub use subscriber::{init_subscriber, SubscriberConfig};
