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
//! - `RYEOS_TRACE_JSON` — if set, output structured JSON instead of pretty human format

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Layer};

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
            json_output: std::env::var("RYEOS_TRACE_JSON").is_ok(),
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
            default_filter:
                "ryeosd=info,ryeos_engine=info,ryeos_state=info,ryeos_executor=info,ryeos_app=info"
                    .into(),
            ..Self::default()
        }
    }

    /// Config suitable for the ryeosd daemon with an ndjson file sink.
    ///
    /// Installs the stderr layer (human-readable) PLUS a second
    /// `fmt::layer().json()` that appends structured ndjson lines to
    /// `<app_root>/.ai/state/trace-events.ndjson`. The file survives daemon
    /// restart so test harnesses can tail across runs, and is size-capped:
    /// past [`TRACE_ROTATE_BYTES`] it rotates to `trace-events.ndjson.1`
    /// (replacing the previous generation), bounding disk usage at ~2× the
    /// cap regardless of daemon lifetime.
    pub fn for_daemon_with_file_sink(state_dir: &Path) -> Self {
        // Ensure the .ai/state/ directory exists before opening the file.
        let state_dir_path = state_dir.join(".ai").join("state");
        if let Err(e) = std::fs::create_dir_all(&state_dir_path) {
            // Log but continue — the file open below will produce a clearer
            // error if the directory truly can't be created.
            eprintln!(
                "warn: failed to create trace dir {}: {e}",
                state_dir_path.display()
            );
        }

        let trace_path = state_dir
            .join(".ai")
            .join("state")
            .join("trace-events.ndjson");
        let writer = SharedFileWriter::open(&trace_path, TRACE_ROTATE_BYTES)
            .expect("failed to open trace-events.ndjson for writing");

        // Build the registry: stderr (human) + file (ndjson).
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new(
                "ryeosd=info,ryeos_engine=info,ryeos_state=info,ryeos_executor=info,ryeos_app=info",
            )
        });

        // File layer: structured JSON, span NEW/CLOSE events.
        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(writer)
            .with_ansi(false)
            .with_span_events(
                tracing_subscriber::fmt::format::FmtSpan::NEW
                    | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
            );

        // Stderr layer: human-readable for operator convenience. The writer
        // must be stderr explicitly — fmt::layer() defaults to stdout, which
        // the daemon runs with /dev/null (and stdout is reserved for
        // structured results everywhere else in the system), so a default
        // writer here silently discards every human-readable daemon line,
        // including the boot heartbeats `ryeos start` points operators at.
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(true)
            .with_filter(filter.clone());

        let file_layer = file_layer.with_filter(filter);

        let _ = tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer)
            .try_init();

        Self {
            default_filter:
                "ryeosd=info,ryeos_engine=info,ryeos_state=info,ryeos_executor=info,ryeos_app=info"
                    .into(),
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

    /// Config suitable for CLI tools (ryeos-core-tools, etc.).
    pub fn for_cli_tool() -> Self {
        Self {
            default_filter: "info".into(),
            ..Self::default()
        }
    }
}

/// Rotate the trace sink once the file passes this many bytes.
pub const TRACE_ROTATE_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB

/// A size-capped `MakeWriter` sharing one appending `File` across all tracing
/// writes. Each write acquires the mutex, appends a line, and releases. Once
/// the file passes the rotation threshold it is renamed to `<name>.1`
/// (replacing any previous generation) and a fresh file is opened — total
/// disk usage stays bounded at ~2× the threshold.
pub struct SharedFileWriter(Arc<Mutex<TraceSink>>);

struct TraceSink {
    file: std::fs::File,
    path: std::path::PathBuf,
    bytes: u64,
    rotate_at: u64,
}

impl TraceSink {
    /// Rename the current file to `<name>.1` and open a fresh one. Failures
    /// are swallowed (a trace sink must never take the daemon down); the
    /// counter resets either way so a failing rotation is retried once per
    /// threshold's worth of writes, not on every line.
    fn rotate(&mut self) {
        let mut rotated_name = self
            .path
            .file_name()
            .map(ToOwned::to_owned)
            .unwrap_or_default();
        rotated_name.push(".1");
        let rotated = self.path.with_file_name(rotated_name);
        if std::fs::rename(&self.path, &rotated).is_ok() {
            if let Ok(fresh) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                self.file = fresh;
            }
        }
        self.bytes = 0;
    }
}

impl SharedFileWriter {
    /// Open (or create) an appending sink at `path`, rotating once it grows
    /// past `rotate_at` bytes. An already-oversized file rotates on the
    /// first write.
    pub fn open(path: &Path, rotate_at: u64) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self(Arc::new(Mutex::new(TraceSink {
            file,
            path: path.to_path_buf(),
            bytes,
            rotate_at,
        }))))
    }
}

impl std::io::Write for &SharedFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut sink = self
            .0
            .lock()
            .map_err(|e| std::io::Error::other(format!("lock poisoned: {e}")))?;
        if sink.bytes >= sink.rotate_at {
            sink.rotate();
        }
        let n = sink.file.write(buf)?;
        sink.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut sink = self
            .0
            .lock()
            .map_err(|e| std::io::Error::other(format!("lock poisoned: {e}")))?;
        sink.file.flush()
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

    // Tracing output MUST go to stderr — stdout is reserved for
    // structured runtime results (e.g. directive-runtime writes its
    // terminal `RuntimeResult` JSON to stdout for the daemon to
    // parse). Mixing tracing into stdout silently corrupts the
    // protocol and surfaces as "failed to parse runtime stdout"
    // errors at the daemon. The default `tracing_subscriber::fmt()`
    // writer is `std::io::stdout`, so this override is required.
    let _ = if config.json_output {
        tracing_subscriber::fmt()
            .json()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .with_target(config.with_target)
            .with_file(config.with_file)
            .with_thread_ids(config.with_thread_ids)
            .try_init()
    } else {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
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
    fn file_sink_rotates_past_the_size_cap() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trace-events.ndjson");
        let writer = SharedFileWriter::open(&path, 32).unwrap();

        // First writes land in the primary file.
        (&writer)
            .write_all(b"line one, sized to fill the cap entirely\n")
            .unwrap();
        // The cap is now exceeded, so the next write rotates first.
        (&writer).write_all(b"line two\n").unwrap();

        let rotated = tmp.path().join("trace-events.ndjson.1");
        assert!(rotated.exists(), "expected a rotated generation");
        assert!(
            std::fs::read_to_string(&rotated)
                .unwrap()
                .contains("line one"),
            "rotated file should hold the pre-rotation content"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line two\n",
            "primary file should hold only post-rotation content"
        );

        // A second rotation replaces the previous generation.
        (&writer)
            .write_all(b"line three, also fills the cap entirely\n")
            .unwrap();
        (&writer).write_all(b"line four\n").unwrap();
        assert!(
            std::fs::read_to_string(&rotated)
                .unwrap()
                .contains("line two"),
            "rotation should replace the previous generation"
        );
    }

    #[test]
    fn file_sink_rotates_an_already_oversized_file_on_first_write() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("trace-events.ndjson");
        std::fs::write(&path, vec![b'x'; 64]).unwrap();

        let writer = SharedFileWriter::open(&path, 32).unwrap();
        (&writer).write_all(b"fresh\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        assert!(tmp.path().join("trace-events.ndjson.1").exists());
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
