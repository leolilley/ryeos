use std::path::{Component, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::state::AppState;

/// Typed error for `resolve_project_context`. Replaces the prior
/// anyhow-only return so `api/execute.rs` can pattern-match the
/// failure mode instead of substring-matching `Display`. The variants
/// map 1:1 to the typed `DispatchError` variants the HTTP layer
/// returns (`ProjectSourcePushFirst` → 409, `CheckoutFailed` →
/// 502, `Other` → 500).
#[derive(Debug, Error)]
pub enum ProjectSourceError {
    /// No CAS HEAD pushed for this project. The Display preserves
    /// V5.2's exact wording so `dispatch_pin.rs::pin_native_runtime_with_pushed_head`
    /// continues to hold byte-identically. The path string is
    /// embedded inline.
    #[error("no pushed HEAD for project '{project_path}' — push first")]
    PushFirst { project_path: String },
    /// CAS checkout / snapshot fetch failed for an existing pushed
    /// HEAD. Carries the underlying detail so operators can see what
    /// went wrong (snapshot not in CAS, materialization race, etc.).
    #[error("project source checkout failed: {0}")]
    CheckoutFailed(String),
    /// Anything else that goes wrong while resolving the project
    /// source — internal state-store errors, manifest parsing failures,
    /// etc. Mapped to HTTP 500 by the API layer.
    #[error("project source resolution failed: {0}")]
    Other(String),
}

impl From<anyhow::Error> for ProjectSourceError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err.to_string())
    }
}

/// RAII cleanup for the optional checkout-derived tempdir produced by
/// [`resolve_project_context`]. Created when `temp_dir` is `Some`
/// (i.e. for `pushed_head` / snapshot project sources), it removes the
/// directory when it goes out of scope. Idempotent — [`Self::disarm`]
/// consumes the guard without removing the directory if you need to
/// hand the path to a long-running detached owner.
pub(crate) struct TempDirGuard(Option<PathBuf>);

impl TempDirGuard {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self(path)
    }

    /// Disarm the guard (consume without cleanup). Returns the path so
    /// callers can hand it to a runner / detached thread that takes
    /// over lifecycle ownership.
    pub(crate) fn disarm(mut self) -> Option<PathBuf> {
        self.0.take()
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

/// How the project root is determined for execution.
///
/// Tagged enum — callers specify `{ "kind": "live_fs" }` or
/// `{ "kind": "pushed_head" }`. Extensible to future variants
/// like `snapshot` or `named_ref`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[derive(Default)]
pub enum ProjectSource {
    /// Execute against the live filesystem project path.
    #[default]
    LiveFs,
    /// Resolve the acting principal's HEAD ref for the project
    /// and checkout from CAS.
    PushedHead,
}


/// Request-scoped project execution context.
///
/// Built BEFORE item resolution to ensure the engine resolves, verifies,
/// and plans against the correct project root. For `PushedHead`, this
/// includes CAS checkout into a temp directory.
///
/// Ownership of `temp_dir` is transferred to `ExecutionGuard` in the
/// runner for lifecycle management.
#[derive(Debug)]
pub struct ResolvedProjectContext {
    /// The path to resolve and execute against (may be a CAS checkout dir).
    pub effective_path: PathBuf,
    /// The original project_path from the request (used for ref lookup, fold-back).
    pub original_path: PathBuf,
    /// Which source mode was used.
    pub source: ProjectSource,
    /// CAS snapshot hash (set for PushedHead).
    pub snapshot_hash: Option<String>,
    /// Temp directory to clean up (CAS checkout dir for PushedHead).
    pub temp_dir: Option<PathBuf>,
}

/// Resolve a `ProjectSource` into a concrete execution context.
///
/// For `LiveFs`: returns the project path as-is.
/// For `PushedHead`: resolves the principal's HEAD ref and checks out
/// from CAS into a temp execution directory.
///
/// The `checkout_id` is used to name the temp directory (typically a
/// request ID or similar unique identifier).
pub fn resolve_project_context(
    state: &AppState,
    source: &ProjectSource,
    project_path: &std::path::Path,
    _principal_id: &str,
    checkout_id: &str,
) -> Result<ResolvedProjectContext, ProjectSourceError> {
    let original_path = project_path.to_path_buf();

    let ctx = match source {
        ProjectSource::LiveFs => ResolvedProjectContext {
            effective_path: original_path.clone(),
            original_path,
            source: source.clone(),
            snapshot_hash: None,
            temp_dir: None,
        },
        ProjectSource::PushedHead => {
            let project_str = project_path.to_string_lossy();
            let project_hash = lillux::cas::sha256_hex(project_str.as_bytes());
            let cas_root = state.state_store.cas_root()?;
            let cas = lillux::cas::CasStore::new(cas_root.clone());

            let snap_hash = state
                .state_store
                .with_state_db(|db| db.read_project_head(&project_hash))?
                .ok_or_else(|| ProjectSourceError::PushFirst {
                    project_path: project_str.to_string(),
                })?;

            let snap_obj = cas
                .get_object(&snap_hash)
                .map_err(|e| ProjectSourceError::CheckoutFailed(e.to_string()))?
                .ok_or_else(|| {
                    ProjectSourceError::CheckoutFailed(format!(
                        "snapshot {} not found in CAS",
                        snap_hash
                    ))
                })?;
            let snapshot = ryeos_state::objects::ProjectSnapshot::from_value(&snap_obj)
                .map_err(|e| ProjectSourceError::CheckoutFailed(e.to_string()))?;

            let manifest_hash = &snapshot.project_manifest_hash;
            let exec_dir = state
                .config
                .state_dir
                .join("executions")
                .join(checkout_id);
            let cache = crate::execution::cache::MaterializationCache::new(
                state.config.state_dir.join("cache").join("snapshots"),
            );
            crate::execution::checkout_project(&cas_root, manifest_hash, &exec_dir, Some(&cache))
                .map_err(|e| ProjectSourceError::CheckoutFailed(e.to_string()))?;

            ResolvedProjectContext {
                effective_path: exec_dir.clone(),
                original_path,
                source: source.clone(),
                snapshot_hash: Some(snap_hash.clone()),
                temp_dir: Some(exec_dir),
            }
        }
    };

    tracing::info!(
        source = ?ctx.source,
        effective_path = %ctx.effective_path.display(),
        original_path = %ctx.original_path.display(),
        has_temp_dir = ctx.temp_dir.is_some(),
        "resolved project execution context"
    );

    Ok(ctx)
}

/// Normalize a project path for use as a stable identity key.
///
/// - Makes relative paths absolute (via current_dir)
/// - Lexically resolves `.` and `..` components
/// - Strips trailing separators (except root `/`)
///
/// Does NOT call `std::fs::canonicalize` to avoid resolving symlinks
/// and requiring filesystem access (paths are used as ref keys, not
/// just filesystem lookups).
pub fn normalize_project_path(raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);

    // Make absolute if relative
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };

    // Lexically clean: resolve `.` and `..` without filesystem access
    let mut cleaned = PathBuf::new();
    for component in abs.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                cleaned.pop();
            }
            other => cleaned.push(other),
        }
    }

    // Ensure we don't return empty
    if cleaned.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        cleaned
    }
}
