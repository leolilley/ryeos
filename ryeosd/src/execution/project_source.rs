use std::path::{Component, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// How the project root is determined for execution.
///
/// Tagged enum — callers specify `{ "kind": "live_fs" }` or
/// `{ "kind": "pushed_head" }`. Extensible to future variants
/// like `snapshot` or `named_ref`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectSource {
    /// Execute against the live filesystem project path.
    LiveFs,
    /// Resolve the acting principal's HEAD ref for the project
    /// and checkout from CAS.
    PushedHead,
}

impl Default for ProjectSource {
    fn default() -> Self {
        Self::LiveFs
    }
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
    principal_id: &str,
    checkout_id: &str,
) -> Result<ResolvedProjectContext> {
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
            let project_ref = state
                .refs_store()
                .resolve_project_ref(principal_id, &project_str)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no pushed HEAD for project '{}' — push first",
                        project_str
                    )
                })?;

            let snap_hash = &project_ref.snapshot_hash;
            let cas = state.cas_store();
            let snap_obj = cas
                .get_object(snap_hash)?
                .ok_or_else(|| anyhow::anyhow!("snapshot {} not found in CAS", snap_hash))?;
            let snapshot = crate::cas::ProjectSnapshot::from_json(&snap_obj)?;

            let manifest_hash = &snapshot.project_manifest_hash;
            let exec_dir = state
                .config
                .state_dir
                .join("executions")
                .join(checkout_id);
            let cache = crate::execution::cache::MaterializationCache::new(
                state.config.state_dir.join("cache").join("snapshots"),
            );
            crate::execution::checkout_project(cas, manifest_hash, &exec_dir, Some(&cache))?;

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
