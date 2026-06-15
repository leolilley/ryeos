//! Operator-key signing — validated local sign.
//!
//! Used by:
//! - `ryeos sign <ref>` (CLI verb → tool:ryeos/core/sign → ryeos-core-tools binary)
//! - `ryeos dev resign <ref>` (dev convenience)
//! - `ryeos dev build-bundle` (re-signs every YAML it touches)
//!
//! The flow:
//!   1. Parse the canonical ref (`<kind>:<bare-id>`).
//!   2. Build the engine's `TrustStore` and `KindRegistry`.
//!   3. Resolve the ref to a single file within the project source.
//!   4. Parse the file via the kind's parser.
//!   5. Apply the kind schema's `metadata.rules` and run
//!      `validate_metadata_anchoring` — refuses on failure with the
//!      typed error.
//!   6. Sign in place (atomic rename) using the operator signing key.
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
use std::sync::Arc;

use ryeos_engine::handlers::HandlerRegistry;
use ryeos_engine::parsers::{ParserDispatcher, ParserRegistry};
use ryeos_engine::roots;
use ryeos_engine::trust::TrustStore;

/// Where to look for the item to sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignSource {
    Project,
}

impl SignSource {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "project" => Ok(Self::Project),
            "system" => bail!(
                "source `system` is rejected — bundle items are signed by their \
                 author key during bundle authoring, not re-signed in place"
            ),
            "operator" => {
                bail!("source `operator` is rejected — app-root config is not an item source")
            }
            other => bail!("unknown source `{other}` (expected: project)"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
        }
    }
}

