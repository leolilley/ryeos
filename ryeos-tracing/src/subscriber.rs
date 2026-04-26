//! Unified subscriber initialization for all Rye OS binaries.
//!
//! # Usage
//!
//! ```no_run
//! ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::default());
//! ```
//!
//! # Environment Variables
//!
//! - `RUST_LOG` — standard tracing filter (e.g. `"ryeosd=debug,ryeos_engine=info"`)
//! - `RYE_TRACE_JSON` — if set, output structured JSON instead of pretty human format

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing_subscriber::{EnvFilter, Layer};
use tracing_subscriber::prelude::*;

/// Configuration for the tracing subscriber.
#[derive(Debug)]
pub struct SubscriberConfig {
    /// Tracing filter string. Falls back to `RUST_LOG` env var, then this default.
    pub default_filter: String,
    /// If true, output structured JSON (for log aggregation).
    pub json_output: bool,
    /// Include module path in output.
    pub with_target: bool,
    /// Include file:line in output.
    pub with_file: bool,
    /// Include thread IDs in output.
    pub with_thread_ids: bool,
}

impl Default for SubscriberConfig {
    fn default() -> Self {
        Self {
            default_filter: "info".into(),
            json_output: std::env::var("RYE_TRACE_JSON").is_ok(),
            with_target: true,
            with_file: false,
            with_thread_ids: false,
        }
    }
}

impl SubscriberConfig {
    /// Config suitable for the ryeosd daemon.
    pub fn for_daemon() -> Self {
        Self {
            default_filter: "ryeosd=info,ryeos_engine=info,ryeos_state=info".into(),
            ..Self::default()
        }
    }

    /// Config suitable for the ryeosd daemon with an ndjson file sink.
    ///
    /// Installs the stderr layer (human-readable) PLUS a second
    /// `fmt::layer().json()` that appends structured ndjson lines to
    /// `<state_dir>/.ai/state/trace-events.ndjson`. The file is opened
    /// once with append mode and shared across all writes via
    /// `Arc<Mutex<File>>`. Survives daemon restart — the file
    /// persists so test harnesses can tail across runs.
    pub fn for_daemon_with_file_sink(state_dir: &Path) -> Self {
        // Ensure the .ai/state/ directory exists before opening the file.
        let _ = std::fs::create_dir_all(state_dir.join(".ai").join("state"));

        let trace_path = state_dir.join(".ai").join("state").join("trace-events.ndjson");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace_path)
            .expect("failed to open trace-events.ndjson for writing");

        // Build the registry: stderr (human) + file (ndjson).
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| {
                EnvFilter::new("ryeosd=info,ryeos_engine=info,ryeos_state=info")
            });

        let writer = Arc::new(Mutex::new(file));

        // File layer: structured JSON, span NEW/CLOSE events.
        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(SharedFileWriter(writer.clone()))
            .with_ansi(false)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW | tracing_subscriber::fmt::format::FmtSpan::CLOSE);

        // Stderr layer: human-readable for operator convenience.
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_filter(filter.clone());

        let file_layer = file_layer.with_filter(filter);

        let _ = tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer)
            .try_init();

        Self {
            default_filter: "ryeosd=info,ryeos_engine=info,ryeos_state=info".into(),
            ..Self::default()
        }
    }

    /// Config suitable for the graph runtime.
    pub fn for_graph_runtime() -> Self {
        Self {
            default_filter: "ryeos_graph_runtime=info,ryeos_engine=info".into(),
            ..Self::default()
        }
    }

    /// Config suitable for the directive runtime.
    pub fn for_directive_runtime() -> Self {
        Self {
            default_filter: "ryeos_directive_runtime=info,ryeos_engine=info".into(),
            ..Self::default()
        }
    }

    /// Config suitable for CLI tools (rye-fetch, rye-sign, etc.).
    pub fn for_cli_tool() -> Self {
        Self {
            default_filter: "info".into(),
            ..Self::default()
        }
    }
}

/// A `MakeWriter` that shares a single opened `File` across all tracing
/// writes via `Arc<Mutex<File>>`. Opens once with append mode; each write
/// acquires the mutex, appends a line, and releases.
pub struct SharedFileWriter(Arc<Mutex<std::fs::File>>);

impl SharedFileWriter {
    /// Create a new shared file writer from an already-opened file.
    pub fn new(file: Arc<Mutex<std::fs::File>>) -> Self {
        Self(file)
    }
}

impl std::io::Write for &SharedFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut file = self.0.lock().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("lock poisoned: {e}"))
        })?;
        file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut file = self.0.lock().map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("lock poisoned: {e}"))
        })?;
        file.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedFileWriter {
    type Writer = &'a Self;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

/// Initialize the global tracing subscriber.
///
/// This is idempotent — safe to call multiple times. If a global subscriber
/// is already installed (e.g. by an earlier call or by a test harness),
/// subsequent calls are silent no-ops.
///
/// The `RUST_LOG` environment variable overrides `config.default_filter` when set.
pub fn init_subscriber(config: SubscriberConfig) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.default_filter));

    let _ = if config.json_output {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_target(config.with_target)
            .with_file(config.with_file)
            .with_thread_ids(config.with_thread_ids)
            .try_init()
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(config.with_target)
            .with_file(config.with_file)
            .with_thread_ids(config.with_thread_ids)
            .try_init()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_sensible() {
        let config = SubscriberConfig::default();
        assert!(!config.json_output);
        assert!(config.with_target);
        assert!(!config.with_file);
        assert!(!config.with_thread_ids);
    }

    #[test]
    fn preset_configs_have_distinct_filters() {
        let daemon = SubscriberConfig::for_daemon();
        let graph = SubscriberConfig::for_graph_runtime();
        let directive = SubscriberConfig::for_directive_runtime();
        let cli = SubscriberConfig::for_cli_tool();

        assert_ne!(daemon.default_filter, graph.default_filter);
        assert_ne!(daemon.default_filter, directive.default_filter);
        assert_ne!(daemon.default_filter, cli.default_filter);
    }
}
