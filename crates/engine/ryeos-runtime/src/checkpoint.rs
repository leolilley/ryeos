//! `CheckpointWriter` — persistent, restart-safe state for replay-aware
//! (`native_resume`) tools.
//!
//! The daemon allocates a per-thread checkpoint directory under
//! `<config.app_root>/threads/<thread_id>/checkpoints/` at spawn time
//! (when the spec declares `runtime.handlers.native_resume`) and
//! injects its path as the `RYEOS_CHECKPOINT_DIR` env var. Tools call
//! `CheckpointWriter::from_env()` to attach to that directory and
//! periodically `write()` their replay state to it; on daemon restart
//! the resume path re-spawns the tool with `RYEOS_RESUME=1` and the same
//! `RYEOS_CHECKPOINT_DIR`, and the tool calls `load_latest()` to recover.
//!
//! Atomicity: every `write` goes to `latest.json.tmp.<pid>.<rand>`
//! first, then `rename()`s into place. A crash during write therefore
//! never leaves a partial `latest.json`.
//!
//! Checkpoint payloads are schema-agnostic JSON, but they share the runtime
//! expression language's depth/node/byte shape ceiling. That common boundary
//! applies before persistence, after daemon-owned follow-result splicing, and
//! while loading so every checkpoint path accepts the same bounded domain.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EvaluationContext, EvaluationLimits, EvaluationSession, ExpressionError};

const LATEST_FILE: &str = "latest.json";

/// Maximum serialized size of one checkpoint file. Checkpoints are written as
/// compact JSON, so this is the same four-MiB ceiling enforced by the runtime
/// JSON shape contract rather than a second, whitespace-dependent allowance.
pub const MAX_CHECKPOINT_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Runtime JSON limits with enough inspection fuel to visit every accepted
/// node and byte once. Shape validation is not expression evaluation, so a
/// checkpoint near the result ceiling must not fail merely because the normal
/// expression-evaluation fuel budget is smaller.
pub fn checkpoint_shape_limits() -> EvaluationLimits {
    let defaults = EvaluationLimits::default();
    let fuel = defaults
        .max_result_bytes
        .saturating_add(defaults.max_result_nodes)
        .saturating_add(1);
    EvaluationLimits { fuel, ..defaults }
}

/// Validate a borrowed checkpoint or checkpoint-bound envelope without
/// cloning it. Graph persistence, graph resume, and daemon follow aggregation
/// all call this contract so their accepted JSON domain cannot diverge.
pub fn validate_checkpoint_shape(
    value: &Value,
    field: &str,
) -> std::result::Result<(), ExpressionError> {
    let context = EvaluationContext::new();
    let limits = checkpoint_shape_limits();
    EvaluationSession::with_context(&context, &limits).validate_value(value, field)
}

fn read_checkpoint_json(path: &Path) -> Result<Value> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let declared_len = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?
        .len();
    if declared_len > MAX_CHECKPOINT_FILE_BYTES as u64 {
        bail!(
            "checkpoint {} is {declared_len} bytes; maximum is {MAX_CHECKPOINT_FILE_BYTES}",
            path.display()
        );
    }

    // The metadata check avoids an unnecessary read for an already-large file;
    // `take(MAX + 1)` is the authoritative cap if the file grows after that
    // check. Never reserve from attacker-controlled metadata above the limit.
    let capacity = usize::try_from(declared_len)
        .unwrap_or(MAX_CHECKPOINT_FILE_BYTES)
        .min(MAX_CHECKPOINT_FILE_BYTES);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_CHECKPOINT_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() > MAX_CHECKPOINT_FILE_BYTES {
        bail!(
            "checkpoint {} exceeds the {MAX_CHECKPOINT_FILE_BYTES}-byte maximum",
            path.display()
        );
    }

    serde_json::from_slice(&bytes).with_context(|| format!("parse checkpoint {}", path.display()))
}

/// The top-level checkpoint field the follow machinery splices a followed child's
/// terminal envelope into, and that a resuming graph walker reads to consume it.
/// It lives here — the shared checkpoint crate both the daemon (which splices, via
/// [`CheckpointWriter::copy_latest_with_splice`]) and the graph runtime (which
/// reads) depend on — so the wire key has ONE definition, not a literal duplicated
/// across crates that cannot see each other.
pub const FOLLOW_RESULT_KEY: &str = "follow_result";

