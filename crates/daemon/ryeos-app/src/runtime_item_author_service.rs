//! Daemon-mediated runtime callback for signed project item authoring.
//!
//! Runtimes may propose bytes for a project item, but they never receive an
//! item-signing key. The daemon authenticates the live runtime callback,
//! authorizes the target with `ryeos.author.<kind>.<bare_id>`, derives the path
//! from the kind schema, injects signed provenance, signs with the daemon's
//! trusted identity, and atomically writes the normal project-space item.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use ryeos_bundle::runtime_authority::{item_author_cap, validate_bare_id_pattern};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::kind_registry::validate_metadata_anchoring;
use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::callback_token::{CallbackCapability, ThreadAuthState};
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
        let project_root = cap
            .provenance
            .durable_live_write_root("project")
            .context("runtime item authoring requires durable live-project write authority")?;
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
        let canonical_ai_root = open_author_ai_root(project_root)?;
        let canonical_kind_dir =
            ensure_relative_dir_no_symlinks(canonical_ai_root, Path::new(&kind_schema.directory))?;
        let bare_path = Path::new(&canonical.bare_id);
        let bare_parent = bare_path.parent().unwrap_or_else(|| Path::new(""));
        let canonical_parent = ensure_relative_dir_no_symlinks(canonical_kind_dir, bare_parent)?;
        let stem_name = bare_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "target filename is not valid unicode: {}",
                    canonical.bare_id
                )
            })?;
        // Lock the canonical ref, not the eventual extension-specific target.
        // Two concurrent creates choosing different declared extensions must
        // not both pass the duplicate-ref check.
        let author_lock_key = project_root
            .join(ryeos_engine::AI_DIR)
            .join(&kind_schema.directory)
            .join(&canonical.bare_id);
        let target_lock = author_target_lock(&author_lock_key);
        let _commit_guard = target_lock.lock().unwrap_or_else(|e| e.into_inner());

        let (ext, target_name, target) = resolve_author_target(
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
            &canonical_parent.ai_root,
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
            &canonical_parent.ai_root,
            &target,
        )
        .map_err(|e| {
            anyhow!("path-anchoring validator refused final authored item `{canonical}`: {e}")
        })?;

        let mut incumbent = canonical_parent
            .directory
            .open_regular(&target_name, false)
            .with_context(|| format!("open incumbent authored item {}", target.display()))?;
        if params.mode == AuthorMode::Create && incumbent.is_some() {
            bail!(
                "item already exists for canonical ref {canonical}: {}",
                target.display()
            );
        }
        if params.mode == AuthorMode::Upsert {
            if let Some(expected) = params.expected_digest.as_deref() {
                enforce_expected_digest_from_file(
                    incumbent.as_mut(),
                    &target,
                    expected,
                    &source_format.signature.prefix,
                    source_format.signature.suffix.as_deref(),
                )?;
            }
        }

        write_atomic(
            &canonical_parent.directory,
            &target_name,
            &target,
            &signed,
            params.mode,
            incumbent.as_ref(),
        )?;

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
    canonical_parent: &AuthorDirectory,
    stem_name: &str,
    requested_ext: Option<&str>,
    mode: AuthorMode,
) -> Result<(String, OsString, PathBuf)> {
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
            let name = OsString::from(format!("{stem_name}{ext}"));
            Ok((
                ext.to_string(),
                name.clone(),
                canonical_parent.path.join(name),
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
                let name = OsString::from(format!("{stem_name}{ext}"));
                Ok((
                    ext.to_string(),
                    name.clone(),
                    canonical_parent.path.join(name),
                ))
            }
            None => match existing.as_slice() {
                [item] => Ok((item.ext.clone(), item.name.clone(), item.path.clone())),
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
    name: OsString,
    path: PathBuf,
}

struct AuthorDirectory {
    directory: lillux::secure_fs::PinnedDirectory,
    path: PathBuf,
    ai_root: PathBuf,
}

fn open_author_ai_root(project_root: &Path) -> Result<AuthorDirectory> {
    let project = lillux::secure_fs::PinnedDirectory::open(project_root)?.ok_or_else(|| {
        anyhow!(
            "authorized project root disappeared: {}",
            project_root.display()
        )
    })?;
    let ai_name = OsStr::new(ryeos_engine::AI_DIR);
    let directory = project.open_child_directory(ai_name)?.ok_or_else(|| {
        anyhow!(
            "authorized project has no {} directory",
            ryeos_engine::AI_DIR
        )
    })?;
    let ai_root = project_root.join(ryeos_engine::AI_DIR);
    Ok(AuthorDirectory {
        directory,
        path: ai_root.clone(),
        ai_root,
    })
}

fn existing_canonical_item_files(
    kind_schema: &ryeos_engine::kind_registry::KindSchema,
    canonical_parent: &AuthorDirectory,
    stem_name: &str,
) -> Result<Vec<ExistingItemFile>> {
    let mut existing = Vec::new();
    for spec in &kind_schema.extensions {
        let name = OsString::from(format!("{}{}", stem_name, spec.ext));
        let path = canonical_parent.path.join(&name);
        match canonical_parent.directory.open_entry(&name, false)? {
            Some(lillux::secure_fs::PinnedDirectoryEntry::Regular(_)) => {
                existing.push(ExistingItemFile {
                    ext: spec.ext.clone(),
                    name,
                    path,
                });
            }
            Some(lillux::secure_fs::PinnedDirectoryEntry::Directory(_)) => bail!(
                "refusing to author over non-file item path: {}",
                path.display()
            ),
            None => {}
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

fn ensure_relative_dir_no_symlinks(
    root: AuthorDirectory,
    relative: &Path,
) -> Result<AuthorDirectory> {
    let mut current = root;
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            bail!(
                "refusing non-relative authored item directory component in {}",
                relative.display()
            );
        };
        let next_path = current.path.join(segment);
        let next = current
            .directory
            .open_or_create_child(segment, 0o755)
            .with_context(|| {
                format!(
                    "open or create authored item directory {}",
                    next_path.display()
                )
            })?;
        current = AuthorDirectory {
            directory: next,
            path: next_path,
            ai_root: current.ai_root,
        };
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

fn write_atomic(
    parent: &lillux::secure_fs::PinnedDirectory,
    target_name: &OsStr,
    target: &Path,
    content: &str,
    mode: AuthorMode,
    incumbent: Option<&std::fs::File>,
) -> Result<()> {
    if mode == AuthorMode::Create && incumbent.is_some() {
        bail!(
            "refusing to replace existing authored item {}",
            target.display()
        );
    }
    parent
        .atomic_write_if_same(target_name, incumbent, content.as_bytes(), 0o644)
        .with_context(|| format!("atomically publish authored item {}", target.display()))
}

/// Per-target keyed lock registry for the author read-compare-commit critical
/// section.
///
/// **Design note (deliberate — not drift):** a process-scoped keyed lock map,
/// keyed by durable project + canonical ref. Descriptor-relative publication
/// independently refuses an unexpected incumbent inode, while this lock keeps
/// two daemon callbacks from selecting different extensions for one ref. The
/// map grows one entry per distinct ref authored during the daemon lifetime.
fn author_target_lock(canonical_ref_key: &Path) -> Arc<Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut map = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(canonical_ref_key.to_path_buf())
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
fn enforce_expected_digest_from_file(
    incumbent: Option<&mut std::fs::File>,
    target: &Path,
    expected: &str,
    prefix: &str,
    suffix: Option<&str>,
) -> Result<()> {
    match read_incumbent_state_from_file(incumbent, target, prefix, suffix)? {
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
fn read_incumbent_state_from_file(
    incumbent: Option<&mut std::fs::File>,
    target: &Path,
    prefix: &str,
    suffix: Option<&str>,
) -> Result<IncumbentState> {
    let Some(incumbent) = incumbent else {
        return Ok(IncumbentState::Missing);
    };
    incumbent
        .rewind()
        .with_context(|| format!("seek incumbent item {}", target.display()))?;
    let mut content = String::new();
    incumbent
        .read_to_string(&mut content)
        .with_context(|| format!("read incumbent item {}", target.display()))?;
    for line in content.lines() {
        let trimmed = line.trim();
        let Some(after_prefix) = trimmed.strip_prefix(prefix).map(str::trim_start) else {
            continue;
        };
        let Some(after_marker) = after_prefix
            .strip_prefix(PROVENANCE_MARKER)
            .map(str::trim_start)
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
fn read_incumbent_state(
    target: &Path,
    prefix: &str,
    suffix: Option<&str>,
) -> Result<IncumbentState> {
    let mut incumbent = match std::fs::File::open(target) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("open incumbent item {}", target.display()))
        }
    };
    read_incumbent_state_from_file(incumbent.as_mut(), target, prefix, suffix)
}

#[cfg(test)]
fn enforce_expected_digest(
    target: &Path,
    expected: &str,
    prefix: &str,
    suffix: Option<&str>,
) -> Result<()> {
    let mut incumbent = match std::fs::File::open(target) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("open incumbent item {}", target.display()))
        }
    };
    enforce_expected_digest_from_file(incumbent.as_mut(), target, expected, prefix, suffix)
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
    fn descriptor_publication_writes_only_the_selected_durable_root() {
        let durable = tempfile::tempdir().unwrap();
        let disposable = tempfile::tempdir().unwrap();
        std::fs::create_dir(durable.path().join(ryeos_engine::AI_DIR)).unwrap();
        std::fs::create_dir(disposable.path().join(ryeos_engine::AI_DIR)).unwrap();

        let parent = ensure_relative_dir_no_symlinks(
            open_author_ai_root(durable.path()).unwrap(),
            Path::new("knowledge/arc/games/test"),
        )
        .unwrap();
        let name = OsStr::new("model.md");
        let target = parent.path.join(name);
        write_atomic(
            &parent.directory,
            name,
            &target,
            "first",
            AuthorMode::Create,
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        assert!(!disposable
            .path()
            .join(".ai/knowledge/arc/games/test/model.md")
            .exists());

        let incumbent = parent.directory.open_regular(name, false).unwrap().unwrap();
        write_atomic(
            &parent.directory,
            name,
            &target,
            "second",
            AuthorMode::Upsert,
            Some(&incumbent),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "second");
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
        let target = write_file(
            dir.path(),
            "bad.yaml",
            "value: 1\n\n# ryeos:authored: {not json\n",
        );
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
        assert!(
            err.contains("expected-d") && err.contains("actual-d"),
            "{err}"
        );
        assert!(err.contains("T-incumbent"), "{err}");
    }

    #[test]
    fn enforce_conflicts_when_target_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("absent.yaml");
        let err = enforce_expected_digest(&target, "d1", "#", None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("conflict") && err.contains("no incumbent"),
            "{err}"
        );
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
