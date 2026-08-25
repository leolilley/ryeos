//! Item resolution — project-first space search with clash diagnostics.
//!
//! Resolution order: project → bundles.
//! Clash warnings emitted when items exist in multiple spaces.
//!
//! All directory names and extension lists come from `KindSchema`.
//! This module never hardcodes kind strings, directories, or extensions.

use std::path::{Path, PathBuf};

use crate::canonical_ref::CanonicalRef;
use crate::contracts::{
    ItemSourceRoot, ItemSpace, ProbedAbsence, ShadowedCandidate, SignatureEnvelope, SignatureHeader,
};
use crate::error::EngineError;
use crate::kind_registry::KindSchema;

/// Maximum source bytes accepted for one resolvable RyeOS item.
///
/// This is shared by live-filesystem and admitted-CAS reads so changing the
/// source authority cannot change the item language's resource contract.
pub const MAX_ITEM_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// Read one live item source under the same bound used by admitted content.
pub fn read_item_source_no_follow(path: &std::path::Path) -> Result<String, EngineError> {
    let bytes = lillux::read_regular_file_bounded_no_follow(path, MAX_ITEM_SOURCE_BYTES).map_err(
        |error| {
            EngineError::Internal(format!(
                "securely read item source {}: {error:#}",
                path.display()
            ))
        },
    )?;
    String::from_utf8(bytes).map_err(|error| {
        EngineError::Internal(format!(
            "item source {} is not UTF-8: {error}",
            path.display()
        ))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredBundleRoot {
    pub name: String,
    pub canonical_root: PathBuf,
}

/// A single labeled search root.
#[derive(Debug, Clone)]
pub struct ResolutionRoot {
    pub space: ItemSpace,
    pub identity: ItemSourceRoot,
    /// Human-readable label, e.g. "project", "bundle:standard"
    pub label: String,
    /// Path to the `.ai/` directory
    pub ai_root: PathBuf,
    /// Exact registered content root, when this root was admitted from a
    /// canonical project or bundle registration.
    ///
    /// A caller that only supplies a loose `.ai` search path does not thereby
    /// grant access to its parent directory. Consumers of named content roots
    /// must require this explicit authority rather than reconstructing it from
    /// filesystem layout.
    pub content_root: Option<PathBuf>,
}

/// Ordered list of search roots for item resolution.
///
/// Constructed in project-first order: project, then bundles.
#[derive(Debug, Clone)]
pub struct ResolutionRoots {
    /// Search roots in resolution priority order (first match wins)
    pub ordered: Vec<ResolutionRoot>,
}

impl ResolutionRoots {
    /// Build from already-normalized `.ai` roots.
    /// The project `.ai` root is ordered first, then bundle `.ai` roots.
    pub fn from_flat(project_ai_root: Option<PathBuf>, bundle_ai_roots: Vec<PathBuf>) -> Self {
        let mut ordered = Vec::new();

        if let Some(project_ai_root) = project_ai_root {
            ordered.push(ResolutionRoot {
                space: ItemSpace::Project,
                identity: ItemSourceRoot::Search {
                    label: "project".to_owned(),
                },
                label: "project".to_owned(),
                ai_root: project_ai_root,
                content_root: None,
            });
        }

        for (i, bundle_ai_root) in bundle_ai_roots.iter().enumerate() {
            ordered.push(ResolutionRoot {
                space: ItemSpace::Bundle,
                identity: ItemSourceRoot::Search {
                    label: format!("bundle:{i}"),
                },
                label: format!("bundle:{i}"),
                ai_root: bundle_ai_root.clone(),
                content_root: None,
            });
        }

        Self { ordered }
    }

    /// Build from canonical source roots. Unlike [`Self::from_flat`], these
    /// inputs are bundle/project directories, so this constructor appends
    /// `.ai` exactly once.
    pub fn from_registered(
        project_root: Option<PathBuf>,
        bundles: &[RegisteredBundleRoot],
    ) -> Self {
        let mut ordered = Vec::new();
        if let Some(project_root) = project_root {
            ordered.push(ResolutionRoot {
                space: ItemSpace::Project,
                identity: ItemSourceRoot::Project,
                label: "project".to_owned(),
                ai_root: project_root.join(crate::AI_DIR),
                content_root: Some(project_root),
            });
        }
        ordered.extend(bundles.iter().map(|bundle| ResolutionRoot {
            space: ItemSpace::Bundle,
            identity: ItemSourceRoot::Bundle {
                name: bundle.name.clone(),
            },
            label: format!("bundle:{}", bundle.name),
            ai_root: bundle.canonical_root.join(crate::AI_DIR),
            content_root: Some(bundle.canonical_root.clone()),
        }));
        Self { ordered }
    }

    /// Resolve one authority-bearing source identity to its exact registered
    /// content root. Diagnostic paths and labels never participate in the
    /// identity join; they are checked only for consistency after the typed
    /// winner has been selected.
    pub fn authoritative_root(
        &self,
        identity: &ItemSourceRoot,
        space: ItemSpace,
        source_path: Option<&Path>,
    ) -> Result<&ResolutionRoot, EngineError> {
        if matches!(identity, ItemSourceRoot::Search { .. }) {
            return Err(EngineError::Internal(
                "a loose search root cannot supply content authority".to_owned(),
            ));
        }
        if !identity.matches_space(space) {
            return Err(EngineError::Internal(format!(
                "typed source root {identity:?} contradicts source space {space:?}"
            )));
        }
        let mut matches = self
            .ordered
            .iter()
            .filter(|root| root.identity == *identity && root.space == space);
        let root = matches.next().ok_or_else(|| {
            EngineError::Internal(format!(
                "typed source root {identity:?} is absent from the admitted resolution roots"
            ))
        })?;
        if matches.next().is_some() {
            return Err(EngineError::Internal(format!(
                "typed source root {identity:?} is duplicated in the admitted resolution roots"
            )));
        }
        let content_root = root.content_root.as_deref().ok_or_else(|| {
            EngineError::Internal(format!(
                "typed source root {identity:?} has no registered content authority"
            ))
        })?;
        let expected_ai_root = content_root.join(crate::AI_DIR);
        if root.ai_root != expected_ai_root {
            return Err(EngineError::Internal(format!(
                "typed source root {identity:?} has incoherent content and .ai roots"
            )));
        }
        if let Some(path) = source_path
            && !path.starts_with(&root.ai_root)
        {
            return Err(EngineError::Internal(format!(
                "resolved source {} contradicts typed root {identity:?}",
                path.display()
            )));
        }
        Ok(root)
    }

    pub fn authoritative_bundle(&self, name: &str) -> Result<&ResolutionRoot, EngineError> {
        self.authoritative_root(
            &ItemSourceRoot::Bundle {
                name: name.to_owned(),
            },
            ItemSpace::Bundle,
            None,
        )
    }

    /// Return the exact registered project content root, if this resolution
    /// generation has project space. A loose search path is not promoted into
    /// project authority.
    pub fn authoritative_project_root(&self) -> Result<Option<&Path>, EngineError> {
        let mut projects = self
            .ordered
            .iter()
            .filter(|root| root.space == ItemSpace::Project);
        let Some(_) = projects.next() else {
            return Ok(None);
        };
        if projects.next().is_some() {
            return Err(EngineError::Internal(
                "multiple project roots exist in one resolution generation".to_owned(),
            ));
        }
        let project =
            self.authoritative_root(&ItemSourceRoot::Project, ItemSpace::Project, None)?;
        Ok(project.content_root.as_deref())
    }

    /// Return installed bundle content roots in resolution order. Every entry
    /// must carry an exact bundle identity; labels and `.ai` parents are never
    /// used to reconstruct authority.
    pub fn authoritative_bundle_roots(&self) -> Result<Vec<&Path>, EngineError> {
        let mut result = Vec::new();
        for root in self
            .ordered
            .iter()
            .filter(|root| root.space == ItemSpace::Bundle)
        {
            let ItemSourceRoot::Bundle { name } = &root.identity else {
                return Err(EngineError::Internal(
                    "bundle space contains a non-authoritative search root".to_owned(),
                ));
            };
            let selected = self.authoritative_bundle(name)?;
            result.push(selected.content_root.as_deref().ok_or_else(|| {
                EngineError::Internal(format!(
                    "typed bundle root {name} has no registered content authority"
                ))
            })?);
        }
        Ok(result)
    }
}

/// Full result of item resolution, including clash diagnostics.
#[derive(Debug, Clone)]
pub struct ResolutionResult {
    pub winner_path: PathBuf,
    pub winner_space: ItemSpace,
    pub winner_root_identity: ItemSourceRoot,
    pub winner_label: String,
    pub matched_ext: String,
    /// `.ai/` root directory under which the winner was found.
    /// Needed by the path-anchoring validator so it can compute the
    /// expected `<ai_root>/<kind.directory>` base for `match: path`
    /// rules without re-deriving it by walking parent components.
    pub winner_ai_root: PathBuf,
    pub shadowed: Vec<ShadowedCandidate>,
    /// Paths probed and found absent at a precedence at least as high as the
    /// winner (negative dependencies of this resolution). Empty when the
    /// winner is the first, highest-precedence candidate probed.
    pub probed_absent: Vec<ProbedAbsence>,
}

/// Resolve a canonical ref to a concrete file path, space, and clash info.
///
/// Searches roots in order (project-first). Returns the first match plus
/// all lower-priority matches (shadowed candidates).
#[tracing::instrument(
    level = "debug",
    name = "engine:resolve_ref",
    skip(roots, kind_schema),
    fields(ref = %ref_)
)]
pub fn resolve_item_full(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    ref_: &CanonicalRef,
) -> Result<ResolutionResult, EngineError> {
    resolve_item_full_inner(roots, kind_schema, ref_, None)
}

/// Resolve one canonical ref while treating the supplied admitted project
/// content as the sole existence authority for project-space candidates.
/// Bundle candidates still come from the retained registered generation.
pub fn resolve_item_full_under_project_authority(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    ref_: &CanonicalRef,
    project_root: &std::path::Path,
    project_content: &dyn crate::project_content::AuthoritativeProjectContent,
) -> Result<ResolutionResult, EngineError> {
    resolve_item_full_inner(
        roots,
        kind_schema,
        ref_,
        Some((project_root, project_content)),
    )
}

fn resolve_item_full_inner(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    ref_: &CanonicalRef,
    project_authority: Option<(
        &std::path::Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<ResolutionResult, EngineError> {
    if kind_schema.excludes_relative_path(std::path::Path::new(&ref_.bare_id)) {
        let mut searched_spaces = Vec::new();
        for root in &roots.ordered {
            let space = root.space.as_str().to_owned();
            if !searched_spaces.contains(&space) {
                searched_spaces.push(space);
            }
        }
        return Err(EngineError::ItemNotFound {
            canonical_ref: ref_.to_string(),
            searched_spaces,
        });
    }
    let mut winner: Option<(PathBuf, ItemSpace, ItemSourceRoot, String, String, PathBuf)> = None;
    let mut shadowed = Vec::new();
    let mut probed_absent = Vec::new();
    let mut searched_spaces = Vec::new();

    for root in &roots.ordered {
        let space_label = root.space.as_str().to_owned();
        if !searched_spaces.contains(&space_label) {
            searched_spaces.push(space_label);
        }

        let kind_dir = root.ai_root.join(&kind_schema.directory);
        for ext_spec in &kind_schema.extensions {
            let path = kind_dir.join(format!("{}{}", ref_.bare_id, ext_spec.ext));
            tracing::trace!(candidate = %path.display(), label = %root.label, "checking candidate path");
            let is_file = match (root.space, project_authority) {
                (ItemSpace::Project, Some((project_root, content))) => {
                    if root.ai_root != project_root.join(crate::AI_DIR) {
                        return Err(EngineError::Internal(format!(
                            "project resolution root {} differs from admitted root {}",
                            root.ai_root.display(),
                            project_root.display()
                        )));
                    }
                    let relative = path.strip_prefix(project_root).map_err(|_| {
                        EngineError::Internal(format!(
                            "project candidate {} is outside admitted root {}",
                            path.display(),
                            project_root.display()
                        ))
                    })?;
                    !content.validates_absence(relative)?
                }
                _ => match lillux::inspect_optional_entry_no_follow(&path) {
                    Ok(Some(lillux::secure_fs::PinnedEntryType::Regular)) => true,
                    Ok(None) => false,
                    Ok(Some(_)) => false,
                    Err(error) => {
                        return Err(EngineError::ItemResolutionUnavailable {
                            canonical_ref: ref_.to_string(),
                            path,
                            source: std::io::Error::other(error.to_string()),
                        });
                    }
                },
            };
            if is_file {
                if winner.is_none() {
                    winner = Some((
                        path,
                        root.space,
                        root.identity.clone(),
                        root.label.clone(),
                        ext_spec.ext.clone(),
                        root.ai_root.clone(),
                    ));
                } else {
                    shadowed.push(ShadowedCandidate {
                        label: root.label.clone(),
                        space: root.space,
                        path,
                    });
                }
                break; // Only match one extension per root (first ext wins)
            } else if winner.is_none() {
                // Absent at a precedence >= the (not-yet-found) winner: a
                // negative dependency. If this exact path (or an earlier
                // sibling) appears, it becomes the new winner. Absences after
                // the winner is found cannot change the winner, so skip them.
                probed_absent.push(ProbedAbsence {
                    space: root.space,
                    path,
                });
            }
        }
    }

    match winner {
        Some((path, space, root_identity, label, ext, ai_root)) => {
            if !shadowed.is_empty() {
                tracing::debug!(
                    item_ref = %ref_,
                    resolved_from = %label,
                    shadowed_count = shadowed.len(),
                    "item exists in multiple spaces"
                );
            }
            Ok(ResolutionResult {
                winner_path: path,
                winner_space: space,
                winner_root_identity: root_identity,
                winner_label: label,
                matched_ext: ext,
                winner_ai_root: ai_root,
                shadowed,
                probed_absent,
            })
        }
        None => Err(EngineError::ItemNotFound {
            canonical_ref: ref_.to_string(),
            searched_spaces,
        }),
    }
}

/// Read the exact source selected by a resolution result. Project winners are
/// read from admitted CAS content; bundle winners are read through Lillux's
/// no-follow path boundary.
pub fn read_resolved_source_under_project_authority(
    result: &ResolutionResult,
    project_root: &std::path::Path,
    project_content: &dyn crate::project_content::AuthoritativeProjectContent,
) -> Result<String, EngineError> {
    let bytes = if result.winner_space == ItemSpace::Project {
        let relative = result.winner_path.strip_prefix(project_root).map_err(|_| {
            EngineError::Internal(format!(
                "project winner {} is outside admitted root {}",
                result.winner_path.display(),
                project_root.display()
            ))
        })?;
        project_content
            .read_file(relative, MAX_ITEM_SOURCE_BYTES)?
            .ok_or_else(|| {
                EngineError::Internal(format!(
                    "admitted project winner disappeared from content authority: {}",
                    relative.display()
                ))
            })?
    } else {
        return read_item_source_no_follow(&result.winner_path);
    };
    String::from_utf8(bytes).map_err(|error| {
        EngineError::Internal(format!(
            "resolved source {} is not UTF-8: {error}",
            result.winner_path.display()
        ))
    })
}

/// Winner-only resolve: returns just the winner without clash info.
pub fn resolve_item(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    ref_: &CanonicalRef,
) -> Result<(PathBuf, ItemSpace, String), EngineError> {
    let result = resolve_item_full(roots, kind_schema, ref_)?;
    Ok((result.winner_path, result.winner_space, result.matched_ext))
}

/// Enumerate every canonical ref of `kind_schema` reachable from `roots`,
/// honouring resolution priority and the kind schema's own
/// `directory`, `excluded_directories`, and `extensions` declarations.
///
/// Walks `<root.ai_root>/<kind_schema.directory>/` recursively for each
/// root in `roots.ordered`. Files whose extension matches one of
/// `kind_schema.extensions[].ext` produce a `CanonicalRef { kind,
/// bare_id }` where `bare_id` is the path relative to the kind
/// directory with the matched extension stripped (slashes preserved
/// for nested layouts: `<dir>/ryeos/core/read.py` → `ryeos/core/read`).
///
/// Precedence semantics mirror `resolve_item_full`: the first root in
/// `roots.ordered` to surface a given `bare_id` wins, later occurrences
/// in lower-priority roots are silently dropped. Symlink loops, hidden
/// directories (starting with `.`), and IO errors on individual entries
/// are skipped — the loud failure modes are reserved for **resolving**
/// (where the caller asked for a specific ref); enumeration is a
/// best-effort discovery primitive.
///
/// Returns refs in deterministic order — sorted by `bare_id` after
/// precedence-collapse — so callers can rely on stable output for
/// caching / fingerprinting.
///
/// NB: this is intentionally **schema-driven**. There is no hardcoded
/// extension list, no hardcoded subdirectory; adding a new format =
/// adding an entry to the kind schema's `formats` block. Runtime /
/// daemon code consuming this primitive must not duplicate either.
pub fn enumerate_kind_refs(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    kind: &str,
) -> Vec<CanonicalRef> {
    enumerate_kind_refs_inner(roots, kind_schema, kind, None).unwrap_or_default()
}

pub fn enumerate_kind_refs_under_project_authority(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    kind: &str,
    project_root: &std::path::Path,
    project_content: &dyn crate::project_content::AuthoritativeProjectContent,
) -> Result<Vec<CanonicalRef>, EngineError> {
    enumerate_kind_refs_inner(
        roots,
        kind_schema,
        kind,
        Some((project_root, project_content)),
    )
}

const MAX_CORPUS_ENUMERATION_FILES: usize = 100_000;
const MAX_CORPUS_TRAVERSAL_ENTRIES: usize = MAX_CORPUS_ENUMERATION_FILES * 2;
const MAX_CORPUS_TRAVERSAL_DEPTH: usize = 64;

fn enumerate_kind_refs_inner(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    kind: &str,
    project_authority: Option<(
        &std::path::Path,
        &dyn crate::project_content::AuthoritativeProjectContent,
    )>,
) -> Result<Vec<CanonicalRef>, EngineError> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for root in &roots.ordered {
        let kind_dir = root.ai_root.join(&kind_schema.directory);
        let mut relative_files = Vec::new();
        match (root.space, project_authority) {
            (ItemSpace::Project, Some((project_root, content))) => {
                if root.ai_root != project_root.join(crate::AI_DIR) {
                    return Err(EngineError::Internal(format!(
                        "project corpus root {} differs from admitted root {}",
                        root.ai_root.display(),
                        project_root.display()
                    )));
                }
                let prefix = Path::new(crate::AI_DIR).join(&kind_schema.directory);
                relative_files.extend(
                    content
                        .list_files(&prefix, true, MAX_CORPUS_ENUMERATION_FILES)?
                        .into_iter()
                        .map(|entry| entry.relative_path),
                );
            }
            _ => {
                let mut count = 0_usize;
                lillux::visit_regular_files_no_follow_bounded(
                    &kind_dir,
                    lillux::DirectoryTraversalBudget::new(
                        MAX_CORPUS_TRAVERSAL_ENTRIES,
                        MAX_CORPUS_TRAVERSAL_DEPTH,
                    ),
                    |relative, is_directory| {
                        let hidden = relative.components().any(|component| {
                            component
                                .as_os_str()
                                .to_str()
                                .is_some_and(|name| name.starts_with('.'))
                        });
                        let excluded = is_directory
                            && relative.file_name().and_then(|name| name.to_str()).is_some_and(
                                |name| kind_schema.excluded_directories.iter().any(|value| value == name),
                            );
                        Ok(hidden || excluded)
                    },
                    |relative, _file| {
                        count = count.saturating_add(1);
                        if count > MAX_CORPUS_ENUMERATION_FILES {
                            anyhow::bail!(
                                "corpus enumeration exceeds {MAX_CORPUS_ENUMERATION_FILES} regular files"
                            );
                        }
                        relative_files.push(relative.to_path_buf());
                        Ok(())
                    },
                )
                .map_err(|error| EngineError::Internal(format!(
                    "securely enumerate corpus root {}: {error:#}",
                    kind_dir.display()
                )))?;
            }
        }
        for relative in relative_files {
            if kind_schema.excludes_relative_path(&relative)
                || relative.components().any(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .is_none_or(|name| name.starts_with('.'))
                })
            {
                continue;
            }
            let Some(extension) = kind_schema.extensions.iter().find(|extension| {
                relative
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(&extension.ext))
            }) else {
                continue;
            };
            let Some(relative_text) = relative.to_str() else {
                continue;
            };
            let Some(bare_id) = relative_text.strip_suffix(&extension.ext) else {
                continue;
            };
            let bare_id = bare_id.replace('\\', "/");
            if !bare_id.is_empty() && seen.insert(bare_id.clone()) {
                refs.push(CanonicalRef {
                    kind: kind.to_owned(),
                    bare_id,
                    suffix: None,
                });
            }
        }
    }
    refs.sort_by(|left, right| left.bare_id.cmp(&right.bare_id));
    Ok(refs)
}