/// Sign (or re-sign) an item in place after enforcing the kind
/// schema's path-anchoring validator.
///
/// `item_ref` may be a single canonical ref OR a glob pattern in the
/// bare-id position. Examples:
///   * `directive:hello`             — single item
///   * `tool:ryeos/core/sign`          — single item
///   * `tool:ryeos/core/*`             — all tools at one path level
///   * `tool:*`                      — every tool recursively
///   * `directive:agent/**/*`        — every directive under `agent/`
///
/// `project_path` is required when `source = Project`; ignored
/// otherwise. The operator signing key is loaded from
/// `<app_root>/.ai/config/keys/signing/private_key.pem`.
///
/// Returns a `BatchReport` always — single-item refs produce a one-
/// element vec. Per-item failures are collected; a failed validator
/// or sign on one item does NOT stop the batch.
pub fn run_sign(
    item_ref: &str,
    project_path: Option<&Path>,
    source: SignSource,
) -> Result<BatchReport> {
    let bundle_roots = {
        let app_root = match std::env::var("RYEOS_APP_ROOT") {
            Ok(p) => PathBuf::from(p),
            Err(_) => dirs::data_dir()
                .map(|d| d.join("ryeos"))
                .expect("could not determine XDG data directory"),
        };
        let bundles_dir = app_root.join(ryeos_engine::AI_DIR).join("bundles");
        let mut additional: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&bundles_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    additional.push(entry.path());
                }
            }
        }
        roots::bundle_roots(&additional)
    };

    let operator_config_root = roots::runtime_root().ok().map(|root| root.config());
    let trust_store = TrustStore::load(
        project_path,
        operator_config_root
            .as_deref()
            .ok_or_else(|| anyhow!("cannot resolve app root"))?,
    )
    .with_context(|| "load trust store")?;

    let kinds = build_kind_registry(&bundle_roots, &trust_store)?;

    // `sign` takes a canonical ref (`graph:foo/bar`), not a file path. Operators
    // and LLMs routinely try the path they just edited; rather than a bare
    // "malformed canonical ref", reverse-map a path-shaped arg to its ref and
    // tell them exactly what to run. (We don't auto-sign the path — never sign
    // a file the caller didn't name by ref.)
    let target = match parse_sign_target(item_ref) {
        Ok(t) => t,
        Err(e) => {
            if let Some(hint) = path_arg_to_ref_hint(item_ref, source, project_path, &kinds) {
                bail!(
                    "`{item_ref}` is a file path; `sign` takes a canonical ref. Run:\n    \
                     ryeos sign {hint}"
                );
            }
            return Err(e);
        }
    };

    let kind_schema = kinds
        .get(&target.kind)
        .ok_or_else(|| anyhow!("unknown kind `{}` — no kind schema registered", target.kind))?;

    let parsers = build_parser_dispatcher(&bundle_roots, &kinds, &trust_store)?;

    let kind_dir = source_kind_dir(kind_schema, source, project_path)?;
    let ai_root = source_ai_root(source, project_path)?;

    let targets = if is_glob(&target.bare_id) {
        glob_match_items(&kind_dir, kind_schema, &target.bare_id)?
    } else {
        // Single-item: the bare_id resolves to exactly one file (or
        // nothing). Mirror Python: try each declared extension in
        // order, first match wins.
        let mut found = None;
        for spec in &kind_schema.extensions {
            let candidate = kind_dir.join(format!("{}{}", target.bare_id, spec.ext));
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(p) => vec![p],
            None => bail!(
                "item `{}:{}` not found in {} (searched {} with extensions {:?})",
                target.kind,
                target.bare_id,
                source.label(),
                kind_dir.display(),
                kind_schema.extension_strs()
            ),
        }
    };

    if targets.is_empty() {
        bail!(
            "no items matched `{}:{}` in {} (searched {})",
            target.kind,
            target.bare_id,
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
        let display_ref = format!("{}:{}", target.kind, bare_id);

        match sign_one(&file_path, &target.kind, kind_schema, &ai_root, &parsers) {
            Ok(SignOneResult {
                outcome: SignOutcome::Signed(sig),
                warnings,
            }) => report.signed.push(ItemOutcome {
                item_ref: display_ref,
                signature: Some(sig),
                error: None,
                warnings,
            }),
            Ok(SignOneResult {
                outcome:
                    SignOutcome::Unchanged {
                        file,
                        signer_fingerprint,
                    },
                warnings,
            }) => report.validated.push(ItemOutcome {
                item_ref: display_ref,
                signature: Some(SignatureReport {
                    file,
                    signer_fingerprint,
                    signature_line: "unchanged — already validly signed".to_string(),
                    updated_at: String::new(),
                }),
                error: None,
                warnings,
            }),
            Err(e) => report.failed.push(ItemOutcome {
                item_ref: display_ref,
                signature: None,
                error: Some(format!("{e:#}")),
                warnings: Vec::new(),
            }),
        }
    }

    Ok(report)
}

