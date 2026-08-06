//! External content declaration, manifest, and locator authority.
//!
//! An admitted program's identity covers its own composed definition, but not
//! the content it executes *against*. This module names that content: a signed
//! item declares what matters to its behavior, admission walks it under a
//! pinned root, and the resulting manifest becomes part of executable
//! identity.
//!
//! A manifest is not merely an observation. Its file entries reference CAS
//! blobs, so an admitted realization can be rebuilt byte-for-byte long after
//! the live content moved or vanished. Recording a digest without its bytes
//! would prove what was seen while leaving nothing to execute.
//!
//! Scope is deliberately bounded: content is declared, never discovered. What
//! an item does not declare stays unknown to RyeOS and is reported as unknown
//! rather than implied to be sealed.

use std::collections::BTreeSet;
use std::ffi::OsStr;

use serde::{Deserialize, Serialize};

pub use ryeos_state::objects::{ExternalContentKind, ExternalContentMode};
/// Entries admitted from one declaration. Matches the generic object-closure
/// link ceiling: a manifest that cannot be traversed by garbage collection is
/// unusable regardless of how well it hashes.
pub const MAX_DECLARATION_ENTRIES: usize =
    ryeos_state::objects::MAX_EXTERNAL_CONTENT_ENTRIES;
/// Bytes admitted from one declaration.
pub const MAX_DECLARATION_BYTES: u64 =
    ryeos_state::objects::MAX_EXTERNAL_CONTENT_TOTAL_BYTES;
/// Bytes admitted from one regular file. This matches the generic CAS
/// closure's per-blob transport bound.
pub const MAX_DECLARATION_FILE_BYTES: u64 =
    ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES;
/// Entries admitted across every declaration of one launch. The closure limit
/// bounds an object graph, not a single declaration, so the aggregate is what
/// actually has to hold.
pub const MAX_LAUNCH_ENTRIES: usize = ryeos_state::objects::MAX_EXTERNAL_CONTENT_ENTRIES;
/// Bytes admitted across every declaration of one launch.
pub const MAX_LAUNCH_BYTES: u64 = ryeos_state::objects::MAX_EXTERNAL_CONTENT_TOTAL_BYTES;
/// Directory depth below a declared root.
pub const MAX_DECLARATION_DEPTH: usize = 64;
/// Bytes in one manifest-relative path.
pub const MAX_ENTRY_PATH_BYTES: usize = ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES;
/// Symlink targets at or below this length are carried inline; longer targets
/// become CAS blobs so that materialization can always rebuild the link.
pub const MAX_INLINE_SYMLINK_TARGET_BYTES: usize =
    ryeos_state::objects::MAX_INLINE_SYMLINK_TARGET_BYTES;
pub const MAX_SYMLINK_TARGET_BYTES: usize =
    ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES as usize;
/// Declarations one item may carry.
pub const MAX_DECLARATIONS_PER_ITEM: usize = 8;
/// Exclusion patterns one declaration may carry.
pub const MAX_EXCLUDES_PER_DECLARATION: usize = 32;
/// Canonical internal entry name for a `kind: file` realization. Source
/// locator names are evidence-only; the logical mount carries binding
/// identity, so host basenames must not leak into the content manifest.
pub const FILE_REALIZATION_ENTRY_PATH: &str = "content";

/// Who is declaring, with enough identity to answer the bundle row of the
/// authority matrix.
///
/// `ItemSpace` says an item came from bundle space but not *which* bundle, and
/// "any bundle may name any bundle" is exactly the cross-tenant oracle the
/// matrix exists to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaringAuthority<'a> {
    Project,
    Node,
    Bundle(&'a str),
}

impl DeclaringAuthority<'_> {
    pub fn label(&self) -> String {
        match self {
            Self::Project => "project".to_string(),
            Self::Node => "node".to_string(),
            Self::Bundle(name) => format!("bundle:{name}"),
        }
    }
}

/// Roots a locator may name.
///
/// Enumerated rather than free-form: a declaration names an already-admitted
/// root, never a host path, so no authored content can direct the daemon at an
/// arbitrary location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalContentRoot {
    /// The admitted project `.ai/` root.
    ProjectAi,
    /// The admitted project directory that contains `.ai/`.
    ProjectFiles,
    /// The node data root.
    NodeFiles,
    /// One exactly-named installed bundle root.
    Bundle(String),
}

impl Serialize for ExternalContentRoot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.label())
    }
}

