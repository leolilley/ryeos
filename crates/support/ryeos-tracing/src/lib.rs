//! Shared tracing infrastructure for the Rye OS workspace.
//!
//! Provides:
//! - [`init_subscriber`] — unified subscriber initialization for all binaries
//! - [`mod@test`] — trace-capture harness for asserting spans in tests (enable `test-harness` feature)

pub mod cache_metrics;
pub mod subscriber;

#[cfg(any(test, feature = "test-harness"))]
pub mod test;

pub use cache_metrics::{
    CacheMetricSample, flush_cache_metrics, flush_cache_metrics_due, record_cache_metric,
};
pub use subscriber::{SubscriberConfig, init_subscriber};
