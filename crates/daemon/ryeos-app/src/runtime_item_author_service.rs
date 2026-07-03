//! Daemon-mediated runtime callback for signed project item authoring.
//!
//! Runtimes may propose bytes for a project item, but they never receive an
//! item-signing key. The daemon authenticates the live runtime callback,
//! authorizes the target with `ryeos.author.<kind>.<bare_id>`, derives the path
//! from the kind schema, injects signed provenance, signs with the daemon's
//! trusted identity, and atomically writes the normal project-space item.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use ryeos_bundle::runtime_authority::{item_author_cap, validate_bare_id_pattern};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::kind_registry::validate_metadata_anchoring;
use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::callback_token::{CallbackCapability, ThreadAuthState};
use crate::execution_provenance::ProjectSourceKind;
use crate::identity::NodeIdentity;

const PROVENANCE_MARKER: &str = "ryeos:authored:";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAuthorItemParams {
    pub thread_id: String,
    pub item_ref: String,
    pub content: String,
    #[serde(default)]
    pub mode: AuthorMode,
    #[serde(default)]
    pub format_ext: Option<String>,
    /// Compare-and-swap guard for `mode: upsert`. When set, the upsert commits
    /// only if the incumbent item's authored `content_digest` equals this value;
    /// otherwise it fails with a conflict carrying the incumbent's provenance.
    /// Absent = unguarded upsert (last-writer-wins).
    #[serde(default)]
    pub expected_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthorMode {
    #[default]
    Create,
    Upsert,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeAuthorItemResponse {
    pub item_ref: String,
    pub path: String,
    pub mode: AuthorMode,
    pub signer_fingerprint: String,
    pub content_digest: String,
    pub required_capability: String,
}

pub struct RuntimeItemAuthorService;

impl RuntimeItemAuthorService {
    pub fn author(
        identity: &NodeIdentity,
        authorizer: &Authorizer,
        cap: &CallbackCapability,
        thread_auth: &ThreadAuthState,
        params: RuntimeAuthorItemParams,
    ) -> Result<RuntimeAuthorItemResponse> {
        if cap.provenance.project_source() != ProjectSourceKind::LiveFs {
            bail!("runtime item authoring only supports live filesystem project provenance in v1");
        }
        if params.content.contains(PROVENANCE_MARKER) {
            bail!("runtime-authored item content must not contain `{PROVENANCE_MARKER}`");
        }

        let canonical = CanonicalRef::parse(&params.item_ref)
            .with_context(|| format!("invalid item_ref `{}`", params.item_ref))?;
        if canonical.suffix.is_some() {
            bail!("runtime item authoring target must not include a ref suffix");
        }
        validate_bare_id_pattern("item author target bare_id", &canonical.bare_id)
            .map_err(anyhow::Error::msg)?;

        let required_cap = item_author_cap(&canonical.kind, &canonical.bare_id);
        authorizer
            .authorize(&cap.effective_caps, &AuthorizationPolicy::require(&required_cap))
            .with_context(|| {
                format!(
                    "missing required capability: {required_cap} — item authoring is daemon-mediated runtime authority"
                )
            })?;

        // Use the per-request engine captured on the callback capability. That
        // engine includes the exact project/bundle overlays used to launch this
        // runtime; the daemon's process-wide engine is not a safe fallback.
        let engine = cap.provenance.request_engine();
        let kind_schema = engine.kinds.get(&canonical.kind).ok_or_else(|| {
            anyhow!(
                "unknown kind `{}` — no kind schema registered for item authoring",
                canonical.kind
            )
        })?;
        let project_root = cap.provenance.effective_path();
        let ai_root = project_root.join(ryeos_engine::AI_DIR);
        let canonical_ai_root = ai_root
            .canonicalize()
            .with_context(|| format!("canonicalize ai root {}", ai_root.display()))?;
        let canonical_kind_dir =
            ensure_relative_dir_no_symlinks(&canonical_ai_root, Path::new(&kind_schema.directory))?;
        let bare_path = Path::new(&canonical.bare_id);
        let bare_parent = bare_path.parent().unwrap_or_else(|| Path::new(""));
        let canonical_parent = ensure_relative_dir_no_symlinks(&canonical_kind_dir, bare_parent)?;
        let stem_name = bare_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "target filename is not valid unicode: {}",
                    canonical.bare_id
                )
            })?;
        let (ext, target) = resolve_author_target(
            kind_schema,
            &canonical,
            &canonical_parent,
            stem_name,
            params.format_ext.as_deref(),
            params.mode,
        )?;
        let source_format = kind_schema.resolved_format_for(&ext).ok_or_else(|| {
            anyhow!(
                "kind `{}` does not declare extension `{ext}`",
                canonical.kind
            )
        })?;
        if contains_signature_line(
            &params.content,
            &source_format.signature.prefix,
            source_format.signature.suffix.as_deref(),
        ) {
            bail!(
                "runtime-authored item content must be unsigned; signature lines are daemon-owned"
            );
        }

        let parsed = engine
            .parser_dispatcher
            .dispatch(
                &source_format.parser,
                &params.content,
                Some(&target),
                &source_format.signature,
            )
            .with_context(|| format!("parse authored item body for `{}`", canonical))?;
        validate_metadata_anchoring(
            &parsed,
            &kind_schema.extraction_rules,
            &kind_schema.directory,
            &canonical_ai_root,
            &target,
        )
        .map_err(|e| {
            anyhow!("path-anchoring validator refused authored item `{canonical}`: {e}")
        })?;

        let content_digest = lillux::cas::sha256_hex(params.content.as_bytes());
        let provenance = json!({
            "authored_by": "runtime",
            "authored_at": lillux::time::iso8601_now(),
            "thread_id": params.thread_id.clone(),
            "invocation_id": cap.invocation_id.clone(),
            "parent_item_ref": cap.item_ref.clone(),
            "acting_principal": thread_auth.acting_principal.clone(),
            "effective_bundle_id": cap.effective_bundle_id.clone(),
            "target_ref": canonical.to_string(),
            "format_ext": ext.clone(),
            "path": target.display().to_string(),
            "authoring_capability": required_cap.clone(),
            "content_digest": content_digest.clone(),
        });
        let body = append_provenance_comment(
            &params.content,
            &source_format.signature.prefix,
            source_format.signature.suffix.as_deref(),
            &serde_json::to_string(&provenance)?,
        );
        let signed = lillux::signature::sign_content(
            &body,
            identity.signing_key(),
            &source_format.signature.prefix,
            source_format.signature.suffix.as_deref(),
        );
        let final_parsed = engine
            .parser_dispatcher
            .dispatch(
                &source_format.parser,
                &signed,
                Some(&target),
                &source_format.signature,
            )
            .with_context(|| format!("parse final signed authored item for `{}`", canonical))?;
        validate_metadata_anchoring(
            &final_parsed,
            &kind_schema.extraction_rules,
            &kind_schema.directory,
            &canonical_ai_root,
            &target,
        )
        .map_err(|e| {
            anyhow!("path-anchoring validator refused final authored item `{canonical}`: {e}")
        })?;

        // Per-target critical section: read-incumbent → compare → commit must be
        // atomic against a concurrent author of the same item, or the CAS guard is
        // a TOCTOU. The lock is keyed by the canonicalized target path and serializes
        // author calls within this daemon process (the sole item writer under the
        // live-fs v1 contract). See `author_target_lock`.
        let target_lock = author_target_lock(&target);
        let _commit_guard = target_lock.lock().unwrap_or_else(|e| e.into_inner());

        if params.mode == AuthorMode::Upsert {
            if let Some(expected) = params.expected_digest.as_deref() {
                enforce_expected_digest(
                    &target,
                    expected,
                    &source_format.signature.prefix,
                    source_format.signature.suffix.as_deref(),
                )?;
            }
        }

        write_atomic(&target, &signed, params.mode)?;

        Ok(RuntimeAuthorItemResponse {
            item_ref: canonical.to_string(),
            path: target.display().to_string(),
            mode: params.mode,
            signer_fingerprint: identity.fingerprint().to_string(),
            content_digest,
            required_capability: required_cap,
        })
    }
}

