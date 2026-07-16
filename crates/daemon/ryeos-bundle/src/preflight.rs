use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::sync::Arc;

use ryeos_engine::contracts::{InstanceViolationCode, SignatureEnvelope};
use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::item_resolution::parse_signature_header;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::trust::TrustStore;

use crate::manifest::{
    derive_provides_kinds, materialize_manifest, parse_current_manifest_body, BundleManifestSource,
};

mod structure;

use structure::{collect_files_recursive, is_runtime_support_file, validate_regular_tree};

const IDENTITY_COMPOSER: &str = "handler:ryeos/core/identity";

/// Severity of a preflight validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightIssueSeverity {
    /// Blocking — bundle verification fails.
    Error,
    /// Non-blocking — informational for authors.
    Warning,
}

/// A single validation issue found during bundle preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightIssue {
    /// Relative path of the item within the bundle (e.g. `tools/my_tool.py`).
    pub item_path: String,
    /// Whether this blocks verification.
    pub severity: PreflightIssueSeverity,
    /// Machine-readable violation code.
    pub code: InstanceViolationCode,
    /// Dot-separated path within the item value (e.g. `launch.mode`).
    pub path: String,
    /// Human-readable description of what was expected.
    pub expected: String,
    /// Human-readable description of what was found.
    pub found: String,
}

/// Warnings collected during a successful preflight verification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightReport {
    /// Non-blocking issues (e.g. unknown fields with strict_fields: warn).
    pub warnings: Vec<PreflightIssue>,
}

impl PreflightReport {
    /// Returns true if there are no warnings.
    pub fn is_clean(&self) -> bool {
        self.warnings.is_empty()
    }
}

/// Collect instance validation issues for identity-composed kinds.
///
/// Returns an empty vec for non-identity composers (extends-based kinds
/// are validated post-composition in the resolution pipeline).
fn collect_identity_contract_issues(
    item_rel: &Path,
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    parsed: &serde_json::Value,
) -> Vec<PreflightIssue> {
    if kind_schema.composer != IDENTITY_COMPOSER {
        return Vec::new();
    }

    let item_path = item_rel.to_string_lossy().to_string();

    let report = kind_schema
        .composed_value_contract
        .validate_instance(parsed);

    let mut issues = Vec::with_capacity(report.errors.len() + report.warnings.len());
    for v in &report.errors {
        issues.push(PreflightIssue {
            item_path: item_path.clone(),
            severity: PreflightIssueSeverity::Error,
            code: v.code.clone(),
            path: v.path.clone(),
            expected: v.expected.clone(),
            found: v.found.clone(),
        });
    }
    for v in &report.warnings {
        issues.push(PreflightIssue {
            item_path: item_path.clone(),
            severity: PreflightIssueSeverity::Warning,
            code: v.code.clone(),
            path: v.path.clone(),
            expected: v.expected.clone(),
            found: v.found.clone(),
        });
    }
    issues
}

/// Format a preflight issue as a human-readable string.
fn format_preflight_issue(issue: &PreflightIssue) -> String {
    let label = match issue.severity {
        PreflightIssueSeverity::Error => "contract violation",
        PreflightIssueSeverity::Warning => "contract warning",
    };
    format!(
        "{}: {} [{}] {}: expected {}, found {}",
        issue.item_path, label, issue.code, issue.path, issue.expected, issue.found,
    )
}

pub fn preflight_verify_bundle(source_path: &Path, app_root: &Path) -> Result<()> {
    // Runtime uses signed bundle registrations under `.ai/node/bundles/*.yaml`,
    // not raw `.ai/bundles/*` scans. Generic bundle install verifies against
    // the current verified installed set so new bundles can depend on already-
    // installed bundles without ambient unregistered state affecting preflight.
    let installed_bundle_roots: Vec<PathBuf> =
        crate::installed::load_installed_bundle_records(app_root)
            .context("preflight: load installed bundle registrations")?
            .into_iter()
            .map(|record| record.bundle_root)
            .collect();
    let node_config_root = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).config();
    let sandbox = Arc::new(
        ryeos_engine::sandbox::SandboxRuntime::load(app_root)
            .context("preflight: load node sandbox policy")?,
    );
    preflight_verify_bundle_in_context(
        source_path,
        &installed_bundle_roots,
        &node_config_root,
        sandbox,
    )
}

