//! User-key signing — operator-side validated sign.
//!
//! Used by:
//! - `rye sign <ref>` (CLI verb → tool:rye/core/sign → rye-sign binary)
//! - `rye dev resign <ref>` (dev convenience)
//! - `rye dev build-bundle` (re-signs every YAML it touches)
//!
//! The flow:
//!   1. Parse the canonical ref (`<kind>:<bare-id>`).
//!   2. Build the engine's three-tier `TrustStore` and `KindRegistry`.
//!   3. Resolve the ref to a single file within `source` (project | user;
//!      system is rejected — those are bundle items, signed by bundle keys).
//!   4. Parse the file via the kind's parser.
//!   5. Apply the kind schema's `metadata.rules` and run
//!      `validate_metadata_anchoring` — refuses on failure with the
//!      typed error.
//!   6. Sign in place (atomic rename) using the user signing key.
//!
//! Does NOT hardcode kinds. Adding a new kind = adding a new
//! `kind-schema.yaml`; this tool picks it up automatically.
//!
//! System-source signing is intentionally rejected: bundle items are
//! signed by the bundle author's key during bundle authoring, never
//! re-signed in place by an operator.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use lillux::crypto::{DecodePrivateKey, SigningKey};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::SignatureEnvelope;
use ryeos_engine::kind_registry::{validate_metadata_anchoring, KindRegistry, KindSchema};
use ryeos_engine::parsers::{NativeParserHandlerRegistry, ParserDispatcher, ParserRegistry};
use ryeos_engine::roots;
use ryeos_engine::trust::TrustStore;

/// Where to look for the item to sign. The system tier is intentionally
/// not a variant — bundle items are signed by their author key, never
/// re-signed in place by an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignSource {
    Project,
    User,
}

impl SignSource {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "project" => Ok(Self::Project),
            "user" => Ok(Self::User),
            "system" => bail!(
                "source `system` is rejected — bundle items are signed by their \
                 author key during bundle authoring, not re-signed in place"
            ),
            other => bail!("unknown source `{other}` (expected: project | user)"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

/// Sign (or re-sign) an item in place after enforcing the kind
/// schema's path-anchoring validator.
///
/// `item_ref` may be a single canonical ref OR a glob pattern in the
/// bare-id position. Examples:
///   * `directive:hello`             — single item
///   * `tool:rye/core/sign`          — single item
///   * `tool:rye/core/*`             — all tools at one path level
///   * `tool:*`                      — every tool recursively
///   * `directive:agent/**/*`        — every directive under `agent/`
///
/// `project_path` is required when `source = Project`; ignored
/// otherwise. The user signing key is loaded from `RYE_SIGNING_KEY`
/// env or `~/.ai/config/keys/signing/private_key.pem` as fallback.
///
/// Returns a `BatchReport` always — single-item refs produce a one-
/// element vec. Per-item failures are collected; a failed validator
/// or sign on one item does NOT stop the batch.
pub fn run_sign(
    item_ref: &str,
    project_path: Option<&Path>,
    source: SignSource,
) -> Result<BatchReport> {
    let canonical = CanonicalRef::parse(item_ref)
        .map_err(|e| anyhow!("malformed canonical ref `{item_ref}`: {e}"))?;

    let user_root = roots::user_root().ok();
    let system_roots = {
        let state_dir = match std::env::var("RYEOS_STATE_DIR") {
            Ok(p) => PathBuf::from(p),
            Err(_) => dirs::state_dir()
                .map(|d| d.join("ryeosd"))
                .unwrap_or_else(|| PathBuf::from(".ryeosd")),
        };
        let bundles_dir = state_dir.join(ryeos_engine::AI_DIR).join("bundles");
        let mut additional: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&bundles_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    additional.push(entry.path());
                }
            }
        }
        roots::system_roots(&additional)
    };

    let trust_store = TrustStore::load_three_tier(
        project_path,
        user_root.as_deref(),
        &system_roots,
    )
    .with_context(|| "load trust store")?;

    let kinds = build_kind_registry(&system_roots, &trust_store)?;
    let kind_schema = kinds
        .get(&canonical.kind)
        .ok_or_else(|| {
            anyhow!(
                "unknown kind `{}` — no kind schema registered",
                canonical.kind
            )
        })?;

    let parsers = build_parser_dispatcher(
        &system_roots,
        user_root.as_deref(),
        &kinds,
        &trust_store,
    )?;

    let kind_dir = source_kind_dir(kind_schema, source, project_path, user_root.as_deref())?;
    let ai_root = source_ai_root(source, project_path, user_root.as_deref())?;

    let targets = if is_glob(&canonical.bare_id) {
        glob_match_items(&kind_dir, kind_schema, &canonical.bare_id)?
    } else {
        // Single-item: the bare_id resolves to exactly one file (or
        // nothing). Mirror Python: try each declared extension in
        // order, first match wins.
        let mut found = None;
        for spec in &kind_schema.extensions {
            let candidate = kind_dir.join(format!("{}{}", canonical.bare_id, spec.ext));
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(p) => vec![p],
            None => bail!(
                "item `{}:{}` not found in {} (searched {} with extensions {:?})",
                canonical.kind,
                canonical.bare_id,
                source.label(),
                kind_dir.display(),
                kind_schema.extension_strs()
            ),
        }
    };

    if targets.is_empty() {
        bail!(
            "no items matched `{}:{}` in {} (searched {})",
            canonical.kind,
            canonical.bare_id,
            source.label(),
            kind_dir.display()
        );
    }

    // Determinism: sort so the report ordering is stable across runs.
    let mut targets = targets;
    targets.sort();

    let mut report = BatchReport::default();

    for file_path in targets {
        let bare_id = derive_bare_id(&file_path, &kind_dir, kind_schema)
            .unwrap_or_else(|| file_path.display().to_string());
        let display_ref = format!("{}:{}", canonical.kind, bare_id);

        match sign_one(&file_path, kind_schema, &ai_root, &parsers) {
            Ok(sig) => report.signed.push(ItemOutcome {
                item_ref: display_ref,
                signature: Some(sig),
                error: None,
            }),
            Err(e) => report.failed.push(ItemOutcome {
                item_ref: display_ref,
                signature: None,
                error: Some(format!("{e:#}")),
            }),
        }
    }

    Ok(report)
}

