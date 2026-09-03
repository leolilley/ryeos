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
//! Durability: every `write` goes through the shared `lillux` durable
//! atomic-replace primitive — write a temp sibling, `fsync` it, `rename()`
//! into place, then `fsync` the containing directory. A crash never leaves a
//! partial `latest.json`, and a committed checkpoint survives a power loss or
//! kernel panic, not only a process crash. This is the same barrier vault
//! secrets and bundle journals rely on; the checkpoint is the most
//! durability-critical write in the runtime and must not be weaker.
//!
//! Checkpoint payloads are schema-agnostic JSON, but they share the runtime
//! expression language's depth/node/byte shape ceiling. That common boundary
//! applies before persistence, after daemon-owned follow-result splicing, and
//! while loading so every checkpoint path accepts the same bounded domain.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EvaluationContext, EvaluationLimits, EvaluationSession, ExpressionError};

const LATEST_FILE: &str = "latest.json";

/// Maximum serialized size of one checkpoint file. Checkpoints are written as
/// compact JSON, so this is the same four-MiB ceiling enforced by the runtime
/// JSON shape contract rather than a second, whitespace-dependent allowance.
pub const MAX_CHECKPOINT_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Runtime JSON limits for checkpoint shape inspection. Result validation has
/// an allowance derived from the byte/node ceilings independently of
/// expression-computation fuel, so a checkpoint near the result ceiling does
/// not fail merely because the normal expression budget is smaller.
pub fn checkpoint_shape_limits() -> EvaluationLimits {
    EvaluationLimits::default()
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

fn read_checkpoint_bytes(directory: &lillux::PinnedDirectory) -> Result<Option<Vec<u8>>> {
    let Some(file) = directory
        .open_pinned_regular(Path::new(LATEST_FILE).as_os_str(), false)
        .with_context(|| format!("open checkpoint in {}", directory.path().display()))?
    else {
        return Ok(None);
    };
    file.read_bounded(MAX_CHECKPOINT_FILE_BYTES as u64)
        .map(Some)
        .map_err(|error| {
            anyhow::anyhow!(
                "read checkpoint {} under {MAX_CHECKPOINT_FILE_BYTES}-byte maximum: {error:#}",
                file.path().display()
            )
        })
}

fn read_checkpoint_json(directory: &lillux::PinnedDirectory) -> Result<Option<Value>> {
    let Some(bytes) = read_checkpoint_bytes(directory)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("parse checkpoint in {}", directory.path().display()))
}

