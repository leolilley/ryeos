//! Generic CAS object-graph closure collection.
//!
//! This module owns schema-defined traversal from CAS object roots to the
//! transitive set of reachable object and blob hashes. It intentionally
//! does not discover roots from refs; callers provide root object hashes.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const DEFAULT_MAX_OBJECTS: usize = 10_000;
const DEFAULT_MAX_BLOBS: usize = 10_000;
const DEFAULT_MAX_OBJECT_BYTES: u64 = 1024 * 1024;
const DEFAULT_MAX_BLOB_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BLOB_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_LINKS_PER_OBJECT: usize = 10_000;

/// Transitive closure for one or more CAS object roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectClosureReport {
    /// Root object hashes requested by the caller.
    pub roots: Vec<String>,
    /// All reachable JSON object hashes, including roots that are present
    /// or referenced even if their object body is missing/malformed.
    pub object_hashes: BTreeSet<String>,
    /// All reachable blob hashes.
    pub blob_hashes: BTreeSet<String>,
    /// Blob hashes referenced by reachable objects but absent from CAS.
    pub missing_blobs: Vec<MissingDependency>,
    /// Object hashes that were referenced but not present in CAS.
    pub missing_objects: Vec<MissingDependency>,
    /// Object hashes whose JSON body or schema-defined edges were malformed.
    pub malformed_objects: Vec<MalformedObject>,
    /// Objects with a kind this collector does not know how to traverse.
    pub unsupported_objects: Vec<UnsupportedObjectKind>,
}