impl<'de> Deserialize<'de> for ExternalContentRoot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The wire form is the same `bundle:<name>` string authors write, so a
        // declaration reads the way the contract documents it rather than as
        // an externally tagged enum.
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl ExternalContentRoot {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "project_ai" => Ok(Self::ProjectAi),
            "project_files" => Ok(Self::ProjectFiles),
            "node_files" => Ok(Self::NodeFiles),
            other => match other.strip_prefix("bundle:") {
                Some(name) if !name.is_empty() => Ok(Self::Bundle(name.to_string())),
                _ => Err(format!("unsupported external content root: {other}")),
            },
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::ProjectAi => "project_ai".to_string(),
            Self::ProjectFiles => "project_files".to_string(),
            Self::NodeFiles => "node_files".to_string(),
            Self::Bundle(name) => format!("bundle:{name}"),
        }
    }

    pub fn contract_class(&self) -> &'static str {
        match self {
            Self::ProjectAi => "project_ai",
            Self::ProjectFiles => "project_files",
            Self::NodeFiles => "node_files",
            Self::Bundle(_) => "bundle:own",
        }
    }

    /// May an item admitted from `space` declare content under this root?
    ///
    /// Without this check a project-authored declaration could make the daemon
    /// walk node or bundle space. No bytes reach the declarer, but presence,
    /// absence, and size would — a read and membership oracle obtained purely
    /// by authoring a file.
    pub fn declarable_from(&self, declarer: DeclaringAuthority<'_>) -> bool {
        match (self, declarer) {
            (
                Self::ProjectAi | Self::ProjectFiles,
                DeclaringAuthority::Project | DeclaringAuthority::Node,
            ) => true,
            (Self::NodeFiles, DeclaringAuthority::Node) => true,
            // A bundle may name only itself. `ItemSpace::Bundle` alone cannot
            // express this, and accepting any bundle name would hand every
            // bundle a presence oracle over every other one.
            (Self::Bundle(named), DeclaringAuthority::Bundle(own)) => named == own,
            _ => false,
        }
    }
}

/// Parse and validate the effective declaration value emitted by the
/// kind-declared composition path.
///
/// Absence and explicit `[]` remain distinct at this boundary: absence means
/// the item did not declare the field, while an empty vector is a positive
/// declaration of no realized external content.
pub fn declarations_from_composed(
    composed: &serde_json::Value,
    contract: Option<&crate::kind_registry::ExecutionExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<Option<Vec<ExternalContentDeclaration>>> {
    let authored = composed.get("external_content");
    let Some(contract) = contract else {
        if authored.is_some() {
            anyhow::bail!(
                "item declares `external_content` but its signed kind has no execution.external_content contract"
            );
        }
        return Ok(None);
    };
    let Some(value) = authored else {
        return Ok(None);
    };
    if value.is_null() {
        anyhow::bail!("external_content must be an array; null does not mean inherit or none");
    }
    let declarations: Vec<ExternalContentDeclaration> = serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("invalid external_content declaration: {error}"))?;
    if declarations.len() > contract.max_declarations {
        anyhow::bail!(
            "item declares {} external content entries; its signed kind permits {}",
            declarations.len(),
            contract.max_declarations
        );
    }
    validate_declarations(&declarations, declarer)?;
    for declaration in &declarations {
        if !contract
            .allowed_roots
            .iter()
            .any(|allowed| allowed == declaration.locator.root.contract_class())
        {
            anyhow::bail!(
                "external content `{}` names root `{}` which its signed kind does not permit",
                declaration.id,
                declaration.locator.root.label()
            );
        }
    }
    Ok(Some(declarations))
}

/// How a declared digest is treated at admission.
/// Where declared content lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentLocator {
    pub root: ExternalContentRoot,
    /// Canonical root-relative path. Validated before any filesystem access,
    /// so `..` is unrepresentable rather than merely rejected later.
    pub path: String,
}

/// One authored declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentDeclaration {
    pub id: String,
    pub kind: ExternalContentKind,
    pub locator: ExternalContentLocator,
    pub mode: ExternalContentMode,
    /// Required when `mode` is `Pinned`, forbidden otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Basename patterns to skip: a literal name or `*.ext`. A matching
    /// directory prunes its whole subtree, which is what makes excluding
    /// build caches meaningful.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Optional evidence extractor for human-readable package metadata. Never
    /// affects identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_hint: Option<String>,
    /// Logical mount target relative to the runtime's realization root. The
    /// binding destination is part of identity: identical bytes mounted
    /// somewhere else are a different program.
    pub mount: String,
}