fn validate_extension(
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    ext: &str,
) -> Result<()> {
    if !ext.starts_with('.') {
        bail!("format_ext must include leading dot");
    }
    if kind_schema.spec_for(ext).is_none() {
        bail!(
            "format_ext `{}` is not declared for kind (declared: {:?})",
            ext,
            kind_schema.extension_strs()
        );
    }
    Ok(())
}

fn resolve_author_target(
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    canonical: &CanonicalRef,
    canonical_parent: &Path,
    stem_name: &str,
    requested_ext: Option<&str>,
    mode: AuthorMode,
) -> Result<(String, PathBuf)> {
    if let Some(ext) = requested_ext {
        validate_extension(kind_schema, ext)?;
    }

    let existing = existing_canonical_item_files(kind_schema, canonical_parent, stem_name)?;
    match mode {
        AuthorMode::Create => {
            if !existing.is_empty() {
                bail!(
                    "item already exists for canonical ref {}: {}",
                    canonical,
                    existing_paths(&existing)
                );
            }
            let ext = requested_ext.ok_or_else(|| {
                anyhow!(
                    "format_ext is required when creating new item {}; declared extensions: {:?}",
                    canonical,
                    kind_schema.extension_strs()
                )
            })?;
            Ok((
                ext.to_string(),
                canonical_parent.join(format!("{stem_name}{ext}")),
            ))
        }
        AuthorMode::Upsert => match requested_ext {
            Some(ext) => {
                if let Some(other) = existing.iter().find(|item| item.ext != ext) {
                    bail!(
                        "refusing to create duplicate file for canonical ref {}: requested `{}` but existing item uses `{}` at {}",
                        canonical,
                        ext,
                        other.ext,
                        other.path.display()
                    );
                }
                Ok((
                    ext.to_string(),
                    canonical_parent.join(format!("{stem_name}{ext}")),
                ))
            }
            None => match existing.as_slice() {
                [item] => Ok((item.ext.clone(), item.path.clone())),
                [] => bail!(
                    "format_ext is required when upserting new item {}; declared extensions: {:?}",
                    canonical,
                    kind_schema.extension_strs()
                ),
                _ => bail!(
                    "ambiguous duplicate files for canonical ref {}: {}",
                    canonical,
                    existing_paths(&existing)
                ),
            },
        },
    }
}

