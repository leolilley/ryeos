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
use crate::kind_registry::{InProcessRegistryKind, KindRegistry, KindSchema};

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

/// How a derived resolution generation uses one named registered bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisteredBundleSelection {
    /// Search only the named bundle; every other registered bundle is absent
    /// from the derived generation.
    ///
    /// No selection that reorders the full registered list exists: a derived
    /// generation in which one bundle's copies outrank the others would let
    /// that bundle govern every later resolution of a run.
    Only,
}

/// Why a named registered bundle could not be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisteredBundleSelectionRefusal {
    /// No registered bundle carries the name.
    NotRegistered,
    /// More than one registered bundle carries the name.
    RegisteredMoreThanOnce,
}

impl std::fmt::Display for RegisteredBundleSelectionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotRegistered => "no registered bundle has this name",
            Self::RegisteredMoreThanOnce => "more than one registered bundle has this name",
        })
    }
}

/// Derive the registered bundle list for one named-bundle selection.
///
/// The name must identify exactly one registered bundle. Each entry keeps its
/// typed identity and canonical root, so items resolved through the derived
/// list carry the same source identity and trust as through the full list.
pub fn select_registered_bundle(
    registered: &[RegisteredBundleRoot],
    name: &str,
    selection: RegisteredBundleSelection,
) -> Result<Vec<RegisteredBundleRoot>, EngineError> {
    let mut named = registered.iter().filter(|bundle| bundle.name == name);
    let refused = |refusal| EngineError::RegisteredBundleSelectionRefused {
        name: name.to_owned(),
        refusal,
    };
    let selected = named
        .next()
        .ok_or_else(|| refused(RegisteredBundleSelectionRefusal::NotRegistered))?;
    if named.next().is_some() {
        return Err(refused(
            RegisteredBundleSelectionRefusal::RegisteredMoreThanOnce,
        ));
    }
    match selection {
        RegisteredBundleSelection::Only => Ok(vec![selected.clone()]),
    }
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
        // A kind that names the spaces allowed to supply it never takes a
        // candidate from another space. A file there is reported, so a
        // planted item is visible, but it can neither win nor shadow.
        let space_admitted = kind_schema
            .launch_trust
            .as_ref()
            .is_none_or(|launch_trust| launch_trust.admits_space(root.space));
        if space_admitted {
            let space_label = root.space.as_str().to_owned();
            if !searched_spaces.contains(&space_label) {
                searched_spaces.push(space_label);
            }
        }

        let kind_dir = root.ai_root.join(&kind_schema.directory);
        let candidate_exists = |path: &Path| -> Result<bool, EngineError> {
            match (root.space, project_authority) {
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
                    Ok(!content.validates_absence(relative)?)
                }
                _ => match lillux::inspect_optional_entry_no_follow(path) {
                    Ok(Some(lillux::secure_fs::PinnedEntryType::Regular)) => Ok(true),
                    Ok(None) | Ok(Some(_)) => Ok(false),
                    Err(error) => Err(EngineError::ItemResolutionUnavailable {
                        canonical_ref: ref_.to_string(),
                        path: path.to_path_buf(),
                        source: std::io::Error::other(format!("{error:#}")),
                    }),
                },
            }
        };
        for (ext_index, ext_spec) in kind_schema.extensions.iter().enumerate() {
            let path = kind_dir.join(format!("{}{}", ref_.bare_id, ext_spec.ext));
            tracing::trace!(candidate = %path.display(), label = %root.label, "checking candidate path");
            let is_file = candidate_exists(&path)?;
            if is_file && !space_admitted {
                tracing::warn!(
                    item_ref = %ref_,
                    ignored = %path.display(),
                    space = root.space.as_str(),
                    "ignoring a candidate from a space this kind does not admit"
                );
                shadowed.push(ShadowedCandidate {
                    label: root.label.clone(),
                    space: root.space,
                    path,
                });
                break;
            }
            if !space_admitted {
                // Nothing in this space can change the winner, so its
                // absences are not dependencies of the resolution.
                continue;
            }
            if is_file {
                if winner.is_none() {
                    // One name, one item: a second format of the same name in
                    // the winning folder is refused rather than silently
                    // ranked, and each later format's absence is a dependency
                    // so a collision appearing later is never served from a
                    // stale resolution.
                    let relative = |extension: &str| {
                        PathBuf::from(format!(
                            "{}/{}/{}{}",
                            crate::AI_DIR,
                            kind_schema.directory,
                            ref_.bare_id,
                            extension
                        ))
                    };
                    for later in &kind_schema.extensions[ext_index + 1..] {
                        let sibling = kind_dir.join(format!("{}{}", ref_.bare_id, later.ext));
                        if candidate_exists(&sibling)? {
                            return Err(EngineError::AmbiguousResolution {
                                canonical_ref: ref_.to_string(),
                                space: format!("{} ({})", root.space.as_str(), root.label),
                                candidates: vec![relative(&ext_spec.ext), relative(&later.ext)],
                            });
                        }
                        probed_absent.push(ProbedAbsence {
                            space: root.space,
                            path: sibling,
                        });
                    }
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
                break; // A root supplies at most one candidate.
            } else if winner.is_none() {
                // Absent at a precedence >= the (not-yet-found) winner: a
                // negative dependency. If this exact path (or an earlier
                // sibling) appears, it becomes the new winner.
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
    try_enumerate_kind_refs(roots, kind_schema, kind).unwrap_or_default()
}

/// Discover candidates while preserving traversal/budget failures for callers
/// that report inventory coverage. This is a bounded live filesystem read, not
/// an authenticated or atomic snapshot of the installed corpus.
pub fn try_enumerate_kind_refs(
    roots: &ResolutionRoots,
    kind_schema: &KindSchema,
    kind: &str,
) -> Result<Vec<CanonicalRef>, EngineError> {
    enumerate_kind_refs_inner(roots, kind_schema, kind, None)
}

/// Enumerate the installed item corpus whose signed kind schemas select one
/// daemon-owned in-process registry.
///
/// This is the registry-side discovery boundary: callers name the registry
/// they implement, while kind names, directories, formats, and item IDs all
/// remain authored by admitted schemas and installed content.
pub fn enumerate_in_process_registry_refs(
    roots: &ResolutionRoots,
    kinds: &KindRegistry,
    registry: InProcessRegistryKind,
) -> Result<Vec<CanonicalRef>, EngineError> {
    let mut refs = Vec::new();
    for (kind, schema) in kinds.kinds_for_in_process_registry(registry) {
        refs.extend(enumerate_kind_refs_inner(roots, schema, kind, None)?);
    }
    refs.sort_by_key(ToString::to_string);
    Ok(refs)
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
        // Enumeration lists only what resolution could select.
        if kind_schema
            .launch_trust
            .as_ref()
            .is_some_and(|launch_trust| !launch_trust.admits_space(root.space))
        {
            continue;
        }
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
            content: None,
            execution: Some(crate::kind_registry::ExecutionSchema {
                effect_class_ceiling: None,
                workspace_access: None,
                network_authority_ceiling: None,
                pushed_head_declaration: None,
                filesystem_authority_ceiling: None,
                target: None,
                endpoint: None,
                resource_authority_ceiling: None,
                subprocess_limits: None,
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
            launch_trust: None,
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
    fn fallible_enumeration_preserves_traversal_budget_failure() {
        let root = tempdir();
        let schema = make_kind_schema("tools", vec![(".yaml", "yaml")]);
        let mut nested = root.join("tools");
        for _ in 0..(MAX_CORPUS_TRAVERSAL_DEPTH + 2) {
            nested.push("nested");
        }
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("item.yaml"), "name: item").unwrap();
        let roots = ResolutionRoots::from_flat(None, vec![root.clone()]);
        assert!(try_enumerate_kind_refs(&roots, &schema, "tool").is_err());
        // The best-effort wrapper deliberately keeps its lossy contract.
        assert!(enumerate_kind_refs(&roots, &schema, "tool").is_empty());
        fs::remove_dir_all(root).unwrap();
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
    fn two_formats_of_one_name_in_one_folder_are_refused() {
        let root = tempdir();
        let schema = make_kind_schema(
            "tools",
            vec![(".py", "python/tool-header"), (".yaml", "yaml")],
        );
        write_item(&root, "tools", "demo/x", ".py", "# python");
        write_item(&root, "tools", "demo/x", ".yaml", "executor_id: run");
        let roots = ResolutionRoots::from_flat(None, vec![root.clone()]);
        let item = CanonicalRef::parse("tool:demo/x").unwrap();
        match resolve_item_full(&roots, &schema, &item) {
            Err(EngineError::AmbiguousResolution { candidates, .. }) => {
                assert_eq!(
                    candidates,
                    [
                        PathBuf::from(".ai/tools/demo/x.py"),
                        PathBuf::from(".ai/tools/demo/x.yaml")
                    ]
                );
            }
            other => panic!("expected an ambiguous-format refusal, got {other:?}"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_single_format_records_its_later_siblings_as_absences() {
        let root = tempdir();
        let schema = make_kind_schema(
            "tools",
            vec![(".py", "python/tool-header"), (".yaml", "yaml")],
        );
        write_item(&root, "tools", "demo/x", ".py", "# python");
        let roots = ResolutionRoots::from_flat(None, vec![root.clone()]);
        let item = CanonicalRef::parse("tool:demo/x").unwrap();
        let result = resolve_item_full(&roots, &schema, &item).unwrap();
        assert!(
            result
                .probed_absent
                .iter()
                .any(|absence| absence.path.ends_with("tools/demo/x.yaml"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_same_name_across_roots_is_a_shadow_not_a_collision() {
        let project = tempdir();
        let bundle = tempdir();
        let schema = make_kind_schema(
            "tools",
            vec![(".py", "python/tool-header"), (".yaml", "yaml")],
        );
        write_item(&project, "tools", "demo/x", ".yaml", "executor_id: run");
        write_item(&bundle, "tools", "demo/x", ".py", "# python");
        let roots = ResolutionRoots::from_flat(Some(project.clone()), vec![bundle.clone()]);
        let item = CanonicalRef::parse("tool:demo/x").unwrap();
        let result = resolve_item_full(&roots, &schema, &item).unwrap();
        assert_eq!(result.winner_space, ItemSpace::Project);
        assert_eq!(result.shadowed.len(), 1);
        fs::remove_dir_all(project).unwrap();
        fs::remove_dir_all(bundle).unwrap();
    }

    fn bundle_only_schema() -> KindSchema {
        let mut schema = make_kind_schema("services", vec![(".yaml", "yaml")]);
        schema.launch_trust = Some(crate::kind_registry::launch_trust::LaunchTrustPolicy {
            allowed_spaces: vec![ItemSpace::Bundle],
            allowed_trust: vec![crate::resolution::TrustClass::TrustedBundle],
            operator_may_admit: Vec::new(),
        });
        schema
    }

    #[test]
    fn a_project_file_never_wins_for_a_bundle_only_kind_and_is_reported() {
        let project = tempdir();
        let bundle = tempdir();
        write_item(
            &project,
            "services",
            "health/status",
            ".yaml",
            "endpoint: remote.list",
        );
        write_item(
            &bundle,
            "services",
            "health/status",
            ".yaml",
            "endpoint: health.status",
        );
        let roots = ResolutionRoots::from_flat(Some(project.clone()), vec![bundle.clone()]);
        let schema = bundle_only_schema();
        let item = CanonicalRef::parse("service:health/status").unwrap();

        let result = resolve_item_full(&roots, &schema, &item).unwrap();
        assert_eq!(result.winner_space, ItemSpace::Bundle);
        assert!(result.winner_path.starts_with(&bundle));
        assert_eq!(result.shadowed.len(), 1);
        assert_eq!(result.shadowed[0].space, ItemSpace::Project);
        assert!(
            result
                .probed_absent
                .iter()
                .all(|absence| absence.space != ItemSpace::Project)
        );
        assert_eq!(enumerate_kind_refs(&roots, &schema, "service").len(), 1);

        fs::remove_dir_all(project).unwrap();
        fs::remove_dir_all(bundle).unwrap();
    }

    #[test]
    fn a_project_only_item_of_a_bundle_only_kind_is_not_found() {
        let project = tempdir();
        let bundle = tempdir();
        write_item(
            &project,
            "services",
            "planted/x",
            ".yaml",
            "endpoint: remote.configure",
        );
        let roots = ResolutionRoots::from_flat(Some(project.clone()), vec![bundle.clone()]);
        let schema = bundle_only_schema();
        let item = CanonicalRef::parse("service:planted/x").unwrap();
        assert!(matches!(
            resolve_item_full(&roots, &schema, &item),
            Err(EngineError::ItemNotFound { .. })
        ));
        assert!(enumerate_kind_refs(&roots, &schema, "service").is_empty());

        fs::remove_dir_all(project).unwrap();
        fs::remove_dir_all(bundle).unwrap();
    }

    fn registered_pair() -> (PathBuf, PathBuf, Vec<RegisteredBundleRoot>) {
        let early = tempdir().join("early");
        let chosen = tempdir().join("chosen");
        fs::create_dir_all(early.join(crate::AI_DIR)).unwrap();
        fs::create_dir_all(chosen.join(crate::AI_DIR)).unwrap();
        let registered = vec![
            RegisteredBundleRoot {
                name: "early".to_owned(),
                canonical_root: early.clone(),
            },
            RegisteredBundleRoot {
                name: "chosen".to_owned(),
                canonical_root: chosen.clone(),
            },
        ];
        (early, chosen, registered)
    }

    #[test]
    fn named_bundle_item_resolution_finds_the_named_bundle_item_over_an_earlier_bundle() {
        let (early, chosen, registered) = registered_pair();
        let schema = make_kind_schema("tools", vec![(".py", "python/tool-header")]);
        let ai = crate::AI_DIR;
        write_item(&early.join(ai), "tools", "chosen/hook", ".py", "# early");
        write_item(&chosen.join(ai), "tools", "chosen/hook", ".py", "# chosen");
        let ref_ = CanonicalRef::parse("tool:chosen/hook").unwrap();

        let all = ResolutionRoots::from_registered(None, &registered);
        let unselected = resolve_item_full(&all, &schema, &ref_).unwrap();
        assert!(unselected.winner_path.starts_with(&early));

        {
            let selection = RegisteredBundleSelection::Only;
            let selected = select_registered_bundle(&registered, "chosen", selection).unwrap();
            let roots = ResolutionRoots::from_registered(None, &selected);
            let result = resolve_item_full(&roots, &schema, &ref_).unwrap();
            assert!(result.winner_path.starts_with(&chosen), "{selection:?}");
            assert_eq!(result.winner_space, ItemSpace::Bundle);
            assert_eq!(
                result.winner_root_identity,
                ItemSourceRoot::Bundle {
                    name: "chosen".to_owned()
                }
            );
            assert_eq!(result.winner_ai_root, chosen.join(ai));
        }
    }

    #[test]
    fn named_bundle_item_resolution_does_not_fall_back_to_another_bundle() {
        let (early, _chosen, registered) = registered_pair();
        let schema = make_kind_schema("tools", vec![(".py", "python/tool-header")]);
        write_item(
            &early.join(crate::AI_DIR),
            "tools",
            "chosen/hook",
            ".py",
            "# early",
        );
        let ref_ = CanonicalRef::parse("tool:chosen/hook").unwrap();

        let only = select_registered_bundle(&registered, "chosen", RegisteredBundleSelection::Only)
            .unwrap();
        let roots = ResolutionRoots::from_registered(None, &only);
        assert!(matches!(
            resolve_item_full(&roots, &schema, &ref_),
            Err(EngineError::ItemNotFound { .. })
        ));
        assert!(roots.authoritative_bundle("early").is_err());
    }

    #[test]
    fn named_bundle_item_resolution_selects_exactly_the_named_entry() {
        let registered = ["a", "b", "c"]
            .into_iter()
            .map(|name| RegisteredBundleRoot {
                name: name.to_owned(),
                canonical_root: PathBuf::from(format!("/bundles/{name}")),
            })
            .collect::<Vec<_>>();
        let only =
            select_registered_bundle(&registered, "b", RegisteredBundleSelection::Only).unwrap();
        assert_eq!(only, vec![registered[1].clone()]);
    }

    #[test]
    fn named_bundle_item_resolution_refuses_an_unregistered_or_duplicated_name() {
        let registered = vec![
            RegisteredBundleRoot {
                name: "twice".to_owned(),
                canonical_root: PathBuf::from("/bundles/one"),
            },
            RegisteredBundleRoot {
                name: "twice".to_owned(),
                canonical_root: PathBuf::from("/bundles/two"),
            },
        ];
        {
            let selection = RegisteredBundleSelection::Only;
            assert!(matches!(
                select_registered_bundle(&registered, "absent", selection),
                Err(EngineError::RegisteredBundleSelectionRefused {
                    refusal: RegisteredBundleSelectionRefusal::NotRegistered,
                    name: _,
                })
            ));
            assert!(matches!(
                select_registered_bundle(&registered, "twice", selection),
                Err(EngineError::RegisteredBundleSelectionRefused {
                    refusal: RegisteredBundleSelectionRefusal::RegisteredMoreThanOnce,
                    name: _,
                })
            ));
        }
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
    fn resolve_finds_an_item_in_any_declared_format() {
        let project_root = tempdir();
        // Only the second declared format exists: every format is probed,
        // and the single match resolves. Two formats of one name in one
        // folder are refused (see two_formats_of_one_name_in_one_folder_are_refused).
        let schema = make_kind_schema(
            "tools",
            vec![(".py", "python/tool-header"), (".yaml", "yaml/yaml")],
        );

        write_item(&project_root, "tools", "my_tool", ".yaml", "name: yaml");

        let roots = ResolutionRoots::from_flat(Some(project_root), vec![]);
        let ref_ = CanonicalRef::parse("tool:my_tool").unwrap();

        let (path, _, ext) = resolve_item(&roots, &schema, &ref_).unwrap();
        assert_eq!(ext, ".yaml");
        assert!(path.to_string_lossy().ends_with(".yaml"));
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
