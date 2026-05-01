//! `VerifyDepsHandler` — claims the top-level `verify_deps` block on
//! a tool/runtime item.
//!
//! Mirrors Python `PrimitiveExecutor._verify_tool_dependencies`
//! (lines 1251-1322 of `primitive_executor.py`) and the driver
//! gating at lines 226-232.
//!
//! Behavior:
//!
//!   * `enabled: false` → no-op.
//!   * `scope: "tool_file"` → no-op (entry point already verified
//!     during chain walk).
//!   * For active scopes, walk the configured base directory:
//!       - `tool_siblings`: chain[0].source_path.parent(), non-recursive.
//!       - `tool_dir`     : chain[0].source_path.parent(), respects
//!         `recursive`. **Default scope.**
//!         Prune `exclude_dirs`, filter by `extensions`, verify each file.
//!   * Per-file verification: read content, parse signature header
//!     using the kind whose `formats` declares this extension, then
//!     check `content_hash`. Mismatch → `EngineError::ContentHashMismatch`.
//!     Unsigned → `tracing::warn!` (matches `allow_unsigned=True`).
//!   * Symlink escapes (resolved path outside `base`) → hard error
//!     (Python parity, lines 1314-1319).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::EngineError;
use crate::item_resolution::parse_signature_header;
use crate::runtime::{CompileContext, RuntimeHandler};
use crate::trust::content_hash_after_signature;

pub const KEY: &str = "verify_deps";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyDepsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// File suffixes to include, e.g. `[".py"]`. Empty list = no-op.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Directory names to skip during traversal (matched against
    /// each path component, not full paths).
    #[serde(default = "default_exclude_dirs")]
    pub exclude_dirs: HashSet<String>,
    #[serde(default = "default_recursive")]
    pub recursive: bool,
    /// `"tool_file"` | `"tool_siblings"` | `"tool_dir"`.
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_exclude_dirs() -> HashSet<String> {
    [
        "__pycache__".to_owned(),
        ".venv".to_owned(),
        "node_modules".to_owned(),
        ".git".to_owned(),
    ]
    .into_iter()
    .collect()
}

fn default_recursive() -> bool {
    true
}

fn default_scope() -> String {
    "tool_dir".to_owned()
}

pub struct VerifyDepsHandler;

impl RuntimeHandler for VerifyDepsHandler {
    fn key(&self) -> &'static str {
        KEY
    }

    fn phase(&self) -> crate::runtime::HandlerPhase {
        crate::runtime::HandlerPhase::Verify
    }

    fn cardinality(&self) -> crate::runtime::HandlerCardinality {
        // Python parity: first chain element with verify_deps_config
        // wins (primitive_executor.py:1265-1268).
        crate::runtime::HandlerCardinality::FirstWins
    }

    #[tracing::instrument(
        name = "engine:verify_deps",
        skip(self, block, ctx),
        fields(
            item_ref = %ctx.chain[ctx.current_index].resolved_ref,
            chain_index = ctx.current_index,
        )
    )]
    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError> {
        let intermediate = &ctx.chain[ctx.current_index];
        let cfg: VerifyDepsConfig =
            serde_json::from_value(block.clone()).map_err(|e| EngineError::InvalidRuntimeConfig {
                path: intermediate.source_path.display().to_string(),
                reason: format!("invalid verify_deps block: {e}"),
            })?;

        if !cfg.enabled {
            return Ok(());
        }

        // tool_file scope: entry point is already verified by the
        // chain walker, so nothing to do.
        if cfg.scope == "tool_file" {
            return Ok(());
        }

        // Resolve base directory from scope.
        let (base, recursive) = resolve_base(&cfg, ctx)?;
        let base = base.canonicalize().map_err(|e| EngineError::InvalidRuntimeConfig {
            path: base.display().to_string(),
            reason: format!("could not canonicalise verify_deps base: {e}"),
        })?;

        let extensions: HashSet<String> = cfg.extensions.iter().cloned().collect();
        if extensions.is_empty() {
            // Python iterates with `if filepath.suffix not in
            // extensions: continue` — empty extensions → nothing
            // matches → no-op. Match.
            return Ok(());
        }

        walk_and_verify(&base, &base, recursive, &cfg.exclude_dirs, &extensions, ctx)?;
        Ok(())
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Resolve `(base_dir, effective_recursive)` for the configured
/// scope.
fn resolve_base(
    cfg: &VerifyDepsConfig,
    ctx: &CompileContext<'_>,
) -> Result<(PathBuf, bool), EngineError> {
    let chain0_dir = ctx.chain[0]
        .source_path
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| EngineError::InvalidRuntimeConfig {
            path: ctx.chain[0].source_path.display().to_string(),
            reason: "chain root has no parent directory".to_string(),
        })?;
    match cfg.scope.as_str() {
        "tool_siblings" => Ok((chain0_dir, false)),
        "tool_dir" => Ok((chain0_dir, cfg.recursive)),
        other => Err(EngineError::InvalidRuntimeConfig {
            path: ctx.chain[ctx.current_index]
                .source_path
                .display()
                .to_string(),
            reason: format!(
                "unknown verify_deps scope: {other} \
                 (valid: `tool_file`, `tool_siblings`, `tool_dir`)"
            ),
        }),
    }
}

