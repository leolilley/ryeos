//! Generic CAS object-graph closure collection.
//!
//! This module owns schema-defined traversal from CAS object roots to the
//! transitive set of reachable object and blob hashes. It intentionally
//! does not discover roots from refs; callers provide root object hashes.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const DEFAULT_MAX_OBJECTS: usize = 10_000;
// One external realization may reference 10,000 blobs. Capsules also carry
// executable/protocol blobs, so the closure-wide ceiling must leave bounded
// headroom rather than making a contract-valid realization untransferable.
const DEFAULT_MAX_BLOBS: usize = 20_000;
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
    /// All reachable hashes in the distinct large-object store. Residency is
    /// verified by callers that hold that store; these are never opened as CAS
    /// blobs.
    pub large_object_hashes: BTreeSet<String>,
    /// Blob hashes referenced by reachable objects but absent from CAS.
    pub missing_blobs: Vec<MissingDependency>,
    /// Object hashes that were referenced but not present in CAS.
    pub missing_objects: Vec<MissingDependency>,
    /// Object hashes whose JSON body or schema-defined edges were malformed.
    pub malformed_objects: Vec<MalformedObject>,
    /// Objects with a kind this collector does not know how to traverse.
    pub unsupported_objects: Vec<UnsupportedObjectKind>,
    /// Typed outer-schema mismatches retained for fail-closed callers that
    /// must distinguish a clean-cut epoch boundary from malformed content.
    incompatible_current_schemas: Vec<crate::objects::IncompatibleCurrentObjectSchema>,
}

impl ObjectClosureReport {
    pub fn is_complete(&self) -> bool {
        self.missing_objects.is_empty()
            && self.missing_blobs.is_empty()
            && self.malformed_objects.is_empty()
            && self.unsupported_objects.is_empty()
    }