/// Sign a single resolved file: parse, validate, then sign in place.
fn sign_one(
    file_path: &Path,
    kind_schema: &KindSchema,
    ai_root: &Path,
    parsers: &ParserDispatcher,
) -> Result<SignatureReport> {
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("read {}", file_path.display()))?;
    let matched_ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .ok_or_else(|| anyhow!("file {} has no extension", file_path.display()))?;
    let source_format = kind_schema.resolved_format_for(&matched_ext).ok_or_else(|| {
        anyhow!(
            "extension `{}` not registered for kind — kind schema declares: {:?}",
            matched_ext,
            kind_schema.extension_strs()
        )
    })?;

    let parsed = parsers
        .dispatch(
            &source_format.parser,
            &content,
            Some(file_path),
            &source_format.signature,
        )
        .with_context(|| format!("parse {}", file_path.display()))?;

    validate_metadata_anchoring(
        &parsed,
        &kind_schema.extraction_rules,
        &kind_schema.directory,
        ai_root,
        file_path,
    )
    .map_err(|e| anyhow!("path-anchoring validator refused {}: {e}", file_path.display()))?;

    sign_in_place(file_path, &source_format.signature)
}

fn is_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Expand a glob pattern in the bare-id position to all matching
/// item files inside `kind_dir`. Mirrors the Python sign tool:
///   * pattern with `/` → `{pattern}{ext}` (interpreted from
///     `kind_dir`)
///   * pattern without `/` → `**/{pattern}{ext}` (recursive)
///   * literal `*` → `**/*{ext}` (every item)
fn glob_match_items(
    kind_dir: &Path,
    kind_schema: &KindSchema,
    pattern: &str,
) -> Result<Vec<PathBuf>> {
    use glob::glob_with;
    use glob::MatchOptions;

    if !kind_dir.is_dir() {
        return Ok(Vec::new());
    }

    let opts = MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };

    let mut matches: Vec<PathBuf> = Vec::new();
    for spec in &kind_schema.extensions {
        let ext = &spec.ext;
        let pat_with_ext = if pattern == "*" {
            format!("**/*{ext}")
        } else if pattern.contains('/') {
            if pattern.ends_with(ext) {
                pattern.to_string()
            } else {
                format!("{pattern}{ext}")
            }
        } else {
            format!("**/{pattern}{ext}")
        };

        let full_pattern = format!("{}/{}", kind_dir.display(), pat_with_ext);
        let entries = glob_with(&full_pattern, opts)
            .with_context(|| format!("invalid glob pattern: {full_pattern}"))?;
        for entry in entries.flatten() {
            if entry.is_file() {
                matches.push(entry);
            }
        }
    }

    matches.sort();
    matches.dedup();
    Ok(matches)
}