impl ObjectClosureReport {
    pub fn is_complete(&self) -> bool {
        self.missing_objects.is_empty()
            && self.missing_blobs.is_empty()
            && self.malformed_objects.is_empty()
            && self.unsupported_objects.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDependency {
    pub hash: String,
    pub referenced_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedObject {
    pub hash: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedObjectKind {
    pub hash: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectLinks {
    pub object_hashes: Vec<String>,
    pub blob_hashes: Vec<String>,
    pub unsupported_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ExpectedObject {
    Any,
    Kind(&'static str),
    ItemSource { item_ref: String },
}

#[derive(Debug, Clone)]
struct ObjectEdge {
    hash: String,
    expected: ExpectedObject,
    history_graph: Option<HistoryGraph>,
}

#[derive(Debug, Clone)]
struct LoadedObjectIdentity {
    kind: String,
    item_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HistoryGraph {
    ProjectSnapshotParents,
    ChainStatePredecessors,
    ThreadEventChainPredecessors,
    ThreadEventThreadPredecessors,
    BundleEventPredecessors,
}

impl HistoryGraph {
    fn label(self) -> &'static str {
        match self {
            Self::ProjectSnapshotParents => "project_snapshot parent DAG",
            Self::ChainStatePredecessors => "chain_state predecessor chain",
            Self::ThreadEventChainPredecessors => "thread_event chain predecessor graph",
            Self::ThreadEventThreadPredecessors => "thread_event thread predecessor graph",
            Self::BundleEventPredecessors => "bundle_event predecessor chain",
        }
    }
}

impl ExpectedObject {
    fn validate(&self, identity: &LoadedObjectIdentity) -> Result<(), String> {
        match self {
            Self::Any => Ok(()),
            Self::Kind(expected) if identity.kind == *expected => Ok(()),
            Self::Kind(expected) => Err(format!(
                "object edge expected kind {expected}, got {}",
                identity.kind
            )),
            Self::ItemSource { item_ref }
                if identity.kind == "item_source"
                    && identity.item_ref.as_deref() == Some(item_ref.as_str()) =>
            {
                Ok(())
            }
            Self::ItemSource { item_ref } if identity.kind != "item_source" => Err(format!(
                "source_manifest entry {item_ref:?} expected kind item_source, got {}",
                identity.kind
            )),
            Self::ItemSource { item_ref } => Err(format!(
                "source_manifest key {item_ref:?} does not match embedded item_source item_ref {:?}",
                identity.item_ref.as_deref().unwrap_or("<missing>")
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectClosureLimits {
    pub max_objects: usize,
    pub max_blobs: usize,
    pub max_object_bytes: u64,
    pub max_blob_bytes: u64,
    pub max_total_blob_bytes: u64,
    pub max_links_per_object: usize,
}

/// Load one CAS JSON object by its exact requested identity.
///
/// This is the shared authority boundary for consumers that act on a CAS
/// object directly rather than through a complete closure report. It rejects
/// symlinks, bodies stored under the wrong hash, and semantically equivalent
/// but non-canonical JSON encodings.
pub fn load_exact_cas_object(
    cas_root: &Path,
    requested_hash: &str,
    max_bytes: u64,
) -> anyhow::Result<Value> {
    if !lillux::valid_hash(requested_hash)
        || requested_hash.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        anyhow::bail!("invalid requested CAS object hash {requested_hash}");
    }
    let bytes = read_cas_file_no_follow(cas_root, "objects", requested_hash, ".json", max_bytes)?
        .ok_or_else(|| anyhow::anyhow!("CAS object {requested_hash} is missing"))?;
    let actual_hash = lillux::sha256_hex(&bytes);
    if actual_hash != requested_hash {
        anyhow::bail!(
            "CAS object hash mismatch: requested {requested_hash}, bytes hash to {actual_hash}"
        );
    }
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse CAS object {requested_hash}"))?;
    let canonical = lillux::canonical_json(&value)
        .with_context(|| format!("failed to canonicalize CAS object {requested_hash}"))?;
    if canonical.as_bytes() != bytes.as_slice() {
        anyhow::bail!("CAS object {requested_hash} is not stored as canonical JSON bytes");
    }
    Ok(value)
}

/// Load one exact object through an already-selected CAS authority.
pub fn load_exact_cas_object_with_cas(
    cas: &lillux::CasStore,
    requested_hash: &str,
    max_bytes: u64,
) -> anyhow::Result<Value> {
    if !is_canonical_hash(requested_hash) {
        anyhow::bail!("invalid requested CAS object hash {requested_hash}");
    }
    let value = cas
        .get_object(requested_hash)?
        .ok_or_else(|| anyhow::anyhow!("CAS object {requested_hash} is missing"))?;
    let canonical = lillux::canonical_json(&value)
        .with_context(|| format!("failed to canonicalize CAS object {requested_hash}"))?;
    let byte_len = u64::try_from(canonical.len()).unwrap_or(u64::MAX);
    if byte_len > max_bytes {
        anyhow::bail!("CAS object {requested_hash} exceeds byte limit: {byte_len} > {max_bytes}");
    }
    Ok(value)
}

/// Load one CAS blob by its exact requested identity without following links.
pub fn load_exact_cas_blob(
    cas_root: &Path,
    requested_hash: &str,
    max_bytes: u64,
) -> anyhow::Result<Vec<u8>> {
    if !lillux::valid_hash(requested_hash)
        || requested_hash.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        anyhow::bail!("invalid requested CAS blob hash {requested_hash}");
    }
    let bytes = read_cas_file_no_follow(cas_root, "blobs", requested_hash, "", max_bytes)?
        .ok_or_else(|| anyhow::anyhow!("CAS blob {requested_hash} is missing"))?;
    let actual_hash = lillux::sha256_hex(&bytes);
    if actual_hash != requested_hash {
        anyhow::bail!(
            "CAS blob hash mismatch: requested {requested_hash}, bytes hash to {actual_hash}"
        );
    }
    Ok(bytes)
}

/// Load one exact blob through an already-selected CAS authority.
pub fn load_exact_cas_blob_with_cas(
    cas: &lillux::CasStore,
    requested_hash: &str,
    max_bytes: u64,
) -> anyhow::Result<Vec<u8>> {
    if !is_canonical_hash(requested_hash) {
        anyhow::bail!("invalid requested CAS blob hash {requested_hash}");
    }
    let bytes = cas
        .get_blob(requested_hash)?
        .ok_or_else(|| anyhow::anyhow!("CAS blob {requested_hash} is missing"))?;
    let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if byte_len > max_bytes {
        anyhow::bail!("CAS blob {requested_hash} exceeds byte limit: {byte_len} > {max_bytes}");
    }
    Ok(bytes)
}

impl Default for ObjectClosureLimits {
    fn default() -> Self {
        Self {
            max_objects: DEFAULT_MAX_OBJECTS,
            max_blobs: DEFAULT_MAX_BLOBS,
            max_object_bytes: DEFAULT_MAX_OBJECT_BYTES,
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            max_total_blob_bytes: DEFAULT_MAX_TOTAL_BLOB_BYTES,
            max_links_per_object: DEFAULT_MAX_LINKS_PER_OBJECT,
        }
    }
}

impl ObjectClosureLimits {
    /// Full-project generations may legitimately contain model weights and
    /// datasets larger than the generic control-plane defaults. Counts remain
    /// bounded; byte totals are accounted while transport streams each blob.
    pub fn for_project_snapshot_transport() -> Self {
        Self {
            max_objects: 100_000,
            max_blobs: 100_000,
            max_object_bytes: 32 * 1024 * 1024,
            max_blob_bytes: 16 * 1024 * 1024 * 1024,
            max_total_blob_bytes: 64 * 1024 * 1024 * 1024,
            max_links_per_object: 100_000,
        }
    }

    pub fn unbounded_for_local_maintenance() -> Self {
        Self {
            max_objects: usize::MAX,
            max_blobs: usize::MAX,
            max_object_bytes: u64::MAX,
            max_blob_bytes: u64::MAX,
            max_total_blob_bytes: u64::MAX,
            max_links_per_object: usize::MAX,
        }
    }
}

/// Read one sharded CAS file without following symlinks. Every component below
/// the supplied CAS root is opened descriptor-relative with `O_NOFOLLOW`, and
/// the final descriptor must be a regular file.
pub(crate) fn read_cas_file_no_follow(
    cas_root: &Path,
    namespace: &str,
    hash: &str,
    extension: &str,
    max_bytes: u64,
) -> anyhow::Result<Option<Vec<u8>>> {
    let path = lillux::shard_path(cas_root, namespace, hash, extension);
    let relative = path
        .strip_prefix(cas_root)
        .context("sharded CAS path escaped CAS root")?;
    let mut components = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            anyhow::bail!("sharded CAS path has unsafe component: {}", path.display());
        };
        components.push(component.to_os_string());
    }
    if components.is_empty() {
        anyhow::bail!("CAS file path is empty");
    }
    let Some(mut directory) = lillux::PinnedDirectory::open(cas_root)? else {
        return Ok(None);
    };
    for component in &components[..components.len() - 1] {
        let Some(child) = directory.open_child_directory(component)? else {
            return Ok(None);
        };
        directory = child;
    }
    let final_name = components
        .last()
        .expect("non-empty CAS path has a final component");
    let Some(file) = directory.open_regular(final_name, false)? else {
        return Ok(None);
    };
    let metadata = file.metadata().context("inspect opened CAS entry")?;
    if metadata.len() > max_bytes {
        anyhow::bail!(
            "CAS entry {} exceeds byte limit: {} > {}",
            path.display(),
            metadata.len(),
            max_bytes
        );
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read CAS entry {}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        anyhow::bail!(
            "CAS entry {} exceeded byte limit while reading",
            path.display()
        );
    }
    Ok(Some(bytes))
}

/// Collect the schema-defined object/blob closure reachable from `roots`.
pub fn collect_object_closure(
    cas_root: &Path,
    roots: impl IntoIterator<Item = String>,
) -> anyhow::Result<ObjectClosureReport> {
    let mut check = || Ok(());
    collect_object_closure_with_limits_and_check(
        cas_root,
        roots,
        ObjectClosureLimits::unbounded_for_local_maintenance(),
        &mut check,
    )
}

pub(crate) fn collect_object_closure_with_cas_and_check(
    cas: &lillux::CasStore,
    roots: impl IntoIterator<Item = String>,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<ObjectClosureReport> {
    collect_object_closure_from_source(
        ClosureCas::Pinned(cas),
        roots,
        ObjectClosureLimits::unbounded_for_local_maintenance(),
        check,
    )
}

/// Collect an unbounded closure from an already-pinned CAS authority.
///
/// Long-running authority-sensitive operations must not turn a retained CAS
/// descriptor back into a pathname between individual object reads.
pub(crate) fn collect_object_closure_with_cas(
    cas: &lillux::CasStore,
    roots: impl IntoIterator<Item = String>,
) -> anyhow::Result<ObjectClosureReport> {
    let mut check = || Ok(());
    collect_object_closure_with_cas_and_check(cas, roots, &mut check)
}

/// Collect the schema-defined object/blob closure reachable from `roots`,
/// aborting once more than `max_objects` object hashes would be visited.
pub fn collect_object_closure_with_limit(
    cas_root: &Path,
    roots: impl IntoIterator<Item = String>,
    max_objects: Option<usize>,
) -> anyhow::Result<ObjectClosureReport> {
    let mut limits = ObjectClosureLimits::unbounded_for_local_maintenance();
    if let Some(max_objects) = max_objects {
        limits.max_objects = max_objects;
    }
    collect_object_closure_with_limits(cas_root, roots, limits)
}

/// Collect the schema-defined object/blob closure reachable from `roots`,
/// enforcing object-count, per-object-byte, and per-object-link limits.
pub fn collect_object_closure_with_limits(
    cas_root: &Path,
    roots: impl IntoIterator<Item = String>,
    limits: ObjectClosureLimits,
) -> anyhow::Result<ObjectClosureReport> {
    let mut check = || Ok(());
    collect_object_closure_with_limits_and_check(cas_root, roots, limits, &mut check)
}

/// Collect a bounded closure from one already-selected CAS authority.
/// Callers can retain the same store for subsequently exporting the reported
/// objects and blobs, so traversal and payload reads cannot observe different
/// runtime roots.
pub fn collect_object_closure_with_cas_and_limits(
    cas: &lillux::CasStore,
    roots: impl IntoIterator<Item = String>,
    limits: ObjectClosureLimits,
) -> anyhow::Result<ObjectClosureReport> {
    let mut check = || Ok(());
    collect_object_closure_from_source(ClosureCas::Pinned(cas), roots, limits, &mut check)
}

fn collect_object_closure_with_limits_and_check(
    cas_root: &Path,
    roots: impl IntoIterator<Item = String>,
    limits: ObjectClosureLimits,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<ObjectClosureReport> {
    collect_object_closure_from_source(ClosureCas::Path(cas_root), roots, limits, check)
}

#[derive(Clone, Copy)]
enum ClosureCas<'a> {
    Path(&'a Path),
    Pinned(&'a lillux::CasStore),
}

impl ClosureCas<'_> {
    fn read_object(self, hash: &str, max_bytes: u64) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Self::Path(root) => read_cas_file_no_follow(root, "objects", hash, ".json", max_bytes),
            Self::Pinned(cas) => {
                let Some((file, size)) = cas.open_object(hash)? else {
                    return Ok(None);
                };
                if size > max_bytes {
                    anyhow::bail!("CAS object {hash} exceeds byte limit {max_bytes}");
                }
                let mut bytes = Vec::with_capacity(usize::try_from(size)?);
                file.take(max_bytes.saturating_add(1))
                    .read_to_end(&mut bytes)?;
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
                    anyhow::bail!("CAS object {hash} exceeded byte limit while reading");
                }
                Ok(Some(bytes))
            }
        }
    }

    fn inspect_blob(self, hash: &str, max_bytes: u64) -> anyhow::Result<Option<(u64, String)>> {
        let (mut file, size) = match self {
            Self::Path(root) => {
                let path = lillux::shard_path(root, "blobs", hash, "");
                let relative = path
                    .strip_prefix(root)
                    .context("blob path escaped CAS root")?;
                let components = relative
                    .components()
                    .map(|component| match component {
                        std::path::Component::Normal(value) => Ok(value.to_os_string()),
                        _ => anyhow::bail!("CAS blob path has unsafe component"),
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let Some(mut directory) = lillux::PinnedDirectory::open(root)? else {
                    return Ok(None);
                };
                for component in &components[..components.len().saturating_sub(1)] {
                    let Some(child) = directory.open_child_directory(component)? else {
                        return Ok(None);
                    };
                    directory = child;
                }
                let Some(name) = components.last() else {
                    anyhow::bail!("CAS blob path is empty");
                };
                let Some(file) = directory.open_regular(name, false)? else {
                    return Ok(None);
                };
                let size = file.metadata()?.len();
                (file, size)
            }
            Self::Pinned(cas) => {
                let Some(opened) = cas.open_blob(hash)? else {
                    return Ok(None);
                };
                opened
            }
        };
        if size > max_bytes {
            anyhow::bail!("CAS blob {hash} exceeds byte limit {max_bytes}");
        }
        let mut digest = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(u64::try_from(read)?);
            if total > max_bytes {
                anyhow::bail!("CAS blob {hash} exceeded byte limit while reading");
            }
            digest.update(&buffer[..read]);
        }
        if total != size {
            anyhow::bail!("CAS blob {hash} changed size while being verified");
        }
        Ok(Some((total, format!("{:x}", digest.finalize()))))
    }
}

fn collect_object_closure_from_source(
    cas: ClosureCas<'_>,
    roots: impl IntoIterator<Item = String>,
    limits: ObjectClosureLimits,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<ObjectClosureReport> {
    let mut report = ObjectClosureReport::default();
    let mut queue: VecDeque<(String, Option<String>, ExpectedObject)> = VecDeque::new();
    let mut loaded_identities = HashMap::<String, LoadedObjectIdentity>::new();
    let mut history_edges = BTreeMap::<HistoryGraph, BTreeMap<String, BTreeSet<String>>>::new();
    let mut total_blob_bytes = 0_u64;

    for root in roots {
        report.roots.push(root.clone());
        queue.push_back((root, None, ExpectedObject::Any));
    }

    while let Some((hash, referenced_by, expected)) = queue.pop_front() {
        check()?;
        if !is_canonical_hash(&hash) {
            report.malformed_objects.push(MalformedObject {
                hash,
                reason: "invalid object hash".to_string(),
            });
            continue;
        }

        if let Some(identity) = loaded_identities.get(&hash) {
            if let Err(reason) = expected.validate(identity) {
                report
                    .malformed_objects
                    .push(MalformedObject { hash, reason });
            }
            continue;
        }

        if !report.object_hashes.insert(hash.clone()) {
            continue;
        }

        if report.object_hashes.len() > limits.max_objects {
            anyhow::bail!(
                "object closure exceeds max_objects: {} > {}",
                report.object_hashes.len(),
                limits.max_objects
            );
        }

        let content = match cas.read_object(&hash, limits.max_object_bytes)? {
            Some(content) => content,
            None => {
                report.missing_objects.push(MissingDependency {
                    hash,
                    referenced_by,
                });
                continue;
            }
        };
        let actual_hash = lillux::sha256_hex(&content);
        if actual_hash != hash {
            let reason = format!("object bytes hash mismatch: requested {hash}, got {actual_hash}");
            report
                .malformed_objects
                .push(MalformedObject { hash, reason });
            continue;
        }
        let value: Value = match serde_json::from_slice(&content) {
            Ok(value) => value,
            Err(err) => {
                report.malformed_objects.push(MalformedObject {
                    hash,
                    reason: format!("invalid JSON: {err}"),
                });
                continue;
            }
        };
        let canonical = match lillux::canonical_json(&value) {
            Ok(canonical) => canonical,
            Err(error) => {
                report.malformed_objects.push(MalformedObject {
                    hash,
                    reason: format!("object cannot be canonicalized: {error}"),
                });
                continue;
            }
        };
        if canonical.as_bytes() != content.as_slice() {
            report.malformed_objects.push(MalformedObject {
                hash,
                reason: "object is not stored as canonical JSON bytes".to_string(),
            });
            continue;
        }

        if let Err(reason) = validate_current_object(&value) {
            report
                .malformed_objects
                .push(MalformedObject { hash, reason });
            continue;
        }

        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .expect("validate_current_object requires a string kind")
            .to_string();
        let identity = LoadedObjectIdentity {
            item_ref: (kind == "item_source").then(|| {
                value
                    .get("item_ref")
                    .and_then(Value::as_str)
                    .expect("validated item_source has item_ref")
                    .to_string()
            }),
            kind,
        };
        loaded_identities.insert(hash.clone(), identity.clone());
        if let Err(reason) = expected.validate(&identity) {
            report
                .malformed_objects
                .push(MalformedObject { hash, reason });
            continue;
        }

        let links = match object_links(&value) {
            Ok(links) => links,
            Err(reason) => {
                report
                    .malformed_objects
                    .push(MalformedObject { hash, reason });
                continue;
            }
        };

        let object_edges = match typed_object_edges(&value) {
            Ok(edges) => edges,
            Err(reason) => {
                report
                    .malformed_objects
                    .push(MalformedObject { hash, reason });
                continue;
            }
        };

        let link_count = object_edges.len().saturating_add(links.blob_hashes.len());
        if link_count > limits.max_links_per_object {
            anyhow::bail!(
                "object {hash} exceeds max_links_per_object: {} > {}",
                link_count,
                limits.max_links_per_object
            );
        }

        if let Some(kind) = links.unsupported_kind {
            report
                .unsupported_objects
                .push(UnsupportedObjectKind { hash, kind });
            continue;
        }

        for edge in object_edges {
            if let Some(graph) = edge.history_graph {
                history_edges
                    .entry(graph)
                    .or_default()
                    .entry(hash.clone())
                    .or_default()
                    .insert(edge.hash.clone());
            }
            queue.push_back((edge.hash, Some(hash.clone()), edge.expected));
        }
        let project_file_size = if identity.kind == "project_file" {
            Some(
                crate::objects::ProjectFile::from_value(&value)
                    .map_err(|error| anyhow::anyhow!(error))?
                    .size,
            )
        } else {
            None
        };
        for blob in links.blob_hashes {
            check()?;
            if is_canonical_hash(&blob) {
                match cas
                    .inspect_blob(&blob, limits.max_blob_bytes)
                    .with_context(|| {
                        format!(
                            "enforce max_blob_bytes={} for referenced blob {blob}",
                            limits.max_blob_bytes
                        )
                    })? {
                    Some((actual_size, actual_hash)) => {
                        if let Some(expected_size) = project_file_size {
                            if actual_size != expected_size {
                                report.malformed_objects.push(MalformedObject {
                                    hash: hash.clone(),
                                    reason: format!(
                                        "project_file declares size {expected_size}, but blob {blob} has size {actual_size}"
                                    ),
                                });
                                continue;
                            }
                        }
                        if !report.blob_hashes.contains(&blob) {
                            if report.blob_hashes.len() + 1 > limits.max_blobs {
                                anyhow::bail!(
                                    "object closure exceeds max_blobs: {} > {}",
                                    report.blob_hashes.len() + 1,
                                    limits.max_blobs
                                );
                            }
                            total_blob_bytes = total_blob_bytes.saturating_add(actual_size);
                            if total_blob_bytes > limits.max_total_blob_bytes {
                                anyhow::bail!(
                                    "object closure exceeds max_total_blob_bytes: {} > {}",
                                    total_blob_bytes,
                                    limits.max_total_blob_bytes
                                );
                            }
                        }
                        if actual_hash != blob {
                            report.malformed_objects.push(MalformedObject {
                                hash: hash.clone(),
                                reason: format!(
                                    "referenced blob {blob} bytes hash to {actual_hash}"
                                ),
                            });
                            continue;
                        }
                    }
                    None => {
                        report.missing_blobs.push(MissingDependency {
                            hash: blob,
                            referenced_by: Some(hash.clone()),
                        });
                        continue;
                    }
                }
                report.blob_hashes.insert(blob);
            } else {
                report.malformed_objects.push(MalformedObject {
                    hash: hash.clone(),
                    reason: format!("invalid blob hash: {blob}"),
                });
            }
        }
    }

    for (graph, edges) in history_edges {
        if let Some(hash) = cyclic_graph_member(&edges) {
            report.malformed_objects.push(MalformedObject {
                hash,
                reason: format!("{} contains a cycle", graph.label()),
            });
        }
    }

    report.missing_objects.sort_by(|a, b| a.hash.cmp(&b.hash));
    report.missing_blobs.sort_by(|a, b| a.hash.cmp(&b.hash));
    report
        .malformed_objects
        .sort_by(|a, b| (&a.hash, &a.reason).cmp(&(&b.hash, &b.reason)));
    report.malformed_objects.dedup();
    report
        .unsupported_objects
        .sort_by(|a, b| a.hash.cmp(&b.hash));
    Ok(report)
}

fn cyclic_graph_member(edges: &BTreeMap<String, BTreeSet<String>>) -> Option<String> {
    let mut nodes = BTreeSet::new();
    for (source, targets) in edges {
        nodes.insert(source.clone());
        nodes.extend(targets.iter().cloned());
    }
    let mut incoming = nodes
        .iter()
        .cloned()
        .map(|node| (node, 0usize))
        .collect::<BTreeMap<_, _>>();
    for targets in edges.values() {
        for target in targets {
            *incoming
                .get_mut(target)
                .expect("all history targets were inserted") += 1;
        }
    }
    let mut ready = incoming
        .iter()
        .filter_map(|(node, count)| (*count == 0).then_some(node.clone()))
        .collect::<BTreeSet<_>>();
    let mut visited = 0usize;
    while let Some(node) = ready.pop_first() {
        visited += 1;
        if let Some(targets) = edges.get(&node) {
            for target in targets {
                let count = incoming
                    .get_mut(target)
                    .expect("all history targets have an incoming count");
                *count -= 1;
                if *count == 0 {
                    ready.insert(target.clone());
                }
            }
        }
    }
    if visited == nodes.len() {
        None
    } else {
        incoming
            .into_iter()
            .find_map(|(node, count)| (count > 0).then_some(node))
    }
}

fn typed_object_edges(value: &Value) -> Result<Vec<ObjectEdge>, String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing object kind".to_string())?;
    let mut edges = Vec::new();
    match kind {
        "attestation" => {
            push_required_object_edge(value, "subject_hash", ExpectedObject::Any, None, &mut edges)?
        }
        "chain_state" => {
            push_optional_object_edge(
                value,
                "prev_chain_state_hash",
                ExpectedObject::Kind("chain_state"),
                Some(HistoryGraph::ChainStatePredecessors),
                &mut edges,
            )?;
            push_optional_object_edge(
                value,
                "last_event_hash",
                ExpectedObject::Kind("thread_event"),
                None,
                &mut edges,
            )?;
            let threads = value
                .get("threads")
                .and_then(Value::as_object)
                .ok_or_else(|| "chain_state missing threads object".to_string())?;
            for entry in threads.values() {
                push_required_object_edge(
                    entry,
                    "snapshot_hash",
                    ExpectedObject::Kind("thread_snapshot"),
                    None,
                    &mut edges,
                )?;
                push_optional_object_edge(
                    entry,
                    "last_event_hash",
                    ExpectedObject::Kind("thread_event"),
                    None,
                    &mut edges,
                )?;
            }
        }
        "thread_snapshot" => {
            for field in ["base_project_snapshot_hash", "result_project_snapshot_hash"] {
                push_optional_object_edge(
                    value,
                    field,
                    ExpectedObject::Kind("project_snapshot"),
                    None,
                    &mut edges,
                )?;
            }
            push_optional_object_edge(
                value,
                "last_event_hash",
                ExpectedObject::Kind("thread_event"),
                None,
                &mut edges,
            )?;
        }
        "thread_event" => {
            push_optional_object_edge(
                value,
                "prev_chain_event_hash",
                ExpectedObject::Kind("thread_event"),
                Some(HistoryGraph::ThreadEventChainPredecessors),
                &mut edges,
            )?;
            push_optional_object_edge(
                value,
                "prev_thread_event_hash",
                ExpectedObject::Kind("thread_event"),
                Some(HistoryGraph::ThreadEventThreadPredecessors),
                &mut edges,
            )?;
        }
        "bundle_event" => push_optional_object_edge(
            value,
            "prev_chain_event_hash",
            ExpectedObject::Kind("bundle_event"),
            Some(HistoryGraph::BundleEventPredecessors),
            &mut edges,
        )?,
        "project_snapshot" => {
            push_required_object_edge(
                value,
                "project_tree_hash",
                ExpectedObject::Kind("project_tree"),
                None,
                &mut edges,
            )?;
            push_required_object_edge(
                value,
                "effective_policy_hash",
                ExpectedObject::Kind("project_snapshot_policy"),
                None,
                &mut edges,
            )?;
            let parents = value
                .get("parent_hashes")
                .and_then(Value::as_array)
                .ok_or_else(|| "project_snapshot missing parent_hashes array".to_string())?;
            for parent in parents {
                let hash = parent.as_str().ok_or_else(|| {
                    "project_snapshot parent_hashes contains non-string".to_string()
                })?;
                push_typed_hash(
                    hash,
                    ExpectedObject::Kind("project_snapshot"),
                    Some(HistoryGraph::ProjectSnapshotParents),
                    &mut edges,
                )?;
            }
        }
        "source_manifest" => {
            let hashes = value
                .get("item_source_hashes")
                .and_then(Value::as_object)
                .ok_or_else(|| "source_manifest missing item_source_hashes object".to_string())?;
            for (item_ref, hash) in hashes {
                let hash = hash.as_str().ok_or_else(|| {
                    "source_manifest item_source_hashes contains non-string".to_string()
                })?;
                push_typed_hash(
                    hash,
                    ExpectedObject::ItemSource {
                        item_ref: item_ref.clone(),
                    },
                    None,
                    &mut edges,
                )?;
            }
        }
        "project_tree" => {
            let hashes = value
                .get("files")
                .and_then(Value::as_object)
                .ok_or_else(|| "project_tree missing files object".to_string())?;
            for hash in hashes.values() {
                let hash = hash
                    .as_str()
                    .ok_or_else(|| "project_tree files contains non-string".to_string())?;
                push_typed_hash(hash, ExpectedObject::Kind("project_file"), None, &mut edges)?;
            }
        }
        "project_file" | "project_snapshot_policy" => {}
        "item_source" => {}
        _ => return Ok(Vec::new()),
    }
    edges.sort_by(|left, right| {
        (&left.hash, &left.expected, &left.history_graph).cmp(&(
            &right.hash,
            &right.expected,
            &right.history_graph,
        ))
    });
    edges.dedup_by(|left, right| {
        left.hash == right.hash
            && left.expected == right.expected
            && left.history_graph == right.history_graph
    });
    Ok(edges)
}

fn push_required_object_edge(
    value: &Value,
    field: &str,
    expected: ExpectedObject,
    history_graph: Option<HistoryGraph>,
    out: &mut Vec<ObjectEdge>,
) -> Result<(), String> {
    let hash = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required hash field {field}"))?;
    push_typed_hash(hash, expected, history_graph, out)
}

fn push_optional_object_edge(
    value: &Value,
    field: &str,
    expected: ExpectedObject,
    history_graph: Option<HistoryGraph>,
    out: &mut Vec<ObjectEdge>,
) -> Result<(), String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(hash)) => push_typed_hash(hash, expected, history_graph, out),
        Some(_) => Err(format!(
            "optional hash field {field} is not a string or null"
        )),
    }
}

fn push_typed_hash(
    hash: &str,
    expected: ExpectedObject,
    history_graph: Option<HistoryGraph>,
    out: &mut Vec<ObjectEdge>,
) -> Result<(), String> {
    if !is_canonical_hash(hash) {
        return Err(format!("invalid hash: {hash}"));
    }
    out.push(ObjectEdge {
        hash: hash.to_string(),
        expected,
        history_graph,
    });
    Ok(())
}

/// Validate one current CAS object using the same typed wire model and
/// invariant checks as its authoritative reader. Link extraction alone is not
/// validation: it must not make an old-schema or partially typed object a GC
/// root merely because a few hash-shaped fields can be found.
fn validate_current_object(value: &Value) -> Result<(), String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing object kind".to_string())?;
    let result: anyhow::Result<()> = match kind {
        "attestation" => crate::objects::Attestation::from_value(value).map(|_| ()),
        "chain_state" => serde_json::from_value::<crate::objects::ChainState>(value.clone())
            .context("deserialize chain_state")
            .and_then(|object| object.validate()),
        "thread_snapshot" => {
            serde_json::from_value::<crate::objects::ThreadSnapshot>(value.clone())
                .context("deserialize thread_snapshot")
                .and_then(|object| object.validate())
        }
        "thread_event" => serde_json::from_value::<crate::objects::ThreadEvent>(value.clone())
            .context("deserialize thread_event")
            .and_then(|object| object.validate()),
        "bundle_event" => {
            serde_json::from_value::<crate::objects::BundleEventObject>(value.clone())
                .context("deserialize bundle_event")
                .and_then(|object| object.validate())
        }
        "project_snapshot" => crate::objects::ProjectSnapshot::from_value(value).map(|_| ()),
        "project_tree" => crate::objects::ProjectTree::from_value(value).map(|_| ()),
        "project_file" => crate::objects::ProjectFile::from_value(value).map(|_| ()),
        "project_snapshot_policy" => {
            crate::objects::ProjectSnapshotPolicy::from_value(value).map(|_| ())
        }
        "source_manifest" => crate::objects::SourceManifest::from_value(value).map(|_| ()),
        "item_source" => crate::objects::ItemSource::from_value(value).map(|_| ()),
        _ => return Ok(()),
    };
    result.map_err(|error| format!("invalid {kind} object: {error:#}"))
}

/// Extract schema-defined links from one CAS object value.
pub fn object_links(value: &Value) -> Result<ObjectLinks, String> {
    let kind = value
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing object kind".to_string())?;

    let mut links = ObjectLinks::default();

    match kind {
        "attestation" => push_required_hash(value, "subject_hash", &mut links.object_hashes)?,
        "chain_state" => {
            push_optional_hash(value, "prev_chain_state_hash", &mut links.object_hashes)?;
            push_optional_hash(value, "last_event_hash", &mut links.object_hashes)?;
            let Some(threads) = value.get("threads").and_then(|v| v.as_object()) else {
                return Err("chain_state missing threads object".to_string());
            };
            for entry in threads.values() {
                push_required_hash(entry, "snapshot_hash", &mut links.object_hashes)?;
                push_optional_hash(entry, "last_event_hash", &mut links.object_hashes)?;
            }
        }
        "thread_snapshot" => {
            push_optional_hash(
                value,
                "base_project_snapshot_hash",
                &mut links.object_hashes,
            )?;
            push_optional_hash(
                value,
                "result_project_snapshot_hash",
                &mut links.object_hashes,
            )?;
            push_optional_hash(value, "last_event_hash", &mut links.object_hashes)?;
        }
        "thread_event" => {
            push_optional_hash(value, "prev_chain_event_hash", &mut links.object_hashes)?;
            push_optional_hash(value, "prev_thread_event_hash", &mut links.object_hashes)?;
        }
        "bundle_event" => {
            push_optional_hash(value, "prev_chain_event_hash", &mut links.object_hashes)?;
            let attachments = value
                .get("attachments")
                .map(|value| {
                    value
                        .as_array()
                        .ok_or_else(|| "bundle_event attachments is not an array".to_string())
                })
                .transpose()?
                .cloned()
                .unwrap_or_default();
            for attachment in &attachments {
                push_required_hash(attachment, "blob_hash", &mut links.blob_hashes)?;
            }
        }
        "project_snapshot" => {
            push_required_hash(value, "project_tree_hash", &mut links.object_hashes)?;
            push_required_hash(value, "effective_policy_hash", &mut links.object_hashes)?;
            let parents = value
                .get("parent_hashes")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "project_snapshot missing parent_hashes array".to_string())?;
            for parent in parents {
                let Some(hash) = parent.as_str() else {
                    return Err("project_snapshot parent_hashes contains non-string".to_string());
                };
                push_hash(hash, &mut links.object_hashes)?;
            }
        }
        "source_manifest" => {
            let hashes = value
                .get("item_source_hashes")
                .and_then(|v| v.as_object())
                .ok_or_else(|| "source_manifest missing item_source_hashes object".to_string())?;
            for hash_value in hashes.values() {
                let Some(hash) = hash_value.as_str() else {
                    return Err(
                        "source_manifest item_source_hashes contains non-string".to_string()
                    );
                };
                push_hash(hash, &mut links.object_hashes)?;
            }
        }
        "project_tree" => {
            let hashes = value
                .get("files")
                .and_then(Value::as_object)
                .ok_or_else(|| "project_tree missing files object".to_string())?;
            for hash_value in hashes.values() {
                let Some(hash) = hash_value.as_str() else {
                    return Err("project_tree files contains non-string".to_string());
                };
                push_hash(hash, &mut links.object_hashes)?;
            }
        }
        "project_file" => push_required_hash(value, "blob_hash", &mut links.blob_hashes)?,
        "project_snapshot_policy" => {}
        "item_source" => push_required_hash(value, "content_blob_hash", &mut links.blob_hashes)?,
        other => links.unsupported_kind = Some(other.to_string()),
    }

    links.object_hashes.sort();
    links.object_hashes.dedup();
    links.blob_hashes.sort();
    links.blob_hashes.dedup();
    Ok(links)
}

fn push_required_hash(value: &Value, field: &str, out: &mut Vec<String>) -> Result<(), String> {
    let hash = value
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing required hash field {field}"))?;
    push_hash(hash, out)
}

fn push_optional_hash(value: &Value, field: &str, out: &mut Vec<String>) -> Result<(), String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(hash)) => push_hash(hash, out),
        Some(_) => Err(format!(
            "optional hash field {field} is not a string or null"
        )),
    }
}