/// Sign a single resolved file: parse, validate, then sign in place.
/// Returns `SignOutcome::Unchanged` if the file was already validly signed.
fn sign_one(
    file_path: &Path,
    kind_name: &str,
    kind_schema: &KindSchema,
    ai_root: &Path,
    parsers: &ParserDispatcher,
) -> Result<SignOneResult> {
    let content =
        fs::read_to_string(file_path).with_context(|| format!("read {}", file_path.display()))?;
    let matched_ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .ok_or_else(|| anyhow!("file {} has no extension", file_path.display()))?;
    let source_format = kind_schema
        .resolved_format_for(&matched_ext)
        .ok_or_else(|| {
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
    .map_err(|e| {
        anyhow!(
            "path-anchoring validator refused {}: {e}",
            file_path.display()
        )
    })?;

    let warnings = sign_warnings(kind_name, &parsed);
    let outcome = sign_in_place(file_path, &content, &source_format.signature)?;
    Ok(SignOneResult { outcome, warnings })
}

fn sign_warnings(kind_name: &str, parsed: &serde_json::Value) -> Vec<String> {
    let mut warnings = Vec::new();

    if kind_name == "tool"
        && parsed
            .get("executor_id")
            .is_some_and(serde_json::Value::is_null)
    {
        warnings.push(
            "tool declares `executor_id: null`; this is valid for terminal executor-chain endpoints, but the tool is not directly invokable as a requested item"
                .to_string(),
        );
    }

    // A graph-shaped YAML placed under `.ai/tools/**/graphs/` resolves as a
    // `tool:` (resolution keys off the directory, not `tool_type`), never as a
    // `graph:`. This is a common trap — flag it at sign time, pointing at the
    // canonical location.
    if kind_name == "tool" && looks_graph_shaped(parsed) {
        warnings.push(
            "this item looks like a graph (graph-shaped body) but is being signed as a `tool:`. \
             Graphs only resolve from `.ai/graphs/<id>/`; a file under `.ai/tools/**/graphs/` \
             resolves as `tool:...`, not `graph:...`. Move it to `.ai/graphs/` and sign \
             `graph:<id>` if you intended a graph."
                .to_string(),
        );
    }

    warnings
}

/// Heuristic: does this parsed item body look like a graph definition?
/// True when it declares `tool_type: graph`, sits in a `*/graphs` category,
/// or carries the graph control structure (`config.start` + `config.nodes`).
fn looks_graph_shaped(parsed: &serde_json::Value) -> bool {
    if parsed.get("tool_type").and_then(|v| v.as_str()) == Some("graph") {
        return true;
    }
    if parsed
        .get("category")
        .and_then(|v| v.as_str())
        .is_some_and(|c| c == "graphs" || c.ends_with("/graphs"))
    {
        return true;
    }
    parsed
        .get("config")
        .map(|cfg| cfg.get("nodes").is_some() && cfg.get("start").is_some())
        .unwrap_or(false)
}

#[derive(Debug)]
struct SignTarget {
    kind: String,
    bare_id: String,
}

fn parse_sign_target(item_ref: &str) -> Result<SignTarget> {
    if !is_glob_ref(item_ref) {
        let canonical = CanonicalRef::parse(item_ref)
            .map_err(|e| anyhow!("malformed canonical ref `{item_ref}`: {e}"))?;
        if canonical.suffix.is_some() {
            bail!("malformed canonical ref `{item_ref}`: sign refs do not support suffixes");
        }
        return Ok(SignTarget {
            kind: canonical.kind,
            bare_id: canonical.bare_id,
        });
    }

    let colon_pos = item_ref
        .find(':')
        .ok_or_else(|| anyhow!("malformed canonical ref `{item_ref}`: bare refs are rejected"))?;
    let kind = &item_ref[..colon_pos];
    let bare_id = &item_ref[colon_pos + 1..];

    if kind.is_empty()
        || !kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        bail!("malformed canonical ref `{item_ref}`: invalid kind `{kind}`");
    }
    if bare_id.is_empty() {
        bail!("malformed canonical ref `{item_ref}`: empty bare_id after kind");
    }
    if bare_id.contains('@') {
        bail!("malformed canonical ref `{item_ref}`: glob refs do not support suffixes");
    }
    if bare_id.starts_with('/')
        || bare_id.ends_with('/')
        || bare_id.contains("//")
        || bare_id
            .split('/')
            .any(|segment| segment == "." || segment == "..")
    {
        bail!("malformed canonical ref `{item_ref}`: unsafe glob bare_id `{bare_id}`");
    }
    if !bare_id
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '*' | '?'))
    {
        bail!("malformed canonical ref `{item_ref}`: glob bare_id contains invalid characters: {bare_id}");
    }

    Ok(SignTarget {
        kind: kind.to_string(),
        bare_id: bare_id.to_string(),
    })
}

