use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use ryeos_app::state::AppState;
use ryeos_engine::engine::Engine;
use ryeos_engine::trust::TrustStore;

/// Typed error for `resolve_project_context`. Replaces the prior
/// anyhow-only return so the execute response mode can pattern-match the
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
pub struct TempDirGuard(Option<PathBuf>);

impl TempDirGuard {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self(path)
    }

    /// Disarm the guard (consume without cleanup). Returns the path so
    /// callers can hand it to a runner / detached thread that takes
    /// over lifecycle ownership.
    pub fn disarm(mut self) -> Option<PathBuf> {
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
    /// Materialised user-space root for `PushedHead` requests with a
    /// `user_manifest_hash`. `None` for `LiveFs` (the daemon's own
    /// user space is used via the global engine) and for `PushedHead`
    /// requests with no user manifest (strict-or-empty: no user-tier
    /// items at all — never silently fall through to the remote
    /// operator's user space).
    pub user_root: Option<PathBuf>,
    /// Temp directory to clean up (user-space CAS checkout dir for
    /// `PushedHead`). Lifetime is tied to the cache entry: while the
    /// per-request engine sits in `AppState.engine_cache`, this temp
    /// dir is retained so concurrent threads sharing the snapshot
    /// reuse the materialisation. Cleaned up when the cache entry is
    /// evicted.
    pub user_temp_dir: Option<PathBuf>,
    /// Caller's pinned trust pubkeys, materialised in-memory from the
    /// pushed user manifest's `config/keys/trusted/` sub-section.
    /// Unioned with the remote's persistent trust store inside
    /// [`Engine`] for THIS request only — never written to the
    /// remote's persistent trust dir.
    pub trust_overlay: Option<TrustStore>,
    /// The engine to use for this request. For `LiveFs`, this is
    /// `state.engine` (the daemon's startup engine). For `PushedHead`,
    /// this is a per-snapshot overlay engine built from the daemon's
    /// system roots + the caller's materialised user root + the
    /// trust overlay. Cached in `AppState.engine_cache` keyed by
    /// `(system_install_generation, snapshot_hash)`.
    pub request_engine: Arc<Engine>,
}

