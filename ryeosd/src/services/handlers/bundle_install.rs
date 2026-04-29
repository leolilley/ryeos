//! `bundle.install` — install a downstream bundle via node-config writer.
//!
//! Copies source to `<state_dir>/.ai/bundles/<name>/`, writes a signed
//! `kind: node` `section: bundles` item at `<state_dir>/.ai/node/bundles/<name>.yaml`.
//!
//! `core` cannot be installed — it is the packaged base root laid down at
//! `system_data_dir` out-of-band (by the OS package installer or manual copy).
//!
//! OfflineOnly: the daemon must be stopped (engine reload not implemented).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{NativeParserHandlerRegistry, ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

#[derive(Debug, serde::Deserialize)]
pub struct Request {
    /// Bundle name; becomes the install directory name.
    pub name: String,
    /// Source directory to copy from.
    pub source_path: PathBuf,
}

fn validate_name(name: &str) -> Result<()> {
    if name == "core" {
        bail!(
            "\"core\" cannot be installed via bundle.install — \
             it is the packaged base root and must be laid down at \
             system_data_dir out-of-band"
        );
    }
    if name.is_empty()
        || name
            .contains(|c: char| c == '/' || c == '\\' || c == '.' || c.is_whitespace())
    {
        bail!(
            "invalid bundle name '{}': must be non-empty and contain no path separators, \
             dots, or whitespace",
            name
        );
    }
    Ok(())
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    validate_name(&req.name)?;

    if !req.source_path.is_dir() {
        bail!(
            "source_path is not a directory: {}",
            req.source_path.display()
        );
    }

    let bundles_root = state.config.state_dir.join(".ai").join("bundles");
    let target = bundles_root.join(&req.name);

    if target.exists() {
        bail!(
            "bundle '{}' already installed at {}",
            req.name,
            target.display()
        );
    }

    // Preflight verification: build a temporary KindRegistry + TrustStore
    // from system_data_dir + user space + this bundle's own kind schemas,
    // then validate every signable item in the bundle before any
    // filesystem mutation.
    preflight_verify(&req.source_path, &state)?;

    fs::create_dir_all(&bundles_root).with_context(|| {
        format!(
            "failed to create bundles root {}",
            bundles_root.display()
        )
    })?;

    copy_dir_recursive(&req.source_path, &target).with_context(|| {
        format!(
            "failed to copy bundle from {} to {}",
            req.source_path.display(),
            target.display()
        )
    })?;

    let canonical_target = target
        .canonicalize()
        .context("failed to canonicalize installed bundle path")?;

    // Write signed kind: node bundle registration
    let config_item_path = crate::node_config::writer::write_signed_node_item(
        &state.config.state_dir.join(".ai").join("node"),
        "bundles",
        &req.name,
        &serde_json::json!({ "path": canonical_target }),
        &state.identity,
    )?;

    let report = serde_json::json!({
        "name": req.name,
        "path": canonical_target.display().to_string(),
        "config_item": config_item_path.display().to_string(),
    });
    Ok(report)
}

