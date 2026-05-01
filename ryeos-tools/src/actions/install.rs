//! Shared bundle-install preflight logic.
//!
//! `service:bundle/install` (in `ryeosd/src/services/handlers/bundle_install.rs`)
//! and the operator-side `rye init` standard-bundle path both call into
//! [`preflight_verify_bundle`] to enforce the trust contract:
//!
//! - All signable items in the bundle MUST be signed.
//! - The signer fingerprint MUST already be in the operator's trust store
//!   (loaded from project + user tier; system_data_dir and the bundle
//!   itself contribute ONLY kind schemas + parser tools, never trust docs).
//! - Path-anchoring validator MUST pass for every item.
//!
//! Parser descriptors are sourced from system_data_dir, the operator's
//! user tier, AND the bundle being verified — bundles MAY introduce new
//! parsers alongside new kinds. Trust still gates loading.
//!
//! Refusal modes:
//! - Untrusted signer → install rejected. The operator must
//!   `rye trust pin <fingerprint>` before retrying.
//! - Unsigned file under a kind directory → install rejected.
//! - Tampered content (hash mismatch) → install rejected.
//!
//! There is NO auto-import of trust docs from the bundle being installed.
//! Bundles do not ship trust docs in the published-bundle format any more;
//! see `docs/POST-KINDS-FLIP-PLAN.md` step 6.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use ryeos_engine::kind_registry::KindRegistry;
use std::sync::Arc;

use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

/// Verify every signable item in a bundle source tree.
///
/// `source_path`: bundle root to verify (the directory containing `.ai/`).
/// `system_data_dir`: where `core` lives — provides the kind schemas + parser
///   tools used to parse and validate `source_path` items. Does NOT
///   contribute trust docs.
/// `user_root`: parent of `~/.ai/`. Provides the operator's trust store
///   (`.ai/config/keys/trusted/`).
///
/// Returns `Ok(())` if every item parsed, validated, and verified against
/// the operator trust store. On any failure, returns an error listing every
/// failed item; install/copy is refused.
pub fn preflight_verify_bundle(
    source_path: &Path,
    system_data_dir: &Path,
    user_root: Option<&Path>,
) -> Result<()> {
    let ai_dir = source_path.join(ryeos_engine::AI_DIR);
    if !ai_dir.is_dir() {
        bail!(
            "preflight: source has no .ai/ at {}",
            source_path.display()
        );
    }

    // 1. Kind schemas come from system_data_dir + the bundle itself.
    //    The bundle's own kind schemas are loaded so its items can be
    //    parsed; this does not bypass trust because each kind schema is
    //    itself signature-verified via the loaded trust store.
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
            "preflight: no kind schemas in system_data_dir ({}) or bundle ({})",
            system_data_dir.display(),
            bundle_kinds.display()
        );
    }

    // 2. Trust comes from operator tiers ONLY (project + user). The
    //    `system_roots` arg to `load_three_tier` is intentionally empty —
    //    bundle-internal `.ai/config/keys/trusted/` directories are NOT
    //    a trust source. Pin keys with `rye trust pin` instead.
    let trust_store = TrustStore::load_three_tier(None, user_root, &[])
        .context("preflight: load operator trust store")?;
    if trust_store.is_empty() {
        bail!(
            "preflight: operator trust store is empty — run `rye init` to \
             pin the platform author key, or `rye trust pin <fingerprint>` \
             to pin a third-party publisher"
        );
    }

    // 3. Load kind schemas (verified against trust store).
    let kinds = KindRegistry::load_base(&schema_roots, &trust_store)
        .context("preflight: load kind schemas")?;

    // 4. Load parser tools. Search roots: system, user, and the bundle
    //    being verified — bundles MAY ship their own parser descriptors
    //    needed for new kinds they introduce. Trust still gates loading.
    //
    //    Dedupe by canonicalized path: when `source_path == system_data_dir`
    //    (e.g. preflight verifying core in place during `rye init`) the
    //    same root must not be walked twice — `HandlerRegistry::load_base`
    //    rejects duplicate handler refs across roots.
    let mut parser_search_roots: Vec<PathBuf> = Vec::new();
    let mut seen_roots: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut push_unique = |path: PathBuf,
                           roots: &mut Vec<PathBuf>,
                           seen: &mut std::collections::HashSet<PathBuf>| {
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            roots.push(path);
        }
    };
    push_unique(system_data_dir.to_path_buf(), &mut parser_search_roots, &mut seen_roots);
    if let Some(ur) = user_root {
        push_unique(ur.to_path_buf(), &mut parser_search_roots, &mut seen_roots);
    }
    push_unique(source_path.to_path_buf(), &mut parser_search_roots, &mut seen_roots);

    // Diagnostic: warn if the source ships a legacy bundle-internal
    // trust dir, which the engine no longer treats as a trust source.
    let legacy_trust = source_path
        .join(ryeos_engine::AI_DIR)
        .join("config/keys/trusted");
    if legacy_trust.is_dir() {
        tracing::warn!(
            path = %legacy_trust.display(),
            "bundle ships a legacy `.ai/config/keys/trusted/` dir which is \
             ignored — pin the publisher key with `rye trust pin <fingerprint>` \
             instead"
        );
    }
    let (parser_tools, _dups) =
        ParserRegistry::load_base(&parser_search_roots, &trust_store, &kinds)
            .context("preflight: load parser tools")?;
    let handler_registry = HandlerRegistry::load_base(&parser_search_roots, &trust_store)
        .context("preflight: load handler descriptors")?;
    let parser_dispatcher =
        ParserDispatcher::new(parser_tools, Arc::new(handler_registry));

    // 5. Walk every signable file under each kind dir.
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

        let mut files: Vec<PathBuf> = Vec::new();
        collect_files_recursive(&kind_dir, &mut files);

        for file_path in files {
            let ext = file_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if kind_schema.spec_for(&format!(".{ext}")).is_none() {
                continue;
            }

            let rel = file_path.strip_prefix(&ai_dir).unwrap_or(&file_path);

            let content = match fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(e) => {
                    failures.push(format!("{}: read failed: {e}", rel.display()));
                    continue;
                }
            };

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
                    failures.push(format!("{}: parse failed: {e}", rel.display()));
                    continue;
                }
            };

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

            let sig_header = ryeos_engine::item_resolution::parse_signature_header(
                &content,
                &source_format.signature,
            );
            match sig_header {
                Some(header) => {
                    if !trust_store.is_trusted(&header.signer_fingerprint) {
                        failures.push(format!(
                            "{}: signer {} not in operator trust store \
                             (run `rye trust pin {}` to trust this publisher)",
                            rel.display(),
                            header.signer_fingerprint,
                            header.signer_fingerprint
                        ));
                        continue;
                    }
                    if let Err(e) = ryeos_engine::trust::verify_item_signature(
                        &content,
                        &header,
                        &source_format.signature,
                        &trust_store,
                    ) {
                        failures.push(format!(
                            "{}: signature verification failed: {e}",
                            rel.display()
                        ));
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