/// Recursive (or non-recursive) walk; on hit, dispatch to
/// `verify_file`. Symlinks are NOT followed — `read_dir` returns
/// directory entries with their `file_type()` reporting symlink-ness.
fn walk_and_verify(
    base: &Path,
    dir: &Path,
    recursive: bool,
    exclude_dirs: &HashSet<String>,
    extensions: &HashSet<String>,
    ctx: &CompileContext<'_>,
) -> Result<(), EngineError> {
    let entries = std::fs::read_dir(dir).map_err(|e| EngineError::InvalidRuntimeConfig {
        path: dir.display().to_string(),
        reason: format!("verify_deps: could not read dir: {e}"),
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| EngineError::InvalidRuntimeConfig {
            path: dir.display().to_string(),
            reason: format!("verify_deps: dir entry error: {e}"),
        })?;
        let ftype = entry.file_type().map_err(|e| EngineError::InvalidRuntimeConfig {
            path: entry.path().display().to_string(),
            reason: format!("verify_deps: could not stat: {e}"),
        })?;
        let path = entry.path();

        if ftype.is_symlink() {
            // Skip symlinks entirely — Python uses
            // `followlinks=False` in os.walk; symlink handling for
            // FILES is checked via canonicalisation below. For
            // top-level symlinks we err on the side of skipping.
            continue;
        }

        if ftype.is_dir() {
            if !recursive {
                continue;
            }
            let name = entry.file_name();
            if exclude_dirs.contains(name.to_string_lossy().as_ref()) {
                continue;
            }
            walk_and_verify(base, &path, recursive, exclude_dirs, extensions, ctx)?;
            continue;
        }

        if !ftype.is_file() {
            continue;
        }

        // Extension filter.
        let suffix = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        if !extensions.contains(&suffix) {
            continue;
        }

        // Symlink escape guard (Python lines 1314-1319): the
        // resolved real path must be inside `base`.
        let real = path.canonicalize().map_err(|e| EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: format!("verify_deps: could not canonicalise: {e}"),
        })?;
        if !real.starts_with(base) {
            return Err(EngineError::InvalidRuntimeConfig {
                path: path.display().to_string(),
                reason: format!(
                    "verify_deps: symlink escape — {} resolves to {}",
                    path.display(),
                    real.display()
                ),
            });
        }

        verify_file(&path, ctx)?;
    }
    Ok(())
}

/// Read, parse signature, and verify content hash. Unsigned files
/// warn but do not fail (`allow_unsigned=True` in Python).
fn verify_file(path: &Path, ctx: &CompileContext<'_>) -> Result<(), EngineError> {
    let content = std::fs::read_to_string(path).map_err(|e| EngineError::InvalidRuntimeConfig {
        path: path.display().to_string(),
        reason: format!("verify_deps: could not read file: {e}"),
    })?;

    // Find a kind whose `formats` declares this extension. Iterate
    // deterministically (sorted by kind name) to guarantee a stable
    // pick if multiple kinds share an extension.
    let suffix = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let kind_schema = pick_kind_for_extension(ctx, &suffix).ok_or_else(|| {
        EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: format!(
                "verify_deps: no registered kind owns extension `{suffix}` — \
                 cannot determine signature envelope"
            ),
        }
    })?;
    let ext_spec = kind_schema
        .spec_for(&suffix)
        .ok_or_else(|| EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: format!(
                "verify_deps: kind has no extension spec for `{suffix}` (internal)"
            ),
        })?;
    let envelope = &ext_spec.signature;

    match parse_signature_header(&content, envelope) {
        None => {
            tracing::warn!(
                file = %path.display(),
                "verify_deps: unsigned file (allow_unsigned=true)"
            );
        }
        Some(header) => {
            let recomputed = content_hash_after_signature(&content, envelope).ok_or_else(|| {
                EngineError::InvalidRuntimeConfig {
                    path: path.display().to_string(),
                    reason: "verify_deps: could not locate signature line".to_string(),
                }
            })?;
            if recomputed != header.content_hash {
                return Err(EngineError::ContentHashMismatch {
                    canonical_ref: path.display().to_string(),
                    expected: header.content_hash.clone(),
                    actual: recomputed,
                });
            }
            tracing::debug!(file = %path.display(), "verify_deps: signature ok");
        }
    }
    Ok(())
}