/// Parse a `ryeos:signed:<timestamp>:<content_hash>:<sig_b64>:<signer_fp>` header
/// from file content, using the envelope to locate the signature line.
pub fn parse_signature_header(
    content: &str,
    envelope: &SignatureEnvelope,
) -> Option<SignatureHeader> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }

    // Determine which lines to inspect. Line 2 is only a signature
    // candidate when line 1 is a real shebang; otherwise accepting a
    // line-2 signature would exclude arbitrary line 1 content from the
    // signed hash.
    let candidates: Vec<usize> =
        if envelope.after_shebang && lines.first().is_some_and(|line| line.starts_with("#!")) {
            let mut c = Vec::new();
            if lines.len() > 1 {
                c.push(1);
            }
            c.push(0);
            c
        } else {
            vec![0]
        };

    for idx in candidates {
        let line = lines[idx];
        if let Some(header) = try_parse_signature_line(line, envelope) {
            return Some(header);
        }
    }

    None
}

fn try_parse_signature_line(line: &str, envelope: &SignatureEnvelope) -> Option<SignatureHeader> {
    let header = lillux::signature::parse_signature_line(
        line,
        &envelope.prefix,
        envelope.suffix.as_deref(),
    )?;
    Some(SignatureHeader {
        timestamp: header.timestamp,
        content_hash: header.content_hash,
        signature_b64: header.signature_b64,
        signer_fingerprint: header.signer_fingerprint,
    })
}