/// Reverse-map a path-shaped argument to the canonical ref `sign` expects.
///
/// Returns e.g. `graph:snap-track/daily_profile_pipeline` for
/// `.ai/graphs/snap-track/daily_profile_pipeline.yaml`. `None` if the arg
/// doesn't look like a path, isn't under the source `.ai/` root, or doesn't
/// match a known kind directory + extension. Only used to produce a helpful
/// error — it never causes signing.
fn path_arg_to_ref_hint(
    arg: &str,
    source: SignSource,
    project_path: Option<&Path>,
    kinds: &KindRegistry,
) -> Option<String> {
    // Only attempt for path-shaped args (avoid hijacking refs/globs).
    let looks_path =
        arg.contains('/') && (Path::new(arg).exists() || Path::new(arg).extension().is_some());
    if !looks_path {
        return None;
    }

    let ai_root = source_ai_root(source, project_path).ok()?;
    // Canonicalize both sides so a relative arg resolves against cwd and the
    // `.ai/` prefix strip works regardless of how either was spelled.
    let abs = std::fs::canonicalize(arg).ok()?;
    let ai_abs = std::fs::canonicalize(&ai_root).ok()?;
    let rel = abs.strip_prefix(&ai_abs).ok()?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");

    for kind in kinds.kinds() {
        let Some(dir) = kinds.directory(kind) else {
            continue;
        };
        let Some(rest) = rel_str.strip_prefix(&format!("{dir}/")) else {
            continue;
        };
        for ext in kinds.extension_strs(kind).unwrap_or_default() {
            if let Some(bare) = rest.strip_suffix(ext) {
                return Some(format!("{kind}:{bare}"));
            }
        }
    }
    None
}

fn is_glob_ref(s: &str) -> bool {
    s.contains('*') || s.contains('?')
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
) -> Result<PathBuf> {
    let ai_root = source_ai_root(source, project_path)?;
    Ok(ai_root.join(&kind_schema.directory))
}

fn source_ai_root(source: SignSource, project_path: Option<&Path>) -> Result<PathBuf> {
    match source {
        SignSource::Project => {
            let p =
                project_path.ok_or_else(|| anyhow!("source=project requires --project path"))?;
            Ok(p.join(ryeos_engine::AI_DIR))
        }
    }
}

/// Result of a batch sign call. Always populated, even for
/// single-item refs (vec of length 1).
///
/// `validated` contains items that were already validly signed and left
/// untouched. `signed` contains items that were (re-)signed. `failed`
/// collects per-item errors. The vectors are ordered by on-disk path.
#[derive(Debug, Default, serde::Serialize)]
pub struct BatchReport {
    /// Items already validly signed, left untouched.
    pub validated: Vec<ItemOutcome>,
    /// Items (re-)signed because unsigned, invalid, wrong signer, or content changed.
    pub signed: Vec<ItemOutcome>,
    pub failed: Vec<ItemOutcome>,
}

impl BatchReport {
    pub fn is_total_success(&self) -> bool {
        self.failed.is_empty()
    }