    pub(crate) fn decisive_incompatible_current_schema(
        &self,
    ) -> Option<&crate::objects::IncompatibleCurrentObjectSchema> {
        self.incompatible_current_schemas
            .iter()
            .find(|mismatch| !mismatch.is_predecessor())
            .or_else(|| self.incompatible_current_schemas.first())
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
    pub large_object_hashes: Vec<String>,
    pub unsupported_kind: Option<String>,
}

/// Meaning-blind registration surface for durable object contracts owned by
/// layers above state. State retains and traverses the declared typed edges;
/// it never imports or interprets the registering domain's Rust types.
pub struct ObjectContractRegistration {
    pub kind: &'static str,
    pub validate: fn(&Value) -> anyhow::Result<()>,
    pub links: fn(&Value) -> Result<RegisteredObjectLinks, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisteredObjectExpectation {
    Any,
    Kind(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredObjectEdge {
    pub hash: String,
    pub expected: RegisteredObjectExpectation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegisteredObjectLinks {
    pub object_edges: Vec<RegisteredObjectEdge>,
    pub blob_hashes: Vec<String>,
    pub large_object_hashes: Vec<String>,
}

static REGISTERED_OBJECT_CONTRACTS: OnceLock<Vec<ObjectContractRegistration>> = OnceLock::new();

/// Install the application-owned object contracts exactly once, before any
/// closure walk. Duplicate built-in kinds and malformed/duplicate external
/// kinds fail closed. Repeated installation is never treated as an alias.
pub fn install_object_contracts(
    mut registrations: Vec<ObjectContractRegistration>,
) -> anyhow::Result<()> {
    registrations.sort_by_key(|registration| registration.kind);
    let mut prior = None;
    for registration in &registrations {
        if registration.kind.is_empty()
            || registration.kind.len() > 128
            || !registration
                .kind
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            anyhow::bail!(
                "registered object kind is not canonical: {}",
                registration.kind
            );
        }
        if contracts::CURRENT_OBJECT_KINDS.contains(&registration.kind) {
            anyhow::bail!(
                "registered object kind {} collides with state-owned contract",
                registration.kind
            );
        }
        if prior == Some(registration.kind) {
            anyhow::bail!("registered object kind {} is duplicated", registration.kind);
        }
        prior = Some(registration.kind);
    }
    REGISTERED_OBJECT_CONTRACTS
        .set(registrations)
        .map_err(|_| anyhow::anyhow!("application object contracts were already installed"))
}

/// Exact object kinds the current closure decoder admits.
///
/// Durable writers and maintenance coverage tests use this inventory so a new
/// current kind cannot remain implicit in a private match arm.
pub fn current_object_kinds() -> Vec<&'static str> {
    let mut kinds = contracts::CURRENT_OBJECT_KINDS.to_vec();
    if let Some(registered) = REGISTERED_OBJECT_CONTRACTS.get() {
        kinds.extend(registered.iter().map(|contract| contract.kind));
    }
    kinds.sort_unstable();
    kinds
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ExpectedObject {
    Any,
    Kind(&'static str),
    OneOf(&'static [&'static str]),
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

#[path = "object_closure/contracts.rs"]
mod contracts;
use contracts::ContractLinks;

fn registered_contract(kind: &str) -> Option<&'static ObjectContractRegistration> {
    let registrations = REGISTERED_OBJECT_CONTRACTS.get()?;
    registrations
        .binary_search_by_key(&kind, |registration| registration.kind)
        .ok()
        .map(|index| &registrations[index])
}

fn decode_registered(value: &Value) -> anyhow::Result<Option<ContractLinks>> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing object kind"))?;
    let Some(contract) = registered_contract(kind) else {
        return Ok(None);
    };
    (contract.validate)(value).with_context(|| format!("invalid registered {kind} object"))?;
    registered_links(contract, value)
        .map(Some)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("invalid registered {kind} object links"))
}

fn links_registered(value: &Value) -> Result<Option<ContractLinks>, String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing object kind".to_string())?;
    let Some(contract) = registered_contract(kind) else {
        return Ok(None);
    };
    registered_links(contract, value).map(Some)
}

fn registered_links(
    contract: &ObjectContractRegistration,
    value: &Value,
) -> Result<ContractLinks, String> {
    let declared = (contract.links)(value)?;
    let mut links = ContractLinks::leaf();
    for edge in declared.object_edges {
        push_typed_hash(
            &edge.hash,
            match edge.expected {
                RegisteredObjectExpectation::Any => ExpectedObject::Any,
                RegisteredObjectExpectation::Kind(kind) => ExpectedObject::Kind(kind),
            },
            None,
            &mut links.object_edges,
        )?;
    }
    for hash in declared.blob_hashes {
        if !is_canonical_hash(&hash) {
            return Err(format!("invalid registered blob hash: {hash}"));
        }
        links.blob_hashes.push(hash);
    }
    for hash in declared.large_object_hashes {
        if !is_canonical_hash(&hash) {
            return Err(format!("invalid registered large-object hash: {hash}"));
        }
        links.large_object_hashes.push(hash);
    }
    Ok(links.finish())
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
            Self::OneOf(expected) if expected.contains(&identity.kind.as_str()) => Ok(()),
            Self::OneOf(expected) => Err(format!(
                "object edge expected one of {}, got {}",
                expected.join(", "),
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

        let decoded = match contracts::decode(&value) {
            Ok(Some(decoded)) => decoded,
            Ok(None) => {
                let kind = value
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing>")
                    .to_string();
                report
                    .unsupported_objects
                    .push(UnsupportedObjectKind { hash, kind });
                continue;
            }
            Err(error) => {
                if let Some(mismatch) = error.chain().find_map(|cause| {
                    cause
                        .downcast_ref::<crate::objects::IncompatibleCurrentObjectSchema>()
                        .cloned()
                }) {
                    report.incompatible_current_schemas.push(mismatch);
                }
                report.malformed_objects.push(MalformedObject {
                    hash,
                    reason: format!("{error:#}"),
                });
                continue;
            }
        };

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

        let object_edges = decoded.object_edges;

        let link_count = object_edges
            .len()
            .saturating_add(decoded.blob_hashes.len())
            .saturating_add(decoded.large_object_hashes.len());
        if link_count > limits.max_links_per_object {
            anyhow::bail!(
                "object {hash} exceeds max_links_per_object: {} > {}",
                link_count,
                limits.max_links_per_object
            );
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
        for large_object in decoded.large_object_hashes {
            if !is_canonical_hash(&large_object) {
                report.malformed_objects.push(MalformedObject {
                    hash: hash.clone(),
                    reason: format!("invalid large-object hash: {large_object}"),
                });
                continue;
            }
            report.large_object_hashes.insert(large_object);
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
        for blob in decoded.blob_hashes {
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
                        if let Some(expected_size) = project_file_size
                            && actual_size != expected_size
                        {
                            report.malformed_objects.push(MalformedObject {
                                hash: hash.clone(),
                                reason: format!(
                                    "project_file declares size {expected_size}, but blob {blob} has size {actual_size}"
                                ),
                            });
                            continue;
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

#[cfg(test)]
fn typed_object_edges(value: &Value) -> Result<Vec<ObjectEdge>, String> {
    Ok(contracts::links(value)?
        .map(|links| links.object_edges)
        .unwrap_or_default())
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
#[cfg(test)]
fn validate_current_object(value: &Value) -> anyhow::Result<()> {
    contracts::decode(value).map(|_| ())
}
/// Extract schema-defined links from one CAS object value.
pub fn object_links(value: &Value) -> Result<ObjectLinks, String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing object kind".to_string())?;
    let Some(decoded) = contracts::links(value)? else {
        return Ok(ObjectLinks {
            unsupported_kind: Some(kind.to_string()),
            ..ObjectLinks::default()
        });
    };
    let mut object_hashes = decoded
        .object_edges
        .into_iter()
        .map(|edge| edge.hash)
        .collect::<Vec<_>>();
    object_hashes.sort();
    object_hashes.dedup();
    Ok(ObjectLinks {
        object_hashes,
        blob_hashes: decoded.blob_hashes,
        large_object_hashes: decoded.large_object_hashes,
        unsupported_kind: None,
    })
}
/// Manifest hashes for every external realization sealed into one capsule.
///
/// This parses the one reserved durable slot using the shared wire type. A
/// shape probe over arbitrary derived values would let unrelated data create
/// false CAS edges and would make GC semantics depend on field-name
/// coincidence.
fn external_realization_manifest_hashes(capsule: &Value) -> Result<Vec<String>, String> {
    let Some(derived) = capsule
        .get("exact_program")
        .and_then(|program| program.get("resolution_output"))
        .and_then(|resolution| resolution.get("composed"))
        .and_then(|composed| composed.get("derived"))
        .and_then(Value::as_object)
    else {
        return Ok(Vec::new());
    };

    let Some(value) = derived.get(crate::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY) else {
        return Ok(Vec::new());
    };
    crate::objects::ExternalContentRealizationSet::from_value(value)
        .map(|set| set.manifest_hashes())
        .map_err(|error| format!("invalid external realization set: {error}"))
}

fn persistent_session_external_realization_manifest_hashes(
    capsule: &Value,
) -> Result<Vec<String>, String> {
    let Some(derived) = capsule
        .get("exact_program")
        .and_then(|program| program.get("resolution_output"))
        .and_then(|resolution| resolution.get("composed"))
        .and_then(|composed| composed.get("derived"))
        .and_then(Value::as_object)
    else {
        return Ok(Vec::new());
    };
    let Some(value) = derived.get(crate::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY) else {
        return Ok(Vec::new());
    };
    crate::objects::ExternalContentRealizationSet::from_value(value)
        .map(|set| set.manifest_hashes())
        .map_err(|error| format!("invalid persistent-session external realization set: {error}"))
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
        assert!(report.is_complete(), "{report:?}");
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
        assert!(
            wrong.malformed_objects[0]
                .reason
                .contains("object bytes hash mismatch")
        );

        let pretty = serde_json::to_vec_pretty(&value).unwrap();
        let pretty_hash = lillux::sha256_hex(&pretty);
        write_raw_object_at(&cas_root, &pretty_hash, &pretty);
        let noncanonical = collect_object_closure(&cas_root, [pretty_hash]).unwrap();
        assert!(!noncanonical.is_complete());
        assert!(
            noncanonical.malformed_objects[0]
                .reason
                .contains("canonical JSON")
        );
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
        assert!(
            report
                .malformed_objects
                .iter()
                .any(|object| object.reason.contains("does not match embedded"))
        );
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
            json!({"kind": "thread_snapshot", "schema": 4}),
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
    fn state_anchor_event_reaches_manifest_and_manifest_reaches_every_blob() {
        let manifest_hash = h("ac");
        let event = json!({
            "kind": "thread_event",
            "event_type": "milestone",
            "prev_chain_event_hash": null,
            "prev_thread_event_hash": null,
            "payload": {
                "kind": "state_anchor",
                "payload": {
                    "schema_version": 2,
                    "label": "checkpoint",
                    "state_digest": format!("sha256:{manifest_hash}"),
                    "manifest_ref": format!("cas:{manifest_hash}"),
                    "runtime": {},
                    "metadata": {}
                },
                "graph_run_id": "G-test",
                "definition_ref": "graph:test/solve",
                "effective_definition_digest": h("de"),
                "node": "solve",
                "step": 3
            }
        });
        let edges = typed_object_edges(&event).unwrap();
        assert!(edges.iter().any(|edge| {
            edge.hash == manifest_hash && edge.expected == ExpectedObject::Kind("state_manifest")
        }));
        let mut predecessor = event;
        predecessor["payload"]["payload"]["schema_version"] = json!(1);
        assert!(
            typed_object_edges(&predecessor)
                .unwrap_err()
                .contains("current contract")
        );

        let restore_hash = h("bd");
        let input_hash = h("ce");
        let links = object_links(&json!({
            "kind": "state_manifest",
            "schema": 1,
            "contract": "domain.restore.v1",
            "publisher_chain_root_id": "T-root",
            "publisher_thread_id": "T-root",
            "restore": {
                "name": "restore",
                "media_type": "application/json",
                "blob_hash": restore_hash,
                "size_bytes": 1
            },
            "objects": [{
                "name": "engine",
                "media_type": "application/octet-stream",
                "blob_hash": input_hash,
                "size_bytes": 1
            }]
        }))
        .unwrap();
        assert_eq!(links.blob_hashes, vec![restore_hash, input_hash]);
    }

    #[test]
    fn admitted_direct_execution_closure_reaches_executable_blob() {
        let executable_blob_hash = h("de");
        let links = object_links(&json!({
            "kind": "admitted_launch_capsule",
            "project_authority": {"kind": "projectless", "environment": {"kind": "none"}},
            "execution_closure": {
                "driver": "direct_item_executor",
                "execution_plan": {},
                "protocol_descriptor_document": "# signed",
                "command": {
                    "authority": "content_addressed",
                    "executable_blob_hash": executable_blob_hash
                }
            }
        }))
        .unwrap();
        assert_eq!(links.blob_hashes, vec![executable_blob_hash]);
    }

    #[test]
    fn admitted_pinned_cow_capsule_reaches_base_and_operational_generations() {
        let base_snapshot_hash = h("ba");
        let operational_snapshot_hash = h("cd");
        let links = object_links(&json!({
            "kind": "admitted_launch_capsule",
            "project_authority": {
                "kind": "pinned_generation",
                "base_snapshot_hash": base_snapshot_hash,
                "snapshot_hash": operational_snapshot_hash
            },
            "execution_closure": {
                "driver": "direct_item_executor",
                "execution_plan": {},
                "protocol_descriptor_document": "# signed",
                "command": {
                    "authority": {
                        "executable_identity": {
                            "blob_hash": h("de")
                        }
                    }
                }
            }
        }))
        .unwrap();
        assert!(links.object_hashes.contains(&base_snapshot_hash));
        assert!(links.object_hashes.contains(&operational_snapshot_hash));
    }

    #[test]
    fn admitted_managed_execution_closure_reaches_executor_blob() {
        let executor_blob_hash = h("ce");
        let links = object_links(&json!({
            "kind": "admitted_launch_capsule",
            "project_authority": {"kind": "projectless", "environment": {"kind": "none"}},
            "execution_closure": {
                "driver": "managed_runtime",
                "prepared_runtime_launch": {},
                "runtime_descriptor_document": "# signed",
                "protocol_descriptor_document": "# signed",
                "executor_blob_hash": executor_blob_hash
            }
        }))
        .unwrap();
        assert_eq!(links.blob_hashes, vec![executor_blob_hash]);
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

    #[test]
    fn a_capsule_edge_reaches_its_realization_manifest() {
        let manifest_hash = h("aa");
        // The wire key is asserted literally on purpose: renaming the
        // reserved derived slot must break this test, not silently detach
        // every realization from GC reachability.
        let capsule = json!({
            "kind": "admitted_launch_capsule",
            "project_authority": {"kind": "live_project"},
            "execution_closure": {
                "driver": "direct_item_executor",
                "execution_plan": {},
                "protocol_descriptor_document": "# signed",
                "command": {"authority": "runtime_path"}
            },
            "exact_program": {
                "resolution_output": {
                    "composed": {
                        "derived": {
                            "effective_external_realizations": [{
                                "id": "sim",
                                "kind": "tree",
                                "mode": "captured",
                                "manifest_hash": manifest_hash,
                                "entry_count": 1,
                                "total_bytes": 5,
                                "mount": "vendor/sim"
                            }]
                        }
                    }
                }
            }
        });
        let edges = typed_object_edges(&capsule).unwrap();
        assert!(edges.iter().any(|edge| {
            edge.hash == manifest_hash
                && edge.expected == ExpectedObject::OneOf(contracts::EXTERNAL_MANIFEST_KINDS)
        }));

        // Malformed derived data is a closure error, never an empty edge set:
        // silently dropping the edge is exactly how a realization becomes
        // collectable garbage.
        let mut malformed = capsule;
        malformed["exact_program"]["resolution_output"]["composed"]["derived"]["effective_external_realizations"] =
            json!([{"id": "sim"}]);
        assert!(
            typed_object_edges(&malformed)
                .unwrap_err()
                .contains("invalid external realization set")
        );
    }

    #[test]
    fn an_external_manifest_closure_reaches_every_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let file_blob = write_blob(&cas_root, b"hello");
        let target_blob = write_blob(&cas_root, "t".repeat(2000).as_bytes());
        let manifest = write_object(
            &cas_root,
            &json!({
                "kind": "external_content_manifest",
                "schema": "ryeos.external_content.tree.v1",
                "entries": [
                    {"path": "content", "kind": "file", "mode": 0o644,
                     "blob_hash": file_blob, "size": 5},
                    {"path": "link", "kind": "symlink", "target_blob": target_blob}
                ],
                "entry_count": 2,
                "total_bytes": 5
            }),
        );
        let report = collect_object_closure(&cas_root, [manifest]).unwrap();
        assert!(report.is_complete());
        assert!(report.blob_hashes.contains(&file_blob));
        assert!(report.blob_hashes.contains(&target_blob));
    }

    #[test]
    fn a_realization_with_a_missing_blob_is_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let ghost = h("ee");
        let manifest = write_object(
            &cas_root,
            &json!({
                "kind": "external_content_manifest",
                "schema": "ryeos.external_content.tree.v1",
                "entries": [{"path": "content", "kind": "file", "mode": 0o644,
                             "blob_hash": ghost, "size": 1}],
                "entry_count": 1,
                "total_bytes": 1
            }),
        );
        let report = collect_object_closure(&cas_root, [manifest]).unwrap();
        assert!(!report.is_complete());
        assert!(report.missing_blobs.iter().any(|blob| blob.hash == ghost));
    }

    #[test]
    fn a_large_manifest_reports_each_storage_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let large_hash = h("ac");
        let small_blob = write_blob(&cas_root, b"small");
        let manifest = write_object(
            &cas_root,
            &json!({
                "kind": crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
                "schema": crate::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
                "entries": [
                    {
                        "path": "config.json",
                        "kind": "file",
                        "mode": 0o644,
                        "blob_hash": small_blob,
                        "size": 5
                    },
                    {
                        "path": "model.safetensors",
                        "kind": "file",
                        "mode": 0o644,
                        "file_sha256": large_hash,
                        "size": 5,
                        "chunk_size": crate::objects::MIN_LARGE_CONTENT_CHUNK_BYTES,
                        "chunk_hashes": [h("bd")]
                    }
                ],
                "entry_count": 2,
                "total_bytes": 10
            }),
        );
        let report = collect_object_closure(&cas_root, [manifest]).unwrap();
        assert!(report.is_complete(), "{report:?}");
        assert!(report.blob_hashes.contains(&small_blob));
        assert_eq!(report.large_object_hashes, BTreeSet::from([large_hash]));
    }

    #[test]
    fn current_registry_contains_every_baseline_specialized_object_kind() {
        let kinds = current_object_kinds();
        for kind in [
            crate::objects::EXECUTION_IDENTITY_KIND,
            crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            crate::objects::EFFECT_RECORD_KIND,
            crate::objects::STATE_MANIFEST_KIND,
        ] {
            assert!(
                kinds.contains(&kind),
                "missing current object contract: {kind}"
            );
        }
    }
}