/// Pick the first kind (sorted by kind name) whose `formats`
/// declares the given extension.
fn pick_kind_for_extension<'a>(
    ctx: &'a CompileContext<'a>,
    suffix: &str,
) -> Option<&'a crate::kind_registry::KindSchema> {
    let mut names: Vec<&str> = ctx.kinds.kinds().collect();
    names.sort();
    for name in names {
        if let Some(schema) = ctx.kinds.get(name) {
            if schema.spec_for(suffix).is_some() {
                return Some(schema);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_resolution::ResolutionRoots;
    use crate::kind_registry::KindRegistry;
    use crate::parsers::ParserDispatcher;
    use crate::runtime::{ChainIntermediate, SpecOverrides, TemplateContext};
    use crate::trust::TrustStore;
    use serde_json::{json, Map, Value};
    use std::collections::HashMap;
    use std::path::PathBuf;

    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct TempDir { path: PathBuf }
    impl TempDir {
        fn new() -> std::io::Result<Self> {
            let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rye_vd_{}_{}_{}", std::process::id(), nanos, n
            ));
            std::fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
        fn path(&self) -> &std::path::Path { &self.path }
    }
    impl Drop for TempDir {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.path); }
    }

    fn empty_registry() -> KindRegistry {
        KindRegistry::empty()
    }

    fn empty_dispatcher() -> ParserDispatcher {
        crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors()
    }

    fn empty_trust() -> TrustStore {
        TrustStore::empty()
    }

    fn empty_roots() -> ResolutionRoots {
        ResolutionRoots::from_flat(None, None, vec![])
    }

    fn make_intermediate(parsed: Value, source: PathBuf) -> ChainIntermediate {
        ChainIntermediate {
            executor_id: "tool:test".to_owned(),
            resolved_ref: "tool:test".to_owned(),
            kind: "tool".to_owned(),
            source_path: source,
            parsed,
        }
    }

    static NULL_PARAMS: Value = Value::Null;

    fn make_ctx<'a>(
        chain: &'a [ChainIntermediate],
        kinds: &'a KindRegistry,
        parsers: &'a ParserDispatcher,
        trust: &'a TrustStore,
        roots: &'a ResolutionRoots,
    ) -> CompileContext<'a> {
        CompileContext {
            template_ctx: TemplateContext::new(chain[0].source_path.clone()),
            env: HashMap::new(),
            spec_overrides: SpecOverrides::default(),
            params: Value::Object(Map::new()),
            original_params: &NULL_PARAMS,
            chain,
            current_index: 0,
            roots,
            parsers,
            kinds,
            trust_store: trust,
            project_root: None,
        }
    }

    #[test]
    fn disabled_handler_no_op() {
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("t.py");
        std::fs::write(&tool, "x").unwrap();
        let chain = vec![make_intermediate(json!({}), tool)];
        let kinds = empty_registry();
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        let block = json!({"enabled": false});
        VerifyDepsHandler.apply(&block, &mut ctx).unwrap();
    }

    #[test]
    fn tool_file_scope_is_no_op() {
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("t.py");
        std::fs::write(&tool, "x").unwrap();
        let chain = vec![make_intermediate(json!({}), tool)];
        let kinds = empty_registry();
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        let block = json!({"enabled": true, "scope": "tool_file", "extensions": [".py"]});
        VerifyDepsHandler.apply(&block, &mut ctx).unwrap();
    }

    #[test]
    fn empty_extensions_is_no_op() {
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("t.py");
        std::fs::write(&tool, "x").unwrap();
        let chain = vec![make_intermediate(json!({}), tool)];
        let kinds = empty_registry();
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        let block = json!({
            "enabled": true,
            "scope": "tool_dir",
            "extensions": []
        });
        VerifyDepsHandler.apply(&block, &mut ctx).unwrap();
    }

    #[test]
    fn unknown_field_rejected() {
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("t.py");
        std::fs::write(&tool, "x").unwrap();
        let chain = vec![make_intermediate(json!({}), tool)];
        let kinds = empty_registry();
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        let block = json!({"enabled": true, "bogus": 1});
        let err = VerifyDepsHandler.apply(&block, &mut ctx).unwrap_err();
        match err {
            EngineError::InvalidRuntimeConfig { reason, .. } => {
                assert!(reason.contains("bogus"), "got {reason}");
            }
            other => panic!("expected InvalidRuntimeConfig, got {other:?}"),
        }
    }

    #[test]
    fn unknown_scope_is_loud_error() {
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("t.py");
        std::fs::write(&tool, "x").unwrap();
        let chain = vec![make_intermediate(json!({}), tool)];
        let kinds = empty_registry();
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        let block = json!({
            "enabled": true,
            "scope": "bogus",
            "extensions": [".py"]
        });
        let err = VerifyDepsHandler.apply(&block, &mut ctx).unwrap_err();
        match err {
            EngineError::InvalidRuntimeConfig { reason, .. } => {
                assert!(reason.contains("unknown verify_deps scope"), "got {reason}");
            }
            other => panic!("expected InvalidRuntimeConfig, got {other:?}"),
        }
    }
}
