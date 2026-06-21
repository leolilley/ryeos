//! Local node log reader for `ryeos logs`.
//!
//! Tails the node's `trace-events.ndjson` and the daemon startup stderr log
//! straight from the app root. Offline by design: reads files directly, so it
//! still works when the daemon failed to start or crashed — the case where a
//! live handler could not answer.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct LogStream {
    pub path: PathBuf,
    /// Whether the file exists/was readable.
    pub present: bool,
    /// The last N lines (oldest first), or empty when absent.
    pub lines: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct NodeLogsReport {
    pub app_root: PathBuf,
    pub trace_events: LogStream,
    pub startup_stderr: LogStream,
}

fn tail(path: PathBuf, n: usize) -> LogStream {
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let all: Vec<&str> = content.lines().collect();
            let start = all.len().saturating_sub(n);
            let lines = all[start..].iter().map(|s| s.to_string()).collect();
            LogStream {
                path,
                present: true,
                lines,
            }
        }
        Err(_) => LogStream {
            path,
            present: false,
            lines: Vec::new(),
        },
    }
}

/// Read the node's logs from `<app_root>/.ai/state/`, returning the last
/// `lines` lines of `trace-events.ndjson` and `ryeosd-start.stderr.log`.
pub fn read_node_logs(app_root: &Path, lines: usize) -> NodeLogsReport {
    let state = app_root.join(ryeos_engine::AI_DIR).join("state");
    NodeLogsReport {
        app_root: app_root.to_path_buf(),
        trace_events: tail(state.join("trace-events.ndjson"), lines),
        startup_stderr: tail(state.join("ryeosd-start.stderr.log"), lines),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails_present_and_absent_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".ai/state");
        std::fs::create_dir_all(&state).unwrap();
        let body: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(state.join("trace-events.ndjson"), body).unwrap();
        // startup stderr deliberately absent.

        let report = read_node_logs(tmp.path(), 3);
        assert!(report.trace_events.present);
        assert_eq!(report.trace_events.lines, vec!["line8", "line9", "line10"]);
        assert!(!report.startup_stderr.present);
        assert!(report.startup_stderr.lines.is_empty());
    }
}