#[derive(Debug)]
struct ExistingItemFile {
    ext: String,
    path: PathBuf,
}

fn existing_canonical_item_files(
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    canonical_parent: &Path,
    stem_name: &str,
) -> Result<Vec<ExistingItemFile>> {
    let mut existing = Vec::new();
    for spec in &kind_schema.extensions {
        let path = canonical_parent.join(format!("{}{}", stem_name, spec.ext));
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    bail!(
                        "refusing to author item through symlink: {}",
                        path.display()
                    );
                }
                if !meta.is_file() {
                    bail!(
                        "refusing to author over non-file item path: {}",
                        path.display()
                    );
                }
                existing.push(ExistingItemFile {
                    ext: spec.ext.clone(),
                    path,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
        }
    }
    Ok(existing)
}

fn existing_paths(existing: &[ExistingItemFile]) -> String {
    existing
        .iter()
        .map(|item| item.path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn ensure_relative_dir_no_symlinks(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            bail!(
                "refusing non-relative authored item directory component in {}",
                relative.display()
            );
        };
        let next = current.join(segment);
        match std::fs::symlink_metadata(&next) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    bail!(
                        "refusing to create authored item directory through symlink: {}",
                        next.display()
                    );
                }
                if !meta.is_dir() {
                    bail!(
                        "refusing to create authored item directory over non-directory: {}",
                        next.display()
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&next)
                    .with_context(|| format!("create dir {}", next.display()))?;
                let meta = std::fs::symlink_metadata(&next)
                    .with_context(|| format!("stat created dir {}", next.display()))?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    bail!(
                        "created authored item path is not a plain directory: {}",
                        next.display()
                    );
                }
            }
            Err(e) => return Err(e).with_context(|| format!("stat {}", next.display())),
        }
        current = next;
    }
    Ok(current)
}

fn append_provenance_comment(
    content: &str,
    prefix: &str,
    suffix: Option<&str>,
    json: &str,
) -> String {
    let mut body = content.trim_end().to_string();
    body.push_str("\n\n");
    match suffix {
        Some(suffix) => body.push_str(&format!("{prefix} {PROVENANCE_MARKER} {json} {suffix}\n")),
        None => body.push_str(&format!("{prefix} {PROVENANCE_MARKER} {json}\n")),
    }
    body
}

fn contains_signature_line(content: &str, prefix: &str, suffix: Option<&str>) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim();
        let Some(after_prefix) = trimmed.strip_prefix(prefix).map(str::trim_start) else {
            return false;
        };
        let payload_area = match suffix {
            Some(s) => after_prefix
                .trim_end()
                .strip_suffix(s)
                .map(str::trim_end)
                .unwrap_or_else(|| after_prefix.trim_end()),
            None => after_prefix.trim_end(),
        };
        payload_area.starts_with(lillux::signature::SIGNATURE_PREFIX)
            || payload_area == "ryeos:signed"
            || payload_area.starts_with("ryeos:signed ")
    })
}

