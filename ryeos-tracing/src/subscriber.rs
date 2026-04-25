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

use tracing_subscriber::EnvFilter;

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
