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

/// Cap on how many trailing bytes are read to satisfy a tail, so a multi-GB
/// `trace-events.ndjson` never has to be loaded whole.
const TAIL_BYTE_CAP: u64 = 4 * 1024 * 1024;

fn tail(path: PathBuf, n: usize) -> LogStream {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => {
            return LogStream {
                path,
                present: false,
                lines: Vec::new(),
            }
        }
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let read_from = len.saturating_sub(TAIL_BYTE_CAP);
    if read_from > 0 && file.seek(SeekFrom::Start(read_from)).is_err() {
        return LogStream {
            path,
            present: true,
            lines: Vec::new(),
        };
    }

    let mut bytes = Vec::new();
    let _ = file.take(TAIL_BYTE_CAP).read_to_end(&mut bytes);
    let buf = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = buf.lines().collect();
    // When we started mid-file, the first line is a partial fragment — drop it.
    if read_from > 0 && !all.is_empty() {
        all.remove(0);
    }
    let start = all.len().saturating_sub(n);
    LogStream {
        path,
        present: true,
        lines: all[start..].iter().map(|s| s.to_string()).collect(),
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
