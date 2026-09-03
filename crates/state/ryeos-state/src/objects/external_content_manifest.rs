//! Content-addressed manifest for realized external content.
//!
//! The state layer treats a realization structurally, exactly as it treats a
//! restore manifest: it knows that entries name blobs and that those blobs
//! must stay reachable, and it knows nothing about what the content means.
//! Identity semantics — declaration, authority, ordering — belong to the
//! engine that built the manifest.
//!
//! Reachability is the reason this object is typed. A realization referenced
//! only from a sealed program's derived values would be invisible to closure
//! traversal, and garbage collection would reclaim blobs a resumable chain
//! still needs to execute against.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXTERNAL_CONTENT_MANIFEST_KIND: &str = "external_content_manifest";
pub const EXTERNAL_CONTENT_TREE_SCHEMA: &str = "ryeos.external_content.tree.v2";
pub const EXTERNAL_REALIZATIONS_DERIVED_KEY: &str = "effective_external_realizations";
/// The single manifest path a file-shaped realization stores its content
/// under. Wire-level: both manifest kinds spell file shape the same way.
pub const FILE_REALIZATION_ENTRY_PATH: &str = "content";
/// Matches the generic closure link ceiling: a manifest that cannot be
/// traversed is unusable however well it hashes.
pub const MAX_EXTERNAL_CONTENT_ENTRIES: usize = 10_000;
pub const MAX_EXTERNAL_CONTENT_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_EXTERNAL_CONTENT_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_EXTERNAL_CONTENT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Pure DoS sanity for byte claims on the tier-blind realization wire. The
/// real byte policy is per storage tier and lives where the manifest kind is
/// known: the content caps above on this object and in its capture walk, and
/// the large-content caps on that manifest object and at ingest.
pub const MAX_REALIZATION_CLAIMED_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub const MAX_EXTERNAL_CONTENT_PATH_BYTES: usize = 4096;
pub const MAX_INLINE_SYMLINK_TARGET_BYTES: usize = 4096;
pub const MAX_SYMLINK_TARGET_BYTES: u64 = 4096;
/// Match the kernel's useful symlink-chain ceiling. A realization that needs
/// more expansions cannot be consumed reliably and is also an avoidable
/// validation DoS surface.
pub const MAX_INTERNAL_SYMLINK_EXPANSIONS: usize = 40;

/// Prove that a realization symlink resolves lexically inside its manifest
/// root. Realizations retain bytes, not ambient filesystem authority: an
/// absolute target or a relative target that walks above the manifest root
/// would make execution depend on host content outside the captured tree.
pub fn validate_internal_symlink_target(entry_path: &str, target: &[u8]) -> anyhow::Result<()> {
    if target.is_empty() || target.contains(&0) || target.len() as u64 > MAX_SYMLINK_TARGET_BYTES {
        anyhow::bail!("manifest symlink entry `{entry_path}` has an invalid target");
    }
    let target = std::str::from_utf8(target).map_err(|_| {
        anyhow::anyhow!("manifest symlink entry `{entry_path}` has a non-UTF-8 target")
    })?;
    let target_path = std::path::Path::new(target);
    if target_path.is_absolute() {
        anyhow::bail!("manifest symlink entry `{entry_path}` has an absolute target");
    }

    let mut depth = std::path::Path::new(entry_path)
        .parent()
        .map(|parent| parent.components().count())
        .unwrap_or(0);
    for component in target_path.components() {
        match component {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if depth > 0 => depth -= 1,
            std::path::Component::ParentDir => {
                anyhow::bail!("manifest symlink entry `{entry_path}` escapes the realization root")
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                anyhow::bail!("manifest symlink entry `{entry_path}` has a non-relative target")
            }
        }
    }
    Ok(())
}