/// Closed status domain for one child in a daemon-built follow-fanout resume
/// payload. The executor writes this shared wire type and the graph runtime
/// deserializes the same type; neither side compares status strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanoutItemStatus {
    Completed,
    Failed,
}

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
    /// `RYEOS_CHECKPOINT_DIR` env var. Returns `None` when the env is
    /// unset, which means the tool was not launched with `native_resume`
    /// (or is running outside the daemon entirely — e.g. unit tests).
    pub fn from_env() -> Option<Self> {
        std::env::var("RYEOS_CHECKPOINT_DIR")
            .ok()
            .map(|s| Self::new(PathBuf::from(s)))
    }

    /// True iff the daemon launched this run as a resume (`RYEOS_RESUME=1`).
    /// Tools should check this on startup and `load_latest()` if true.
    pub fn is_resume() -> bool {
        std::env::var("RYEOS_RESUME").ok().as_deref() == Some("1")
    }

    /// Copy the latest checkpoint from `from_dir` into `to_dir` — used by the
    /// daemon to seed a continuation successor's checkpoint dir from its
    /// predecessor's, so the successor's `load_latest()` resumes mid-run.
    /// Returns `Ok(true)` if a checkpoint was found and copied, `Ok(false)` if
    /// the source dir has none.
    pub fn copy_latest(from_dir: &Path, to_dir: &Path) -> Result<bool> {
        let src = from_dir.join(LATEST_FILE);
        if !src.exists() {
            return Ok(false);
        }
        std::fs::create_dir_all(to_dir).with_context(|| format!("create {}", to_dir.display()))?;
        std::fs::copy(&src, to_dir.join(LATEST_FILE))
            .with_context(|| format!("copy {} -> {}", src.display(), to_dir.display()))?;
        Ok(true)
    }

    /// Copy the latest checkpoint from `from_dir` into `to_dir`, splicing an extra
    /// top-level `key: value` into the copied payload (atomically). The follow-
    /// resume launcher uses this to seed a suspended parent's successor with the
    /// parent's checkpoint PLUS the followed child's terminal envelope, so the
    /// resumed walker consumes the result at the follow node instead of
    /// re-suspending. Returns `Ok(false)` if `from_dir` has no checkpoint.
    ///
    /// This is the ONE place the daemon reads a checkpoint payload; it stays a
    /// shallow top-level object merge, never a schema-aware transform.
    pub fn copy_latest_with_splice(
        from_dir: &Path,
        to_dir: &Path,
        key: &str,
        value: Value,
    ) -> Result<bool> {
        let src = from_dir.join(LATEST_FILE);
        if !src.exists() {
            return Ok(false);
        }
        let mut payload = read_checkpoint_json(&src)?;
        let obj = payload
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("checkpoint {} is not a JSON object", src.display()))?;
        obj.insert(key.to_string(), value);
        validate_checkpoint_shape(&payload, "spliced checkpoint payload").map_err(|error| {
            anyhow::anyhow!("spliced checkpoint payload exceeded runtime JSON bounds: {error}")
        })?;
        // Atomic write into the successor's dir via the same tmp+rename path.
        Self::new(to_dir.to_path_buf()).write(&payload)?;
        Ok(true)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Atomically replace `latest.json` with the serialized `state`.
    pub fn write(&self, state: &Value) -> Result<()> {
        validate_checkpoint_shape(state, "checkpoint payload").map_err(|error| {
            anyhow::anyhow!("checkpoint payload exceeded runtime JSON bounds: {error}")
        })?;
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create checkpoint dir {}", self.dir.display()))?;
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
        let bytes = serde_json::to_vec(state).context("serialize checkpoint payload")?;
        if bytes.len() > MAX_CHECKPOINT_FILE_BYTES {
            bail!(
                "serialized checkpoint is {} bytes; maximum is {MAX_CHECKPOINT_FILE_BYTES}",
                bytes.len()
            );
        }
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
        let value = read_checkpoint_json(&path)?;
        validate_checkpoint_shape(&value, "checkpoint payload").map_err(|error| {
            anyhow::anyhow!("checkpoint payload exceeded runtime JSON bounds: {error}")
        })?;
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
    fn copy_latest_seeds_successor_dir_and_reports_absence() {
        let from = TempDir::new().unwrap();
        let to = TempDir::new().unwrap();
        // No checkpoint in the source yet → nothing copied.
        assert!(!CheckpointWriter::copy_latest(from.path(), to.path()).unwrap());
        // Write one, copy it forward, and confirm the destination resumes it.
        CheckpointWriter::new(from.path())
            .write(&json!({"node": "b", "step": 2}))
            .unwrap();
        assert!(CheckpointWriter::copy_latest(from.path(), to.path()).unwrap());
        let loaded = CheckpointWriter::new(to.path())
            .load_latest()
            .unwrap()
            .unwrap();
        assert_eq!(loaded["node"], "b");
        assert_eq!(loaded["step"], 2);
    }

    #[test]
    fn copy_latest_with_splice_merges_follow_result_into_copy() {
        let from = TempDir::new().unwrap();
        let to = TempDir::new().unwrap();
        // No source checkpoint → nothing spliced.
        assert!(!CheckpointWriter::copy_latest_with_splice(
            from.path(),
            to.path(),
            FOLLOW_RESULT_KEY,
            json!({"ignored": true})
        )
        .unwrap());

        // The parent's checkpoint carries its own cursor; the splice adds the child
        // result under FOLLOW_RESULT_KEY without disturbing the rest.
        CheckpointWriter::new(from.path())
            .write(&json!({"node": "await", "step": 7}))
            .unwrap();
        let child_env = json!({"success": true, "outputs": {"answer": 42}});
        assert!(CheckpointWriter::copy_latest_with_splice(
            from.path(),
            to.path(),
            FOLLOW_RESULT_KEY,
            child_env.clone()
        )
        .unwrap());

        let resumed = CheckpointWriter::new(to.path())
            .load_latest()
            .unwrap()
            .unwrap();
        assert_eq!(resumed["node"], "await");
        assert_eq!(resumed["step"], 7);
        assert_eq!(resumed[FOLLOW_RESULT_KEY], child_env);
        // The source is untouched — the splice only writes the destination.
        assert!(CheckpointWriter::new(from.path())
            .load_latest()
            .unwrap()
            .unwrap()
            .get(FOLLOW_RESULT_KEY)
            .is_none());
    }

    #[test]
    fn copy_latest_with_splice_rejects_non_object_checkpoint() {
        let from = TempDir::new().unwrap();
        let to = TempDir::new().unwrap();
        CheckpointWriter::new(from.path())
            .write(&json!([1, 2, 3]))
            .unwrap();
        // A non-object payload has no top level to splice into — an error, not a
        // silent drop of the child result.
        assert!(CheckpointWriter::copy_latest_with_splice(
            from.path(),
            to.path(),
            FOLLOW_RESULT_KEY,
            json!({})
        )
        .is_err());
    }

    #[test]
    fn load_latest_rejects_oversized_file_before_parsing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LATEST_FILE);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CHECKPOINT_FILE_BYTES as u64 + 1).unwrap();

        let error = CheckpointWriter::new(tmp.path()).load_latest().unwrap_err();
        assert!(error.to_string().contains("maximum"));
        assert!(error
            .to_string()
            .contains(&MAX_CHECKPOINT_FILE_BYTES.to_string()));
    }

    #[test]
    fn splice_rejects_combined_payload_over_shape_limit() {
        let from = TempDir::new().unwrap();
        let to = TempDir::new().unwrap();
        CheckpointWriter::new(from.path())
            .write(&json!({"parts": vec![Value::Null; 99_990]}))
            .unwrap();

        let error = CheckpointWriter::copy_latest_with_splice(
            from.path(),
            to.path(),
            FOLLOW_RESULT_KEY,
            Value::Array(vec![Value::Null; 16]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("JSON node limit"));
        assert!(!to.path().join(LATEST_FILE).exists());
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
        let prev = std::env::var("RYEOS_CHECKPOINT_DIR").ok();
        std::env::remove_var("RYEOS_CHECKPOINT_DIR");
        let w = CheckpointWriter::from_env();
        assert!(w.is_none());
        if let Some(v) = prev {
            std::env::set_var("RYEOS_CHECKPOINT_DIR", v);
        }
    }

    #[test]
    fn is_resume_reads_env_flag() {
        let prev = std::env::var("RYEOS_RESUME").ok();
        std::env::set_var("RYEOS_RESUME", "1");
        assert!(CheckpointWriter::is_resume());
        std::env::set_var("RYEOS_RESUME", "0");
        assert!(!CheckpointWriter::is_resume());
        std::env::remove_var("RYEOS_RESUME");
        assert!(!CheckpointWriter::is_resume());
        if let Some(v) = prev {
            std::env::set_var("RYEOS_RESUME", v);
        }
    }
}