    pub fn total(&self) -> usize {
        self.validated.len() + self.signed.len() + self.failed.len()
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

fn build_kind_registry(bundle_roots: &[PathBuf], trust_store: &TrustStore) -> Result<KindRegistry> {
    // Kind schemas are node-tier items: they live exclusively under
    // `<bundle-root>/.ai/node/engine/kinds/` in node bundles laid down
    // at the bundle roots. They do NOT participate in the
    // project + bundle resolution that operator-edited
    // items use, so this loader scans only bundle roots.
    let mut search = Vec::new();
    for r in bundle_roots {
        let p = r.join(ryeos_engine::AI_DIR).join("node/engine/kinds");
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
    bundle_roots: &[PathBuf],
    kinds: &KindRegistry,
    trust_store: &TrustStore,
) -> Result<ParserDispatcher> {
    let search: Vec<PathBuf> = bundle_roots.to_vec();
    let tagged_search: Vec<(PathBuf, ryeos_engine::resolution::TrustClass)> = bundle_roots
        .iter()
        .map(|r| {
            (
                r.clone(),
                ryeos_engine::resolution::TrustClass::TrustedBundle,
            )
        })
        .collect();
    let (parser_tools, _duplicates) = ParserRegistry::load_base(&search, trust_store, kinds)
        .with_context(|| "load parser tool descriptors")?;
    let handlers = HandlerRegistry::load_base(&tagged_search, trust_store)
        .with_context(|| "load handler descriptors")?;
    Ok(ParserDispatcher::new(parser_tools, Arc::new(handlers)))
}

/// Sign a file in place using the kind's signature envelope.
///
/// Idempotent: if the file already carries a valid signature (matching body
/// hash, signer fingerprint, and cryptographic verification) for the current
/// user key, the file is left untouched and `SignOutcome::Unchanged` is
/// returned. Otherwise the file is (re-)signed atomically.
///
/// Loads the operator signing key from
/// `<app_root>/.ai/config/keys/signing/private_key.pem`.
fn sign_in_place(
    input: &Path,
    validated_content: &str,
    envelope: &SignatureEnvelope,
) -> Result<SignOutcome> {
    let signing_key = load_user_signing_key()?;
    let verifying_key = signing_key.verifying_key();
    let fingerprint = lillux::signature::compute_fingerprint(&verifying_key);

    let stripped = lillux::signature::strip_signature_lines_with_envelope(
        validated_content,
        &envelope.prefix,
        envelope.suffix.as_deref(),
    );

    // Check if already validly signed: hash + fingerprint + sig verification
    if is_already_validly_signed_operator(
        validated_content,
        &stripped,
        &verifying_key,
        &fingerprint,
        envelope,
    ) {
        return Ok(SignOutcome::Unchanged {
            file: input.display().to_string(),
            signer_fingerprint: fingerprint,
        });
    }

    let signed = lillux::signature::sign_content_with_options(
        &stripped,
        &signing_key,
        &envelope.prefix,
        envelope.suffix.as_deref(),
        envelope.after_shebang,
    );

    // Atomic write
    let tmp = input.with_extension(format!("signed.tmp.{}", std::process::id()));
    fs::write(&tmp, &signed).with_context(|| format!("write tmp {}", tmp.display()))?;
    fs::rename(&tmp, input)
        .with_context(|| format!("rename {} -> {}", tmp.display(), input.display()))?;

    let signature_line = extract_signature_line(&signed, &envelope.prefix)
        .unwrap_or_else(|| "signature applied".to_string());

    Ok(SignOutcome::Signed(SignatureReport {
        file: input.display().to_string(),
        signer_fingerprint: fingerprint,
        signature_line,
        updated_at: lillux::time::iso8601_now(),
    }))
}

/// Check whether `existing` (full file content) already carries a valid
/// signature for `body` (stripped content) signed by `signing_key`.
///
/// Returns true only when all three conditions hold:
///   1. the parsed header's content hash matches the body,
///   2. the signer fingerprint matches the current key, and
///   3. the signature verifies against the hash.
fn is_already_validly_signed_operator(
    existing: &str,
    body: &str,
    verifying_key: &lillux::crypto::VerifyingKey,
    fingerprint: &str,
    envelope: &SignatureEnvelope,
) -> bool {
    let Some(header) = ryeos_engine::item_resolution::parse_signature_header(existing, envelope)
    else {
        return false;
    };

    let signed_body = lillux::signature::content_to_sign(body, envelope.after_shebang);
    lillux::signature::is_valid_signature_for(
        &header.content_hash,
        &header.signature_b64,
        &header.signer_fingerprint,
        signed_body,
        verifying_key,
        fingerprint,
    )
}

/// Outcome of signing a single item via `sign_in_place`.
struct SignOneResult {
    outcome: SignOutcome,
    warnings: Vec<String>,
}

enum SignOutcome {
    /// Already valid, left untouched.
    Unchanged {
        file: String,
        signer_fingerprint: String,
    },
    /// (Re-)signed in place.
    Signed(SignatureReport),
}

#[derive(Debug, serde::Serialize)]
pub struct SignatureReport {
    pub file: String,
    pub signer_fingerprint: String,
    pub signature_line: String,
    pub updated_at: String,
}

pub fn load_user_signing_key() -> Result<SigningKey> {
    let path: PathBuf = roots::runtime_root()
        .context("cannot resolve app root for operator signing key")?
        .operator_signing_key_path();

    let pem = fs::read_to_string(&path)
        .with_context(|| format!("read signing key from {}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem).context("parse signing key (Ed25519 PKCS8 PEM)")
}

fn extract_signature_line(content: &str, prefix: &str) -> Option<String> {
    let needle = format!("{prefix} ryeos:signed:");
    content
        .lines()
        .find(|l| l.starts_with(&needle))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::{EncodePrivateKey, SigningKey};
    use rand::rngs::OsRng;
    use std::sync::{Mutex, MutexGuard};

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn sign_source_parses_project() {
        assert_eq!(SignSource::parse("project").unwrap(), SignSource::Project);
    }

    #[test]
    fn sign_source_rejects_operator() {
        let err = SignSource::parse("operator").unwrap_err();
        assert!(
            err.to_string().contains("operator") && err.to_string().contains("rejected"),
            "expected operator-rejected error, got: {err}"
        );
    }

    #[test]
    fn sign_source_rejects_system() {
        let err = SignSource::parse("system").unwrap_err();
        assert!(
            err.to_string().contains("system") && err.to_string().contains("rejected"),
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

    #[test]
    fn parse_sign_target_accepts_globs() {
        let target = parse_sign_target("knowledge:smoke/*").unwrap();
        assert_eq!(target.kind, "knowledge");
        assert_eq!(target.bare_id, "smoke/*");

        let target = parse_sign_target("directive:agent/**/*").unwrap();
        assert_eq!(target.kind, "directive");
        assert_eq!(target.bare_id, "agent/**/*");
    }

    #[test]
    fn parse_sign_target_rejects_suffixes() {
        let err = parse_sign_target("knowledge:smoke/entry@t:2026-06-07T00:00:00Z").unwrap_err();
        assert!(err.to_string().contains("do not support suffixes"));

        let err = parse_sign_target("knowledge:smoke/*@t:2026-06-07T00:00:00Z").unwrap_err();
        assert!(err.to_string().contains("do not support suffixes"));
    }

    #[test]
    fn parse_sign_target_rejects_unsafe_globs() {
        let err = parse_sign_target("knowledge:../*").unwrap_err();
        assert!(err.to_string().contains("unsafe glob bare_id"));

        let err = parse_sign_target("knowledge:smoke/[abc]").unwrap_err();
        assert!(err.to_string().contains("invalid characters"));
    }

    #[test]
    fn sign_in_place_uses_validated_content_not_reread_bytes() {
        let _guard = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let key = SigningKey::generate(&mut OsRng);
        let key_path = tmp
            .path()
            .join(ryeos_engine::AI_DIR)
            .join("config")
            .join("keys")
            .join("signing")
            .join("private_key.pem");
        std::fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        let pem = key.to_pkcs8_pem(Default::default()).unwrap();
        std::fs::write(&key_path, pem.as_bytes()).unwrap();
        let old_app_root = std::env::var_os("RYEOS_APP_ROOT");
        std::env::set_var("RYEOS_APP_ROOT", tmp.path());

        let item_path = tmp.path().join("item.yaml");
        std::fs::write(&item_path, "name: unvalidated\n").unwrap();
        let envelope = SignatureEnvelope {
            prefix: "#".to_string(),
            suffix: None,
            after_shebang: false,
        };

        let validated = "name: validated\n";
        let outcome = sign_in_place(&item_path, validated, &envelope).unwrap();
        assert!(matches!(outcome, SignOutcome::Signed(_)));

        let written = std::fs::read_to_string(&item_path).unwrap();
        let stripped = lillux::signature::strip_signature_lines_with_envelope(
            &written,
            &envelope.prefix,
            envelope.suffix.as_deref(),
        );
        assert_eq!(stripped, validated);
        assert!(!written.contains("unvalidated"));

        if let Some(old_app_root) = old_app_root {
            std::env::set_var("RYEOS_APP_ROOT", old_app_root);
        } else {
            std::env::remove_var("RYEOS_APP_ROOT");
        }
    }

    #[test]
    fn sign_warnings_flags_tool_with_null_executor_id() {
        let parsed = serde_json::json!({"executor_id": null});
        let warnings = sign_warnings("tool", &parsed);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("executor_id: null"));

        assert!(
            sign_warnings("tool", &serde_json::json!({"executor_id": "@subprocess"})).is_empty()
        );
        assert!(sign_warnings("knowledge", &parsed).is_empty());
    }
}