impl ExternalContentDeclaration {
    pub fn validate(&self, declarer: DeclaringAuthority<'_>) -> anyhow::Result<()> {
        validate_declaration_id(&self.id)?;
        validate_relative_path("external content locator path", &self.locator.path)?;
        validate_relative_path("external content mount target", &self.mount)?;
        if ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&self.mount) {
            anyhow::bail!(
                "external content mount target `{}` overlaps a protected project namespace",
                self.mount
            );
        }

        if !self.locator.root.declarable_from(declarer) {
            anyhow::bail!(
                "an item admitted from {} may not declare external content under {}",
                declarer.label(),
                self.locator.root.label()
            );
        }
        if let ExternalContentRoot::Bundle(name) = &self.locator.root
            && name.is_empty()
        {
            anyhow::bail!("external content bundle root must name a bundle");
        }

        match (self.mode, self.digest.as_deref()) {
            (ExternalContentMode::Pinned, None) => {
                anyhow::bail!(
                    "pinned external content `{}` must carry its expected digest",
                    self.id
                );
            }
            (ExternalContentMode::Captured, Some(_)) => {
                anyhow::bail!(
                    "captured external content `{}` must not carry a digest; \
                     a captured declaration records what is present",
                    self.id
                );
            }
            (ExternalContentMode::Pinned, Some(digest)) => {
                if !lillux::cas::valid_hash(digest) {
                    anyhow::bail!(
                        "pinned external content `{}` digest is not a canonical hash",
                        self.id
                    );
                }
            }
            (ExternalContentMode::Captured, None) => {}
        }

        if self.exclude.len() > MAX_EXCLUDES_PER_DECLARATION {
            anyhow::bail!(
                "external content `{}` declares {} exclusions; the bound is {MAX_EXCLUDES_PER_DECLARATION}",
                self.id,
                self.exclude.len()
            );
        }
        for pattern in &self.exclude {
            validate_exclude_pattern(pattern)?;
        }
        Ok(())
    }
}

/// Validate a complete declaration list for one item.
pub fn validate_declarations(
    declarations: &[ExternalContentDeclaration],
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<()> {
    if declarations.len() > MAX_DECLARATIONS_PER_ITEM {
        anyhow::bail!(
            "item declares {} external content entries; the bound is {MAX_DECLARATIONS_PER_ITEM}",
            declarations.len()
        );
    }
    let mut ids = BTreeSet::new();
    let mut mounts = BTreeSet::new();
    for declaration in declarations {
        declaration.validate(declarer)?;
        if !ids.insert(declaration.id.as_str()) {
            anyhow::bail!(
                "external content id `{}` is declared more than once",
                declaration.id
            );
        }
        if !mounts.insert(declaration.mount.as_str()) {
            anyhow::bail!(
                "external content mount target `{}` is declared more than once",
                declaration.mount
            );
        }
    }
    let mounts = mounts.into_iter().collect::<Vec<_>>();
    for (index, left) in mounts.iter().enumerate() {
        for right in mounts.iter().skip(index + 1) {
            if path_contains(left, right) || path_contains(right, left) {
                anyhow::bail!(
                    "external content mount targets `{left}` and `{right}` overlap"
                );
            }
        }
    }
    Ok(())
}

fn path_contains(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn validate_declaration_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.len() > 64 {
        anyhow::bail!("external content id must be 1..=64 bytes: {id:?}");
    }
    let supported = |byte: u8| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    };
    if !id.bytes().all(supported) {
        anyhow::bail!("external content id has an unsupported character: {id:?}");
    }
    Ok(())
}

fn validate_exclude_pattern(pattern: &str) -> anyhow::Result<()> {
    if pattern.is_empty() || pattern.len() > 128 {
        anyhow::bail!("external content exclusion must be 1..=128 bytes: {pattern:?}");
    }
    if pattern.contains('/') {
        anyhow::bail!(
            "external content exclusion matches a basename and may not contain a path separator: {pattern:?}"
        );
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        if !suffix.starts_with('.') || suffix.len() < 2 || suffix.contains('*') {
            anyhow::bail!("external content suffix exclusion must be `*.ext`: {pattern:?}");
        }
        return Ok(());
    }
    if pattern.contains('*') {
        anyhow::bail!(
            "external content exclusion supports a literal basename or a `*.ext` suffix only: {pattern:?}"
        );
    }
    Ok(())
}