/// Compute a SHA-256 hex digest of the given content.
pub fn content_hash(content: &str) -> String {
    lillux::signature::content_hash(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind_registry::ExtensionSpec;
    use ryeos_tracing::test as trace_test;
    use std::fs;
    use std::path::Path;

    #[test]
    fn authoritative_roots_select_typed_identity_not_nested_path_prefix() {
        let roots = ResolutionRoots::from_registered(
            Some(PathBuf::from("/projects/demo")),
            &[
                RegisteredBundleRoot {
                    name: "outer".to_owned(),
                    canonical_root: PathBuf::from("/bundles/root"),
                },
                RegisteredBundleRoot {
                    name: "inner".to_owned(),
                    canonical_root: PathBuf::from("/bundles/root/nested"),
                },
            ],
        );
        let selected = roots
            .authoritative_root(
                &ItemSourceRoot::Bundle {
                    name: "inner".to_owned(),
                },
                ItemSpace::Bundle,
                Some(Path::new("/bundles/root/nested/.ai/tools/example.yaml")),
            )
            .unwrap();
        assert_eq!(
            selected.content_root.as_deref(),
            Some(Path::new("/bundles/root/nested"))
        );
        assert!(
            roots
                .authoritative_root(
                    &ItemSourceRoot::Bundle {
                        name: "outer".to_owned(),
                    },
                    ItemSpace::Bundle,
                    Some(Path::new("/bundles/root/nested/.ai/tools/example.yaml")),
                )
                .is_err()
        );
    }

    #[test]
    fn loose_search_and_duplicate_typed_roots_cannot_supply_authority() {
        let loose = ResolutionRoots::from_flat(None, vec![PathBuf::from("/bundle/.ai")]);
        assert!(loose.authoritative_bundle_roots().is_err());

        let duplicate = ResolutionRoots {
            ordered: vec![
                ResolutionRoot {
                    space: ItemSpace::Bundle,
                    identity: ItemSourceRoot::Bundle {
                        name: "same".to_owned(),
                    },
                    label: "diagnostic-a".to_owned(),
                    ai_root: PathBuf::from("/a/.ai"),
                    content_root: Some(PathBuf::from("/a")),
                },
                ResolutionRoot {
                    space: ItemSpace::Bundle,
                    identity: ItemSourceRoot::Bundle {
                        name: "same".to_owned(),
                    },
                    label: "diagnostic-b".to_owned(),
                    ai_root: PathBuf::from("/b/.ai"),
                    content_root: Some(PathBuf::from("/b")),
                },
            ],
        };
        assert!(duplicate.authoritative_bundle("same").is_err());
    }

    fn make_kind_schema(directory: &str, extensions: Vec<(&str, &str)>) -> KindSchema {
        KindSchema {
            directory: directory.to_owned(),
            excluded_directories: Vec::new(),
            extraction_rules: std::collections::HashMap::new(),
            resolution: Vec::new(),
            effective_trust: crate::kind_registry::EffectiveTrustPolicy::default(),
            execution: Some(crate::kind_registry::ExecutionSchema {
                effect_class_ceiling: None,
                aliases: std::collections::HashMap::new(),
                alias_max_depth: 8,
                terminator: None,
                delegate: None,
                thread_profile: None,
                history_policy: None,
                result_policy: None,
                method_dispatch: None,
                methods: std::collections::BTreeMap::new(),
                augmentation_methods: std::collections::BTreeMap::new(),
                launch_augmentations: Vec::new(),
                hooks: None,
                external_content: None,
                source_closure: None,
                effective_validator: None,
                persistent_session: None,
            }),
            extensions: extensions
                .into_iter()
                .map(|(ext, parser)| ExtensionSpec {
                    ext: ext.to_owned(),
                    parser: parser.to_owned(),
                    signature: SignatureEnvelope {
                        prefix: "#".to_owned(),
                        suffix: None,
                        after_shebang: false,
                    },
                })
                .collect(),
            composed_value_contract: crate::contracts::ValueShape::any_mapping(),
            composer: "handler:ryeos/core/identity".to_owned(),
            composer_config: serde_json::Value::Null,
            runtime: None,
            inventory_kinds: Vec::new(),
            inventory_schema_keys: Vec::new(),
            inventory_policy: Default::default(),
        }
    }

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir = std::env::temp_dir().join(format!(
            "rye_resolution_test_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_item(root: &Path, kind_dir: &str, bare_id: &str, ext: &str, content: &str) {
        let dir = root.join(kind_dir);
        // Handle nested bare_ids like "ryeos/bash/bash"
        let file_path = dir.join(format!("{bare_id}{ext}"));
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&file_path, content).unwrap();
    }

    #[test]
    fn enumeration_skips_schema_excluded_directories() {
        let root = tempdir();
        let mut schema = make_kind_schema(
            "tools",
            vec![(".py", "python/tool-header"), (".yaml", "yaml")],
        );
        schema.excluded_directories = vec!["lib".to_owned()];
        write_item(
            &root,
            "tools",
            "ryeos/demo/run",
            ".yaml",
            "executor_id: run",
        );
        write_item(&root, "tools", "ryeos/demo/lib/helper", ".py", "# support");

        let roots = ResolutionRoots::from_flat(None, vec![root.clone()]);
        let refs = enumerate_kind_refs(&roots, &schema, "tool");
        assert_eq!(
            refs.iter()
                .map(|item| item.bare_id.as_str())
                .collect::<Vec<_>>(),
            ["ryeos/demo/run"]
        );
        let excluded = CanonicalRef::parse("tool:ryeos/demo/lib/helper").unwrap();
        assert!(matches!(
            resolve_item_full(&roots, &schema, &excluded),
            Err(EngineError::ItemNotFound { .. })
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_finds_project_space_when_only_source() {
        let project_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/tool-header")]);

        write_item(&project_root, "tools", "my_tool", ".py", "# project");
        write_item(&system_root, "tools", "my_tool", ".py", "# system");

        // When only project has it (bundle root empty), project wins
        let roots = ResolutionRoots::from_flat(Some(project_root.clone()), vec![system_root]);
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (_path, space, ext) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(space, ItemSpace::Project);
        assert_eq!(ext, ".py");
    }

    #[test]
    fn resolve_project_wins_over_system() {
        let project_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/tool-header")]);

        write_item(&system_root, "tools", "my_tool", ".py", "# system");
        write_item(&project_root, "tools", "my_tool", ".py", "# project");

        let roots =
            ResolutionRoots::from_flat(Some(project_root.clone()), vec![system_root.clone()]);
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (path, space, _) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(space, ItemSpace::Project);
        assert!(path.starts_with(&project_root));
    }

    #[test]
    fn probed_absent_records_higher_precedence_misses() {
        // The case that justifies negative-dependency recording: a bundle
        // winner while the project slot is empty. If the project slot later
        // fills, the winner flips — so the probed-and-absent project path is a
        // dependency of the current outcome.
        let project_root = tempdir();
        let bundle_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/tool-header")]);

        write_item(&bundle_root, "tools", "my_tool", ".py", "# bundle");
        let roots =
            ResolutionRoots::from_flat(Some(project_root.clone()), vec![bundle_root.clone()]);
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let result = resolve_item_full(&roots, &schema, &ref_).unwrap();
        assert_eq!(result.winner_space, ItemSpace::Bundle);
        assert_eq!(result.probed_absent.len(), 1, "{:?}", result.probed_absent);
        assert_eq!(result.probed_absent[0].space, ItemSpace::Project);
        assert_eq!(
            result.probed_absent[0].path,
            project_root.join("tools").join("my_tool.py")
        );

        // The shadowing item appears in the project → the winner flips and no
        // higher-precedence absence remains (project is the first root probed).
        write_item(&project_root, "tools", "my_tool", ".py", "# project");
        let result = resolve_item_full(&roots, &schema, &ref_).unwrap();
        assert_eq!(result.winner_space, ItemSpace::Project);
        assert!(
            result.probed_absent.is_empty(),
            "{:?}",
            result.probed_absent
        );
    }

    #[test]
    fn probed_absent_records_earlier_extension_miss_at_winner_root() {
        // Within the winner's own root, an earlier extension probed-and-absent
        // is a negative dependency: "first ext wins", so if the earlier
        // extension appears it becomes the new winner.
        let project_root = tempdir();
        let schema = make_kind_schema(
            "tools",
            vec![(".py", "python/tool-header"), (".yaml", "yaml")],
        );
        write_item(&project_root, "tools", "my_tool", ".yaml", "# yaml");
        let roots = ResolutionRoots::from_flat(Some(project_root.clone()), vec![]);
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let result = resolve_item_full(&roots, &schema, &ref_).unwrap();
        assert_eq!(result.matched_ext, ".yaml");
        assert_eq!(result.probed_absent.len(), 1, "{:?}", result.probed_absent);
        assert_eq!(
            result.probed_absent[0].path,
            project_root.join("tools").join("my_tool.py")
        );
    }

    #[test]
    fn resolve_finds_app_root() {
        let system_root = tempdir();
        let schema = make_kind_schema("directives", vec![(".md", "markdown/xml")]);

        write_item(&system_root, "directives", "init", ".md", "# system");

        let roots = ResolutionRoots::from_flat(None, vec![system_root.clone()]);
        let ref_ = CanonicalRef::parse("directive:init").unwrap();

        let (path, space, _) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(space, ItemSpace::Bundle);
        assert!(path.starts_with(&system_root));
    }

    #[test]
    fn resolve_extension_priority() {
        let project_root = tempdir();
        // .py is listed first, so it should win even though .yaml also exists
        let schema = make_kind_schema(
            "tools",
            vec![(".py", "python/tool-header"), (".yaml", "yaml/yaml")],
        );

        write_item(&project_root, "tools", "my_tool", ".py", "# python");
        write_item(&project_root, "tools", "my_tool", ".yaml", "name: yaml");

        let roots = ResolutionRoots::from_flat(Some(project_root), vec![]);
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (path, _, ext) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(ext, ".py");
        assert!(path.to_string_lossy().ends_with(".py"));
    }

    #[test]
    fn resolve_not_found() {
        let project_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/tool-header")]);

        let roots = ResolutionRoots::from_flat(Some(project_root), vec![]);
        let ref_ = CanonicalRef::parse("tool:nonexistent").unwrap();

        let err = resolve_item(&roots, &schema, &ref_).unwrap_err();
        match err {
            EngineError::ItemNotFound {
                canonical_ref,
                searched_spaces,
            } => {
                assert_eq!(canonical_ref, "tool:nonexistent");
                assert!(searched_spaces.contains(&"project".to_owned()));
            }
            other => panic!("expected ItemNotFound, got: {other:?}"),
        }
    }

    #[test]
    fn resolve_clash_diagnostics() {
        let project_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/tool-header")]);

        write_item(&system_root, "tools", "my_tool", ".py", "# system");
        write_item(&project_root, "tools", "my_tool", ".py", "# project");

        let roots = ResolutionRoots::from_flat(Some(project_root), vec![system_root]);
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let result = resolve_item_full(&roots, &schema, &ref_).unwrap();
        assert_eq!(result.winner_space, ItemSpace::Project);
        assert_eq!(result.winner_label, "project");
        assert_eq!(result.shadowed.len(), 1);
        assert_eq!(result.shadowed[0].space, ItemSpace::Bundle);
    }

    #[test]
    fn parse_signature_header_hash_prefix() {
        let content =
            "# ryeos:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer\nprint('hello')";
        let envelope = SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: false,
        };

        let header = parse_signature_header(content, &envelope).unwrap();
        assert_eq!(header.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(header.content_hash, "abc123");
        assert_eq!(header.signature_b64, "sigB64data");
        assert_eq!(header.signer_fingerprint, "fp_signer");
    }

    #[test]
    fn parse_signature_header_slash_prefix() {
        let content =
            "// ryeos:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer\nconsole.log('hi')";
        let envelope = SignatureEnvelope {
            prefix: "//".to_owned(),
            suffix: None,
            after_shebang: false,
        };

        let header = parse_signature_header(content, &envelope).unwrap();
        assert_eq!(header.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(header.content_hash, "abc123");
        assert_eq!(header.signature_b64, "sigB64data");
        assert_eq!(header.signer_fingerprint, "fp_signer");
    }

    #[test]
    fn parse_signature_header_html_prefix() {
        let content =
            "<!-- ryeos:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer -->\n# Hello";
        let envelope = SignatureEnvelope {
            prefix: "<!--".to_owned(),
            suffix: Some("-->".to_owned()),
            after_shebang: false,
        };

        let header = parse_signature_header(content, &envelope).unwrap();
        assert_eq!(header.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(header.content_hash, "abc123");
        assert_eq!(header.signature_b64, "sigB64data");
        assert_eq!(header.signer_fingerprint, "fp_signer");
    }

    #[test]
    fn parse_signature_header_after_shebang() {
        let content = "#!/usr/bin/env python3\n# ryeos:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer\nprint('hello')";
        let envelope = SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: true,
        };

        let header = parse_signature_header(content, &envelope).unwrap();
        assert_eq!(header.timestamp, "2026-04-10T00:00:00Z");
        assert_eq!(header.content_hash, "abc123");
        assert_eq!(header.signature_b64, "sigB64data");
        assert_eq!(header.signer_fingerprint, "fp_signer");
    }

    #[test]
    fn parse_signature_header_rejects_line_two_without_shebang() {
        let content = "not a shebang\n# ryeos:signed:2026-04-10T00:00:00Z:abc123:sigB64data:fp_signer\nprint('hello')";
        let envelope = SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: true,
        };

        assert!(parse_signature_header(content, &envelope).is_none());
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn resolve_item_full_emits_span() {
        let project_root = tempdir();
        let system_root = tempdir();
        let schema = make_kind_schema("tools", vec![(".py", "python/tool-header")]);
        write_item(&project_root, "tools", "trace_tool", ".py", "# content");

        let roots = ResolutionRoots::from_flat(Some(project_root.clone()), vec![system_root]);
        let ref_ = CanonicalRef::parse("tool:trace_tool").unwrap();

        let (_, spans) = trace_test::capture_traces(|| {
            let _ = resolve_item_full(&roots, &schema, &ref_);
        });

        let span = trace_test::find_span(&spans, "engine:resolve_ref");
        assert!(
            span.is_some(),
            "expected engine:resolve_ref span, got: {:?}",
            spans
                .iter()
                .map(|s: &ryeos_tracing::test::RecordedSpan| &s.name)
                .collect::<Vec<_>>()
        );

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("ref"), Some("tool:trace_tool"));
    }
}