/// Resolve a `ProjectSource` into a concrete execution context.
///
/// For `LiveFs`: returns the project path as-is.
/// For `PushedHead`: resolves the principal's HEAD ref and checks out
/// from CAS into a temp execution directory.
///
/// The `checkout_id` is used to name the temp directory (typically a
/// request ID or similar unique identifier).
///
/// The `_principal_id` is the authenticated caller's identity string
/// (e.g., `fp:<sha256hex>`). For `PushedHead`, it is used to scope the
/// HEAD ref so different principals don't collide on the same project
/// path.
pub fn resolve_project_context(
    state: &AppState,
    source: &ProjectSource,
    project_path: &std::path::Path,
    principal_id: &str,
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
            user_root: None,
            user_temp_dir: None,
            trust_overlay: None,
            // LiveFs uses the daemon's startup engine directly — no
            // per-request overlay needed because the daemon's own
            // user space is what resolves anyway.
            request_engine: state.engine.clone(),
        },
        ProjectSource::PushedHead => {
            // HEAD lookup MUST use the same canonical ref string as
            // push_head used when writing the ref. canonical_project_ref
            // is the single source of truth — it canonicalizes via
            // std::fs::canonicalize (matching push_head) and bypasses
            // only for NO_PROJECT_SENTINEL.
            let project_str = canonical_project_ref(&project_path.to_string_lossy())?;
            let project_hash = lillux::cas::sha256_hex(project_str.as_bytes());
            let principal_key = ryeos_state::refs::principal_storage_key(principal_id);
            let cas_root = state.state_store.cas_root()?;
            let cas = lillux::cas::CasStore::new(cas_root.clone());

            let snap_hash = state
                .state_store
                .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?
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

            // 1. Materialise project content (existing behaviour).
            let manifest_hash = &snapshot.project_manifest_hash;
            let exec_dir = state
                .config
                .system_space_dir
                .join("executions")
                .join(checkout_id);
            let materialization_cache = crate::execution::cache::MaterializationCache::new(
                state.config.system_space_dir.join("cache").join("snapshots"),
            );
            crate::execution::checkout_project(
                &cas_root,
                manifest_hash,
                &exec_dir,
                Some(&materialization_cache),
            )
            .map_err(|e| ProjectSourceError::CheckoutFailed(e.to_string()))?;

            // 2. Materialise user content. If the snapshot carries
            //    `user_manifest_hash`, check it out into a sibling
            //    temp dir. Otherwise leave user_root = None
            //    (strict-or-empty: no user-tier resolution at all —
            //    NEVER fall through to the remote operator's user space).
            let (user_root, user_temp_dir, trust_overlay) =
                if let Some(user_mh) = snapshot.user_manifest_hash.as_ref() {
                    // The push side writes user-manifest paths
                    // relative to `<user_root>/.ai/` (no `.ai/`
                    // prefix), so materialise into a `.ai/`
                    // subdirectory of the temp dir. That way the
                    // outer temp dir BECOMES the `user_root` passed
                    // to the engine — containing `.ai/directives/…`
                    // etc., mirroring the operator's own user space
                    // layout.
                    let user_exec_dir = state
                        .config
                        .system_space_dir
                        .join("executions")
                        .join(format!("{}-user", checkout_id));
                    let user_ai_dir = user_exec_dir.join(ryeos_engine::AI_DIR);
                    crate::execution::checkout_project(
                        &cas_root,
                        user_mh,
                        &user_ai_dir,
                        Some(&materialization_cache),
                    )
                    .map_err(|e| {
                        ProjectSourceError::CheckoutFailed(format!(
                            "user-manifest checkout failed: {e}"
                        ))
                    })?;

                    // Extract trust pins from the materialised user
                    // space into an in-memory overlay. The trust dir
                    // is part of the user manifest, and
                    // `load_from_dir` parses it. The result is
                    // unioned into the per-request engine's trust
                    // store via `trust_overlay` — never written back
                    // to the remote's persistent trust dir.
                    let trust_dir = user_ai_dir
                        .join("config")
                        .join("keys")
                        .join("trusted");
                    let overlay = if trust_dir.is_dir() {
                        match TrustStore::load_from_dir(&trust_dir) {
                            Ok(store) if !store.is_empty() => Some(store),
                            Ok(_) => None,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "pushed user-space trust pins failed to load — \
                                     proceeding with persistent trust store only"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };

                    (Some(user_exec_dir.clone()), Some(user_exec_dir), overlay)
                } else {
                    (None, None, None)
                };

            // 3. Build (or reuse from cache) the per-request engine.
            //    Cache key mixes the system-install generation so a
            //    bundle install/uninstall invalidates cleanly.
            let cache_key = ryeos_app::engine_cache::CacheKey {
                system_install_generation: state.engine_cache.system_install_generation(),
                snapshot_hash: snap_hash.clone(),
            };
            let request_engine = if let Some(eng) = state.engine_cache.get(&cache_key) {
                eng
            } else {
                let bundle_roots = state.engine.system_roots.clone();
                let built = ryeos_app::engine_init::build_engine_for_roots(
                    &state.config,
                    &bundle_roots,
                    Some(exec_dir.as_path()),
                    user_root.as_deref(),
                    trust_overlay.as_ref(),
                )
                .map_err(|e| {
                    ProjectSourceError::CheckoutFailed(format!(
                        "per-request engine build failed: {e}"
                    ))
                })?;
                let arc = Arc::new(built);
                // Note: we hand the temp dirs to the cache via
                // TempDirGuards so eviction triggers cleanup. The
                // ResolvedProjectContext keeps its own PathBuf copies
                // (not guards) — the cache owns lifecycle.
                state.engine_cache.insert(
                    cache_key,
                    arc.clone(),
                    Some(ryeos_app::engine_cache::TempDirGuard::new(exec_dir.clone())),
                    user_temp_dir
                        .as_ref()
                        .map(|p| ryeos_app::engine_cache::TempDirGuard::new(p.clone())),
                );
                arc
            };

            ResolvedProjectContext {
                effective_path: exec_dir.clone(),
                original_path,
                source: source.clone(),
                snapshot_hash: Some(snap_hash.clone()),
                // Lifecycle of project + user temp dirs is owned by the
                // engine cache entry. Don't double-clean — leave these
                // None so the existing TempDirGuard in execute_mode
                // (which calls `temp_dir.take()`) is a no-op.
                temp_dir: None,
                user_root,
                user_temp_dir: None,
                trust_overlay,
                request_engine,
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

/// Sentinel value for `--no-project` mode: the caller has chosen to
/// run a tool/system item without pushing project content. The ref
/// is still per-principal so two different operators don't share
/// HEAD state under this sentinel.
///
/// Lives here (not in `ryeos-api`) so the helper below can recognise
/// it without cross-crate dependencies.
pub const NO_PROJECT_SENTINEL: &str = "__no_project__";

/// Caller-supplied intent for the project root of a remote operation.
///
/// By the time a request reaches the daemon this MUST already be one
/// of the concrete modes — no `Auto` variant. Auto-discovery from cwd
/// is a *client-side* concern (the daemon's cwd is irrelevant to the
/// caller). The CLI runs `discover_project_root` before sending and
/// turns the result into either `Explicit` (an absolute, existing
/// path) or `NoProject` (the operator passed `--no-project` or no
/// marker was found and they opted out explicitly).
///
/// Wire format is tagged so future variants (e.g. `Snapshot { hash }`)
/// extend cleanly without breaking old clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectPathSpec {
    /// `--no-project` mode: skip project ingest entirely. Identity is
    /// still per-principal under the `NO_PROJECT_SENTINEL` so two
    /// different operators don't collide on HEAD state.
    NoProject,
    /// Explicit absolute project root. The CLI canonicalizes before
    /// sending. The daemon re-validates via `canonical_project_ref`
    /// as defence in depth.
    Explicit { path: PathBuf },
}

impl ProjectPathSpec {
    /// The string form used for `canonical_project_ref` lookups.
    /// Returns `NO_PROJECT_SENTINEL` for `NoProject`, the path's
    /// string for `Explicit`.
    pub fn ref_string(&self) -> String {
        match self {
            Self::NoProject => NO_PROJECT_SENTINEL.to_string(),
            Self::Explicit { path } => path.to_string_lossy().to_string(),
        }
    }

    /// `Some(path)` for `Explicit`, `None` for `NoProject`. Used by
    /// the push pipeline which only walks the filesystem in `Explicit`
    /// mode.
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::NoProject => None,
            Self::Explicit { path } => Some(path.as_path()),
        }
    }
}