/// Reverse the file's path back to a `bare_id` for reporting.
/// `<kind_dir>/<bare_id><ext>` → `<bare_id>`.
fn derive_bare_id(file_path: &Path, kind_dir: &Path, kind_schema: &KindSchema) -> Option<String> {
    let rel = file_path.strip_prefix(kind_dir).ok()?;
    let s = rel.to_string_lossy().to_string();
    for spec in &kind_schema.extensions {
        if let Some(stripped) = s.strip_suffix(&spec.ext) {
            return Some(stripped.to_string());
        }
    }
    None
}

fn source_kind_dir(
    kind_schema: &KindSchema,
    source: SignSource,
    project_path: Option<&Path>,
    user_root: Option<&Path>,
) -> Result<PathBuf> {
    let ai_root = source_ai_root(source, project_path, user_root)?;
    Ok(ai_root.join(&kind_schema.directory))
}

fn source_ai_root(
    source: SignSource,
    project_path: Option<&Path>,
    user_root: Option<&Path>,
) -> Result<PathBuf> {
    match source {
        SignSource::Project => {
            let p = project_path.ok_or_else(|| anyhow!("source=project requires --project path"))?;
            Ok(p.join(".ai"))
        }
        SignSource::User => {
            let h = user_root
                .ok_or_else(|| anyhow!("source=user but $HOME (user root) is not set"))?;
            Ok(h.join(".ai"))
        }
    }
}

/// Result of a batch sign call. Always populated, even for
/// single-item refs (vec of length 1).
///
/// `signed` and `failed` are partitioned outcomes; the binary
/// exits non-zero iff `!failed.is_empty()`. Determinism: both
/// vectors are ordered by the on-disk path, so reports are stable
/// across runs.
#[derive(Debug, Default, serde::Serialize)]
pub struct BatchReport {
    pub signed: Vec<ItemOutcome>,
    pub failed: Vec<ItemOutcome>,
}

impl BatchReport {
    pub fn is_total_success(&self) -> bool {
        self.failed.is_empty()
    }

    pub fn total(&self) -> usize {
        self.signed.len() + self.failed.len()
    }
}