/// Canonical path validation, applied before any filesystem access.
///
/// Delegates to the state crate's validator so external content and project
/// content can never disagree about what "contained" means; the only addition
/// is this contract's own length bound.
fn validate_relative_path(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() > MAX_ENTRY_PATH_BYTES {
        anyhow::bail!("{label} exceeds {MAX_ENTRY_PATH_BYTES} bytes");
    }
    ryeos_state::objects::validate_canonical_project_relative_path(value)
        .map_err(|error| anyhow::anyhow!("{label}: {error}"))
}

/// Does a basename match one exclusion pattern?
fn excluded(name: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|pattern| match pattern.strip_prefix('*') {
        Some(suffix) => name.ends_with(suffix),
        None => name == pattern,
    })
}

// ── Manifest ─────────────────────────────────────────────────────────

// The manifest object itself is owned by `ryeos_state::objects`, which is
// where content-addressed objects live and where closure traversal reads
// them. Duplicating it here would create two shapes that must agree forever;
// this module builds the state crate's type directly.
pub use ryeos_state::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_CONTENT_TREE_SCHEMA,
    EXTERNAL_REALIZATIONS_DERIVED_KEY,
    ExternalContentManifestEntry as ManifestEntry, ExternalContentManifestEntryKind,
    ExternalContentManifestObject,
};

/// Security-floor context for one capture. The caller constructs this from
/// the admitted locator root; the walker then evaluates every entry using the
/// complete project-relative path rather than a locator-relative suffix.
pub struct ExternalCapturePolicy<'a> {
    locator_prefix: String,
    admitted_ignore: &'a ryeos_state::ignore::IgnoreMatcher,
}

impl<'a> ExternalCapturePolicy<'a> {
    pub fn for_declaration(
        declaration: &ExternalContentDeclaration,
        admitted_ignore: &'a ryeos_state::ignore::IgnoreMatcher,
    ) -> anyhow::Result<Self> {
        let locator_prefix = match declaration.locator.root {
            ExternalContentRoot::ProjectAi => {
                format!("{}/{}", crate::AI_DIR, declaration.locator.path)
            }
            ExternalContentRoot::ProjectFiles
            | ExternalContentRoot::NodeFiles
            | ExternalContentRoot::Bundle(_) => declaration.locator.path.clone(),
        };
        if ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&locator_prefix)
            || admitted_ignore.is_ignored(&locator_prefix)
        {
            anyhow::bail!(
                "external content locator `{locator_prefix}` is excluded by the admitted capture policy"
            );
        }
        Ok(Self {
            locator_prefix,
            admitted_ignore,
        })
    }

    fn excludes(&self, manifest_relative_path: &str) -> bool {
        let complete_path = format!("{}/{manifest_relative_path}", self.locator_prefix);
        ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&complete_path)
            || self.admitted_ignore.is_ignored(&complete_path)
    }
}

/// Budget shared across every declaration of one launch.
///
/// The per-declaration bounds alone would let eight declarations admit eight
/// times the intended content, so the aggregate is tracked separately and
/// checked before any entry is retained.
#[derive(Debug, Clone)]
pub struct LaunchRealizationBudget {
    remaining_entries: usize,
    remaining_bytes: u64,
}

impl Default for LaunchRealizationBudget {
    fn default() -> Self {
        Self {
            remaining_entries: MAX_LAUNCH_ENTRIES,
            remaining_bytes: MAX_LAUNCH_BYTES,
        }
    }
}

impl LaunchRealizationBudget {
    pub fn charge_entry(&mut self) -> anyhow::Result<()> {
        self.remaining_entries = self.remaining_entries.checked_sub(1).ok_or_else(|| {
            anyhow::anyhow!(
                "external content for this launch exceeds {MAX_LAUNCH_ENTRIES} aggregate entries"
            )
        })?;
        Ok(())
    }

    pub fn charge_bytes(&mut self, bytes: u64) -> anyhow::Result<()> {
        self.remaining_bytes = self.remaining_bytes.checked_sub(bytes).ok_or_else(|| {
            anyhow::anyhow!(
                "external content for this launch exceeds {MAX_LAUNCH_BYTES} aggregate bytes"
            )
        })?;
        Ok(())
    }
}

