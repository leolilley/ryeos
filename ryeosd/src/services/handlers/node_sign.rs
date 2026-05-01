//! `node-sign` — sign daemon-internal `kind: node` items only.
//!
//! This is the daemon's internal signing service. It signs ONLY
//! `kind: node` items in `system` space — the daemon's own node-config
//! writes (bundle registrations, route entries, etc.).
//!
//! For operator edits in project/user space, use `rye-sign` (invokes
//! `ryeos_tools::actions::sign::run_sign` with the user key).
//! For bundle authoring, use `rye-bundle-tool sign-items` (uses the
//! author key explicitly).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::SignatureEnvelope;
use ryeos_engine::kind_registry::{validate_metadata_anchoring, KindSchema};
use ryeos_engine::parsers::ParserDispatcher;

use crate::service_executor::ServiceAvailability;
use crate::service_registry::ServiceDescriptor;
use crate::state::AppState;

/// Where to look for the item to sign. Restricted to `system` for
/// daemon-internal `kind: node` items only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum SignSpace {
    System,
    User,
    Project,
}

impl SignSpace {
    fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Canonical ref of the item to sign, e.g. `directive:hello`,
    /// `node:engine/kinds/config/config`, or a glob like
    /// `tool:rye/core/*`.
    pub item_ref: String,
    /// Space to look for the item in.
    pub space: SignSpace,
    /// Project root (parent of `.ai/`). REQUIRED when
    /// `space == project`; ignored otherwise.
    #[serde(default)]
    pub project_path: Option<PathBuf>,
}

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

#[derive(Debug, serde::Serialize)]
pub struct ItemOutcome {
    pub item_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<SignatureReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct SignatureReport {
    pub file: String,
    pub signer_fingerprint: String,
    pub signature_line: String,
    pub updated_at: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    let report = tokio::task::spawn_blocking(move || -> Result<BatchReport> {
        run_node_sign(&req, &state)
    })
    .await??;
    serde_json::to_value(report).map_err(Into::into)
}

fn run_node_sign(req: &Request, state: &AppState) -> Result<BatchReport> {
    // ── Scope restriction ──
    // node-sign is daemon-internal only. Reject all non-system spaces
    // and reject system space for non-node kinds.
    match req.space {
        SignSpace::User | SignSpace::Project => {
            bail!(
                "service:node-sign does not sign {}-space items — \
                 use `rye-sign` for operator edits (invokes the user signing key)",
                req.space.label()
            );
        }
        SignSpace::System => { /* allowed, but kind checked below */ }
    }

    let canonical = CanonicalRef::parse(&req.item_ref)
        .map_err(|e| anyhow!("malformed canonical ref `{}`: {e}", req.item_ref))?;

    if canonical.kind != "node" {
        bail!(
            "service:node-sign only signs kind=node items in system space — \
             kind=`{}` is not permitted. For bundle authoring use \
             `rye-bundle-tool sign-items`; for operator edits use `rye-sign`",
            canonical.kind
        );
    }

    let kind_schema = state.engine.kinds.get(&canonical.kind).ok_or_else(|| {
        anyhow!(
            "unknown kind `{}` — no kind schema registered",
            canonical.kind
        )
    })?;

    let kind_dirs = space_kind_dirs(req, kind_schema, state)?;
    if kind_dirs.is_empty() {
        bail!(
            "no `{}` directories found in {} space",
            kind_schema.directory,
            req.space.label()
        );
    }

    let mut targets: Vec<(PathBuf, PathBuf)> = Vec::new();
    if is_glob(&canonical.bare_id) {
        for (ai_root, kind_dir) in &kind_dirs {
            for path in glob_match_items(kind_dir, kind_schema, &canonical.bare_id)? {
                targets.push((ai_root.clone(), path));
            }
        }
    } else {
        let mut found = None;
        'outer: for (ai_root, kind_dir) in &kind_dirs {
            for spec in &kind_schema.extensions {
                let candidate = kind_dir.join(format!("{}{}", canonical.bare_id, spec.ext));
                if candidate.is_file() {
                    found = Some((ai_root.clone(), candidate));
                    break 'outer;
                }
            }
        }
        match found {
            Some(p) => targets.push(p),
            None => bail!(
                "item `{}:{}` not found in {} (searched {} dirs with extensions {:?})",
                canonical.kind,
                canonical.bare_id,
                req.space.label(),
                kind_dirs.len(),
                kind_schema.extension_strs()
            ),
        }
    }

    if targets.is_empty() {
        bail!(
            "no items matched `{}:{}` in {} space",
            canonical.kind,
            canonical.bare_id,
            req.space.label()
        );
    }

