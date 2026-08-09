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
//!       - `tool_siblings`: `chain[0].source_path.parent()`, non-recursive.
//!       - `tool_dir`     : `chain[0].source_path.parent()`, respects
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

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::Value;

use crate::error::EngineError;
use crate::item_resolution::parse_signature_header;
use crate::project_content::SealedDependencyContent;
use crate::runtime::{CompileContext, RuntimeHandler};
use crate::trust::content_hash_after_signature;

pub const KEY: &str = "verify_deps";
const MAX_DEPENDENCY_FILES: usize = 4096;
const MAX_DEPENDENCY_TRAVERSAL_ENTRIES: usize = MAX_DEPENDENCY_FILES * 2;
const MAX_DEPENDENCY_TRAVERSAL_DEPTH: usize = 64;
const MAX_DEPENDENCY_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DEPENDENCY_AGGREGATE_BYTES: u64 = 64 * 1024 * 1024;

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
        let cfg: VerifyDepsConfig = serde_json::from_value(block.clone()).map_err(|e| {
            EngineError::InvalidRuntimeConfig {
                path: intermediate.source_path.display().to_string(),
                reason: format!("invalid verify_deps block: {e}"),
            }
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
        if let Some((project_root, project_content)) = ctx.project_authority
            && base.starts_with(project_root)
        {
            return verify_admitted_project_dependencies(
                &base,
                project_root,
                project_content,
                recursive,
                &cfg.exclude_dirs,
                &cfg.extensions,
                ctx,
            );
        }
        let extensions: HashSet<String> = cfg.extensions.iter().cloned().collect();
        if extensions.is_empty() {
            // Python iterates with `if filepath.suffix not in
            // extensions: continue` — empty extensions → nothing
            // matches → no-op. Match.
            return Ok(());
        }

        walk_and_verify(&base, recursive, &cfg.exclude_dirs, &extensions, ctx)?;
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
    recursive: bool,
    exclude_dirs: &HashSet<String>,
    extensions: &HashSet<String>,
    ctx: &CompileContext<'_>,
) -> Result<(), EngineError> {
    let mut file_count = 0_usize;
    let mut aggregate_bytes = 0_u64;
    let present = lillux::visit_regular_files_no_follow_bounded(
        base,
        lillux::DirectoryTraversalBudget::new(
            MAX_DEPENDENCY_TRAVERSAL_ENTRIES,
            MAX_DEPENDENCY_TRAVERSAL_DEPTH,
        ),
        |relative, is_directory| {
            if !is_directory {
                return Ok(false);
            }
            Ok(!recursive
                || relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| exclude_dirs.contains(name)))
        },
        |relative, file| {
            file_count = file_count.saturating_add(1);
            if file_count > MAX_DEPENDENCY_FILES {
                anyhow::bail!("verify_deps exceeds {MAX_DEPENDENCY_FILES} regular files");
            }
            let path = base.join(relative);
            let suffix = path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| format!(".{extension}"))
                .unwrap_or_default();
            if !extensions.contains(&suffix) {
                return Ok(());
            }
            // A dependency covered by an admitted realization mount is
            // judged by the bytes the runtime will execute. Plan build runs
            // in the daemon, where that mount does not exist, so a live read
            // here would verify content the run never sees — and a live edit
            // would refuse a launch whose executable bytes never changed. A
            // covered path the sealed view holds no file for is skipped the
            // same way: the mount replaces the whole subtree at execution,
            // so the live file this walk found never executes.
            let bytes = match ctx
                .sealed_content
                .map(|sealed| sealed.sealed_bytes(&path, MAX_DEPENDENCY_FILE_BYTES))
                .transpose()?
            {
                Some(SealedDependencyContent::Sealed(sealed)) => sealed,
                Some(SealedDependencyContent::Absent) => return Ok(()),
                Some(SealedDependencyContent::Uncovered) | None => {
                    lillux::read_open_regular_file_bounded(file, MAX_DEPENDENCY_FILE_BYTES)?
                }
            };
            aggregate_bytes = aggregate_bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("verify_deps byte count overflow"))?;
            if aggregate_bytes > MAX_DEPENDENCY_AGGREGATE_BYTES {
                anyhow::bail!(
                    "verify_deps exceeds {MAX_DEPENDENCY_AGGREGATE_BYTES} aggregate bytes"
                );
            }
            let content = std::str::from_utf8(&bytes)
                .with_context(|| format!("verify_deps file is not UTF-8: {}", path.display()))?;
            verify_file_content(&path, content, ctx)?;
            Ok(())
        },
    )
    .map_err(|error| match error.downcast::<EngineError>() {
        Ok(error) => error,
        Err(error) => EngineError::InvalidRuntimeConfig {
            path: base.display().to_string(),
            reason: format!("verify_deps secure walk failed: {error:#}"),
        },
    })?;
    if !present {
        return Err(EngineError::InvalidRuntimeConfig {
            path: base.display().to_string(),
            reason: "verify_deps base directory is absent".to_string(),
        });
    }
    Ok(())
}