/// One per-item outcome inside a `BatchReport`.
///
/// Exactly one of `signature` or `error` is `Some`; the
/// `BatchReport.signed` / `BatchReport.failed` partition makes the
/// invariant structural even though serde sees both fields.
#[derive(Debug, serde::Serialize)]
pub struct ItemOutcome {
    pub item_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<SignatureReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn build_kind_registry(
    system_roots: &[PathBuf],
    trust_store: &TrustStore,
) -> Result<KindRegistry> {
    // Kind schemas are node-tier items: they live exclusively under
    // `<bundle-root>/.ai/node/engine/kinds/` in node bundles laid down
    // at the system roots. They do NOT participate in the
    // project/user/system three-tier resolution that operator-edited
    // items use, so this loader scans only system bundle roots.
    let mut search = Vec::new();
    for r in system_roots {
        let p = r.join(".ai/node/engine/kinds");
        if p.exists() {
            search.push(p);
        }
    }

    KindRegistry::load_base(&search, trust_store).with_context(|| "load kind registry")
}

/// Build a `ParserDispatcher` mirroring the daemon's bootstrap.
///
/// Loads `parser:` tool descriptors from every signed
/// `parsers/**/*.kind-schema.yaml`-referenced parser tool the
/// kind registry knows about. The native handler registry provides
/// the in-process parsers (`yaml/yaml`, `markdown/frontmatter`, etc.)
/// that the descriptors point at.
fn build_parser_dispatcher(
    system_roots: &[PathBuf],
    user_root: Option<&Path>,
    kinds: &KindRegistry,
    trust_store: &TrustStore,
) -> Result<ParserDispatcher> {
    let mut search: Vec<PathBuf> = system_roots.to_vec();
    if let Some(u) = user_root {
        search.push(u.to_path_buf());
    }
    let (parser_tools, _duplicates) = ParserRegistry::load_base(&search, trust_store, kinds)
        .with_context(|| "load parser tool descriptors")?;
    let native_handlers = NativeParserHandlerRegistry::with_builtins();
    Ok(ParserDispatcher::new(parser_tools, native_handlers))
}

/// Sign a file in place using the kind's signature envelope. Loads
/// the user signing key from `RYE_SIGNING_KEY` env or
/// `~/.ai/config/keys/signing/private_key.pem`.
fn sign_in_place(input: &Path, envelope: &SignatureEnvelope) -> Result<SignatureReport> {
    let body = fs::read_to_string(input)
        .with_context(|| format!("read {}", input.display()))?;

    let signing_key = load_user_signing_key()?;
    let fingerprint = lillux::sha256_hex(signing_key.verifying_key().as_bytes());

    let stripped = lillux::signature::strip_signature_lines(&body);
    let signed = lillux::signature::sign_content(
        &stripped,
        &signing_key,
        &envelope.prefix,
        envelope.suffix.as_deref(),
    );

    // Atomic write
    let tmp = input.with_extension(format!("signed.tmp.{}", std::process::id()));
    fs::write(&tmp, &signed).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, input)
        .with_context(|| format!("rename {} -> {}", tmp.display(), input.display()))?;

    let signature_line = extract_signature_line(&signed, &envelope.prefix)
        .unwrap_or_else(|| "signature applied".to_string());

    Ok(SignatureReport {
        file: input.display().to_string(),
        signer_fingerprint: fingerprint,
        signature_line,
        updated_at: lillux::time::iso8601_now(),
    })
}

#[derive(Debug, serde::Serialize)]
pub struct SignatureReport {
    pub file: String,
    pub signer_fingerprint: String,
    pub signature_line: String,
    pub updated_at: String,
}

fn load_user_signing_key() -> Result<SigningKey> {
    let path: PathBuf = match std::env::var("RYE_SIGNING_KEY") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let home = std::env::var("HOME").context("HOME not set")?;
            Path::new(&home).join(".ai/config/keys/signing/private_key.pem")
        }
    };

    let pem = fs::read_to_string(&path)
        .with_context(|| format!("read signing key from {}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem).context("parse signing key (Ed25519 PKCS8 PEM)")
}

fn extract_signature_line(content: &str, prefix: &str) -> Option<String> {
    let needle = format!("{prefix} rye:signed:");
    content
        .lines()
        .find(|l| l.starts_with(&needle))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_source_parses_project_and_user() {
        assert_eq!(SignSource::parse("project").unwrap(), SignSource::Project);
        assert_eq!(SignSource::parse("user").unwrap(), SignSource::User);
    }

    #[test]
    fn sign_source_rejects_system() {
        let err = SignSource::parse("system").unwrap_err();
        assert!(
            err.to_string().contains("system")
                && err.to_string().contains("rejected"),
            "expected system-rejected error, got: {err}"
        );
    }

    #[test]
    fn sign_source_rejects_unknown() {
        let err = SignSource::parse("network").unwrap_err();
        assert!(
            err.to_string().contains("unknown source"),
            "expected unknown-source error, got: {err}"
        );
    }

    #[test]
    fn run_sign_rejects_malformed_canonical_ref() {
        // `not-a-ref` has no `:` — CanonicalRef::parse fails. We
        // never reach trust-store / kind-registry loading.
        let err = run_sign("not-a-ref", None, SignSource::Project).unwrap_err();
        assert!(
            err.to_string().contains("malformed canonical ref"),
            "expected malformed-ref error, got: {err}"
        );
    }
}