fn publish_checkpoint_bytes(directory: &lillux::PinnedDirectory, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_CHECKPOINT_FILE_BYTES {
        bail!(
            "serialized checkpoint is {} bytes; maximum is {MAX_CHECKPOINT_FILE_BYTES}",
            bytes.len()
        );
    }
    let incumbent = directory
        .open_pinned_regular(Path::new(LATEST_FILE).as_os_str(), false)
        .with_context(|| format!("pin incumbent checkpoint in {}", directory.path().display()))?;
    directory
        .atomic_write_pinned_if_same(
            Path::new(LATEST_FILE).as_os_str(),
            incumbent.as_ref(),
            bytes,
            0o600,
        )
        .with_context(|| format!("publish checkpoint in {}", directory.path().display()))
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
        Self::from_checkpoint_dir(std::env::var_os("RYEOS_CHECKPOINT_DIR"))
    }

    fn from_checkpoint_dir(dir: Option<impl Into<PathBuf>>) -> Option<Self> {
        dir.map(Self::new)
    }

    /// True iff the daemon launched this run as a resume (`RYEOS_RESUME=1`).
    /// Tools should check this on startup and `load_latest()` if true.
    pub fn is_resume() -> bool {
        Self::is_resume_value(std::env::var("RYEOS_RESUME").ok().as_deref())
    }

    fn is_resume_value(value: Option<&str>) -> bool {
        value == Some("1")
    }

    /// Copy the latest checkpoint from `from_dir` into `to_dir` — used by the
    /// daemon to seed a continuation successor's checkpoint dir from its
    /// predecessor's, so the successor's `load_latest()` resumes mid-run.
    /// Returns `Ok(true)` if a checkpoint was found and copied, `Ok(false)` if
    /// the source dir has none.
    pub fn copy_latest(from_dir: &Path, to_dir: &Path) -> Result<bool> {
        let Some(from) = lillux::PinnedDirectory::open(from_dir)
            .with_context(|| format!("pin checkpoint source {}", from_dir.display()))?
        else {
            return Ok(false);
        };
        let to = lillux::PinnedDirectory::open_or_create(to_dir)
            .with_context(|| format!("pin checkpoint destination {}", to_dir.display()))?;
        Self::copy_latest_pinned(&from, &to)
    }

    /// Copy through exact descriptor-rooted source and destination authorities.
    pub fn copy_latest_pinned(
        from: &lillux::PinnedDirectory,
        to: &lillux::PinnedDirectory,
    ) -> Result<bool> {
        let Some(bytes) = read_checkpoint_bytes(from)? else {
            return Ok(false);
        };
        publish_checkpoint_bytes(to, &bytes)?;
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
        let Some(from) = lillux::PinnedDirectory::open(from_dir)
            .with_context(|| format!("pin checkpoint source {}", from_dir.display()))?
        else {
            return Ok(false);
        };
        let to = lillux::PinnedDirectory::open_or_create(to_dir)
            .with_context(|| format!("pin checkpoint destination {}", to_dir.display()))?;
        Self::copy_latest_with_splice_pinned(&from, &to, key, value)
    }

    pub fn copy_latest_with_splice_pinned(
        from: &lillux::PinnedDirectory,
        to: &lillux::PinnedDirectory,
        key: &str,
        value: Value,
    ) -> Result<bool> {
        let Some(mut payload) = read_checkpoint_json(from)? else {
            return Ok(false);
        };
        let obj = payload.as_object_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "checkpoint in {} is not a JSON object",
                from.path().display()
            )
        })?;
        obj.insert(key.to_string(), value);
        validate_checkpoint_shape(&payload, "spliced checkpoint payload").map_err(|error| {
            anyhow::anyhow!("spliced checkpoint payload exceeded runtime JSON bounds: {error}")
        })?;
        let bytes = serde_json::to_vec(&payload).context("serialize spliced checkpoint payload")?;
        publish_checkpoint_bytes(to, &bytes)?;
        Ok(true)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Durably and atomically replace `latest.json` with the serialized
    /// `state`. Routes through the shared `lillux` durable atomic-replace
    /// primitive (temp + file `fsync` + `rename` + directory `fsync`), so a
    /// resume after a power loss or kernel panic — not only a process crash —
    /// sees the committed checkpoint rather than an older one. The primitive
    /// creates and cleans up its own uniquely named temp sibling.
    pub fn write(&self, state: &Value) -> Result<()> {
        validate_checkpoint_shape(state, "checkpoint payload").map_err(|error| {
            anyhow::anyhow!("checkpoint payload exceeded runtime JSON bounds: {error}")
        })?;
        let directory = lillux::PinnedDirectory::open_or_create(&self.dir)
            .with_context(|| format!("pin checkpoint dir {}", self.dir.display()))?;
        let bytes = serde_json::to_vec(state).context("serialize checkpoint payload")?;
        publish_checkpoint_bytes(&directory, &bytes)?;
        Ok(())
    }

    /// Read the most recent successful `write` payload, if any.
    /// Returns `None` if the file does not exist (first run, no
    /// checkpoint yet) or the directory does not exist.
    pub fn load_latest(&self) -> Result<Option<Value>> {
        let Some(directory) = lillux::PinnedDirectory::open(&self.dir)
            .with_context(|| format!("pin checkpoint dir {}", self.dir.display()))?
        else {
            return Ok(None);
        };
        let Some(value) = read_checkpoint_json(&directory)? else {
            return Ok(None);
        };
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

    #[cfg(unix)]
    #[test]
    fn copy_latest_rejects_symlinked_source_checkpoint() {
        use std::os::unix::fs::symlink;

        let from = TempDir::new().unwrap();
        let to = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), b"{\"secret\":true}").unwrap();
        symlink(outside.path().join("secret"), from.path().join(LATEST_FILE)).unwrap();

        assert!(CheckpointWriter::copy_latest(from.path(), to.path()).is_err());
        assert!(!to.path().join(LATEST_FILE).exists());
    }

    #[test]
    fn copy_latest_with_splice_merges_follow_result_into_copy() {
        let from = TempDir::new().unwrap();
        let to = TempDir::new().unwrap();
        // No source checkpoint → nothing spliced.
        assert!(
            !CheckpointWriter::copy_latest_with_splice(
                from.path(),
                to.path(),
                FOLLOW_RESULT_KEY,
                json!({"ignored": true})
            )
            .unwrap()
        );

        // The parent's checkpoint carries its own cursor; the splice adds the child
        // result under FOLLOW_RESULT_KEY without disturbing the rest.
        CheckpointWriter::new(from.path())
            .write(&json!({"node": "await", "step": 7}))
            .unwrap();
        let child_env = json!({"success": true, "outputs": {"answer": 42}});
        assert!(
            CheckpointWriter::copy_latest_with_splice(
                from.path(),
                to.path(),
                FOLLOW_RESULT_KEY,
                child_env.clone()
            )
            .unwrap()
        );

        let resumed = CheckpointWriter::new(to.path())
            .load_latest()
            .unwrap()
            .unwrap();
        assert_eq!(resumed["node"], "await");
        assert_eq!(resumed["step"], 7);
        assert_eq!(resumed[FOLLOW_RESULT_KEY], child_env);
        // The source is untouched — the splice only writes the destination.
        assert!(
            CheckpointWriter::new(from.path())
                .load_latest()
                .unwrap()
                .unwrap()
                .get(FOLLOW_RESULT_KEY)
                .is_none()
        );
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
        assert!(
            CheckpointWriter::copy_latest_with_splice(
                from.path(),
                to.path(),
                FOLLOW_RESULT_KEY,
                json!({})
            )
            .is_err()
        );
    }

    #[test]
    fn load_latest_rejects_oversized_file_before_parsing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LATEST_FILE);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CHECKPOINT_FILE_BYTES as u64 + 1).unwrap();

        let error = CheckpointWriter::new(tmp.path()).load_latest().unwrap_err();
        assert!(error.to_string().contains("maximum"));
        assert!(
            error
                .to_string()
                .contains(&MAX_CHECKPOINT_FILE_BYTES.to_string())
        );
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
        let w = CheckpointWriter::from_checkpoint_dir(None::<PathBuf>);
        assert!(w.is_none());
    }

    #[test]
    fn is_resume_reads_env_flag() {
        assert!(CheckpointWriter::is_resume_value(Some("1")));
        assert!(!CheckpointWriter::is_resume_value(Some("0")));
        assert!(!CheckpointWriter::is_resume_value(None));
    }
}