fn verify_file_content(
    path: &Path,
    content: &str,
    ctx: &CompileContext<'_>,
) -> Result<(), EngineError> {
    // Find a kind whose `formats` declares this extension. Iterate
    // deterministically (sorted by kind name) to guarantee a stable
    // pick if multiple kinds share an extension.
    let suffix = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let kind_schema =
        pick_kind_for_extension(ctx, &suffix).ok_or_else(|| EngineError::InvalidRuntimeConfig {
            path: path.display().to_string(),
            reason: format!(
                "verify_deps: no registered kind owns extension `{suffix}` — \
                 cannot determine signature envelope"
            ),
        })?;
    let ext_spec =
        kind_schema
            .spec_for(&suffix)
            .ok_or_else(|| EngineError::InvalidRuntimeConfig {
                path: path.display().to_string(),
                reason: format!(
                    "verify_deps: kind has no extension spec for `{suffix}` (internal)"
                ),
            })?;
    let envelope = &ext_spec.signature;

    match parse_signature_header(content, envelope) {
        None => {
            tracing::warn!(
                file = %path.display(),
                "verify_deps: unsigned file (allow_unsigned=true)"
            );
        }
        Some(header) => {
            let recomputed = content_hash_after_signature(content, envelope).ok_or_else(|| {
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

#[allow(clippy::too_many_arguments)]
fn verify_admitted_project_dependencies(
    base: &Path,
    project_root: &Path,
    project_content: &dyn crate::project_content::AuthoritativeProjectContent,
    recursive: bool,
    exclude_dirs: &HashSet<String>,
    extensions: &[String],
    ctx: &CompileContext<'_>,
) -> Result<(), EngineError> {
    let extension_set = extensions.iter().cloned().collect::<HashSet<_>>();
    if extension_set.is_empty() {
        return Ok(());
    }
    let prefix =
        base.strip_prefix(project_root)
            .map_err(|_| EngineError::InvalidRuntimeConfig {
                path: base.display().to_string(),
                reason: "admitted verify_deps base escaped project root".to_string(),
            })?;
    let entries = project_content.list_files(prefix, recursive, MAX_DEPENDENCY_FILES)?;
    let mut aggregate_bytes = 0_u64;
    for entry in entries {
        if entry.relative_path.components().any(|component| {
            let std::path::Component::Normal(component) = component else {
                return true;
            };
            exclude_dirs.contains(component.to_string_lossy().as_ref())
        }) {
            continue;
        }
        let suffix = entry
            .relative_path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{value}"))
            .unwrap_or_default();
        if !extension_set.contains(&suffix) {
            continue;
        }
        let absolute = base.join(&entry.relative_path);
        // Realization mounts overlay the admitted project tree at execution,
        // so a covered path is judged by its sealed bytes here too; the
        // admitted project content answers only for uncovered paths.
        let bytes = match ctx
            .sealed_content
            .map(|sealed| sealed.sealed_bytes(&absolute, MAX_DEPENDENCY_FILE_BYTES))
            .transpose()?
        {
            Some(SealedDependencyContent::Sealed(sealed)) => sealed,
            Some(SealedDependencyContent::Absent) => continue,
            Some(SealedDependencyContent::Uncovered) | None => {
                if entry.size > MAX_DEPENDENCY_FILE_BYTES {
                    return Err(EngineError::InvalidRuntimeConfig {
                        path: absolute.display().to_string(),
                        reason: format!(
                            "verify_deps: admitted dependency exceeds {MAX_DEPENDENCY_FILE_BYTES} bytes"
                        ),
                    });
                }
                let project_relative = prefix.join(&entry.relative_path);
                project_content
                    .read_file(&project_relative, MAX_DEPENDENCY_FILE_BYTES)?
                    .ok_or_else(|| EngineError::InvalidRuntimeConfig {
                        path: absolute.display().to_string(),
                        reason: "verify_deps: admitted dependency disappeared".to_string(),
                    })?
            }
        };
        aggregate_bytes = aggregate_bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| EngineError::InvalidRuntimeConfig {
                path: base.display().to_string(),
                reason: "verify_deps: admitted dependency byte count overflow".to_string(),
            })?;
        if aggregate_bytes > MAX_DEPENDENCY_AGGREGATE_BYTES {
            return Err(EngineError::InvalidRuntimeConfig {
                path: base.display().to_string(),
                reason: format!(
                    "verify_deps: admitted dependencies exceed {MAX_DEPENDENCY_AGGREGATE_BYTES} aggregate bytes"
                ),
            });
        }
        let content =
            std::str::from_utf8(&bytes).map_err(|error| EngineError::InvalidRuntimeConfig {
                path: absolute.display().to_string(),
                reason: format!("verify_deps: admitted dependency is not UTF-8: {error}"),
            })?;
        verify_file_content(&absolute, content, ctx)?;
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
        if let Some(schema) = ctx.kinds.get(name)
            && schema.spec_for(suffix).is_some()
        {
            return Some(schema);
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
    use crate::runtime::{ChainIntermediate, HostEnvBindings, SpecOverrides, TemplateContext};
    use crate::trust::TrustStore;
    use serde_json::{Map, Value, json};
    use std::collections::HashMap;
    use std::path::PathBuf;

    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new() -> std::io::Result<Self> {
            let n = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("rye_vd_{}_{}_{}", std::process::id(), nanos, n));
            std::fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
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
        ResolutionRoots::from_flat(None, vec![])
    }

    fn make_intermediate(parsed: Value, source: PathBuf) -> ChainIntermediate {
        ChainIntermediate {
            executor_id: "tool:test".to_owned(),
            resolved_ref: "tool:test".to_owned(),
            kind: "tool".to_owned(),
            source_path: source,
            source_space: crate::contracts::ItemSpace::Project,
            source_root: crate::contracts::ItemSourceRoot::Search {
                label: "test".to_owned(),
            },
            parsed,
        }
    }

    static NULL_PARAMS: Value = Value::Null;
    static EMPTY_HOST_ENV: std::sync::LazyLock<HostEnvBindings> =
        std::sync::LazyLock::new(HostEnvBindings::default);

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
            env_sources: HashMap::new(),
            spec_overrides: SpecOverrides::default(),
            params: Value::Object(Map::new()),
            original_params: &NULL_PARAMS,
            chain,
            current_index: 0,
            roots,
            parsers,
            kinds,
            trust_store: trust,
            node_trust_store: trust,
            project_root: None,
            project_authority: None,
            sealed_content: None,
            root_trust_class: crate::resolution::TrustClass::TrustedBundle,
            host_env: &EMPTY_HOST_ENV,
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

    // ── Sealed dependency content (realization-covered paths) ───────────

    use crate::project_content::{
        AuthoritativeProjectContent, ProjectContentEntry, SealedDependencyBytes,
    };

    struct StubSealed {
        map: HashMap<PathBuf, SealedDependencyContent>,
    }

    impl SealedDependencyBytes for StubSealed {
        fn sealed_bytes(
            &self,
            absolute_path: &Path,
            _max_bytes: u64,
        ) -> Result<SealedDependencyContent, EngineError> {
            Ok(self
                .map
                .get(absolute_path)
                .cloned()
                .unwrap_or(SealedDependencyContent::Uncovered))
        }
    }

    /// One project-relative file table standing in for an admitted pinned
    /// materialization.
    struct StubProjectContent {
        files: Vec<(PathBuf, Vec<u8>)>,
    }

    impl AuthoritativeProjectContent for StubProjectContent {
        fn list_files(
            &self,
            prefix: &Path,
            _recursive: bool,
            _max_entries: usize,
        ) -> Result<Vec<ProjectContentEntry>, EngineError> {
            Ok(self
                .files
                .iter()
                .filter_map(|(path, content)| {
                    path.strip_prefix(prefix)
                        .ok()
                        .map(|relative| ProjectContentEntry {
                            relative_path: relative.to_path_buf(),
                            content_hash: "unchecked-by-this-branch".to_owned(),
                            size: content.len() as u64,
                            normalized_mode: 0o644,
                        })
                })
                .collect())
        }

        fn read_file(
            &self,
            relative_path: &Path,
            _max_bytes: u64,
        ) -> Result<Option<Vec<u8>>, EngineError> {
            Ok(self
                .files
                .iter()
                .find(|(path, _)| path == relative_path)
                .map(|(_, content)| content.clone()))
        }

        fn validates_file(
            &self,
            _relative_path: &Path,
            _content_hash: &str,
        ) -> Result<bool, EngineError> {
            Ok(true)
        }

        fn validates_absence(&self, _relative_path: &Path) -> Result<bool, EngineError> {
            Ok(true)
        }
    }

    /// Registry with one signed `tool` kind owning `.py` under a `#`
    /// signature envelope, so a verified file's header hash is enforced.
    fn python_tool_registry() -> KindRegistry {
        let schema_dir = TempDir::new().unwrap();
        let sk = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let vk = sk.verifying_key();
        let trust = TrustStore::from_signers(vec![crate::trust::TrustedSigner {
            fingerprint: crate::trust::compute_fingerprint(&vk),
            verifying_key: vk,
            label: None,
        }]);
        let yaml = "\
location:
  directory: tools
formats:
  - extensions: [\".py\"]
    parser: parser:ryeos/core/python/tool-header
    signature:
      prefix: \"#\"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
effective_trust:
  include_references: false
resolution: []
metadata:
  rules:
    name:
      from: filename
";
        let kind_dir = schema_dir.path().join("tool");
        std::fs::create_dir_all(&kind_dir).unwrap();
        std::fs::write(
            kind_dir.join("tool.kind-schema.yaml"),
            lillux::signature::sign_content(yaml, &sk, "#", None),
        )
        .unwrap();
        KindRegistry::load_base(&[schema_dir.path().to_path_buf()], &trust).unwrap()
    }

    /// A dependency whose signed header matches its body, and a live edit of
    /// it whose body no longer matches.
    fn signed_and_drifted() -> (String, String) {
        let sk = lillux::crypto::SigningKey::from_bytes(&[7u8; 32]);
        let signed = lillux::signature::sign_content("VALUE = 1\n", &sk, "#", None);
        let drifted = format!("{signed}VALUE = 2\n");
        (signed, drifted)
    }

    #[test]
    fn sealed_bytes_replace_the_live_read_for_covered_paths() {
        let kinds = python_tool_registry();
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("t.py");
        std::fs::write(&tool, "x").unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let (signed, drifted) = signed_and_drifted();
        let helper = lib.join("helper.py");
        std::fs::write(&helper, &drifted).unwrap();

        let chain = vec![make_intermediate(json!({}), tool)];
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let block = json!({"enabled": true, "scope": "tool_dir", "extensions": [".py"]});

        // Live read: the drifted body contradicts its signed header.
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        let err = VerifyDepsHandler.apply(&block, &mut ctx).unwrap_err();
        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "got {err:?}"
        );

        // Sealed bytes are what will execute; the live drift is invisible.
        let stub = StubSealed {
            map: HashMap::from([(
                helper.clone(),
                SealedDependencyContent::Sealed(signed.into_bytes()),
            )]),
        };
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        ctx.sealed_content = Some(&stub);
        VerifyDepsHandler.apply(&block, &mut ctx).unwrap();
    }

    #[test]
    fn sealed_absent_skips_a_file_the_sealed_view_holds_no_entry_for() {
        let kinds = python_tool_registry();
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("t.py");
        std::fs::write(&tool, "x").unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let (_, drifted) = signed_and_drifted();
        let scratch = lib.join("scratch.py");
        std::fs::write(&scratch, &drifted).unwrap();

        let chain = vec![make_intermediate(json!({}), tool)];
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let block = json!({"enabled": true, "scope": "tool_dir", "extensions": [".py"]});

        let stub = StubSealed {
            map: HashMap::from([(scratch.clone(), SealedDependencyContent::Absent)]),
        };
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        ctx.sealed_content = Some(&stub);
        VerifyDepsHandler.apply(&block, &mut ctx).unwrap();
    }

    #[test]
    fn uncovered_paths_verify_live_even_with_a_sealed_source_present() {
        let kinds = python_tool_registry();
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("t.py");
        std::fs::write(&tool, "x").unwrap();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let (_, drifted) = signed_and_drifted();
        std::fs::write(lib.join("helper.py"), &drifted).unwrap();

        let chain = vec![make_intermediate(json!({}), tool)];
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let block = json!({"enabled": true, "scope": "tool_dir", "extensions": [".py"]});

        let stub = StubSealed {
            map: HashMap::new(),
        };
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        ctx.sealed_content = Some(&stub);
        let err = VerifyDepsHandler.apply(&block, &mut ctx).unwrap_err();
        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn sealed_bytes_override_admitted_project_content() {
        let kinds = python_tool_registry();
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let tool = project_root.join("tools").join("t.py");
        std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
        std::fs::write(&tool, "x").unwrap();
        let (signed, drifted) = signed_and_drifted();
        let admitted = StubProjectContent {
            files: vec![
                (
                    PathBuf::from("tools/helper.py"),
                    drifted.clone().into_bytes(),
                ),
                (
                    PathBuf::from("tools/scratch.py"),
                    drifted.clone().into_bytes(),
                ),
            ],
        };

        let chain = vec![make_intermediate(json!({}), tool)];
        let parsers = empty_dispatcher();
        let trust = empty_trust();
        let roots = empty_roots();
        let block = json!({"enabled": true, "scope": "tool_dir", "extensions": [".py"]});

        // The admitted authority serves drifted bytes for both entries.
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        ctx.project_authority = Some((project_root.as_path(), &admitted));
        let err = VerifyDepsHandler.apply(&block, &mut ctx).unwrap_err();
        assert!(
            matches!(err, EngineError::ContentHashMismatch { .. }),
            "got {err:?}"
        );

        // A realization mount overlays both paths: one sealed clean, one
        // holding no file at all. Neither drifted admitted entry is judged.
        let stub = StubSealed {
            map: HashMap::from([
                (
                    project_root.join("tools/helper.py"),
                    SealedDependencyContent::Sealed(signed.into_bytes()),
                ),
                (
                    project_root.join("tools/scratch.py"),
                    SealedDependencyContent::Absent,
                ),
            ]),
        };
        let mut ctx = make_ctx(&chain, &kinds, &parsers, &trust, &roots);
        ctx.project_authority = Some((project_root.as_path(), &admitted));
        ctx.sealed_content = Some(&stub);
        VerifyDepsHandler.apply(&block, &mut ctx).unwrap();
    }
}