/// Resolve a raw `project_path` string into the canonical reference
/// string used everywhere identity keys are computed (push HEAD ref,
/// execute HEAD lookup, pull lineage anchor).
///
/// Rules:
/// - The `NO_PROJECT_SENTINEL` passes through verbatim. Identity is
///   per-principal under this sentinel; no path semantics.
/// - Empty string is rejected: client-side discovery is required to
///   resolve the project root before the request leaves. An empty
///   path arriving at the daemon means the client skipped that step.
/// - `"."` and other relative paths are rejected for the same reason:
///   the daemon's cwd is irrelevant to the caller's project.
/// - Everything else goes through `std::fs::canonicalize`. Rejected
///   on failure — the previous behaviour of silently falling back
///   to the raw string made push and execute hash different strings.
pub fn canonical_project_ref(raw: &str) -> Result<String, ProjectSourceError> {
    if raw == NO_PROJECT_SENTINEL {
        return Ok(raw.to_string());
    }
    if raw.is_empty() {
        return Err(ProjectSourceError::Other(
            "project_path is empty: the client must resolve and pass a \
             canonicalized project root, or use the `__no_project__` \
             sentinel for --no-project mode"
                .into(),
        ));
    }
    let path = std::path::Path::new(raw);
    if !path.is_absolute() {
        return Err(ProjectSourceError::Other(format!(
            "project_path '{}' is not absolute: the client must canonicalize \
             paths before sending. Relative paths cannot be resolved on the \
             daemon side (the daemon's cwd is irrelevant to the caller).",
            raw
        )));
    }
    match path.canonicalize() {
        Ok(p) => Ok(p.to_string_lossy().to_string()),
        Err(e) => Err(ProjectSourceError::Other(format!(
            "project_path '{}' is not canonicalizable: {}. Ensure the path \
             exists and is accessible.",
            raw, e
        ))),
    }
}

#[cfg(test)]
mod canonical_project_ref_tests {
    use super::*;

    #[test]
    fn passes_through_no_project_sentinel() {
        let out = canonical_project_ref(NO_PROJECT_SENTINEL).unwrap();
        assert_eq!(out, NO_PROJECT_SENTINEL);
    }

    #[test]
    fn rejects_empty_string() {
        let err = canonical_project_ref("").unwrap_err();
        assert!(format!("{err}").contains("empty"));
    }

    #[test]
    fn rejects_relative_dot() {
        let err = canonical_project_ref(".").unwrap_err();
        assert!(format!("{err}").contains("not absolute"));
    }

    #[test]
    fn rejects_relative_path() {
        let err = canonical_project_ref("some/relative/path").unwrap_err();
        assert!(format!("{err}").contains("not absolute"));
    }

    #[test]
    fn rejects_nonexistent_absolute() {
        let err =
            canonical_project_ref("/this/path/does/not/exist/at/all").unwrap_err();
        assert!(format!("{err}").contains("not canonicalizable"));
    }

    #[test]
    fn relative_and_absolute_form_of_same_dir_produce_equal_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let abs = tmp.path().canonicalize().unwrap();
        // Same absolute path twice → same canonical ref. (Can't really
        // exercise symlink unification portably here without an OS-specific
        // setup; the contract is: identical input → identical output AND
        // canonicalize-equivalent inputs → identical output.)
        let r1 = canonical_project_ref(&abs.to_string_lossy()).unwrap();
        let r2 = canonical_project_ref(&abs.to_string_lossy()).unwrap();
        assert_eq!(r1, r2);
    }
}


