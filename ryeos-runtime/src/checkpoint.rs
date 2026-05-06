//! `CheckpointWriter` — persistent, restart-safe state for replay-aware
//! (`native_resume`) tools.
//!
//! The daemon allocates a per-thread checkpoint directory under
//! `<config.system_space_dir>/threads/<thread_id>/checkpoints/` at spawn time
//! (when the spec declares `runtime.handlers.native_resume`) and
//! injects its path as the `RYE_CHECKPOINT_DIR` env var. Tools call
//! `CheckpointWriter::from_env()` to attach to that directory and
//! periodically `write()` their replay state to it; on daemon restart
//! the resume path re-spawns the tool with `RYE_RESUME=1` and the same
//! `RYE_CHECKPOINT_DIR`, and the tool calls `load_latest()` to recover.
//!
//! Atomicity: every `write` goes to `latest.json.tmp.<pid>.<rand>`
//! first, then `rename()`s into place. A crash during write therefore
//! never leaves a partial `latest.json`.
//!
//! This primitive intentionally has no schema knowledge — `write`
//! takes any `serde_json::Value`. Tools are responsible for their own
//! payload shape and migration; the daemon never reads the file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

const LATEST_FILE: &str = "latest.json";

#[derive(Debug, Clone)]
pub struct CheckpointWriter {
    dir: PathBuf,
}

impl CheckpointWriter {
    /// Construct directly against an explicit directory. The directory
    /// is created on first `write` if it does not already exist.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Attach to the daemon-allocated checkpoint dir via the
    /// `RYE_CHECKPOINT_DIR` env var. Returns `None` when the env is
    /// unset, which means the tool was not launched with `native_resume`
    /// (or is running outside the daemon entirely — e.g. unit tests).
    pub fn from_env() -> Option<Self> {
        std::env::var("RYE_CHECKPOINT_DIR")
            .ok()
            .map(|s| Self::new(PathBuf::from(s)))
    }

    /// True iff the daemon launched this run as a resume (`RYE_RESUME=1`).
    /// Tools should check this on startup and `load_latest()` if true.
    pub fn is_resume() -> bool {
        std::env::var("RYE_RESUME").ok().as_deref() == Some("1")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Atomically replace `latest.json` with the serialized `state`.
    pub fn write(&self, state: &Value) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| {
            format!("create checkpoint dir {}", self.dir.display())
        })?;
        let final_path = self.dir.join(LATEST_FILE);
        // Unique suffix from pid + monotonic nanos avoids pulling a
        // `rand` dep just for a temp filename.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_path = self.dir.join(format!(
            "{LATEST_FILE}.tmp.{}.{}",
            std::process::id(),
            nanos
        ));
        let bytes =
            serde_json::to_vec_pretty(state).context("serialize checkpoint payload")?;
        std::fs::write(&tmp_path, &bytes)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &final_path).with_context(|| {
            format!(
                "atomic rename {} -> {}",
                tmp_path.display(),
                final_path.display()
            )
        })?;
        Ok(())
    }

    /// Read the most recent successful `write` payload, if any.
    /// Returns `None` if the file does not exist (first run, no
    /// checkpoint yet) or the directory does not exist.
    pub fn load_latest(&self) -> Result<Option<Value>> {
        let path = self.dir.join(LATEST_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse checkpoint {}", path.display()))?;
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn write_then_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let w = CheckpointWriter::new(tmp.path());
        let payload = json!({"step": 3, "buffer": [1, 2, 3]});
        w.write(&payload).unwrap();
        assert_eq!(w.load_latest().unwrap(), Some(payload));
    }

    #[test]
    fn load_latest_returns_none_when_no_checkpoint_yet() {
        let tmp = TempDir::new().unwrap();
        let w = CheckpointWriter::new(tmp.path());
        assert!(w.load_latest().unwrap().is_none());
    }

    #[test]
    fn load_latest_returns_none_when_dir_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let w = CheckpointWriter::new(tmp.path().join("nope"));
        assert!(w.load_latest().unwrap().is_none());
    }

    #[test]
    fn write_creates_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let w = CheckpointWriter::new(tmp.path().join("a/b/c"));
        w.write(&json!({"x": 1})).unwrap();
        assert!(tmp.path().join("a/b/c/latest.json").exists());
    }

    #[test]
    fn write_replaces_previous_value_atomically() {
        let tmp = TempDir::new().unwrap();
        let w = CheckpointWriter::new(tmp.path());
        w.write(&json!({"v": 1})).unwrap();
        w.write(&json!({"v": 2})).unwrap();
        w.write(&json!({"v": 3})).unwrap();
        assert_eq!(w.load_latest().unwrap(), Some(json!({"v": 3})));
        // No leftover temp files.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().to_string_lossy().to_string();
                if name.starts_with("latest.json.tmp.") {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
    }

    #[test]
    fn from_env_returns_none_without_var() {
        // SAFELY isolate from any caller env.
        let prev = std::env::var("RYE_CHECKPOINT_DIR").ok();
        std::env::remove_var("RYE_CHECKPOINT_DIR");
        let w = CheckpointWriter::from_env();
        assert!(w.is_none());
        if let Some(v) = prev {
            std::env::set_var("RYE_CHECKPOINT_DIR", v);
        }
    }

    #[test]
    fn is_resume_reads_env_flag() {
        let prev = std::env::var("RYE_RESUME").ok();
        std::env::set_var("RYE_RESUME", "1");
        assert!(CheckpointWriter::is_resume());
        std::env::set_var("RYE_RESUME", "0");
        assert!(!CheckpointWriter::is_resume());
        std::env::remove_var("RYE_RESUME");
        assert!(!CheckpointWriter::is_resume());
        if let Some(v) = prev {
            std::env::set_var("RYE_RESUME", v);
        }
    }
}
