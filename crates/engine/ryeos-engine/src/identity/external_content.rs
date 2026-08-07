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
pub use ryeos_state::objects::FILE_REALIZATION_ENTRY_PATH;

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
        let Some(locator) = &declaration.locator else {
            continue;
        };
        if !contract
            .allowed_roots
            .iter()
            .any(|allowed| allowed == locator.root.contract_class())
        {
            anyhow::bail!(
                "external content `{}` names root `{}` which its signed kind does not permit",
                declaration.id,
                locator.root.label()
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
    /// Live source to capture (and, for content-tier pins, to re-verify)
    /// from. Absent only for pinned declarations whose digest resolves to a
    /// large-content manifest: large bytes bind from the store, and there is
    /// no live source to point at honestly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<ExternalContentLocator>,
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
        validate_relative_path("external content mount target", &self.mount)?;
        if ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&self.mount) {
            anyhow::bail!(
                "external content mount target `{}` overlaps a protected project namespace",
                self.mount
            );
        }

        match &self.locator {
            Some(locator) => {
                validate_relative_path("external content locator path", &locator.path)?;
                if !locator.root.declarable_from(declarer) {
                    anyhow::bail!(
                        "an item admitted from {} may not declare external content under {}",
                        declarer.label(),
                        locator.root.label()
                    );
                }
                if let ExternalContentRoot::Bundle(name) = &locator.root
                    && name.is_empty()
                {
                    anyhow::bail!("external content bundle root must name a bundle");
                }
            }
            None => {
                if self.mode != ExternalContentMode::Pinned || self.digest.is_none() {
                    anyhow::bail!(
                        "external content `{}` has no source locator; only a pinned \
                         declaration with a digest may omit one",
                        self.id
                    );
                }
                if !self.exclude.is_empty() {
                    anyhow::bail!(
                        "external content `{}` has no source locator; exclusions apply \
                         to captured sources only",
                        self.id
                    );
                }
            }
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
        let locator = declaration.locator.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "external content `{}` has no source locator; capture policy applies to \
                 captured sources only",
                declaration.id
            )
        })?;
        let locator_prefix = match locator.root {
            ExternalContentRoot::ProjectAi => {
                format!("{}/{}", crate::AI_DIR, locator.path)
            }
            ExternalContentRoot::ProjectFiles
            | ExternalContentRoot::NodeFiles
            | ExternalContentRoot::Bundle(_) => locator.path.clone(),
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Records what reached storage; the walk owns everything else.
    struct RecordingSink {
        stored: Vec<String>,
        lie_size_for: Option<String>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                stored: Vec::new(),
                lie_size_for: None,
            }
        }
    }

    impl ExternalContentBlobSink for RecordingSink {
        fn store_file(
            &mut self,
            mut file: std::fs::File,
            path: &str,
            expected_size: u64,
        ) -> anyhow::Result<(String, u64)> {
            use std::io::Read as _;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            self.stored.push(path.to_string());
            let size = if self.lie_size_for.as_deref() == Some(path) {
                expected_size + 1
            } else {
                bytes.len() as u64
            };
            Ok((lillux::sha256_hex(&bytes), size))
        }

        fn store_target(&mut self, target: &[u8], path: &str) -> anyhow::Result<String> {
            self.stored.push(format!("target:{path}"));
            Ok(lillux::sha256_hex(target))
        }
    }

    fn declaration(path: &str) -> ExternalContentDeclaration {
        serde_json::from_value(serde_json::json!({
            "id": "sim",
            "kind": "tree",
            "locator": {"root": "project_files", "path": path},
            "mode": "captured",
            "mount": "vendor/sim",
        }))
        .expect("test declaration is wire-valid")
    }

    fn pinned(path: &std::path::Path) -> lillux::secure_fs::PinnedDirectory {
        lillux::secure_fs::PinnedDirectory::open(path)
            .expect("open pinned root")
            .expect("declared root exists")
    }

    fn capture(
        root: &std::path::Path,
        excludes: &[String],
        sink: &mut RecordingSink,
    ) -> anyhow::Result<ExternalContentManifestObject> {
        let matcher = ryeos_state::ignore::matcher_from_builtins();
        let policy = ExternalCapturePolicy::for_declaration(&declaration("vendor/sim"), &matcher)?;
        let mut budget = LaunchRealizationBudget::default();
        build_manifest(&pinned(root), excludes, &policy, &mut budget, sink)
    }

    #[test]
    fn floor_excluded_content_never_reaches_capture() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
        std::fs::write(dir.path().join(".env"), b"SECRET=1").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), b"[core]").unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/.env"), b"SECRET=2").unwrap();
        std::fs::write(dir.path().join("nested/ok.txt"), b"ok").unwrap();

        let mut sink = RecordingSink::new();
        let manifest = capture(dir.path(), &[], &mut sink).unwrap();

        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.txt", "nested", "nested/ok.txt"]);
        // The invariant is stronger than manifest shape: excluded bytes must
        // never even be offered to storage.
        assert_eq!(sink.stored, vec!["keep.txt", "nested/ok.txt"]);
        assert_eq!(manifest.total_bytes, 6);
        assert_eq!(manifest.entry_count, 3);
    }

    #[test]
    fn a_locator_inside_the_floor_is_refused() {
        let matcher = ryeos_state::ignore::matcher_from_builtins();
        let error = ExternalCapturePolicy::for_declaration(&declaration(".git"), &matcher)
            .err()
            .expect("a floor locator must refuse");
        assert!(error.to_string().contains("excluded by the admitted capture policy"));
    }

    #[test]
    fn author_excludes_prune_by_name_and_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), b"k").unwrap();
        std::fs::write(dir.path().join("noise.log"), b"n").unwrap();
        std::fs::create_dir(dir.path().join("skip")).unwrap();
        std::fs::write(dir.path().join("skip/inner.rs"), b"i").unwrap();

        let mut sink = RecordingSink::new();
        let manifest = capture(
            dir.path(),
            &["*.log".to_string(), "skip".to_string()],
            &mut sink,
        )
        .unwrap();

        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.rs"]);
        assert_eq!(sink.stored, vec!["keep.rs"]);
    }

    #[test]
    fn entries_are_ordered_and_deterministic_across_creation_order() {
        let build = |names: &[&str]| {
            let dir = tempfile::tempdir().unwrap();
            for name in names {
                std::fs::write(dir.path().join(name), format!("content of {name}")).unwrap();
            }
            let mut sink = RecordingSink::new();
            capture(dir.path(), &[], &mut sink).unwrap()
        };
        let first = build(&["b.txt", "a.txt", "c.txt"]);
        let second = build(&["c.txt", "b.txt", "a.txt"]);
        assert_eq!(first, second);
        assert!(
            first
                .entries
                .windows(2)
                .all(|pair| pair[0].path.as_bytes() < pair[1].path.as_bytes())
        );
        assert_eq!(
            lillux::cas::canonical_json(&serde_json::to_value(&first).unwrap()).unwrap(),
            lillux::cas::canonical_json(&serde_json::to_value(&second).unwrap()).unwrap(),
        );
    }

    #[test]
    fn a_size_lie_from_the_sink_fails_capture() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("volatile.bin"), b"contents").unwrap();

        let mut sink = RecordingSink::new();
        sink.lie_size_for = Some("volatile.bin".to_string());
        let error = capture(dir.path(), &[], &mut sink).unwrap_err();
        assert!(error.to_string().contains("changed size during capture"));
    }

    #[test]
    fn symlinks_are_recorded_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::fs::write(dir.path().join("real/inner.txt"), b"inner").unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("link-dir")).unwrap();
        std::os::unix::fs::symlink("../outside", dir.path().join("escape")).unwrap();
        let long_target = "t".repeat(MAX_INLINE_SYMLINK_TARGET_BYTES + 1);
        std::os::unix::fs::symlink(&long_target, dir.path().join("long")).unwrap();

        let mut sink = RecordingSink::new();
        let manifest = capture(dir.path(), &[], &mut sink).unwrap();

        let entry = |path: &str| {
            manifest
                .entries
                .iter()
                .find(|entry| entry.path == path)
                .unwrap_or_else(|| panic!("manifest records {path}"))
        };
        assert_eq!(entry("escape").target.as_deref(), Some("../outside"));
        assert_eq!(entry("link-dir").target.as_deref(), Some("real"));
        // The directory the symlink names is captured once, as itself; the
        // link is never traversed.
        assert!(!manifest.entries.iter().any(|e| e.path == "link-dir/inner.txt"));
        assert!(manifest.entries.iter().any(|e| e.path == "real/inner.txt"));
        let long = entry("long");
        assert!(long.target.is_none());
        assert!(long.target_blob.is_some());
        assert!(sink.stored.iter().any(|stored| stored == "target:long"));
    }

    #[test]
    fn depth_beyond_the_bound_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for level in 0..=MAX_DECLARATION_DEPTH {
            deep = deep.join(format!("d{level}"));
        }
        std::fs::create_dir_all(&deep).unwrap();

        let mut sink = RecordingSink::new();
        let error = capture(dir.path(), &[], &mut sink).unwrap_err();
        assert!(error.to_string().contains("directory levels"));
    }

    #[test]
    fn non_utf8_names_fail_closed() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = tempfile::tempdir().unwrap();
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff.bin");
        std::fs::write(dir.path().join(bad), b"x").unwrap();

        let mut sink = RecordingSink::new();
        let error = capture(dir.path(), &[], &mut sink).unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn the_launch_budget_is_aggregate_across_declarations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("one.txt"), b"payload").unwrap();

        let matcher = ryeos_state::ignore::matcher_from_builtins();
        let policy =
            ExternalCapturePolicy::for_declaration(&declaration("vendor/sim"), &matcher).unwrap();

        // A previous declaration exhausted the byte budget; this one must
        // fail even though it is well within its own per-declaration bounds.
        let mut budget = LaunchRealizationBudget::default();
        budget.charge_bytes(MAX_LAUNCH_BYTES).unwrap();
        let mut sink = RecordingSink::new();
        let error = build_manifest(&pinned(dir.path()), &[], &policy, &mut budget, &mut sink)
            .unwrap_err();
        assert!(error.to_string().contains("aggregate bytes"));

        let mut budget = LaunchRealizationBudget::default();
        for _ in 0..MAX_LAUNCH_ENTRIES {
            budget.charge_entry().unwrap();
        }
        let mut sink = RecordingSink::new();
        let error = build_manifest(&pinned(dir.path()), &[], &policy, &mut budget, &mut sink)
            .unwrap_err();
        assert!(error.to_string().contains("aggregate entries"));
    }

    // ── Composed-value declarations (the extends-chain product) ─────────

    fn contract(
        allowed_roots: &[&str],
        max_declarations: usize,
    ) -> crate::kind_registry::ExecutionExternalContentDecl {
        crate::kind_registry::ExecutionExternalContentDecl {
            realization_derived: EXTERNAL_REALIZATIONS_DERIVED_KEY.to_string(),
            allowed_roots: allowed_roots.iter().map(|root| root.to_string()).collect(),
            max_declarations,
            large_content: None,
        }
    }

    fn composed_with(declarations: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "external_content": declarations })
    }

    fn tree_declaration(id: &str, root: &str, mount: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "kind": "tree",
            "locator": {"root": root, "path": "vendor/sim"},
            "mode": "captured",
            "mount": mount,
        })
    }

    #[test]
    fn composed_absence_and_an_empty_list_remain_distinct() {
        let contract = contract(&["project_files"], 4);
        // Absence: the item never declared the field.
        assert!(
            declarations_from_composed(
                &serde_json::json!({}),
                Some(&contract),
                DeclaringAuthority::Project,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            declarations_from_composed(&serde_json::json!({}), None, DeclaringAuthority::Project)
                .unwrap()
                .is_none()
        );
        // An explicit empty list is a positive declaration of no content.
        assert_eq!(
            declarations_from_composed(
                &composed_with(serde_json::json!([])),
                Some(&contract),
                DeclaringAuthority::Project,
            )
            .unwrap(),
            Some(Vec::new())
        );
        // Null is neither: it must fail rather than be read as either form.
        assert!(
            declarations_from_composed(
                &composed_with(serde_json::Value::Null),
                Some(&contract),
                DeclaringAuthority::Project,
            )
            .is_err()
        );
    }

    #[test]
    fn authoring_without_a_kind_contract_is_refused() {
        let error = declarations_from_composed(
            &composed_with(serde_json::json!([])),
            None,
            DeclaringAuthority::Project,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("no execution.external_content contract"),
            "got {error:#}"
        );
    }

    #[test]
    fn a_pinned_declaration_parses_through_the_composed_value() {
        let contract = contract(&["project_files"], 4);
        let digest = "a".repeat(64);
        let declarations = declarations_from_composed(
            &composed_with(serde_json::json!([{
                "id": "sim",
                "kind": "tree",
                "locator": {"root": "project_files", "path": "vendor/sim"},
                "mode": "pinned",
                "digest": digest,
                "mount": "vendor/sim",
            }])),
            Some(&contract),
            DeclaringAuthority::Project,
        )
        .unwrap()
        .unwrap();
        assert_eq!(declarations.len(), 1);
        let declaration = &declarations[0];
        assert_eq!(declaration.id, "sim");
        assert_eq!(declaration.kind, ExternalContentKind::Tree);
        assert_eq!(declaration.mode, ExternalContentMode::Pinned);
        assert_eq!(declaration.digest.as_deref(), Some(digest.as_str()));
        assert_eq!(
            declaration.locator.as_ref().unwrap().root.label(),
            "project_files"
        );
        assert_eq!(declaration.mount, "vendor/sim");
    }

    #[test]
    fn the_contract_bounds_declaration_count_and_roots() {
        let error = declarations_from_composed(
            &composed_with(serde_json::json!([
                tree_declaration("one", "project_files", "vendor/one"),
                tree_declaration("two", "project_files", "vendor/two"),
            ])),
            Some(&contract(&["project_files"], 1)),
            DeclaringAuthority::Project,
        )
        .unwrap_err();
        assert!(error.to_string().contains("permits"), "got {error:#}");

        let error = declarations_from_composed(
            &composed_with(serde_json::json!([tree_declaration(
                "sim",
                "project_files",
                "vendor/sim"
            )])),
            Some(&contract(&["project_ai"], 4)),
            DeclaringAuthority::Project,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("does not permit"),
            "got {error:#}"
        );
    }

    #[test]
    fn declarer_authority_gates_roots_before_the_contract() {
        // A project-admitted item may not direct the daemon at node space,
        // even when the kind contract would allow the class.
        let error = declarations_from_composed(
            &composed_with(serde_json::json!([tree_declaration(
                "cfg",
                "node_files",
                "vendor/cfg"
            )])),
            Some(&contract(&["node_files"], 4)),
            DeclaringAuthority::Project,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("may not declare"),
            "got {error:#}"
        );

        // A bundle may name only itself.
        let foreign = declarations_from_composed(
            &composed_with(serde_json::json!([tree_declaration(
                "lib",
                "bundle:other",
                "vendor/lib"
            )])),
            Some(&contract(&["bundle:own"], 4)),
            DeclaringAuthority::Bundle("own"),
        )
        .unwrap_err();
        assert!(
            foreign.to_string().contains("may not declare"),
            "got {foreign:#}"
        );
        let own = declarations_from_composed(
            &composed_with(serde_json::json!([tree_declaration(
                "lib",
                "bundle:own",
                "vendor/lib"
            )])),
            Some(&contract(&["bundle:own"], 4)),
            DeclaringAuthority::Bundle("own"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(own.len(), 1);
    }
}