fn push_hash(hash: &str, out: &mut Vec<String>) -> Result<(), String> {
    if !is_canonical_hash(hash) {
        return Err(format!("invalid hash: {hash}"));
    }
    out.push(hash.to_string());
    Ok(())
}

fn is_canonical_hash(hash: &str) -> bool {
    lillux::valid_hash(hash) && !hash.bytes().any(|b| b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn h(byte: &str) -> String {
        byte.repeat(32)
    }

    fn write_object(cas_root: &Path, value: &Value) -> String {
        let canonical = lillux::canonical_json(value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        hash
    }

    fn write_blob(cas_root: &Path, data: &[u8]) -> String {
        let hash = lillux::sha256_hex(data);
        let path = lillux::shard_path(cas_root, "blobs", &hash, "");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        lillux::atomic_write(&path, data).unwrap();
        hash
    }

    fn write_raw_object_at(cas_root: &Path, hash: &str, bytes: &[u8]) {
        let path = lillux::shard_path(cas_root, "objects", hash, ".json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        lillux::atomic_write(&path, bytes).unwrap();
    }

    #[test]
    fn project_snapshot_reaches_manifest_item_and_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let blob_hash = write_blob(&cas_root, b"hello closure");
        let file = write_object(
            &cas_root,
            &crate::objects::ProjectFile {
                blob_hash: blob_hash.clone(),
                size: b"hello closure".len() as u64,
                normalized_mode: 0o644,
            }
            .to_value(),
        );
        let tree = write_object(
            &cas_root,
            &crate::objects::ProjectTree {
                files: BTreeMap::from([(".ai/directives/test/example.md".to_string(), file)]),
            }
            .to_value(),
        );
        let policy = write_object(
            &cas_root,
            &crate::objects::ProjectSnapshotPolicy::new(
                crate::project_sync::ProjectSyncScope::FullProject,
                Vec::new(),
                Vec::new(),
                BTreeMap::new(),
            )
            .unwrap()
            .to_value(),
        );
        let snapshot = write_object(
            &cas_root,
            &json!({
                "kind": "project_snapshot",
                "schema": crate::objects::ProjectSnapshot::SCHEMA,
                "project_tree_hash": tree,
                "effective_policy_hash": policy,
                "message": null,
                "parent_hashes": [],
                "created_at": "2026-05-29T00:00:00Z",
                "source": "test"
            }),
        );

        let report = collect_object_closure(&cas_root, [snapshot]).unwrap();
        assert!(report.is_complete());
        assert_eq!(report.object_hashes.len(), 4);
        assert!(report.blob_hashes.contains(&blob_hash));
    }

    #[test]
    fn missing_blob_makes_closure_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let blob_hash = h("cd");
        let item = write_object(
            &cas_root,
            &json!({
                "kind": "item_source",
                "item_ref": ".ai/directives/test/example.md",
                "content_blob_hash": blob_hash,
                "integrity": "none",
                "signature_info": null,
                "mode": null
            }),
        );

        let report = collect_object_closure(&cas_root, [item]).unwrap();
        assert!(!report.is_complete());
        assert_eq!(report.missing_blobs.len(), 1);
        assert_eq!(report.missing_blobs[0].hash, blob_hash);
    }

    #[test]
    fn closure_rejects_wrong_object_hash_and_noncanonical_json_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let value = json!({
            "kind": "source_manifest",
            "item_source_hashes": {}
        });
        let canonical = lillux::canonical_json(&value).unwrap();

        let wrong_hash = h("12");
        write_raw_object_at(&cas_root, &wrong_hash, canonical.as_bytes());
        let wrong = collect_object_closure(&cas_root, [wrong_hash]).unwrap();
        assert!(!wrong.is_complete());
        assert!(wrong.malformed_objects[0]
            .reason
            .contains("object bytes hash mismatch"));

        let pretty = serde_json::to_vec_pretty(&value).unwrap();
        let pretty_hash = lillux::sha256_hex(&pretty);
        write_raw_object_at(&cas_root, &pretty_hash, &pretty);
        let noncanonical = collect_object_closure(&cas_root, [pretty_hash]).unwrap();
        assert!(!noncanonical.is_complete());
        assert!(noncanonical.malformed_objects[0]
            .reason
            .contains("canonical JSON"));
    }

    #[test]
    fn closure_rejects_blob_bytes_stored_under_another_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let declared_blob = h("34");
        let blob_path = lillux::shard_path(&cas_root, "blobs", &declared_blob, "");
        std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        lillux::atomic_write(&blob_path, b"different bytes").unwrap();
        let item = write_object(
            &cas_root,
            &json!({
                "kind": "item_source",
                "item_ref": ".ai/directives/test/example.md",
                "content_blob_hash": declared_blob,
                "integrity": "none",
                "signature_info": null,
                "mode": null
            }),
        );

        let report = collect_object_closure(&cas_root, [item]).unwrap();
        assert!(!report.is_complete());
        assert!(report.malformed_objects[0].reason.contains("bytes hash to"));
    }

    #[test]
    fn source_manifest_key_must_match_embedded_item_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let item = write_object(
            &cas_root,
            &json!({
                "kind": "item_source",
                "item_ref": ".ai/directives/test/actual.md",
                "content_blob_hash": h("ab"),
                "integrity": "none",
                "signature_info": null,
                "mode": null
            }),
        );
        let manifest = write_object(
            &cas_root,
            &json!({
                "kind": "source_manifest",
                "item_source_hashes": { ".ai/directives/test/declared.md": item }
            }),
        );

        let report = collect_object_closure(&cas_root, [manifest]).unwrap();
        assert!(!report.is_complete());
        assert!(report
            .malformed_objects
            .iter()
            .any(|object| object.reason.contains("does not match embedded")));
    }

    #[test]
    fn schema_history_graphs_reject_cycles() {
        let edges = BTreeMap::from([
            ("a".to_string(), BTreeSet::from(["b".to_string()])),
            ("b".to_string(), BTreeSet::from(["a".to_string()])),
        ]);
        assert!(cyclic_graph_member(&edges).is_some());

        let dag = BTreeMap::from([
            ("a".to_string(), BTreeSet::from(["b".to_string()])),
            ("b".to_string(), BTreeSet::from(["c".to_string()])),
        ]);
        assert!(cyclic_graph_member(&dag).is_none());
    }

    #[test]
    fn closure_rejects_old_or_structurally_incomplete_current_kinds() {
        for value in [
            json!({"kind": "attestation", "schema": 0}),
            json!({"kind": "chain_state", "schema": 1}),
            json!({"kind": "thread_snapshot", "schema": 3}),
            json!({"kind": "thread_event", "schema": 1}),
            json!({"kind": "bundle_event", "schema": 1}),
            json!({
                "kind": "project_snapshot",
                "schema": crate::objects::ProjectSnapshot::SCHEMA - 1,
                "project_tree_hash": h("11"),
                "effective_policy_hash": h("12"),
                "message": null,
                "parent_hashes": [],
                "created_at": "2026-07-14T00:00:00Z",
                "source": "manual_push"
            }),
            json!({"kind": "source_manifest"}),
            json!({"kind": "item_source"}),
        ] {
            assert!(
                validate_current_object(&value).is_err(),
                "current kind must pass its complete typed validator: {value}"
            );
        }
    }

    #[test]
    fn chain_state_reaches_top_level_last_event_hash() {
        let event_hash = h("ef");
        let links = object_links(&json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": "T-root",
            "prev_chain_state_hash": null,
            "last_event_hash": event_hash,
            "last_chain_seq": 1,
            "updated_at": "2026-05-29T00:00:00Z",
            "threads": {
                "T-root": {
                    "snapshot_hash": h("ab"),
                    "last_event_hash": null,
                    "last_thread_seq": 0,
                    "status": "running"
                }
            }
        }))
        .unwrap();
        assert!(links.object_hashes.contains(&event_hash));
    }

    #[test]
    fn missing_and_unsupported_objects_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let unsupported = write_object(&cas_root, &json!({ "kind": "future_kind" }));
        let missing = h("12");

        let report = collect_object_closure(&cas_root, [unsupported, missing.clone()]).unwrap();
        assert_eq!(report.unsupported_objects.len(), 1);
        assert_eq!(report.missing_objects.len(), 1);
        assert_eq!(report.missing_objects[0].hash, missing);
    }

    #[test]
    fn attestation_reaches_subject_hash() {
        let subject = h("34");
        let links = object_links(&json!({
            "kind": "attestation",
            "schema": 1,
            "subject_hash": subject,
            "claim": "accepted",
            "policy": "test",
            "issuer": "fp:test",
            "issued_at": "2026-05-29T00:00:00Z",
            "expires_at": null,
            "evidence": {},
            "signature": "test"
        }))
        .unwrap();
        assert_eq!(links.object_hashes, vec![subject]);
    }

    #[test]
    fn traversal_stops_when_max_objects_exceeded() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let file = write_object(
            &cas_root,
            &crate::objects::ProjectFile {
                blob_hash: h("cd"),
                size: 1,
                normalized_mode: 0o644,
            }
            .to_value(),
        );
        let tree = write_object(
            &cas_root,
            &crate::objects::ProjectTree {
                files: BTreeMap::from([(".ai/directives/test/example.md".to_string(), file)]),
            }
            .to_value(),
        );
        let policy = write_object(
            &cas_root,
            &crate::objects::ProjectSnapshotPolicy::new(
                crate::project_sync::ProjectSyncScope::FullProject,
                Vec::new(),
                Vec::new(),
                BTreeMap::new(),
            )
            .unwrap()
            .to_value(),
        );
        let snapshot = write_object(
            &cas_root,
            &json!({
                "kind": "project_snapshot",
                "schema": crate::objects::ProjectSnapshot::SCHEMA,
                "project_tree_hash": tree,
                "effective_policy_hash": policy,
                "message": null,
                "parent_hashes": [],
                "created_at": "2026-05-29T00:00:00Z",
                "source": "test"
            }),
        );

        let err = collect_object_closure_with_limit(&cas_root, [snapshot], Some(2)).unwrap_err();
        assert!(err.to_string().contains("exceeds max_objects"));
    }

    #[test]
    fn traversal_rejects_oversized_object() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let value = json!({ "kind": "future_kind", "padding": "x".repeat(256) });
        let hash = write_object(&cas_root, &value);

        let err = collect_object_closure_with_limits(
            &cas_root,
            [hash],
            ObjectClosureLimits {
                max_objects: 8,
                max_blobs: 8,
                max_object_bytes: 32,
                max_blob_bytes: 32,
                max_total_blob_bytes: 32,
                max_links_per_object: 8,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("exceeds byte limit"));
    }

    #[test]
    fn traversal_rejects_too_many_links() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let manifest = write_object(
            &cas_root,
            &json!({
                "kind": "source_manifest",
                "item_source_hashes": {
                    "src/a": h("11"),
                    "src/b": h("22"),
                }
            }),
        );

        let err = collect_object_closure_with_limits(
            &cas_root,
            [manifest],
            ObjectClosureLimits {
                max_objects: 8,
                max_blobs: 8,
                max_object_bytes: 1024,
                max_blob_bytes: 1024,
                max_total_blob_bytes: 1024,
                max_links_per_object: 1,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("exceeds max_links_per_object"));
    }
}