    targets.sort_by(|a, b| a.1.cmp(&b.1));

    let signing_key = state.identity.signing_key();
    let fingerprint = state.identity.fingerprint().to_string();

    let mut report = BatchReport::default();
    for (ai_root, file_path) in targets {
        let bare_id = derive_bare_id(&file_path, kind_schema, &kind_dirs)
            .unwrap_or_else(|| file_path.display().to_string());
        let display_ref = format!("{}:{}", canonical.kind, bare_id);

        match sign_one(
            &file_path,
            kind_schema,
            &ai_root,
            &state.engine.parser_dispatcher,
            signing_key,
            &fingerprint,
        ) {
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

/// Return `(ai_root, kind_dir)` pairs to search in priority order.
/// `system` returns one entry per system bundle root.
fn space_kind_dirs(
    req: &Request,
    kind_schema: &KindSchema,
    state: &AppState,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    use ryeos_engine::AI_DIR;
    match req.space {
        SignSpace::System => {
            let mut out = Vec::new();
            for sys_root in &state.engine.system_roots {
                let ai_root = sys_root.join(AI_DIR);
                let kind_dir = ai_root.join(&kind_schema.directory);
                out.push((ai_root, kind_dir));
            }
            Ok(out)
        }
        SignSpace::User => {
            let user_root = state
                .engine
                .user_root
                .as_ref()
                .ok_or_else(|| anyhow!("space=user requested but daemon has no user root"))?;
            let ai_root = user_root.join(AI_DIR);
            let kind_dir = ai_root.join(&kind_schema.directory);
            Ok(vec![(ai_root, kind_dir)])
        }
        SignSpace::Project => {
            let proj = req
                .project_path
                .as_ref()
                .ok_or_else(|| anyhow!("space=project requires `project_path` in request"))?;
            let ai_root = proj.join(AI_DIR);
            let kind_dir = ai_root.join(&kind_schema.directory);
            Ok(vec![(ai_root, kind_dir)])
        }
    }
}

fn sign_one(
    file_path: &Path,
    kind_schema: &KindSchema,
    ai_root: &Path,
    parsers: &ParserDispatcher,
    signing_key: &lillux::crypto::SigningKey,
    fingerprint: &str,
) -> Result<SignatureReport> {
    let content = std::fs::read_to_string(file_path)
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

    sign_in_place(file_path, &source_format.signature, signing_key, fingerprint)
}

fn sign_in_place(
    input: &Path,
    envelope: &SignatureEnvelope,
    signing_key: &lillux::crypto::SigningKey,
    fingerprint: &str,
) -> Result<SignatureReport> {
    let body = std::fs::read_to_string(input)
        .with_context(|| format!("read {}", input.display()))?;

    let stripped = lillux::signature::strip_signature_lines(&body);
    let signed = lillux::signature::sign_content(
        &stripped,
        signing_key,
        &envelope.prefix,
        envelope.suffix.as_deref(),
    );

    let tmp = input.with_extension(format!("signed.tmp.{}", std::process::id()));
    std::fs::write(&tmp, &signed)
        .with_context(|| format!("write tmp {}", tmp.display()))?;
    std::fs::rename(&tmp, input)
        .with_context(|| format!("rename {} -> {}", tmp.display(), input.display()))?;

    let needle = format!("{} rye:signed:", envelope.prefix);
    let signature_line = signed
        .lines()
        .find(|l| l.starts_with(&needle))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "signature applied".to_string());

    Ok(SignatureReport {
        file: input.display().to_string(),
        signer_fingerprint: fingerprint.to_string(),
        signature_line,
        updated_at: lillux::time::iso8601_now(),
    })
}

fn is_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

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

fn derive_bare_id(
    file_path: &Path,
    kind_schema: &KindSchema,
    kind_dirs: &[(PathBuf, PathBuf)],
) -> Option<String> {
    for (_ai_root, kind_dir) in kind_dirs {
        if let Ok(rel) = file_path.strip_prefix(kind_dir) {
            let s = rel.to_string_lossy().to_string();
            for spec in &kind_schema.extensions {
                if let Some(stripped) = s.strip_suffix(&spec.ext) {
                    return Some(stripped.to_string());
                }
            }
        }
    }
    None
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:node-sign",
    endpoint: "node-sign",
    availability: ServiceAvailability::Both,
    handler: |params, state| {
        Box::pin(async move {
            let req: Request = serde_json::from_value(params)
                .map_err(|e| anyhow::anyhow!("invalid node-sign params: {e}"))?;
            handle(req, state).await
        })
    },
};