/// Preflight verification: validate every signable item in the bundle
/// before copying anything to the install target.
///
/// Builds a temporary KindRegistry + TrustStore from system_data_dir +
/// user space + the bundle's own kind schemas, then walks all signable
/// files, parses them, runs the path-anchoring validator, and checks
/// signatures. Refuses install on first failure.
fn preflight_verify(source_path: &Path, state: &AppState) -> Result<()> {
    let system_data_dir = &state.config.system_data_dir;
    let ai_dir = source_path.join(ryeos_engine::AI_DIR);

    // 1. Collect kind schema roots: system_data_dir + bundle's own
    let mut schema_roots = Vec::new();
    let system_kinds = system_data_dir
        .join(ryeos_engine::AI_DIR)
        .join(ryeos_engine::KIND_SCHEMAS_DIR);
    if system_kinds.is_dir() {
        schema_roots.push(system_kinds);
    }
    let bundle_kinds = ai_dir.join(ryeos_engine::KIND_SCHEMAS_DIR);
    if bundle_kinds.is_dir() {
        schema_roots.push(bundle_kinds.clone());
    }

    if schema_roots.is_empty() {
        bail!(
            "preflight failed: no kind schemas found in system_data_dir ({}) \
             or the bundle itself ({})",
            system_data_dir.display(),
            bundle_kinds.display()
        );
    }

    // 2. Build trust store from system + user tiers
    let user_root = discover_user_root();
    let system_roots = vec![system_data_dir.clone()];
    let trust_store = TrustStore::load_three_tier(
        None, // no project root
        user_root.as_deref(),
        &system_roots,
    )
    .context("preflight: failed to load trust store")?;

    // 3. Load kind schemas
    let kinds = KindRegistry::load_base(&schema_roots, &trust_store)
        .context("preflight: failed to load kind schemas")?;

    // 4. Load parser tools from system + user roots
    let mut parser_search_roots: Vec<PathBuf> = system_roots.clone();
    if let Some(ref ur) = user_root {
        parser_search_roots.push(ur.clone());
    }
    let (parser_tools, _parser_duplicates) =
        ParserRegistry::load_base(&parser_search_roots, &trust_store, &kinds)
            .context("preflight: failed to load parser tools")?;

    let native_handlers = NativeParserHandlerRegistry::with_builtins();
    let parser_dispatcher = ParserDispatcher::new(parser_tools, native_handlers);

    // 5. Walk every signable item in the bundle and validate
    let mut failures: Vec<String> = Vec::new();

    for kind_name in kinds.kinds() {
        let kind_schema = match kinds.get(kind_name) {
            Some(s) => s,
            None => continue,
        };
        let kind_dir = ai_dir.join(&kind_schema.directory);
        if !kind_dir.is_dir() {
            continue;
        }

        // Collect all signable files under this kind directory
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files_recursive(&kind_dir, &mut files);

        for file_path in files {
            let ext = file_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");

            // Only check files whose extension matches this kind
            if kind_schema.spec_for(&format!(".{ext}")).is_none() {
                continue;
            }

            let rel = file_path.strip_prefix(&ai_dir).unwrap_or(&file_path);

            // Read file content
            let content = match fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(e) => {
                    failures.push(format!(
                        "{}: failed to read: {e}",
                        rel.display()
                    ));
                    continue;
                }
            };

            // Parse metadata
            let source_format = match kind_schema.resolved_format_for(&format!(".{ext}")) {
                Some(f) => f,
                None => {
                    failures.push(format!(
                        "{}: no source format for extension .{ext}",
                        rel.display()
                    ));
                    continue;
                }
            };

            let parsed = match parser_dispatcher.dispatch(
                &source_format.parser,
                &content,
                Some(&file_path),
                &source_format.signature,
            ) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(format!(
                        "{}: parse failed: {e}",
                        rel.display()
                    ));
                    continue;
                }
            };

            // Run path-anchoring validator
            if let Err(e) = ryeos_engine::kind_registry::validate_metadata_anchoring(
                &parsed,
                &kind_schema.extraction_rules,
                &kind_schema.directory,
                &ai_dir,
                &file_path,
            ) {
                failures.push(format!("{}: {e}", rel.display()));
                continue;
            }

            // Check signature — must have a valid signature from a trusted signer
            let sig_header = ryeos_engine::item_resolution::parse_signature_header(
                &content,
                &source_format.signature,
            );
            match sig_header {
                Some(header) => {
                    if !trust_store.is_trusted(&header.signer_fingerprint) {
                        failures.push(format!(
                            "{}: signer {} not in trust store",
                            rel.display(),
                            header.signer_fingerprint
                        ));
                        continue;
                    }
                    // Verify the signature cryptographically
                    if let Err(e) = ryeos_engine::trust::verify_item_signature(
                        &content,
                        &header,
                        &source_format.signature,
                        &trust_store,
                    ) {
                        failures.push(format!("{}: signature verification failed: {e}", rel.display()));
                        continue;
                    }
                }
                None => {
                    failures.push(format!(
                        "{}: unsigned — all bundle items must be signed",
                        rel.display()
                    ));
                    continue;
                }
            }
        }
    }

    if !failures.is_empty() {
        let mut msg = format!(
            "preflight verification failed for {} item(s):\n",
            failures.len()
        );
        for f in &failures {
            msg.push_str(&format!("  - {f}\n"));
        }
        bail!("{msg}");
    }

    tracing::info!(
        source = %source_path.display(),
        "preflight verification passed"
    );
    Ok(())
}

/// Recursively collect all files under a directory.
fn collect_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Discover the user-space root (parent of `~/.ai/`).
fn discover_user_root() -> Option<PathBuf> {
    std::env::var_os("USER_SPACE")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read {}", src.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &to)
                .with_context(|| format!("failed to symlink {}", to.display()))?;
            #[cfg(not(unix))]
            {
                let _ = link_target;
                bail!("symlinks unsupported on this platform: {}", from.display());
            }
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("failed to copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle/install",
    endpoint: "bundle.install",
    availability: ServiceAvailability::OfflineOnly,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .context("bundle.install requires { name, source_path }")?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_slashes() {
        assert!(validate_name("foo/bar").is_err());
    }

    #[test]
    fn validate_name_rejects_dots() {
        assert!(validate_name("foo.bar").is_err());
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_name("my-bundle_v2").is_ok());
    }

    #[test]
    fn validate_name_rejects_core() {
        let err = validate_name("core").unwrap_err();
        assert!(
            err.to_string().contains("cannot be installed via bundle.install"),
            "expected core refusal, got: {err}"
        );
    }

    #[test]
    fn copy_dir_copies_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("top.txt"), b"top").unwrap();
        fs::write(src.join("a/mid.txt"), b"mid").unwrap();
        fs::write(src.join("a/b/leaf.txt"), b"leaf").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read(dst.join("top.txt")).unwrap(), b"top");
        assert_eq!(fs::read(dst.join("a/mid.txt")).unwrap(), b"mid");
        assert_eq!(fs::read(dst.join("a/b/leaf.txt")).unwrap(), b"leaf");
    }
}