pub fn preflight_verify_bundle_in_context(
    source_path: &Path,
    dependency_bundle_roots: &[PathBuf],
    node_config_root: &Path,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<()> {
    let _report = preflight_verify_bundle_report_in_context(
        source_path,
        dependency_bundle_roots,
        node_config_root,
        sandbox,
    )?;
    Ok(())
}

/// Verify a candidate bundle whose prospective installed name may differ from
/// its source directory name.
///
/// Install planning supplies the name explicitly so manifest identity is
/// checked against the requested registration name while retaining the source
/// freshness check used for author trees.
pub fn preflight_verify_named_bundle_in_context(
    source_path: &Path,
    expected_bundle_name: &str,
    dependency_bundle_roots: &[PathBuf],
    node_config_root: &Path,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<()> {
    let _report = preflight_verify_named_bundle_report_in_context(
        source_path,
        expected_bundle_name,
        dependency_bundle_roots,
        node_config_root,
        sandbox,
    )?;
    Ok(())
}

/// Verify an author tree against its declared effective bundle id while also
/// returning non-blocking contract warnings. This is the authoring/doctor
/// counterpart to install planning: a checkout directory may be named
/// differently from the bundle it publishes.
pub fn preflight_verify_named_bundle_report_in_context(
    source_path: &Path,
    expected_bundle_name: &str,
    dependency_bundle_roots: &[PathBuf],
    node_config_root: &Path,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<PreflightReport> {
    preflight_verify_bundle_in_context_inner(
        source_path,
        dependency_bundle_roots,
        node_config_root,
        Some(expected_bundle_name),
        true,
        sandbox,
    )
}

/// Re-verify a completed install staging tree using the final bundle identity.
///
/// Staging directories deliberately have temporary filesystem names, so the
/// expected bundle name is supplied separately for manifest identity checks.
/// All other verification reads exclusively from the completed staging tree.
pub fn preflight_verify_bundle_staging_in_context(
    staging_path: &Path,
    expected_bundle_name: &str,
    dependency_bundle_roots: &[PathBuf],
    node_config_root: &Path,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<()> {
    let _report = preflight_verify_bundle_in_context_inner(
        staging_path,
        dependency_bundle_roots,
        node_config_root,
        Some(expected_bundle_name),
        false,
        sandbox,
    )?;
    Ok(())
}

/// Like [`preflight_verify_bundle_in_context`] but returns a
/// [`PreflightReport`] containing non-blocking warnings on success.
///
/// CLI callers should use this to surface contract warnings to the user
/// without failing verification.
pub fn preflight_verify_bundle_report_in_context(
    source_path: &Path,
    dependency_bundle_roots: &[PathBuf],
    node_config_root: &Path,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<PreflightReport> {
    // The core logic below populates `failures` (blocking) and
    // `warnings` (non-blocking). We lift the loop into this function
    // so both public APIs share a single implementation.
    preflight_verify_bundle_in_context_inner(
        source_path,
        dependency_bundle_roots,
        node_config_root,
        None,
        true,
        sandbox,
    )
}

/// Core preflight logic shared by both public entry points.
fn preflight_verify_bundle_in_context_inner(
    source_path: &Path,
    dependency_bundle_roots: &[PathBuf],
    node_config_root: &Path,
    expected_bundle_name: Option<&str>,
    check_source_mtime: bool,
    sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
) -> Result<PreflightReport> {
    let ai_dir = source_path.join(ryeos_engine::AI_DIR);
    validate_regular_tree(&ai_dir).with_context(|| {
        format!(
            "preflight: source has an invalid .ai control tree at {}",
            source_path.display()
        )
    })?;

    let mut schema_roots = Vec::new();
    for root in dependency_bundle_roots {
        let kinds_dir = root
            .join(ryeos_engine::AI_DIR)
            .join(ryeos_engine::KIND_SCHEMAS_DIR);
        if kinds_dir.is_dir() {
            schema_roots.push(kinds_dir);
        }
    }
    let bundle_kinds = ai_dir.join(ryeos_engine::KIND_SCHEMAS_DIR);
    if bundle_kinds.is_dir() {
        schema_roots.push(bundle_kinds.clone());
    }
    if schema_roots.is_empty() {
        bail!(
            "preflight: no kind schemas in installed bundles or candidate bundle ({})",
            bundle_kinds.display()
        );
    }

    let trust_store =
        TrustStore::load(None, node_config_root).context("preflight: load node trust store")?;
    if trust_store.is_empty() {
        bail!(
            "preflight: node trust store is empty — run `ryeos init` to \
             pin the platform author key, or `ryeos trust pin <fingerprint>` \
             to pin a third-party publisher"
        );
    }

    ryeos_engine::binary_resolver::verify_bundle_executor_manifest(source_path, &trust_store)
        .context("preflight: verify node-authorized bundle executables")?;

    let kinds = KindRegistry::load_base(&schema_roots, &trust_store)
        .context("preflight: load kind schemas")?;

    let mut parser_search_roots: Vec<(PathBuf, ryeos_engine::resolution::TrustClass)> = Vec::new();
    let mut seen_roots: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let push_unique = |path: PathBuf,
                       trust: ryeos_engine::resolution::TrustClass,
                       roots: &mut Vec<(PathBuf, ryeos_engine::resolution::TrustClass)>,
                       seen: &mut std::collections::HashSet<PathBuf>| {
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            roots.push((path, trust));
        }
    };
    // Dependency bundles are trusted bundle roots selected by the caller's
    // verification plan. `init --source` passes source-bundle dependency
    // roots; generic install passes currently installed bundle roots.
    for root in dependency_bundle_roots {
        push_unique(
            root.clone(),
            ryeos_engine::resolution::TrustClass::TrustedBundle,
            &mut parser_search_roots,
            &mut seen_roots,
        );
    }
    // The candidate is a prospective installed bundle and therefore receives
    // bundle trust semantics during admission. It is last so dependency
    // content retains the same deterministic precedence as boot.
    push_unique(
        source_path.to_path_buf(),
        ryeos_engine::resolution::TrustClass::TrustedBundle,
        &mut parser_search_roots,
        &mut seen_roots,
    );

    let old_trust = source_path
        .join(ryeos_engine::AI_DIR)
        .join("config/keys/trusted");
    if old_trust.is_dir() {
        tracing::warn!(
            path = %old_trust.display(),
            "bundle ships a old `.ai/config/keys/trusted/` dir which is \
             ignored — pin the publisher key with `ryeos trust pin <fingerprint>` \
             instead"
        );
    }
    let (parser_tools, _dups) = ParserRegistry::load_base(
        &parser_search_roots
            .iter()
            .map(|(p, _)| p.clone())
            .collect::<Vec<_>>(),
        &trust_store,
        &kinds,
    )
    .context("preflight: load parser tools")?;
    let handler_registry = HandlerRegistry::load_base(&parser_search_roots, &trust_store, sandbox)
        .context("preflight: load handler descriptors")?;
    let parser_dispatcher = ParserDispatcher::new(parser_tools, Arc::new(handler_registry));

    let mut failures: Vec<String> = Vec::new();
    let mut warnings: Vec<PreflightIssue> = Vec::new();
    for failure in collect_node_config_failures(&ai_dir, &trust_store) {
        failures.push(failure);
    }
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
        if let Err(e) = collect_files_recursive(&kind_dir, &mut files) {
            failures.push(format!(
                "{}: scan failed: {e}",
                kind_dir
                    .strip_prefix(&ai_dir)
                    .unwrap_or(&kind_dir)
                    .display()
            ));
            continue;
        }

        for file_path in files {
            let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if kind_schema.spec_for(&format!(".{ext}")).is_none() {
                continue;
            }

            let rel = file_path.strip_prefix(&ai_dir).unwrap_or(&file_path);
            if is_runtime_support_file(kind_schema.directory.as_str(), rel) {
                continue;
            }

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
                            "{}: signer {} not in node trust store \
                             (run `ryeos trust pin {}` to trust this publisher)",
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

            // ── Instance validation (identity-composed kinds only) ──
            // For kinds using the identity composer, the parsed value IS
            // the composed value, so we can validate it directly against
            // the kind's `composed_value_contract`. Extends-based kinds
            // are validated post-composition in the resolution pipeline
            // (Slice 2), so we skip them here.
            for issue in collect_identity_contract_issues(rel, kind_schema, &parsed) {
                match issue.severity {
                    PreflightIssueSeverity::Error => {
                        failures.push(format_preflight_issue(&issue));
                    }
                    PreflightIssueSeverity::Warning => {
                        warnings.push(issue);
                    }
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

    match expected_bundle_name {
        Some(expected_bundle_name) => verify_manifest_signature_for_bundle_name(
            &ai_dir,
            source_path,
            expected_bundle_name,
            &trust_store,
            check_source_mtime,
        ),
        None => verify_manifest_signature(&ai_dir, source_path, &trust_store),
    }
    .context("preflight: bundle manifest verification")?;

    if !warnings.is_empty() {
        tracing::info!(
            source = %source_path.display(),
            warnings_count = warnings.len(),
            "preflight verification passed with warnings"
        );
    } else {
        tracing::info!(
            source = %source_path.display(),
            "preflight verification passed"
        );
    }
    Ok(PreflightReport { warnings })
}

pub fn verify_manifest_signature(
    ai_dir: &Path,
    source_path: &Path,
    trust_store: &TrustStore,
) -> Result<()> {
    let expected_bundle_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("source_path has no directory name"))?;
    verify_manifest_signature_for_bundle_name(
        ai_dir,
        source_path,
        expected_bundle_name,
        trust_store,
        true,
    )
}

fn verify_manifest_signature_for_bundle_name(
    ai_dir: &Path,
    source_path: &Path,
    expected_bundle_name: &str,
    trust_store: &TrustStore,
    check_source_mtime: bool,
) -> Result<()> {
    let manifest_path = ai_dir.join("manifest.yaml");
    let manifest_meta = match fs::symlink_metadata(&manifest_path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "bundle '{}' has no regular signed .ai/manifest.yaml; installable bundles require a published manifest",
                expected_bundle_name
            )
        }
        Err(e) => {
            return Err(e).with_context(|| format!("stat manifest {}", manifest_path.display()));
        }
    };
    if !manifest_meta.file_type().is_file() {
        bail!("manifest.yaml must be a regular file, not a symlink or directory");
    }

    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("read manifest {}", manifest_path.display()))?;

    let envelope = SignatureEnvelope {
        prefix: "#".to_string(),
        suffix: None,
        after_shebang: false,
    };

    let sig_header = parse_signature_header(&raw, &envelope).ok_or_else(|| {
        anyhow::anyhow!(
            "manifest.yaml has no valid signature header — \
             it must be generated by the publish pipeline"
        )
    })?;

    if !trust_store.is_trusted(&sig_header.signer_fingerprint) {
        bail!(
            "manifest.yaml: signer {} not in node trust store \
             (run `ryeos trust pin {}` to trust this publisher)",
            sig_header.signer_fingerprint,
            sig_header.signer_fingerprint
        );
    }
    let publisher_trust_path = source_path.join("PUBLISHER_TRUST.toml");
    if publisher_trust_path.is_file() {
        let raw_trust_doc = fs::read_to_string(&publisher_trust_path)
            .with_context(|| format!("read {}", publisher_trust_path.display()))?;
        let trust_doc = ryeos_engine::trust::PublisherTrustDoc::parse(&raw_trust_doc)
            .map_err(|err| anyhow::anyhow!("invalid {}: {err}", publisher_trust_path.display()))?;
        if trust_doc.fingerprint == sig_header.signer_fingerprint {
            if let Some(store_owner) = trust_store
                .get(&sig_header.signer_fingerprint)
                .and_then(|signer| signer.label.as_deref())
            {
                if store_owner != trust_doc.owner {
                    // The owner label is informational — the trust anchor is the
                    // fingerprint, which is already verified as trusted above and
                    // checked cryptographically below. A label rename (e.g.
                    // `official-publisher` -> `ryeos-official`) must not brick a
                    // node whose pinned key still matches. Warn, don't fail.
                    tracing::warn!(
                        fingerprint = %sig_header.signer_fingerprint,
                        bundle_owner = %trust_doc.owner,
                        store_owner = %store_owner,
                        "publisher trust owner label differs from the pinned trust doc \
                         (informational only — the key fingerprint is what's trusted)"
                    );
                }
            }
        }
    }

    ryeos_engine::trust::verify_item_signature(&raw, &sig_header, &envelope, trust_store)
        .map_err(|e| anyhow::anyhow!("manifest.yaml signature verification failed: {e}"))?;

    let body = lillux::signature::strip_signature_lines(&raw);
    let manifest = parse_current_manifest_body(&body, &manifest_path)?;
    manifest.runtime_authority.validate().map_err(|e| {
        anyhow::anyhow!(
            "invalid `runtime_authority` declaration in {}: {e}",
            manifest_path.display()
        )
    })?;

    if manifest.name != expected_bundle_name {
        bail!(
            "manifest identity mismatch: manifest.yaml name is '{}' but \
             bundle directory is '{}' — the manifest must be regenerated",
            manifest.name,
            expected_bundle_name
        );
    }

    let actual_kinds =
        derive_provides_kinds(ai_dir).context("derive actual provides_kinds from kind schemas")?;
    let mut claimed = manifest.provides_kinds.clone();
    claimed.sort();
    claimed.dedup();
    let mut actual_normalized = actual_kinds.clone();
    actual_normalized.sort();
    actual_normalized.dedup();
    if claimed != actual_normalized {
        bail!(
            "manifest provides_kinds mismatch: manifest claims {:?} but \
             actual kind schemas on disk provide {:?} — regenerate the manifest \
             with the publish pipeline",
            manifest.provides_kinds,
            actual_kinds
        );
    }

    let source_manifest_path = ai_dir.join("manifest.source.yaml");
    let source_meta = match fs::symlink_metadata(&source_manifest_path) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| {
                format!("stat manifest source {}", source_manifest_path.display())
            });
        }
    };
    if let Some(source_meta) = source_meta {
        if !source_meta.file_type().is_file() {
            bail!("manifest.source.yaml must be a regular file, not a symlink or directory");
        }
        // Copying into an install staging tree does not preserve source mtimes,
        // so completed-staging verification relies on the exact materialized
        // manifest comparison below instead of temporary copy order.
        if check_source_mtime && source_meta.modified()? > manifest_meta.modified()? {
            bail!(
                "manifest.yaml is older than manifest.source.yaml — regenerate and re-sign manifest.yaml"
            );
        }
        let source_raw = fs::read_to_string(&source_manifest_path)
            .with_context(|| format!("read manifest source {}", source_manifest_path.display()))?;
        let source_body = lillux::signature::strip_signature_lines(&source_raw);
        let source_manifest: BundleManifestSource = serde_yaml::from_str(&source_body)
            .with_context(|| format!("parse manifest source {}", source_manifest_path.display()))?;
        let expected_manifest = materialize_manifest(source_manifest, ai_dir, expected_bundle_name)
            .context("materialize manifest.source.yaml for staleness check")?;
        if manifest != expected_manifest {
            bail!(
                "manifest.yaml is stale relative to manifest.source.yaml — regenerate and re-sign manifest.yaml with the publish pipeline"
            );
        }
    }

    tracing::info!(
        name = %manifest.name,
        provides_kinds = ?manifest.provides_kinds,
        "manifest verification passed"
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightNodeBundleRecord {
    #[allow(dead_code)]
    kind: Option<String>,
    #[allow(dead_code)]
    path: PathBuf,
    #[allow(dead_code)]
    #[serde(default)]
    command_registration_caps: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightNodeRouteRecord {
    id: String,
    path: String,
    methods: std::collections::HashSet<String>,
    auth: String,
    #[allow(dead_code)]
    #[serde(default)]
    auth_config: Option<serde_json::Value>,
    #[allow(dead_code)]
    #[serde(default)]
    limits: PreflightRawLimits,
    response: PreflightRawResponseSpec,
    #[allow(dead_code)]
    #[serde(default)]
    execute: Option<PreflightRawExecute>,
    #[allow(dead_code)]
    #[serde(default)]
    request: PreflightRawRequest,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightRawLimits {
    #[allow(dead_code)]
    #[serde(default = "default_body_max")]
    body_bytes_max: u64,
    #[allow(dead_code)]
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[allow(dead_code)]
    #[serde(default = "default_concurrent_max")]
    concurrent_max: u32,
}

impl Default for PreflightRawLimits {
    fn default() -> Self {
        Self {
            body_bytes_max: default_body_max(),
            timeout_ms: default_timeout(),
            concurrent_max: default_concurrent_max(),
        }
    }
}

fn default_body_max() -> u64 {
    1_048_576
}

fn default_timeout() -> u64 {
    30_000
}

fn default_concurrent_max() -> u32 {
    100
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightRawResponseSpec {
    mode: String,
    #[allow(dead_code)]
    #[serde(default)]
    source: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    source_config: serde_json::Value,
    #[allow(dead_code)]
    #[serde(default)]
    status: Option<u16>,
    #[allow(dead_code)]
    #[serde(default)]
    content_type: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    body_b64: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightRawExecute {
    item_ref: String,
    #[allow(dead_code)]
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PreflightRawRequest {
    #[allow(dead_code)]
    #[serde(default)]
    body: PreflightRawRequestBody,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PreflightRawRequestBody {
    #[default]
    None,
    Raw,
    Text,
    Json,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandRecord {
    tokens: Vec<String>,
    description: String,
    #[serde(default)]
    aliases: Vec<PreflightCommandAliasRecord>,
    #[allow(dead_code)]
    #[serde(default)]
    help: Option<PreflightCommandHelp>,
    #[allow(dead_code)]
    #[serde(default)]
    arguments: Vec<PreflightCommandArgument>,
    #[serde(default)]
    forms: Vec<PreflightCommandForm>,
    #[allow(dead_code)]
    #[serde(default)]
    defaults: std::collections::BTreeMap<String, serde_json::Value>,
    #[allow(dead_code)]
    #[serde(default)]
    parameter_binding: Option<PreflightCommandParameterBinding>,
    #[allow(dead_code)]
    #[serde(default)]
    project: Option<PreflightCommandProject>,
    #[allow(dead_code)]
    #[serde(default)]
    control_flags: Vec<PreflightCommandControlFlag>,
    dispatch: PreflightCommandDispatch,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandControlFlag {
    #[allow(dead_code)]
    flag: String,
    #[allow(dead_code)]
    help: String,
    // Routing destination — validated against the real `ControlFlagBinding`
    // enum by the command model at load; preflight only checks structure.
    #[allow(dead_code)]
    binding: String,
    #[allow(dead_code)]
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandAliasRecord {
    tokens: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    description: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    deprecated: Option<bool>,
    #[allow(dead_code)]
    #[serde(default)]
    replacement_tokens: Option<Vec<String>>,
    #[allow(dead_code)]
    #[serde(default)]
    removed_in: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandHelp {
    #[allow(dead_code)]
    #[serde(default)]
    usage: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    examples: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandArgument {
    name: String,
    #[allow(dead_code)]
    #[serde(default)]
    kind: PreflightCommandArgumentKind,
    positional: usize,
    #[allow(dead_code)]
    #[serde(default)]
    required: bool,
    #[allow(dead_code)]
    #[serde(default)]
    arity: PreflightCommandArgumentArity,
    #[allow(dead_code)]
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandForm {
    #[serde(default)]
    slots: Vec<PreflightCommandSlot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandSlot {
    field: String,
    #[allow(dead_code)]
    #[serde(default)]
    matcher: PreflightCommandArgumentKind,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PreflightCommandArgumentKind {
    #[default]
    String,
    CanonicalRef,
    Path,
    Json,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PreflightCommandArgumentArity {
    #[default]
    One,
    Optional,
    Variadic,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandParameterBinding {
    #[allow(dead_code)]
    mode: PreflightCommandParameterBindingMode,
    #[allow(dead_code)]
    #[serde(default)]
    input_flag: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    single_json_object_arg: bool,
    #[allow(dead_code)]
    #[serde(default)]
    flag_key_normalization: PreflightFlagKeyNormalization,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PreflightCommandParameterBindingMode {
    #[default]
    None,
    TailObject,
    SchemaObject,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PreflightFlagKeyNormalization {
    #[default]
    HyphenToUnderscore,
    Preserve,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightCommandProject {
    #[allow(dead_code)]
    #[serde(default)]
    resolution: PreflightCommandProjectResolution,
    #[allow(dead_code)]
    #[serde(default)]
    default: PreflightCommandProjectDefault,
    #[allow(dead_code)]
    #[serde(default)]
    no_project_flag: bool,
    #[allow(dead_code)]
    #[serde(default)]
    request_project_path: bool,
    #[allow(dead_code)]
    #[serde(default)]
    bind_parameter: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PreflightCommandProjectResolution {
    #[default]
    None,
    Required,
    Optional,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PreflightCommandProjectDefault {
    #[default]
    None,
    DiscoverUpwardAi,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PreflightCommandDispatch {
    Group,
    LocalHandler {
        handler: String,
        #[allow(dead_code)]
        #[serde(default)]
        bootstrap: bool,
    },
    DirectExecuteItemRef {
        item_ref_arg: String,
        #[allow(dead_code)]
        #[serde(default)]
        availability: PreflightCommandAvailability,
    },
    ExecuteRef {
        execute: String,
        #[allow(dead_code)]
        #[serde(default)]
        availability: PreflightCommandAvailability,
    },
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PreflightCommandAvailability {
    #[default]
    Auto,
    Daemon,
    Offline,
    Both,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightHostedNodePolicyRecord {
    #[allow(dead_code)]
    version: String,
    schema_version: String,
    #[allow(dead_code)]
    description: String,
    transport: PreflightHostedNodeTransportPolicy,
    admission: PreflightHostedNodeAdmissionPolicy,
    descriptor: PreflightHostedNodeDescriptorPolicy,
    authorization: PreflightHostedNodeAuthorizationPolicy,
    operations: PreflightHostedNodeOperationsPolicy,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightHostedNodeTransportPolicy {
    public_https_required: bool,
    #[allow(dead_code)]
    loopback_http_allowed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightHostedNodeAdmissionPolicy {
    mode: String,
    token_ttl_secs: u64,
    reject_wildcard_scopes: bool,
    token_delivery: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightHostedNodeDescriptorPolicy {
    require_live_identity_match: bool,
    #[allow(dead_code)]
    #[serde(default)]
    advertised_capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightHostedNodeAuthorizationPolicy {
    authority: String,
    central_bearer_tokens_allowed: bool,
    implicit_cross_node_authority_allowed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightHostedNodeOperationsPolicy {
    #[allow(dead_code)]
    audit_admission_events: bool,
    #[allow(dead_code)]
    audit_grant_changes: bool,
    #[allow(dead_code)]
    prefer_isolated_node_per_principal: bool,
    shared_daemon_multitenancy_enabled: bool,
}

fn collect_node_config_failures(ai_dir: &Path, trust_store: &TrustStore) -> Vec<String> {
    let node_dir = ai_dir.join("node");
    let mut failures = Vec::new();
    let metadata = match fs::symlink_metadata(&node_dir) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return failures,
        Err(err) => {
            failures.push(format!(
                "node: node config scan failed: failed to stat {}: {err}",
                node_dir.display()
            ));
            return failures;
        }
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        failures.push(format!(
            "node: node config scan failed: node config scan encountered symlink at {}",
            node_dir.display()
        ));
        return failures;
    }
    if !file_type.is_dir() {
        return failures;
    }

    let mut files = Vec::new();
    if let Err(e) = collect_node_config_files_recursive(&node_dir, &mut files) {
        failures.push(format!("node: node config scan failed: {e}"));
        return failures;
    }

    for file_path in files {
        let rel = file_path.strip_prefix(ai_dir).unwrap_or(&file_path);
        let rel_str = rel.to_string_lossy();
        if rel_str.starts_with("node/engine/kinds/") {
            continue;
        }

        let Some(section) = file_path
            .strip_prefix(&node_dir)
            .ok()
            .and_then(|path| path.components().next())
            .and_then(|component| component.as_os_str().to_str())
        else {
            continue;
        };

        if matches!(section, "verbs" | "aliases") {
            failures.push(format!(
                "{}: legacy node config section '.ai/node/{}' is no longer supported; use '.ai/node/commands'",
                rel.display(),
                section
            ));
            continue;
        }

        if section == "command_registration" {
            failures.push(format!(
                "{}: command registration policy is node-owned seed/system config; normal bundles may not ship '.ai/node/command_registration'",
                rel.display()
            ));
            continue;
        }

        if !matches!(section, "bundles" | "hosted" | "routes" | "commands") {
            continue;
        }

        match validate_node_config_item(&file_path, &node_dir.join(section), section, trust_store) {
            Ok(()) => {}
            Err(e) => failures.push(format!(
                "{}: node config validation failed: {e}",
                rel.display()
            )),
        }
    }

    failures
}

fn collect_node_config_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(dir)
        .with_context(|| format!("failed to stat node config path {}", dir.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("node config scan encountered symlink at {}", dir.display());
    }
    if !file_type.is_dir() {
        return Ok(());
    }

    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .with_context(|| format!("failed to read node config dir {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to stat node config path {}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            bail!("node config scan encountered symlink at {}", path.display());
        }
        if file_type.is_dir() {
            collect_node_config_files_recursive(&path, out)?;
        } else if file_type.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn validate_node_config_item(
    file_path: &Path,
    section_root: &Path,
    expected_section: &str,
    trust_store: &TrustStore,
) -> Result<()> {
    if !file_path.is_file() || file_path.is_symlink() {
        bail!("not a regular file (symlinks rejected)");
    }
    let ext = file_path.extension().and_then(|ext| ext.to_str());
    if ext != Some("yaml") && ext != Some("yml") {
        bail!("not a .yaml or .yml node config item");
    }

    let content = fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;
    let envelope = SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let header = parse_signature_header(&content, &envelope)
        .context("node config item has no valid signature line")?;
    if !trust_store.is_trusted(&header.signer_fingerprint) {
        bail!(
            "signer {} not in node trust store",
            header.signer_fingerprint
        );
    }
    ryeos_engine::trust::verify_item_signature(&content, &header, &envelope, trust_store)
        .context("signature verification failed")?;

    if !file_path.starts_with(section_root) {
        bail!("not under expected node config section directory");
    }

    let body_str = lillux::signature::strip_signature_lines(&content);
    let body: serde_json::Value =
        serde_yaml::from_str(&body_str).context("failed to parse YAML body")?;
    for forbidden in ["category", "section"] {
        if body.get(forbidden).is_some() {
            bail!(
                "declares legacy structural field '{}' (section/category are derived from path and must not be in node YAML)",
                forbidden
            );
        }
    }

    match expected_section {
        "bundles" => validate_node_bundle_record(file_path, &body),
        "hosted" => validate_hosted_node_policy(file_path, &body),
        "routes" => validate_node_route_record(&body),
        "commands" => validate_node_command_record(file_path, section_root, &body),
        _ => Ok(()),
    }
}

fn validate_node_bundle_record(file_path: &Path, body: &serde_json::Value) -> Result<()> {
    let record: PreflightNodeBundleRecord = serde_json::from_value(body.clone())
        .context("failed to parse bundle node-config record")?;
    if !record.path.is_absolute() {
        bail!("bundle record missing absolute 'path' field");
    }
    if file_path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        != Some("bundles")
    {
        bail!("bundle records must be flat under node/bundles");
    }
    Ok(())
}

fn validate_node_route_record(body: &serde_json::Value) -> Result<()> {
    let record: PreflightNodeRouteRecord =
        serde_json::from_value(body.clone()).context("failed to parse route node-config record")?;
    if record.id.is_empty() {
        bail!("route record missing non-empty 'id'");
    }
    if record.path.is_empty() {
        bail!("route record missing non-empty 'path'");
    }
    if record.methods.is_empty() {
        bail!("route record has empty or missing 'methods' list");
    }
    if record.auth.is_empty() {
        bail!("route record missing non-empty 'auth'");
    }
    if record.response.mode.is_empty() {
        bail!("route response missing non-empty 'mode'");
    }
    // `response.source` is mode-specific: some modes (e.g. `handler`) use
    // canonical refs while others (e.g. `event_stream`) use registry source
    // names such as `dispatch_launch`/`thread_events`. The owning subsystem is
    // the route compiler / `ResponseModeRegistry`, so preflight only performs
    // mode-independent structural validation here and must not parse the value
    // as a canonical ref.
    if let Some(source) = record.response.source.as_deref() {
        if source.trim().is_empty() {
            bail!("route response source must be non-empty when present");
        }
    }
    if let Some(execute) = &record.execute {
        ryeos_engine::canonical_ref::CanonicalRef::parse(&execute.item_ref)
            .with_context(|| format!("invalid route execute item_ref '{}'", execute.item_ref))?;
    }
    Ok(())
}

fn validate_node_command_record(
    file_path: &Path,
    section_root: &Path,
    body: &serde_json::Value,
) -> Result<()> {
    let record: PreflightCommandRecord = serde_json::from_value(body.clone())
        .context("failed to parse command node-config record")?;
    if body.get("name").is_some() {
        bail!("command record declares legacy structural field 'name'");
    }
    let command_id = file_path
        .strip_prefix(section_root)
        .context("failed to derive command id from path")?;
    let mut command_id_path = command_id.to_path_buf();
    command_id_path.set_extension("");
    let command_name = command_id_path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(part) => part
                .to_str()
                .map(|s| s.to_string())
                .context("command path contains non-UTF-8 segment"),
            _ => bail!("command path contains non-normal segment"),
        })
        .collect::<Result<Vec<_>>>()?
        .join("/");
    if command_name.is_empty() {
        bail!("command record has empty path-derived id");
    }
    let stem = file_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("command record has no filename stem")?;
    for segment in command_name.split('/') {
        if !is_valid_command_name(segment) {
            bail!(
                "invalid command path segment '{}': must match ^[a-z][a-z0-9-]*$",
                segment
            );
        }
    }
    if !is_valid_command_name(stem) {
        bail!(
            "invalid command name '{}': must match ^[a-z][a-z0-9-]*$",
            stem
        );
    }
    if record.description.is_empty() {
        bail!("command record missing non-empty 'description'");
    }
    validate_preflight_command_tokens(&command_name, &record.tokens)?;
    validate_preflight_command_dispatch(&command_name, &record.dispatch)?;
    for (arg_idx, argument) in record.arguments.iter().enumerate() {
        if argument.name.is_empty() {
            bail!("{} arguments[{arg_idx}] has empty name", command_name);
        }
        if argument.positional == 0 {
            bail!(
                "{} arguments[{arg_idx}] positional must be greater than zero",
                command_name
            );
        }
    }
    for (form_idx, form) in record.forms.iter().enumerate() {
        for (slot_idx, slot) in form.slots.iter().enumerate() {
            if slot.field.is_empty() {
                bail!(
                    "{} forms[{form_idx}].slots[{slot_idx}] has empty field",
                    command_name
                );
            }
        }
    }
    for (idx, alias) in record.aliases.iter().enumerate() {
        validate_preflight_command_tokens(
            &format!("{} aliases[{idx}]", command_name),
            &alias.tokens,
        )?;
    }
    Ok(())
}

fn validate_hosted_node_policy(file_path: &Path, body: &serde_json::Value) -> Result<()> {
    let record: PreflightHostedNodePolicyRecord = serde_json::from_value(body.clone())
        .context("failed to parse hosted node-config policy")?;
    if file_path.file_stem().and_then(|stem| stem.to_str()) != Some("policy") {
        bail!("hosted-node policy filename must be 'policy'");
    }
    if record.schema_version != "1.0.0" {
        bail!("hosted-node policy schema_version must be '1.0.0'");
    }
    if !record.transport.public_https_required {
        bail!("hosted-node policy must require public HTTPS");
    }
    if record.admission.mode != "one_time_token" {
        bail!("hosted-node admission.mode must be 'one_time_token'");
    }
    if record.admission.token_ttl_secs == 0 {
        bail!("hosted-node admission.token_ttl_secs must be greater than zero");
    }
    if !record.admission.reject_wildcard_scopes {
        bail!("hosted-node policy must reject wildcard admission scopes");
    }
    if record.admission.token_delivery != "out_of_band" {
        bail!("hosted-node admission.token_delivery must be 'out_of_band'");
    }
    if !record.descriptor.require_live_identity_match {
        bail!("hosted-node policy must require live descriptor identity matching");
    }
    if record.authorization.authority != "target_node_authorized_keys" {
        bail!("hosted-node authorization.authority must be 'target_node_authorized_keys'");
    }
    if record.authorization.central_bearer_tokens_allowed {
        bail!("hosted-node policy must not allow central bearer tokens");
    }
    if record.authorization.implicit_cross_node_authority_allowed {
        bail!("hosted-node policy must not allow implicit cross-node authority");
    }
    if record.operations.shared_daemon_multitenancy_enabled {
        bail!("hosted-node policy must not enable shared daemon multitenancy");
    }
    Ok(())
}

fn validate_preflight_command_tokens(name: &str, tokens: &[String]) -> Result<()> {
    if tokens.is_empty() {
        bail!("command '{}' has empty tokens list", name);
    }
    for token in tokens {
        if token.is_empty() {
            bail!("command '{}' has empty token in tokens list", name);
        }
        if token.starts_with('-') {
            bail!("command '{}' has dash-prefixed token '{}'", name, token);
        }
    }
    Ok(())
}

fn validate_preflight_command_dispatch(
    name: &str,
    dispatch: &PreflightCommandDispatch,
) -> Result<()> {
    match dispatch {
        PreflightCommandDispatch::Group => {}
        PreflightCommandDispatch::LocalHandler { handler, .. } if handler.is_empty() => {
            bail!("command '{}' has empty local handler", name);
        }
        PreflightCommandDispatch::LocalHandler { .. } => {}
        PreflightCommandDispatch::DirectExecuteItemRef { item_ref_arg, .. }
            if item_ref_arg.is_empty() =>
        {
            bail!("command '{}' has empty item_ref_arg", name);
        }
        PreflightCommandDispatch::DirectExecuteItemRef { .. } => {}
        PreflightCommandDispatch::ExecuteRef { execute, .. } => {
            ryeos_engine::canonical_ref::CanonicalRef::parse(execute).with_context(|| {
                format!("invalid execute ref '{execute}' in command record '{name}'")
            })?;
        }
    }
    Ok(())
}

fn is_valid_command_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;
    use std::os::unix::fs::PermissionsExt;

    /// Regression: the shipped execute command declares `control_flags`; the
    /// preflight command-record schema must accept it (it previously rejected
    /// the unknown field, breaking `ryeos init`).
    #[test]
    fn preflight_accepts_execute_control_flags() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| {
                p.join("bundles/core/.ai/node/commands/execute.yaml")
                    .is_file()
            })
            .expect("workspace root")
            .join("bundles/core/.ai/node/commands/execute.yaml");
        let raw = std::fs::read_to_string(&path).expect("read execute.yaml");
        let body: String = raw
            .lines()
            .filter(|l| !l.starts_with("# ryeos:signed:"))
            .collect::<Vec<_>>()
            .join("\n");
        let value: serde_json::Value = serde_yaml::from_str(&body).expect("yaml parse");
        let record: PreflightCommandRecord =
            serde_json::from_value(value).expect("preflight command record parse");
        assert_eq!(record.control_flags.len(), 8, "expected 8 control flags");
    }

    struct BundleLayout {
        _tmp: tempfile::TempDir,
        source: PathBuf,
        ai_dir: PathBuf,
        signing_key: SigningKey,
        node_config_root: PathBuf,
    }

    impl BundleLayout {
        fn new(name: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let source = tmp.path().join(name);
            let ai_dir = source.join(".ai");
            fs::create_dir_all(&ai_dir).unwrap();
            let signing_key = SigningKey::generate(&mut OsRng);

            let app_root = tmp.path().join("app");
            let node_config_root = app_root.join(".ai/config");
            let trust_dir = node_config_root.join("keys/trusted");
            fs::create_dir_all(&trust_dir).unwrap();

            ryeos_engine::trust::pin_key(
                &signing_key.verifying_key(),
                "test-publisher",
                &trust_dir,
                None,
            )
            .unwrap();

            Self {
                _tmp: tmp,
                source,
                ai_dir,
                signing_key,
                node_config_root,
            }
        }

        fn trust_store(&self) -> TrustStore {
            TrustStore::load(None, &self.node_config_root).unwrap()
        }

        fn add_kind_schema(&self, kind_name: &str) {
            let schema_dir = self.ai_dir.join("node/engine/kinds").join(kind_name);
            fs::create_dir_all(&schema_dir).unwrap();
            fs::write(
                schema_dir.join(format!("{kind_name}.kind-schema.yaml")),
                "kind: config\ndirectory: mykind\nextensions: []\n",
            )
            .unwrap();
        }

        fn write_signed_manifest(&self, body: &str) {
            let signed = lillux::signature::sign_content(body, &self.signing_key, "#", None);
            fs::write(self.ai_dir.join("manifest.yaml"), &signed).unwrap();
        }

        fn write_manifest_signed_by_other(&self, body: &str) {
            let other_key = SigningKey::generate(&mut OsRng);
            let signed = lillux::signature::sign_content(body, &other_key, "#", None);
            fs::write(self.ai_dir.join("manifest.yaml"), &signed).unwrap();
        }

        fn write_unsigned_manifest(&self, body: &str) {
            fs::write(self.ai_dir.join("manifest.yaml"), body).unwrap();
        }

        fn write_publisher_trust_doc(&self, owner: &str) {
            let vk = self.signing_key.verifying_key();
            let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
            let doc = ryeos_engine::trust::PublisherTrustDoc {
                public_key: format!("ed25519:{key_b64}"),
                fingerprint: ryeos_engine::trust::compute_fingerprint(&vk),
                owner: owner.to_string(),
            };
            fs::write(self.source.join("PUBLISHER_TRUST.toml"), doc.to_toml()).unwrap();
        }

        fn write_tampered_manifest(&self, original_body: &str) {
            let signed =
                lillux::signature::sign_content(original_body, &self.signing_key, "#", None);
            let tampered = signed.replace("version: '1.0'", "version: '9.9'");
            fs::write(self.ai_dir.join("manifest.yaml"), &tampered).unwrap();
        }

        fn sign_and_write(&self, rel: &str, body: &str) {
            let path = self.ai_dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let signed = lillux::signature::sign_content(body, &self.signing_key, "#", None);
            fs::write(path, signed).unwrap();
        }

        fn add_signed_kind_schema(&self, kind_name: &str, body: &str) {
            self.sign_and_write(
                &format!("node/engine/kinds/{kind_name}/{kind_name}.kind-schema.yaml"),
                body,
            );
        }

        fn add_test_parser_kind_schema(&self) {
            self.add_signed_kind_schema(
                "parser",
                &format!(
                    r##"location:
  directory: parsers
formats:
  - extensions: [".yaml", ".yml"]
    parser: parser:test/fixed/fixed
    signature:
      prefix: "#"
effective_trust:
  include_references: false
resolution: []
composer: {IDENTITY_COMPOSER}
composed_value_contract:
  root_type: mapping
  required: {{}}
"##
                ),
            );
        }

        fn add_test_item_kind_schema(&self, composer: &str, contract: &str) {
            self.add_signed_kind_schema(
                "mykind",
                &format!(
                    r##"location:
  directory: items
formats:
  - extensions: [".yaml", ".yml"]
    parser: parser:test/fixed/fixed
    signature:
      prefix: "#"
effective_trust:
  include_references: false
resolution: []
composer: {composer}
composed_value_contract:
{contract}
"##
                ),
            );
        }

        fn add_fixed_parser_descriptor(&self) {
            self.sign_and_write(
                "parsers/test/fixed/fixed.yaml",
                r#"version: "1.0.0"
description: "fixed parser for preflight tests"
handler: "handler:test/fixed-parser"
parser_api_version: 1
parser_config: {}
output_schema:
  root_type: mapping
  required: {}
"#,
            );
        }

        fn add_fixed_parser_handler(&self, value: serde_json::Value) {
            let triple = host_triple();
            let name = "fixed-parser";
            let bin_rel = format!("bin/{triple}/{name}");
            let bin_path = self.ai_dir.join(&bin_rel);
            fs::create_dir_all(bin_path.parent().unwrap()).unwrap();

            let response = serde_json::json!({
                "result": "parse_ok",
                "value": value,
            })
            .to_string();
            let script = format!("#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{response}'\n");
            fs::write(&bin_path, script.as_bytes()).unwrap();
            let mut perms = fs::metadata(&bin_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin_path, perms).unwrap();

            self.sign_and_write(
                "handlers/test/fixed-parser.yaml",
                &format!(
                    r#"category: test
name: fixed-parser
kind: handler
serves: parser
binary_ref: {bin_rel}
abi_version: "v1"
required_caps: []
description: "fixed parser handler for preflight tests"
"#
                ),
            );
            self.write_binary_manifest(&bin_rel, &bin_path);
        }

        fn write_binary_manifest(&self, item_ref: &str, bin_path: &Path) {
            let cas = lillux::cas::CasStore::new(self.ai_dir.join("objects"));
            let bytes = fs::read(bin_path).unwrap();
            let blob_hash = cas.store_blob(&bytes).unwrap();
            let item_source = serde_json::json!({
                "kind": "item_source",
                "item_ref": item_ref,
                "content_blob_hash": blob_hash,
                "integrity": format!("sha256:{blob_hash}"),
                "signature_info": null,
                "mode": 0o755,
            });
            let sidecar_body = lillux::cas::canonical_json(&item_source).unwrap();
            let sidecar =
                lillux::signature::sign_content(&sidecar_body, &self.signing_key, "#", None);
            fs::write(
                bin_path.with_file_name(format!(
                    "{}.item_source.json",
                    bin_path.file_name().unwrap().to_string_lossy()
                )),
                sidecar,
            )
            .unwrap();
            let item_source_hash = cas.store_object(&item_source).unwrap();
            let manifest = serde_json::json!({
                "kind": "source_manifest",
                "item_source_hashes": {
                    item_ref: item_source_hash,
                }
            });
            let manifest_hash = cas.store_object(&manifest).unwrap();
            let manifest_ref = self.ai_dir.join("refs/bundles/manifest");
            fs::create_dir_all(manifest_ref.parent().unwrap()).unwrap();
            let signed_ref = lillux::signature::sign_content(
                &format!(
                    "{}\n{manifest_hash}\n",
                    ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN
                ),
                &self.signing_key,
                "#",
                None,
            );
            fs::write(manifest_ref, signed_ref).unwrap();
        }

        fn write_signed_item(&self, rel: &str, body: &str) {
            self.sign_and_write(rel, body);
        }
    }

    fn host_triple() -> String {
        let output = std::process::Command::new("rustc")
            .args(["-vV"])
            .output()
            .expect("rustc -vV");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find(|l| l.starts_with("host:"))
            .expect("host triple in rustc -vV")
            .strip_prefix("host:")
            .unwrap()
            .trim()
            .to_string()
    }

    fn indented_contract(body: &str) -> String {
        body.lines()
            .map(|line| format!("  {line}\n"))
            .collect::<String>()
    }

    #[test]
    fn verify_manifest_accepts_valid_signed() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        assert!(
            verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).is_ok(),
            "valid signed manifest should pass"
        );
    }

    #[test]
    fn verify_manifest_rejects_unsigned() {
        let layout = BundleLayout::new("test-bundle");
        layout.write_unsigned_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no valid signature header"),
            "should reject unsigned: {msg}"
        );
    }

    #[test]
    fn verify_manifest_rejects_tampered() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_tampered_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("signature verification failed"),
            "should reject tampered: {msg}"
        );
    }

    #[test]
    fn verify_manifest_rejects_untrusted_signer() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_manifest_signed_by_other(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not in node trust store"),
            "should reject untrusted signer: {msg}"
        );
    }

    #[test]
    fn verify_manifest_owner_label_mismatch_is_non_fatal() {
        // The owner label is informational; the trust anchor is the fingerprint
        // (verified trusted + cryptographically). A label mismatch for an
        // otherwise-trusted, validly-signed publisher key must NOT fail
        // verification — it logs a warning instead. Renaming the owner label
        // (e.g. official-publisher -> ryeos-official) must never brick a node.
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        layout.write_publisher_trust_doc("local-dev"); // store owner is "test-publisher"
        let ts = layout.trust_store();

        verify_manifest_signature(&layout.ai_dir, &layout.source, &ts)
            .expect("owner-label mismatch should be a warning, not a verification failure");
    }

    #[test]
    fn verify_manifest_rejects_identity_mismatch() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_signed_manifest(
            "name: wrong-name\nversion: '1.0'\nprovides_kinds:\n  - mykind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("identity mismatch"),
            "should reject name mismatch: {msg}"
        );
        assert!(
            msg.contains("test-bundle") && msg.contains("wrong-name"),
            "should name both sides: {msg}"
        );
    }

    #[test]
    fn verify_manifest_rejects_provides_kinds_mismatch() {
        let layout = BundleLayout::new("test-bundle");
        layout.add_kind_schema("mykind");
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - fake-kind\nrequires_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("provides_kinds mismatch"),
            "should reject provides_kinds mismatch: {msg}"
        );
        assert!(
            msg.contains("fake-kind"),
            "should mention claimed kind: {msg}"
        );
    }

    #[test]
    fn verify_manifest_rejects_stale_manifest_source() {
        let layout = BundleLayout::new("test-bundle");
        fs::write(
            layout.ai_dir.join("manifest.source.yaml"),
            "name: test-bundle\nversion: '2.0'\ndescription: changed\n",
        )
        .unwrap();
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\ndescription: old\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("stale relative to manifest.source.yaml"),
            "should reject stale signed manifest: {msg}"
        );
    }

    #[test]
    fn verify_manifest_accepts_manifest_matching_source() {
        let layout = BundleLayout::new("test-bundle");
        fs::write(
            layout.ai_dir.join("manifest.source.yaml"),
            "name: test-bundle\nversion: '1.0'\ndescription: same\n",
        )
        .unwrap();
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\ndescription: same\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
        );
        let ts = layout.trust_store();
        assert!(verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).is_ok());
    }

    #[test]
    fn verify_manifest_rejects_invalid_runtime_authority_declaration() {
        // A correctly signed manifest whose runtime-authority declaration is
        // structurally invalid (wildcard `event_kind`) must be rejected by
        // preflight — the same rule the runtime loader applies — not approved
        // just because the signature verifies.
        let layout = BundleLayout::new("test-bundle");
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\nruntime_authority:\n  bundle_events:\n    - event_kind: ev_*\n      operations: [append]\n",
        );
        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("runtime_authority") && msg.contains("wildcards"),
            "should reject invalid runtime-authority declaration: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_manifest_rejects_manifest_symlink() {
        let layout = BundleLayout::new("test-bundle");
        std::os::unix::fs::symlink(
            layout.ai_dir.join("missing-manifest.yaml"),
            layout.ai_dir.join("manifest.yaml"),
        )
        .unwrap();

        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("manifest.yaml must be a regular file"),
            "should reject manifest symlink: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_manifest_rejects_manifest_source_symlink() {
        let layout = BundleLayout::new("test-bundle");
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
        );
        std::os::unix::fs::symlink(
            layout.ai_dir.join("missing-manifest.source.yaml"),
            layout.ai_dir.join("manifest.source.yaml"),
        )
        .unwrap();

        let ts = layout.trust_store();
        let err = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("manifest.source.yaml must be a regular file"),
            "should reject manifest source symlink: {msg}"
        );
    }

    #[test]
    fn validate_route_accepts_registry_source_names_without_canonical_ref() {
        // `response.source` semantics are owned by the route compiler /
        // ResponseModeRegistry, not by generic bundle preflight. Registry-backed
        // sources (e.g. the real core `event_stream` route\'s `dispatch_launch`)
        // are NOT canonical refs and must not be parsed as such here. This is the
        // exact case that broke `ryeos init` on the core bundle.
        let event_stream_route = serde_json::json!({
            "id": "execute/stream",
            "path": "/execute/stream",
            "methods": ["POST"],
            "auth": "ryeos_signed",
            "response": {
                "mode": "event_stream",
                "source": "dispatch_launch"
            }
        });
        assert!(
            validate_node_route_record(&event_stream_route).is_ok(),
            "registry source name `dispatch_launch` must pass generic preflight"
        );

        // A canonical-ref-shaped source (e.g. `handler` mode) is equally valid:
        // preflight does not care which form the owning mode uses.
        let handler_route = serde_json::json!({
            "id": "test-route",
            "path": "/test",
            "methods": ["GET"],
            "auth": "none",
            "response": {
                "mode": "handler",
                "source": "handler:test/route-handler"
            }
        });
        assert!(validate_node_route_record(&handler_route).is_ok());

        // The only mode-independent structural rule preflight enforces: when
        // `source` is present it must be non-empty.
        let mut empty_source = handler_route.clone();
        empty_source["response"]["source"] = serde_json::Value::String("   ".to_string());
        let err = validate_node_route_record(&empty_source).unwrap_err();
        assert!(
            err.to_string().contains("must be non-empty"),
            "should reject blank response source: {err}"
        );
    }

    #[test]
    fn node_config_preflight_accepts_signed_valid_command() {
        let layout = BundleLayout::new("test-bundle");
        layout.sign_and_write(
            "node/commands/demo.yaml",
            r#"tokens: ["demo"]
description: Demo command
dispatch:
  kind: execute_ref
  execute: tool:demo/run
aliases:
  - tokens: ["demo", "run"]
    description: Demo command alias
"#,
        );
        let trust_store = layout.trust_store();

        validate_node_config_item(
            &layout.ai_dir.join("node/commands/demo.yaml"),
            &layout.ai_dir.join("node/commands"),
            "commands",
            &trust_store,
        )
        .expect("signed valid command should pass node-config preflight");
    }

    #[test]
    fn node_config_preflight_rejects_unsigned_command() {
        let layout = BundleLayout::new("test-bundle");
        let path = layout.ai_dir.join("node/commands/demo.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"tokens: ["demo"]
description: Demo command
dispatch:
  kind: execute_ref
  execute: tool:demo/run
"#,
        )
        .unwrap();
        let trust_store = layout.trust_store();

        let err = validate_node_config_item(
            &path,
            &layout.ai_dir.join("node/commands"),
            "commands",
            &trust_store,
        )
        .unwrap_err();
        let msg = format!("{err:#}");

        assert!(
            msg.contains("no valid signature line"),
            "expected unsigned node-config rejection, got: {msg}"
        );
    }

    #[test]
    fn node_config_preflight_rejects_legacy_verb_section() {
        let layout = BundleLayout::new("test-bundle");
        layout.sign_and_write(
            "node/verbs/demo.yaml",
            r#"category: verbs
section: verbs
name: demo
description: Legacy verb
execute: tool:demo/run
"#,
        );
        let trust_store = layout.trust_store();

        let failures = collect_node_config_failures(&layout.ai_dir, &trust_store);

        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("legacy node config section")),
            "expected legacy node/verbs rejection, got: {failures:?}"
        );
    }

    #[test]
    fn node_config_preflight_rejects_bundle_authored_command_registration_policy() {
        let layout = BundleLayout::new("test-bundle");
        layout.sign_and_write(
            "node/command_registration/default.yaml",
            r#"claim_rules:
  - claim:
      kind: command.root
      value: execute
    required_caps:
      - ryeos.register.command.root.execute
system_source_caps:
  - ryeos.register.command.root.execute
"#,
        );
        let trust_store = layout.trust_store();

        let failures = collect_node_config_failures(&layout.ai_dir, &trust_store);

        assert!(
            failures.iter().any(|failure| failure
                .contains("command registration policy is node-owned seed/system config")),
            "expected command_registration rejection, got: {failures:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn node_config_preflight_rejects_node_root_symlink() {
        let layout = BundleLayout::new("test-bundle");
        let target = layout._tmp.path().join("outside-node");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, layout.ai_dir.join("node")).unwrap();
        let trust_store = layout.trust_store();

        let failures = collect_node_config_failures(&layout.ai_dir, &trust_store);

        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("node config scan encountered symlink")),
            "expected node root symlink rejection, got: {failures:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn node_config_preflight_rejects_nested_section_symlink() {
        let layout = BundleLayout::new("test-bundle");
        let target = layout._tmp.path().join("outside-commands");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(layout.ai_dir.join("node")).unwrap();
        std::os::unix::fs::symlink(&target, layout.ai_dir.join("node/commands")).unwrap();
        let trust_store = layout.trust_store();

        let failures = collect_node_config_failures(&layout.ai_dir, &trust_store);

        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("node config scan encountered symlink")),
            "expected nested section symlink rejection, got: {failures:?}"
        );
    }

    #[test]
    fn verify_manifest_rejects_missing_manifest() {
        let layout = BundleLayout::new("test-bundle");
        let ts = layout.trust_store();
        let error = verify_manifest_signature(&layout.ai_dir, &layout.source, &ts).unwrap_err();
        assert!(error.to_string().contains("has no regular signed"));
    }

    fn add_real_preflight_fixture(
        layout: &BundleLayout,
        composer: &str,
        contract: &str,
        parsed_value: serde_json::Value,
    ) {
        layout.add_test_parser_kind_schema();
        layout.add_test_item_kind_schema(composer, &indented_contract(contract));
        layout.add_fixed_parser_descriptor();
        layout.add_fixed_parser_handler(parsed_value);
        layout.write_signed_item("items/demo.yaml", "name: demo\n");
        layout.write_signed_manifest(
            "name: test-bundle\nversion: '1.0'\nprovides_kinds:\n  - mykind\n  - parser\nrequires_kinds: []\nuses_kinds: []\n",
        );
    }

    #[test]
    fn preflight_report_real_wiring_rejects_identity_contract_error() {
        let layout = BundleLayout::new("test-bundle");
        add_real_preflight_fixture(
            &layout,
            IDENTITY_COMPOSER,
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
    enum:
      - cli_exec
      - daemon_ui
optional: {}
"#,
            serde_json::json!({ "mode": "web_server" }),
        );

        let err = preflight_verify_bundle_report_in_context(
            &layout.source,
            &[],
            &layout.node_config_root,
            Arc::new(ryeos_engine::sandbox::SandboxRuntime::default()),
        )
        .unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("contract violation [enum_mismatch]"),
            "error: {msg}"
        );
        assert!(msg.contains("items/demo.yaml"), "item path: {msg}");
        assert!(msg.contains("mode"), "field path: {msg}");
    }

    #[test]
    fn preflight_report_real_wiring_skips_non_identity_contract_error() {
        let layout = BundleLayout::new("test-bundle");
        add_real_preflight_fixture(
            &layout,
            "handler:ryeos/core/extends-chain",
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
optional: {}
"#,
            serde_json::json!({ "other": "stuff" }),
        );

        let report = preflight_verify_bundle_report_in_context(
            &layout.source,
            &[],
            &layout.node_config_root,
            Arc::new(ryeos_engine::sandbox::SandboxRuntime::default()),
        )
        .expect("non-identity composer should skip pre-composition contract validation");

        assert!(report.is_clean(), "no warnings expected: {report:?}");
    }

    #[test]
    fn preflight_report_real_wiring_returns_contract_warnings() {
        let layout = BundleLayout::new("test-bundle");
        add_real_preflight_fixture(
            &layout,
            IDENTITY_COMPOSER,
            r#"root_type: mapping
required:
  body:
    type: single
    prim: string
optional: {}
strict_fields: warn
"#,
            serde_json::json!({ "body": "hello", "extra": "field" }),
        );

        let report = preflight_verify_bundle_report_in_context(
            &layout.source,
            &[],
            &layout.node_config_root,
            Arc::new(ryeos_engine::sandbox::SandboxRuntime::default()),
        )
        .expect("warnings should not fail preflight");

        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].severity, PreflightIssueSeverity::Warning);
        assert_eq!(
            report.warnings[0].code,
            InstanceViolationCode::UnexpectedField
        );
        assert_eq!(report.warnings[0].item_path, "items/demo.yaml");
        assert_eq!(report.warnings[0].path, "extra");
    }

    // ── Slice 3 follow-up: Contract diagnostics wiring tests ──────

    /// Helper: build a minimal `ValueShape` from YAML for tests.
    fn shape_from_yaml(yaml: &str) -> ryeos_engine::contracts::ValueShape {
        serde_yaml::from_str(yaml).expect("test contract YAML must parse")
    }

    /// Helper: build a minimal KindSchema for identity-composer tests.
    fn identity_kind_schema(contract_yaml: &str) -> ryeos_engine::kind_registry::KindSchema {
        ryeos_engine::kind_registry::KindSchema {
            directory: "tools".to_string(),
            extensions: vec![],
            extraction_rules: std::collections::HashMap::new(),
            resolution: vec![],
            effective_trust: ryeos_engine::kind_registry::EffectiveTrustPolicy {
                include_references: false,
            },
            execution: None,
            composed_value_contract: shape_from_yaml(contract_yaml),
            composer: IDENTITY_COMPOSER.to_string(),
            composer_config: serde_json::Value::Null,
            runtime: None,
            inventory_kinds: vec![],
            inventory_schema_keys: vec![],
        }
    }

    /// Helper: build a minimal KindSchema for non-identity (extends) composer.
    fn extends_kind_schema(contract_yaml: &str) -> ryeos_engine::kind_registry::KindSchema {
        let mut schema = identity_kind_schema(contract_yaml);
        schema.composer = "handler:ryeos/core/extends-chain".to_string();
        schema
    }

    // ── Tests for collect_identity_contract_issues ────────────────

    #[test]
    fn collect_issues_returns_errors_for_invalid_enum() {
        let kind_schema = identity_kind_schema(
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
    enum:
      - cli_exec
      - daemon_ui
optional: {}
"#,
        );
        let value = serde_json::json!({ "mode": "web_server" });
        let issues = collect_identity_contract_issues(
            &PathBuf::from("tools/my_tool.py"),
            &kind_schema,
            &value,
        );

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, PreflightIssueSeverity::Error);
        assert_eq!(issues[0].code, InstanceViolationCode::EnumMismatch);
        assert_eq!(issues[0].path, "mode");
        assert_eq!(issues[0].item_path, "tools/my_tool.py");
        assert!(issues[0].found.contains("web_server"));
    }

    #[test]
    fn collect_issues_returns_nested_dotted_path() {
        let kind_schema = identity_kind_schema(
            r#"root_type: mapping
required:
  launch:
    type: single
    prim: mapping
    contract:
      root_type: mapping
      required:
        mode:
          type: single
          prim: string
          enum:
            - cli_exec
            - daemon_ui
      optional: {}
optional: {}
"#,
        );
        let value = serde_json::json!({ "launch": {} });
        let issues = collect_identity_contract_issues(
            &PathBuf::from("tools/my_tool.py"),
            &kind_schema,
            &value,
        );

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, PreflightIssueSeverity::Error);
        assert_eq!(issues[0].code, InstanceViolationCode::MissingRequiredField);
        assert_eq!(issues[0].path, "launch.mode");
    }

    #[test]
    fn collect_issues_skips_non_identity_composer() {
        let kind_schema = extends_kind_schema(
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
optional: {}
"#,
        );
        let value = serde_json::json!({ "other": "stuff" });
        let issues = collect_identity_contract_issues(
            &PathBuf::from("directives/my_directive.md"),
            &kind_schema,
            &value,
        );

        assert!(
            issues.is_empty(),
            "non-identity composer should produce no issues"
        );
    }

    #[test]
    fn collect_issues_returns_warnings_for_strict_fields() {
        let kind_schema = identity_kind_schema(
            r#"root_type: mapping
required:
  body:
    type: single
    prim: string
optional: {}
strict_fields: warn
"#,
        );
        let value = serde_json::json!({ "body": "hello", "extra": "field" });
        let issues = collect_identity_contract_issues(
            &PathBuf::from("tools/my_tool.py"),
            &kind_schema,
            &value,
        );

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, PreflightIssueSeverity::Warning);
        assert_eq!(issues[0].code, InstanceViolationCode::UnexpectedField);
        assert_eq!(issues[0].path, "extra");
    }

    #[test]
    fn collect_issues_returns_nothing_for_valid_descriptor() {
        let kind_schema = identity_kind_schema(
            r#"root_type: mapping
required:
  mode:
    type: single
    prim: string
    enum:
      - cli_exec
      - daemon_ui
optional:
  timeout:
    type: single
    prim: integer
strict_fields: warn
"#,
        );
        let value = serde_json::json!({ "mode": "cli_exec", "timeout": 30 });
        let issues = collect_identity_contract_issues(
            &PathBuf::from("tools/my_tool.py"),
            &kind_schema,
            &value,
        );

        assert!(
            issues.is_empty(),
            "valid descriptor should produce no issues"
        );
    }

    // ── Tests for format_preflight_issue ─────────────────────────────

    #[test]
    fn format_issue_includes_all_fields() {
        let issue = PreflightIssue {
            item_path: "tools/my_tool.py".to_string(),
            severity: PreflightIssueSeverity::Error,
            code: InstanceViolationCode::EnumMismatch,
            path: "launch.mode".to_string(),
            expected: "cli_exec, daemon_ui".to_string(),
            found: "web_server".to_string(),
        };
        let formatted = format_preflight_issue(&issue);
        assert!(
            formatted.contains("tools/my_tool.py"),
            "item_path: {formatted}"
        );
        assert!(
            formatted.contains("contract violation"),
            "label: {formatted}"
        );
        assert!(formatted.contains("enum_mismatch"), "code: {formatted}");
        assert!(formatted.contains("launch.mode"), "path: {formatted}");
        assert!(formatted.contains("cli_exec"), "expected: {formatted}");
        assert!(formatted.contains("web_server"), "found: {formatted}");
    }

    #[test]
    fn format_issue_uses_warning_label_for_warnings() {
        let issue = PreflightIssue {
            item_path: "tools/my_tool.py".to_string(),
            severity: PreflightIssueSeverity::Warning,
            code: InstanceViolationCode::UnexpectedField,
            path: "extra".to_string(),
            expected: "known field".to_string(),
            found: "string".to_string(),
        };
        let formatted = format_preflight_issue(&issue);
        assert!(
            formatted.contains("contract warning"),
            "should use warning label: {formatted}"
        );
        assert!(
            !formatted.contains("contract violation"),
            "should not use error label: {formatted}"
        );
    }

    // ── Tests for PreflightReport ────────────────────────────────────

    #[test]
    fn preflight_report_is_clean_when_empty() {
        let report = PreflightReport::default();
        assert!(report.is_clean());
    }

    #[test]
    fn preflight_report_is_dirty_with_warnings() {
        let report = PreflightReport {
            warnings: vec![PreflightIssue {
                item_path: "tools/x.py".to_string(),
                severity: PreflightIssueSeverity::Warning,
                code: InstanceViolationCode::UnexpectedField,
                path: "extra".to_string(),
                expected: "known field".to_string(),
                found: "string".to_string(),
            }],
        };
        assert!(!report.is_clean());
        assert_eq!(report.warnings.len(), 1);
    }

    // NOTE: Real preflight wiring tests (calling
    // `preflight_verify_bundle_in_context` directly with temp bundle
    // fixtures) require parser binaries installed in the worktree.
    // These will be validated in CI. The tests above exercise the
    // production helper functions (`collect_identity_contract_issues`,
    // `format_preflight_issue`) and public types (`PreflightIssue`,
    // `PreflightReport`) that the real wiring calls.
}