/// Sink for content discovered while walking a declaration.
///
/// The walker owns traversal and bounds; storing bytes belongs to whoever owns
/// CAS. Keeping them apart lets the engine define identity without depending
/// on a store, and lets tests exercise the walk without one.
pub trait ExternalContentBlobSink {
    /// Store one regular file, returning its canonical hash and the exact
    /// number of bytes stored.
    ///
    /// Implementations must detect a file mutating during the read and fail
    /// rather than return a hash for bytes that never coexisted. Returning the
    /// ingested size is what keeps the manifest's `size` and `blob_hash`
    /// describing the same content: a stat taken before the read can already
    /// be stale.
    fn store_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)>;

    /// Store an oversized symlink target, returning its canonical hash.
    fn store_target(&mut self, target: &[u8], path: &str) -> anyhow::Result<String>;
}

/// Walk one declared root into a canonical manifest, storing content as it
/// goes.
///
/// Traversal is descriptor-relative from a pinned root, so containment is a
/// property of the descriptor chain rather than of string comparison, and no
/// component can be swapped after it is checked. Symlinks are recorded and
/// never followed; a symlink therefore cannot carry the walk out of the
/// declared root.
#[cfg(unix)]
pub fn build_manifest(
    root: &lillux::secure_fs::PinnedDirectory,
    excludes: &[String],
    capture_policy: &ExternalCapturePolicy<'_>,
    budget: &mut LaunchRealizationBudget,
    sink: &mut dyn ExternalContentBlobSink,
) -> anyhow::Result<ExternalContentManifestObject> {
    let (root_device, _) = root.device_inode()?;
    let mut entries = Vec::new();
    let mut declaration_entries = 0usize;
    let mut declaration_bytes = 0u64;
    let mut total_bytes = 0u64;

    walk_directory(
        root,
        "",
        0,
        root_device,
        excludes,
        capture_policy,
        budget,
        sink,
        &mut entries,
        &mut declaration_entries,
        &mut declaration_bytes,
        &mut total_bytes,
    )?;

    entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let manifest = ExternalContentManifestObject {
        schema: EXTERNAL_CONTENT_TREE_SCHEMA.to_string(),
        kind: EXTERNAL_CONTENT_MANIFEST_KIND.to_string(),
        entry_count: entries.len(),
        entries,
        total_bytes,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// Capture one already-open regular file as the canonical single-entry
/// manifestation of `kind: file`.
#[cfg(unix)]
pub fn build_file_manifest(
    file: std::fs::File,
    display_path: &str,
    budget: &mut LaunchRealizationBudget,
    sink: &mut dyn ExternalContentBlobSink,
) -> anyhow::Result<ExternalContentManifestObject> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("external content file locator is not a regular file: {display_path}");
    }
    let size = metadata.len();
    if size > MAX_DECLARATION_FILE_BYTES || size > MAX_DECLARATION_BYTES {
        anyhow::bail!(
            "external content file {display_path} is {size} bytes; the bound is {MAX_DECLARATION_FILE_BYTES}"
        );
    }
    budget.charge_entry()?;
    budget.charge_bytes(size)?;
    let mode = lillux::secure_fs::normalized_portable_regular_mode(&metadata)?;
    let (blob_hash, stored_size) = sink.store_file(file, display_path, size)?;
    if stored_size != size {
        anyhow::bail!(
            "external content file {display_path} changed size during capture"
        );
    }
    let manifest = ExternalContentManifestObject {
        schema: EXTERNAL_CONTENT_TREE_SCHEMA.to_string(),
        kind: EXTERNAL_CONTENT_MANIFEST_KIND.to_string(),
        entries: vec![ManifestEntry {
            path: FILE_REALIZATION_ENTRY_PATH.to_string(),
            kind: ExternalContentManifestEntryKind::File,
            mode: Some(mode),
            blob_hash: Some(blob_hash),
            size: Some(size),
            target: None,
            target_blob: None,
        }],
        entry_count: 1,
        total_bytes: size,
    };
    manifest.validate()?;
    Ok(manifest)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn walk_directory(
    directory: &lillux::secure_fs::PinnedDirectory,
    prefix: &str,
    depth: usize,
    root_device: u64,
    excludes: &[String],
    capture_policy: &ExternalCapturePolicy<'_>,
    budget: &mut LaunchRealizationBudget,
    sink: &mut dyn ExternalContentBlobSink,
    entries: &mut Vec<ManifestEntry>,
    declaration_entries: &mut usize,
    declaration_bytes: &mut u64,
    total_bytes: &mut u64,
) -> anyhow::Result<()> {
    if depth >= MAX_DECLARATION_DEPTH {
        anyhow::bail!(
            "external content exceeds {MAX_DECLARATION_DEPTH} directory levels at {prefix}"
        );
    }

    for entry in directory.entries_no_follow()? {
        let name = entry
            .name
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "external content entry name under {prefix:?} is not valid UTF-8; \
                     identity cannot be recorded for a path RyeOS cannot represent"
                )
            })?
            .to_string();

        if excluded(&name, excludes) {
            continue;
        }

        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        validate_relative_path("external content manifest path", &path)?;

        // The shared floor receives the complete named-root-relative path; it
        // owns every non-bypassable exclusion and prunes subtrees the same way
        // an author exclusion does.
        if capture_policy.excludes(&path) {
            continue;
        }

        // A bind mount or separate filesystem below the declared root is
        // neither bounded nor reproducible by that root's declaration.
        if entry.containing_device != root_device {
            anyhow::bail!(
                "external content entry {path} is on a different filesystem than its declared root"
            );
        }

        *declaration_entries += 1;
        if *declaration_entries > MAX_DECLARATION_ENTRIES {
            anyhow::bail!(
                "external content declaration exceeds {MAX_DECLARATION_ENTRIES} entries at {path}"
            );
        }
        budget.charge_entry()?;

        match entry.entry_type {
            lillux::secure_fs::PinnedEntryType::Directory => {
                entries.push(ManifestEntry {
                    path: path.clone(),
                    kind: ExternalContentManifestEntryKind::Dir,
                    mode: None,
                    blob_hash: None,
                    size: None,
                    target: None,
                    target_blob: None,
                });
                let child = directory
                    .open_child_directory(OsStr::new(&name))?
                    .ok_or_else(|| {
                        anyhow::anyhow!("external content directory {path} vanished during capture")
                    })?;
                walk_directory(
                    &child,
                    &path,
                    depth + 1,
                    root_device,
                    excludes,
                    capture_policy,
                    budget,
                    sink,
                    entries,
                    declaration_entries,
                    declaration_bytes,
                    total_bytes,
                )?;
            }
            lillux::secure_fs::PinnedEntryType::Regular => {
                let file = directory
                    .open_regular(OsStr::new(&name), false)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("external content file {path} vanished during capture")
                    })?;
                let metadata = file.metadata()?;
                let size = metadata.len();

                if size > MAX_DECLARATION_FILE_BYTES {
                    anyhow::bail!(
                        "external content file {path} is {size} bytes; the per-file bound is {MAX_DECLARATION_FILE_BYTES}"
                    );
                }

                *declaration_bytes = declaration_bytes.saturating_add(size);
                if *declaration_bytes > MAX_DECLARATION_BYTES {
                    anyhow::bail!(
                        "external content declaration exceeds {MAX_DECLARATION_BYTES} bytes at {path}"
                    );
                }
                budget.charge_bytes(size)?;
                *total_bytes = total_bytes.saturating_add(size);

                let mode = lillux::secure_fs::normalized_portable_regular_mode(&metadata)?;
                // Reading through the sink hashes and stores in one pass; a
                // file mutating mid-read fails there rather than yielding a
                // digest of bytes that never existed together.
                let (blob_hash, stored_size) = sink.store_file(file, &path, size)?;
                if stored_size != size {
                    anyhow::bail!(
                        "external content file {path} changed size during capture; \
                         its manifest entry would not describe its stored bytes"
                    );
                }

                entries.push(ManifestEntry {
                    path,
                    kind: ExternalContentManifestEntryKind::File,
                    mode: Some(mode),
                    blob_hash: Some(blob_hash),
                    size: Some(size),
                    target: None,
                    target_blob: None,
                });
            }
            lillux::secure_fs::PinnedEntryType::Symlink => {
                let target = directory
                    .read_symlink_target(OsStr::new(&name), MAX_SYMLINK_TARGET_BYTES)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("external content symlink {path} vanished during capture")
                    })?;
                if target.is_empty() || target.contains(&0) {
                    anyhow::bail!("external content symlink {path} has an invalid target");
                }
                let (inline, stored) = match String::from_utf8(target.clone()) {
                    Ok(text) if target.len() <= MAX_INLINE_SYMLINK_TARGET_BYTES => {
                        (Some(text), None)
                    }
                    _ => (None, Some(sink.store_target(&target, &path)?)),
                };
                entries.push(ManifestEntry {
                    path,
                    kind: ExternalContentManifestEntryKind::Symlink,
                    mode: None,
                    blob_hash: None,
                    size: None,
                    target: inline,
                    target_blob: stored,
                });
            }
            other => {
                anyhow::bail!(
                    "external content entry {path} is a {other:?}; only files, directories, \
                     and symlinks can be realized"
                );
            }
        }
    }
    Ok(())
}