fn write_atomic(target: &Path, content: &str, mode: AuthorMode) -> Result<()> {
    // The target was derived from a canonicalized kind-directory parent. This
    // v1 live-filesystem path treats local runtimes as capability-restricted
    // callers, not as concurrent filesystem adversaries that can swap checked
    // parent directories between validation and commit. A fully adversarial
    // filesystem boundary should replace this path-based commit with an
    // openat/renameat style directory-fd implementation.
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("target path has no parent: {}", target.display()))?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("target filename is not valid unicode: {}", target.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp: PathBuf = parent.join(format!(
        ".{file_name}.authoring.tmp.{}.{}",
        std::process::id(),
        nonce
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(content.as_bytes())?;
            f.sync_all()
        })
        .with_context(|| format!("write tmp {}", tmp.display()))?;
    let commit_result = match mode {
        AuthorMode::Create => std::fs::hard_link(&tmp, target)
            .with_context(|| format!("link {} -> {}", tmp.display(), target.display()))
            .map(|_| {
                let _ = std::fs::remove_file(&tmp);
            }),
        AuthorMode::Upsert => std::fs::rename(&tmp, target)
            .with_context(|| format!("rename {} -> {}", tmp.display(), target.display())),
    };
    if let Err(err) = commit_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Per-target keyed lock registry for the author read-compare-commit critical
/// section.
///
/// **Design note (deliberate — not drift):** a process-scoped keyed lock map,
/// keyed by canonicalized target path. The daemon is the sole item writer under
/// the live-fs v1 contract (`author` bails on non-live-fs provenance and is
/// capability-gated), so serializing within this process is sufficient. If an
/// out-of-process writer is ever introduced, switch to an advisory `flock`, or
/// hang this registry off `AppState`. The map grows one entry per distinct
/// target ever authored — acceptable for v1.
fn author_target_lock(target: &Path) -> Arc<Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut map = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(target.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// The incumbent item's authoring state, read from its signed provenance comment.
#[derive(Debug)]
enum IncumbentState {
    /// No file at the target path.
    Missing,
    /// A file exists. `content_digest` is `None` when it carries no readable
    /// `ryeos:authored:` provenance (hand-written, or a malformed payload).
    Present {
        content_digest: Option<String>,
        thread_id: Option<String>,
        authored_at: Option<String>,
    },
}

/// Reject an upsert whose incumbent's authored `content_digest` does not match
/// the caller's `expected_digest`. All three cases fail loudly as a conflict:
/// missing target, missing/malformed incumbent provenance, digest mismatch.
fn enforce_expected_digest(
    target: &Path,
    expected: &str,
    prefix: &str,
    suffix: Option<&str>,
) -> Result<()> {
    match read_incumbent_state(target, prefix, suffix)? {
        IncumbentState::Missing => bail!(
            "author_item conflict: expected_digest {expected} but no incumbent item exists at {}",
            target.display()
        ),
        IncumbentState::Present {
            content_digest: None,
            ..
        } => bail!(
            "author_item conflict: incumbent at {} has no readable authored provenance to compare against expected_digest {expected}",
            target.display()
        ),
        IncumbentState::Present {
            content_digest: Some(actual),
            thread_id,
            authored_at,
        } => {
            if actual != expected {
                bail!(
                    "author_item conflict: expected_digest {expected} but incumbent content_digest is {actual} \
                     (authored by thread {} at {})",
                    thread_id.as_deref().unwrap_or("<unknown>"),
                    authored_at.as_deref().unwrap_or("<unknown>")
                );
            }
            Ok(())
        }
    }
}

/// Read the incumbent's authoring provenance from its signed file, using the
/// item format's signature comment envelope (prefix/suffix) — NOT a naïve line
/// scan. Read-only; never mutates the file. A malformed provenance payload
/// resolves to `Present { content_digest: None, .. }` (→ conflict), not an error.
fn read_incumbent_state(
    target: &Path,
    prefix: &str,
    suffix: Option<&str>,
) -> Result<IncumbentState> {
    let content = match std::fs::read_to_string(target) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(IncumbentState::Missing),
        Err(e) => {
            return Err(e).with_context(|| format!("read incumbent item {}", target.display()))
        }
    };
    for line in content.lines() {
        let trimmed = line.trim();
        let Some(after_prefix) = trimmed.strip_prefix(prefix).map(str::trim_start) else {
            continue;
        };
        let Some(after_marker) = after_prefix.strip_prefix(PROVENANCE_MARKER).map(str::trim_start)
        else {
            continue;
        };
        let payload = match suffix {
            Some(s) => after_marker
                .trim_end()
                .strip_suffix(s)
                .map(str::trim_end)
                .unwrap_or_else(|| after_marker.trim_end()),
            None => after_marker.trim_end(),
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) else {
            return Ok(IncumbentState::Present {
                content_digest: None,
                thread_id: None,
                authored_at: None,
            });
        };
        let get = |k: &str| json.get(k).and_then(|v| v.as_str()).map(String::from);
        return Ok(IncumbentState::Present {
            content_digest: get("content_digest"),
            thread_id: get("thread_id"),
            authored_at: get("authored_at"),
        });
    }
    // File exists but has no provenance comment line.
    Ok(IncumbentState::Present {
        content_digest: None,
        thread_id: None,
        authored_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn provenance(digest: &str) -> String {
        serde_json::to_string(&json!({
            "content_digest": digest,
            "thread_id": "T-incumbent",
            "authored_at": "2026-07-03T00:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn incumbent_missing_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("absent.yaml");
        assert!(matches!(
            read_incumbent_state(&target, "#", None).unwrap(),
            IncumbentState::Missing
        ));
    }

    #[test]
    fn incumbent_present_with_yaml_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let body = append_provenance_comment("value: 1", "#", None, &provenance("digest-abc"));
        let target = write_file(dir.path(), "item.yaml", &body);
        match read_incumbent_state(&target, "#", None).unwrap() {
            IncumbentState::Present {
                content_digest,
                thread_id,
                ..
            } => {
                assert_eq!(content_digest.as_deref(), Some("digest-abc"));
                assert_eq!(thread_id.as_deref(), Some("T-incumbent"));
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn incumbent_present_with_md_suffix_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let body =
            append_provenance_comment("# doc", "<!--", Some("-->"), &provenance("digest-md"));
        let target = write_file(dir.path(), "item.md", &body);
        match read_incumbent_state(&target, "<!--", Some("-->")).unwrap() {
            IncumbentState::Present { content_digest, .. } => {
                assert_eq!(content_digest.as_deref(), Some("digest-md"))
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn incumbent_without_provenance_has_no_digest() {
        let dir = tempfile::tempdir().unwrap();
        let target = write_file(dir.path(), "plain.yaml", "value: 1\n");
        assert!(matches!(
            read_incumbent_state(&target, "#", None).unwrap(),
            IncumbentState::Present {
                content_digest: None,
                ..
            }
        ));
    }

    #[test]
    fn malformed_provenance_resolves_to_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = write_file(dir.path(), "bad.yaml", "value: 1\n\n# ryeos:authored: {not json\n");
        assert!(matches!(
            read_incumbent_state(&target, "#", None).unwrap(),
            IncumbentState::Present {
                content_digest: None,
                ..
            }
        ));
    }

    #[test]
    fn enforce_ok_when_digest_matches() {
        let dir = tempfile::tempdir().unwrap();
        let body = append_provenance_comment("value: 1", "#", None, &provenance("d1"));
        let target = write_file(dir.path(), "item.yaml", &body);
        enforce_expected_digest(&target, "d1", "#", None).unwrap();
    }

    #[test]
    fn enforce_conflicts_on_mismatch_carrying_incumbent_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let body = append_provenance_comment("value: 1", "#", None, &provenance("actual-d"));
        let target = write_file(dir.path(), "item.yaml", &body);
        let err = enforce_expected_digest(&target, "expected-d", "#", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("conflict"), "{err}");
        assert!(err.contains("expected-d") && err.contains("actual-d"), "{err}");
        assert!(err.contains("T-incumbent"), "{err}");
    }

    #[test]
    fn enforce_conflicts_when_target_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("absent.yaml");
        let err = enforce_expected_digest(&target, "d1", "#", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("conflict") && err.contains("no incumbent"), "{err}");
    }

    #[test]
    fn enforce_conflicts_when_incumbent_has_no_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let target = write_file(dir.path(), "plain.yaml", "value: 1\n");
        let err = enforce_expected_digest(&target, "d1", "#", None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("conflict") && err.contains("no readable authored provenance"),
            "{err}"
        );
    }
}