/// Prove every retained symlink against the complete symlink namespace.
///
/// Checking one target lexically is insufficient: an earlier path component
/// can itself be a symlink, changing the meaning of a later `..`. Resolve each
/// link as the kernel would, memoizing complete link targets and refusing
/// cycles, excessive chains, and any pop above the realization root.
pub fn validate_internal_symlink_graph<'a>(
    symlinks: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> anyhow::Result<()> {
    let mut graph = BTreeMap::<String, String>::new();
    for (path, target) in symlinks {
        super::validate_canonical_project_relative_path(path)?;
        validate_internal_symlink_target(path, target.as_bytes())?;
        if graph.insert(path.to_owned(), target.to_owned()).is_some() {
            anyhow::bail!("manifest repeats symlink entry `{path}`");
        }
    }

    let mut resolved = BTreeMap::<String, (Vec<String>, usize)>::new();
    for path in graph.keys() {
        let mut expansions = 0usize;
        resolve_internal_symlink(
            path,
            &graph,
            &mut resolved,
            &mut BTreeSet::new(),
            &mut expansions,
        )?;
    }
    Ok(())
}

fn resolve_internal_symlink(
    path: &str,
    graph: &BTreeMap<String, String>,
    resolved: &mut BTreeMap<String, (Vec<String>, usize)>,
    visiting: &mut BTreeSet<String>,
    expansions: &mut usize,
) -> anyhow::Result<Vec<String>> {
    if let Some((cached, cost)) = resolved.get(path) {
        charge_internal_symlink_expansions(path, expansions, *cost)?;
        return Ok(cached.clone());
    }
    let initial_expansions = *expansions;
    charge_internal_symlink_expansions(path, expansions, 1)?;
    if !visiting.insert(path.to_owned()) {
        anyhow::bail!("manifest symlink graph contains a cycle at `{path}`");
    }

    let target = graph
        .get(path)
        .ok_or_else(|| anyhow::anyhow!("manifest symlink graph lost entry `{path}`"))?;
    let mut current = path
        .rsplit_once('/')
        .map(|(parent, _)| parent.split('/').map(str::to_owned).collect::<Vec<_>>())
        .unwrap_or_default();
    for component in std::path::Path::new(target).components() {
        match component {
            std::path::Component::Normal(name) => {
                let name = name.to_str().ok_or_else(|| {
                    anyhow::anyhow!("manifest symlink entry `{path}` has a non-UTF-8 target")
                })?;
                current.push(name.to_owned());
                let candidate = current.join("/");
                if graph.contains_key(&candidate) {
                    current = resolve_internal_symlink(
                        &candidate, graph, resolved, visiting, expansions,
                    )?;
                }
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if current.pop().is_none() {
                    anyhow::bail!(
                        "manifest symlink entry `{path}` escapes the realization root through the symlink graph"
                    );
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                anyhow::bail!("manifest symlink entry `{path}` has a non-relative target")
            }
        }
    }
    visiting.remove(path);
    let cost = expansions
        .checked_sub(initial_expansions)
        .ok_or_else(|| anyhow::anyhow!("manifest symlink expansion accounting underflow"))?;
    resolved.insert(path.to_owned(), (current.clone(), cost));
    Ok(current)
}

fn charge_internal_symlink_expansions(
    path: &str,
    expansions: &mut usize,
    amount: usize,
) -> anyhow::Result<()> {
    *expansions = expansions
        .checked_add(amount)
        .ok_or_else(|| anyhow::anyhow!("manifest symlink expansion count overflow"))?;
    if *expansions > MAX_INTERNAL_SYMLINK_EXPANSIONS {
        anyhow::bail!(
            "manifest symlink entry `{path}` exceeds the {MAX_INTERNAL_SYMLINK_EXPANSIONS}-link resolution bound"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContentKind {
    File,
    Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContentMode {
    Pinned,
    Captured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalContentManifestEntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentManifestEntry {
    pub path: String,
    pub kind: ExternalContentManifestEntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentManifestObject {
    pub schema: String,
    pub kind: String,
    pub entries: Vec<ExternalContentManifestEntry>,
    pub entry_count: usize,
    pub total_bytes: u64,
}

/// One identity-bearing external-content realization sealed in the composed
/// program. This durable wire type is shared by admission, recovery, and CAS
/// closure traversal; those boundaries must never infer it from an arbitrary
/// derived-value shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentRealization {
    pub id: String,
    pub kind: ExternalContentKind,
    pub mode: ExternalContentMode,
    pub manifest_hash: String,
    pub entry_count: usize,
    pub total_bytes: u64,
    pub mount: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExternalContentRealizationSet(Vec<ExternalContentRealization>);

impl ExternalContentRealizationSet {
    pub fn new(mut entries: Vec<ExternalContentRealization>) -> anyhow::Result<Self> {
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        let set = Self(entries);
        set.validate()?;
        Ok(set)
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let set: Self = serde_json::from_value(value.clone())?;
        set.validate()?;
        Ok(set)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ExternalContentRealization> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut previous_id: Option<&str> = None;
        let mut mounts = BTreeSet::new();
        let mut entry_total = 0usize;
        let mut byte_total = 0u64;
        for entry in &self.0 {
            validate_realization_id(&entry.id)?;
            super::validate_canonical_project_relative_path(&entry.mount)?;
            if entry.mount.len() > MAX_EXTERNAL_CONTENT_PATH_BYTES {
                anyhow::bail!(
                    "external realization mount exceeds {MAX_EXTERNAL_CONTENT_PATH_BYTES} bytes"
                );
            }
            super::thread_snapshot::validate_canonical_hash(
                "external realization manifest_hash",
                &entry.manifest_hash,
            )?;
            if entry.entry_count > MAX_EXTERNAL_CONTENT_ENTRIES {
                anyhow::bail!(
                    "external realization `{}` exceeds {MAX_EXTERNAL_CONTENT_ENTRIES} entries",
                    entry.id
                );
            }
            if entry.total_bytes > MAX_REALIZATION_CLAIMED_BYTES {
                anyhow::bail!(
                    "external realization `{}` claims more than {MAX_REALIZATION_CLAIMED_BYTES} bytes",
                    entry.id
                );
            }
            entry_total = entry_total
                .checked_add(entry.entry_count)
                .ok_or_else(|| anyhow::anyhow!("external realization entry count overflow"))?;
            byte_total = byte_total
                .checked_add(entry.total_bytes)
                .ok_or_else(|| anyhow::anyhow!("external realization byte count overflow"))?;
            if entry_total > MAX_EXTERNAL_CONTENT_ENTRIES
                || byte_total > MAX_REALIZATION_CLAIMED_BYTES
            {
                anyhow::bail!("external realization set exceeds the per-launch aggregate bound");
            }
            if let Some(previous) = previous_id
                && previous >= entry.id.as_str()
            {
                anyhow::bail!("external realization set is not strictly ordered by id");
            }
            previous_id = Some(&entry.id);
            if !mounts.insert(entry.mount.as_str()) {
                anyhow::bail!(
                    "external realization mount `{}` appears more than once",
                    entry.mount
                );
            }
        }

        let mounts = mounts.into_iter().collect::<Vec<_>>();
        for (index, left) in mounts.iter().enumerate() {
            for right in mounts.iter().skip(index + 1) {
                if path_contains(left, right) || path_contains(right, left) {
                    anyhow::bail!("external realization mounts `{left}` and `{right}` overlap");
                }
            }
        }
        Ok(())
    }

    pub fn manifest_hashes(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|entry| entry.manifest_hash.clone())
            .collect()
    }
}

fn validate_realization_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.len() > 64 {
        anyhow::bail!("external realization id must be 1..=64 bytes");
    }
    if !id.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    }) {
        anyhow::bail!("external realization id has an unsupported character: {id:?}");
    }
    Ok(())
}

fn path_contains(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Prove that a manifest is one reconstructable tree rather than only a set
/// of individually valid paths. Every non-root entry must name every ancestor
/// explicitly, and each such ancestor must be a directory. Materializers can
/// therefore neither invent uncommitted directories nor collide with a
/// regular file or symlink while rebuilding the signed namespace.
pub(super) fn validate_manifest_tree_namespace<'a>(
    entries: impl IntoIterator<Item = (&'a str, ExternalContentManifestEntryKind)>,
) -> anyhow::Result<()> {
    let entries = entries.into_iter().collect::<BTreeMap<_, _>>();
    for path in entries.keys() {
        let mut ancestor = *path;
        while let Some(separator) = ancestor.rfind('/') {
            ancestor = &ancestor[..separator];
            match entries.get(ancestor) {
                Some(ExternalContentManifestEntryKind::Dir) => {}
                Some(_) => anyhow::bail!(
                    "external content manifest path `{path}` has non-directory ancestor `{ancestor}`"
                ),
                None => anyhow::bail!(
                    "external content manifest path `{path}` has absent directory ancestor `{ancestor}`"
                ),
            }
        }
    }
    Ok(())
}

impl ExternalContentManifestObject {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_value(value.clone())?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.kind != EXTERNAL_CONTENT_MANIFEST_KIND {
            anyhow::bail!("unexpected external content manifest kind: {}", self.kind);
        }
        if self.schema != EXTERNAL_CONTENT_TREE_SCHEMA {
            anyhow::bail!(
                "unexpected external content manifest schema: {}",
                self.schema
            );
        }
        if self.entry_count != self.entries.len() {
            anyhow::bail!("external content manifest entry count disagrees with its entries");
        }
        if self.entries.len() > MAX_EXTERNAL_CONTENT_ENTRIES {
            anyhow::bail!(
                "external content manifest carries {} entries; the bound is {MAX_EXTERNAL_CONTENT_ENTRIES}",
                self.entries.len()
            );
        }
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        if canonical.len() > MAX_EXTERNAL_CONTENT_MANIFEST_BYTES {
            anyhow::bail!(
                "external content manifest is {} bytes; the bound is {MAX_EXTERNAL_CONTENT_MANIFEST_BYTES}",
                canonical.len()
            );
        }
        if self.total_bytes > MAX_EXTERNAL_CONTENT_TOTAL_BYTES {
            anyhow::bail!(
                "external content manifest declares {} bytes; the bound is {MAX_EXTERNAL_CONTENT_TOTAL_BYTES}",
                self.total_bytes
            );
        }

        // `total_bytes` enters executable identity, so a manifest may not
        // assert a total its own entries do not sum to.
        let summed: u64 = self
            .entries
            .iter()
            .filter_map(|entry| entry.size)
            .try_fold(0u64, |total, size| total.checked_add(size))
            .ok_or_else(|| anyhow::anyhow!("external content manifest sizes overflow"))?;
        if summed != self.total_bytes {
            anyhow::bail!(
                "external content manifest declares {} total bytes but its entries sum to {summed}",
                self.total_bytes
            );
        }

        let mut previous: Option<&str> = None;
        for entry in &self.entries {
            super::validate_canonical_project_relative_path(&entry.path)?;
            if entry.path.len() > MAX_EXTERNAL_CONTENT_PATH_BYTES {
                anyhow::bail!(
                    "external content manifest path exceeds {MAX_EXTERNAL_CONTENT_PATH_BYTES} bytes"
                );
            }
            // Strict ordering is structural: a duplicate or out-of-order path
            // would make one manifest describe two different trees.
            if let Some(previous) = previous
                && previous.as_bytes() >= entry.path.as_bytes()
            {
                anyhow::bail!(
                    "external content manifest entries are not strictly ordered by path bytes"
                );
            }
            previous = Some(&entry.path);

            match entry.kind {
                ExternalContentManifestEntryKind::File => {
                    let hash = entry.blob_hash.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("manifest file entry `{}` has no blob", entry.path)
                    })?;
                    crate::objects::thread_snapshot::validate_canonical_hash(
                        "external content blob_hash",
                        hash,
                    )?;
                    let size = entry.size.ok_or_else(|| {
                        anyhow::anyhow!("manifest file entry `{}` has no size", entry.path)
                    })?;
                    if size > MAX_EXTERNAL_CONTENT_FILE_BYTES {
                        anyhow::bail!(
                            "manifest file entry `{}` is {size} bytes; the bound is {MAX_EXTERNAL_CONTENT_FILE_BYTES}",
                            entry.path
                        );
                    }
                    if !matches!(entry.mode, Some(0o644) | Some(0o755)) {
                        anyhow::bail!(
                            "manifest file entry `{}` has an invalid normalized mode",
                            entry.path
                        );
                    }
                    if entry.target.is_some() {
                        anyhow::bail!("manifest file entry `{}` carries a link target", entry.path);
                    }
                }
                ExternalContentManifestEntryKind::Dir => {
                    if entry.blob_hash.is_some()
                        || entry.size.is_some()
                        || entry.mode.is_some()
                        || entry.target.is_some()
                    {
                        anyhow::bail!("manifest directory entry `{}` carries content", entry.path);
                    }
                }
                ExternalContentManifestEntryKind::Symlink => {
                    if entry.blob_hash.is_some() || entry.size.is_some() || entry.mode.is_some() {
                        anyhow::bail!("manifest symlink entry `{}` carries a blob", entry.path);
                    }
                    match entry.target.as_deref() {
                        Some(target) => {
                            if target.is_empty()
                                || target.as_bytes().contains(&0)
                                || target.len() > MAX_INLINE_SYMLINK_TARGET_BYTES
                            {
                                anyhow::bail!(
                                    "manifest symlink entry `{}` has an invalid inline target",
                                    entry.path
                                );
                            }
                            validate_internal_symlink_target(&entry.path, target.as_bytes())?;
                        }
                        // A symlink without a target cannot be rebuilt, and a
                        // realization that cannot be rebuilt is not one.
                        None => anyhow::bail!(
                            "manifest symlink entry `{}` cannot be reconstructed without a target",
                            entry.path
                        ),
                    }
                }
            }
        }
        validate_internal_symlink_graph(self.entries.iter().filter_map(|entry| {
            (entry.kind == ExternalContentManifestEntryKind::Symlink).then(|| {
                (
                    entry.path.as_str(),
                    entry.target.as_deref().expect("validated target"),
                )
            })
        }))?;
        validate_manifest_tree_namespace(
            self.entries
                .iter()
                .map(|entry| (entry.path.as_str(), entry.kind)),
        )?;
        Ok(())
    }

    /// Whether this manifest can back a file-shaped realization: exactly one
    /// regular-file entry at the shared fixed file path.
    pub fn is_file_shaped(&self) -> bool {
        self.entries.len() == 1
            && self.entries[0].path == FILE_REALIZATION_ENTRY_PATH
            && self.entries[0].kind == ExternalContentManifestEntryKind::File
    }

    /// Every blob this manifest needs in order to be materialized.
    pub fn referenced_blobs(&self) -> Vec<String> {
        let mut blobs = BTreeSet::new();
        for entry in &self.entries {
            if let Some(hash) = &entry.blob_hash {
                blobs.insert(hash.clone());
            }
        }
        blobs.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: ExternalContentManifestEntryKind) -> ExternalContentManifestEntry {
        ExternalContentManifestEntry {
            path: path.to_string(),
            kind,
            mode: None,
            blob_hash: None,
            size: None,
            target: None,
        }
    }

    fn manifest(entries: Vec<ExternalContentManifestEntry>) -> ExternalContentManifestObject {
        ExternalContentManifestObject {
            schema: EXTERNAL_CONTENT_TREE_SCHEMA.to_string(),
            kind: EXTERNAL_CONTENT_MANIFEST_KIND.to_string(),
            entry_count: entries.len(),
            entries,
            total_bytes: 0,
        }
    }

    #[test]
    fn a_symlink_without_a_target_cannot_be_admitted() {
        let object = manifest(vec![entry(
            "link",
            ExternalContentManifestEntryKind::Symlink,
        )]);
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("cannot be reconstructed"), "got: {error}");
    }

    #[test]
    fn realization_symlinks_must_resolve_inside_the_manifest_root() {
        let mut absolute = entry("bin/python", ExternalContentManifestEntryKind::Symlink);
        absolute.target = Some("/usr/bin/python3".to_owned());
        let error = manifest(vec![absolute]).validate().unwrap_err().to_string();
        assert!(error.contains("absolute target"), "got: {error}");

        let mut escaping = entry("bin/python", ExternalContentManifestEntryKind::Symlink);
        escaping.target = Some("../../usr/bin/python3".to_owned());
        let error = manifest(vec![escaping]).validate().unwrap_err().to_string();
        assert!(
            error.contains("escapes the realization root"),
            "got: {error}"
        );

        let mut internal = entry("bin/python", ExternalContentManifestEntryKind::Symlink);
        internal.target = Some("../lib/python3".to_owned());
        manifest(vec![
            entry("bin", ExternalContentManifestEntryKind::Dir),
            internal,
        ])
        .validate()
        .unwrap();
    }

    #[test]
    fn realization_symlink_graph_cannot_turn_a_later_parent_into_an_escape() {
        let mut redirect = entry("a", ExternalContentManifestEntryKind::Symlink);
        redirect.target = Some(".".to_owned());
        let mut escaping = entry("dir/b", ExternalContentManifestEntryKind::Symlink);
        escaping.target = Some("../a/../outside".to_owned());
        let error = manifest(vec![redirect, escaping])
            .validate()
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("escapes the realization root through the symlink graph"),
            "got: {error}"
        );
    }

    #[test]
    fn realization_symlink_graph_refuses_cycles_and_accepts_internal_chains() {
        let mut first = entry("a", ExternalContentManifestEntryKind::Symlink);
        first.target = Some("b".to_owned());
        let mut second = entry("b", ExternalContentManifestEntryKind::Symlink);
        second.target = Some("a".to_owned());
        let error = manifest(vec![first, second])
            .validate()
            .unwrap_err()
            .to_string();
        assert!(error.contains("contains a cycle"), "got: {error}");

        let mut base = entry("a", ExternalContentManifestEntryKind::Symlink);
        base.target = Some("lib".to_owned());
        let mut chain = entry("bin/current", ExternalContentManifestEntryKind::Symlink);
        chain.target = Some("../a/runtime".to_owned());
        manifest(vec![
            base,
            entry("bin", ExternalContentManifestEntryKind::Dir),
            chain,
        ])
        .validate()
        .unwrap();
    }

    #[test]
    fn realization_symlink_graph_counts_sequential_cached_expansions() {
        let mut symlinks = (0..=MAX_INTERNAL_SYMLINK_EXPANSIONS)
            .map(|index| (format!("a{index:02}"), "dir".to_owned()))
            .collect::<Vec<_>>();
        let target = (0..=MAX_INTERNAL_SYMLINK_EXPANSIONS)
            .map(|index| format!("a{index:02}/.."))
            .chain(std::iter::once("final".to_owned()))
            .collect::<Vec<_>>()
            .join("/");
        symlinks.push(("chain".to_owned(), target));
        let error = validate_internal_symlink_graph(
            symlinks
                .iter()
                .map(|(path, target)| (path.as_str(), target.as_str())),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("link resolution bound"), "got: {error}");
    }

    #[test]
    fn realization_symlink_graph_charges_cached_transitive_expansions() {
        let mut symlinks = (0..39)
            .map(|index| {
                let target = if index == 38 {
                    "dir".to_owned()
                } else {
                    format!("a{:02}", index + 1)
                };
                (format!("a{index:02}"), target)
            })
            .collect::<Vec<_>>();
        symlinks.push(("chain".to_owned(), "a00/../a00/../final".to_owned()));
        let error = validate_internal_symlink_graph(
            symlinks
                .iter()
                .map(|(path, target)| (path.as_str(), target.as_str())),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("link resolution bound"), "got: {error}");
    }

    #[test]
    fn entries_must_be_strictly_ordered_by_path_bytes() {
        let mut first = entry("b", ExternalContentManifestEntryKind::Dir);
        first.path = "b".to_string();
        let object = manifest(vec![
            first,
            entry("a", ExternalContentManifestEntryKind::Dir),
        ]);
        let error = object.validate().unwrap_err().to_string();
        assert!(error.contains("strictly ordered"), "got: {error}");
    }

    #[test]
    fn manifest_tree_requires_explicit_directory_ancestors() {
        let mut nested = entry("lib/runtime", ExternalContentManifestEntryKind::File);
        nested.blob_hash = Some("a".repeat(64));
        nested.size = Some(1);
        nested.mode = Some(0o644);
        let mut missing = manifest(vec![nested.clone()]);
        missing.total_bytes = 1;
        let error = missing.validate().unwrap_err().to_string();
        assert!(error.contains("absent directory ancestor"), "got: {error}");

        let mut collision = entry("lib", ExternalContentManifestEntryKind::Symlink);
        collision.target = Some("runtime-root".to_owned());
        let mut colliding = manifest(vec![collision, nested]);
        colliding.total_bytes = 1;
        let error = colliding.validate().unwrap_err().to_string();
        assert!(error.contains("non-directory ancestor"), "got: {error}");

        let mut regular = entry("lib/runtime", ExternalContentManifestEntryKind::File);
        regular.blob_hash = Some("a".repeat(64));
        regular.size = Some(1);
        regular.mode = Some(0o644);
        let mut valid = manifest(vec![
            entry("lib", ExternalContentManifestEntryKind::Dir),
            regular,
        ]);
        valid.total_bytes = 1;
        valid.validate().unwrap();
    }

    #[test]
    fn referenced_blobs_cover_regular_file_content_only() {
        let mut file = entry("a", ExternalContentManifestEntryKind::File);
        file.blob_hash = Some("a".repeat(64));
        file.size = Some(1);
        file.mode = Some(0o644);
        let mut link = entry("b", ExternalContentManifestEntryKind::Symlink);
        link.target = Some("a".to_owned());

        let mut object = manifest(vec![file, link]);
        object.total_bytes = 1;
        object.validate().unwrap();
        assert_eq!(object.referenced_blobs(), vec!["a".repeat(64)]);
    }

    fn realization(id: &str, mount: &str) -> ExternalContentRealization {
        ExternalContentRealization {
            id: id.to_string(),
            kind: ExternalContentKind::Tree,
            mode: ExternalContentMode::Captured,
            manifest_hash: "a".repeat(64),
            entry_count: 1,
            total_bytes: 10,
            mount: mount.to_string(),
        }
    }

    #[test]
    fn realization_set_round_trips_through_its_wire_value() {
        let set = ExternalContentRealizationSet::new(vec![
            realization("beta", "vendor/beta"),
            realization("alpha", "vendor/alpha"),
        ])
        .unwrap();
        let value = set.to_value().unwrap();
        let restored = ExternalContentRealizationSet::from_value(&value).unwrap();
        assert_eq!(restored, set);
        // Construction ordered by id, and the ordering survives the wire.
        assert_eq!(
            restored
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn overlapping_realization_mounts_are_rejected() {
        let error = ExternalContentRealizationSet::new(vec![
            realization("outer", "vendor/sim"),
            realization("inner", "vendor/sim/lib"),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("overlap"), "got {error}");
    }
}
