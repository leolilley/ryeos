//! Durable recovery protocol for chain-head transitions and projection baselines.
//!
//! The journal is deliberately local operational state: signed heads and CAS
//! objects remain authoritative. One record exists per chain, and transitions
//! are never overwritten. A pending transition must be explicitly repaired or
//! cancelled and compare-acknowledged before another transition is prepared.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub const RECOVERY_PROTOCOL_GENERATION: u32 = 1;

/// Runtime-state child holding the State recovery roots and the CAS lock.
pub const RECOVERY_DIRECTORY_NAME: &str = "recovery";
/// The CAS mutation lock inside [`RECOVERY_DIRECTORY_NAME`].
pub const CAS_MUTATION_LOCK_FILENAME: &str = "cas-mutation.lock";
const PENDING_SCHEMA: u32 = 1;
const GENERATION_SCHEMA: u32 = 1;
const STAGED_ROOTS_SCHEMA: u32 = 2;
const DURABLE_UPLOAD_SCHEMA: u32 = 3;
const MAX_RECOVERY_RECORD_BYTES: u64 = 1024 * 1024;
// Staging records carry roots, unlike small transition journals. This structural
// envelope admits one maximum CAS upload plus one maximum large-content import.
// It is not an admission allowance: node policy still bounds each operation.
// Large-object roots have no aggregate ObjectClosureLimits bound, so count them
// explicitly here rather than pretending they consume the CAS blob allowance.
const MAX_STAGING_ROOTS: usize = crate::object_closure::REMOTE_CLOSURE_MAX_OBJECTS
    + crate::object_closure::REMOTE_CLOSURE_MAX_BLOBS
    + crate::objects::MAX_LARGE_CONTENT_MANIFEST_ENTRIES as usize
    + 2; // completed manifest and publication binding during settlement
// A canonical hash array element costs at most 67 bytes. Retain the existing
// small-record envelope for metadata; never widen unrelated recovery journals.
const MAX_STAGING_RECORD_BYTES: u64 = MAX_RECOVERY_RECORD_BYTES + MAX_STAGING_ROOTS as u64 * 67;
// The authority predates its use by project snapshot apply. Keep the physical
// coordinate stable while generalizing the API: changing the filename would
// strand an authenticated remote-pull journal created by the immediately
// preceding process. This is one authority, not a legacy-name fallback.
const WORKSPACE_JOURNAL_KEY: &str = "remote-pull-journal.key";
static UNIQUE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static SHARED_CAS_GUARD_DEPTH: Cell<usize> = const { Cell::new(0) };
    static EXCLUSIVE_CAS_GUARD_DEPTH: Cell<usize> = const { Cell::new(0) };
    static CAS_GUARD_FILE: RefCell<Option<Rc<File>>> = const { RefCell::new(None) };
    #[cfg(test)]
    static FAIL_AFTER_STAGING_PUBLICATION: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy)]
enum GuardDepth {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadOperation {
    Set,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionPhase {
    Prepared,
    HeadPublished,
    ApplyingProjection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingChainHeadTransition {
    pub schema: u32,
    pub transition_id: String,
    pub chain_root_id: String,
    pub operation: HeadOperation,
    pub phase: TransitionPhase,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub expected_previous_hash: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub target_hash: Option<String>,
    pub prepared_at: String,
}

impl PendingChainHeadTransition {
    pub fn validate(&self) -> Result<()> {
        if self.schema != PENDING_SCHEMA {
            anyhow::bail!("unsupported pending transition schema: {}", self.schema);
        }
        validate_chain_root_id(&self.chain_root_id)?;
        if self.transition_id.is_empty() || self.transition_id.len() > 128 {
            anyhow::bail!("invalid transition_id");
        }
        crate::objects::parse_canonical_timestamp(&self.prepared_at)
            .context("pending transition prepared_at is not canonical")?;
        match self.operation {
            HeadOperation::Set => {
                validate_optional_hash(
                    "expected_previous_hash",
                    self.expected_previous_hash.as_deref(),
                )?;
                let target = self
                    .target_hash
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("pending Set is missing target_hash"))?;
                validate_hash("target_hash", target)?;
            }
            HeadOperation::Remove => {
                let previous = self.expected_previous_hash.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("pending Remove is missing expected_previous_hash")
                })?;
                validate_hash("expected_previous_hash", previous)?;
                if self.target_hash.is_some() {
                    anyhow::bail!("pending Remove must not contain target_hash");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionRecoveryGeneration {
    pub schema: u32,
    pub generation: u32,
    pub projection_schema_epoch: i32,
    pub projection_instance_id: String,
    pub projection_file: String,
    pub established_at: String,
}

impl ProjectionRecoveryGeneration {
    pub fn new(
        projection_schema_epoch: i32,
        projection_instance_id: String,
        projection_file: String,
    ) -> Result<Self> {
        validate_instance_id(&projection_instance_id)?;
        validate_projection_file(&projection_file, &projection_instance_id)?;
        Ok(Self {
            schema: GENERATION_SCHEMA,
            generation: RECOVERY_PROTOCOL_GENERATION,
            projection_schema_epoch,
            projection_instance_id,
            projection_file,
            established_at: lillux::time::iso8601_now(),
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != GENERATION_SCHEMA {
            anyhow::bail!("unsupported recovery generation schema: {}", self.schema);
        }
        if self.generation != RECOVERY_PROTOCOL_GENERATION {
            anyhow::bail!(
                "unsupported recovery protocol generation: {}",
                self.generation
            );
        }
        validate_instance_id(&self.projection_instance_id)?;
        validate_projection_file(&self.projection_file, &self.projection_instance_id)?;
        crate::objects::parse_canonical_timestamp(&self.established_at)
            .context("recovery generation established_at is not canonical")?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RecoveryStore {
    root: PathBuf,
    runtime_directory: std::sync::Arc<lillux::PinnedDirectory>,
    directory: std::sync::Arc<lillux::PinnedDirectory>,
}

/// A scoped in-process borrow of actual recovery roots, not an authority
/// reconstructed from marker bytes. The real exclusive CAS guard remains required.
pub(crate) struct RecoveryAcknowledgementCustody {
    runtime_directory: std::sync::Arc<lillux::PinnedDirectory>,
    directory: std::sync::Arc<lillux::PinnedDirectory>,
}
impl RecoveryAcknowledgementCustody {
    /// Rewrite this exact operation's Active marker to Acknowledged in one
    /// same-coordinate rename, so the marker is never absent between the two
    /// phases. A retry over the same Acknowledged operation only syncs.
    pub(crate) fn finish(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        operation: &ThreadHistoryDiscardOperationRef,
    ) -> Result<()> {
        let mut work = AcknowledgementWork::default();
        self.finish_counted(cas_mutation_guard, operation, &mut work)
    }

    /// `finish` with every bounded read, allocation and filesystem step
    /// tallied into `work`, so the caller's prepaid quote can be checked
    /// against the work actually performed.
    fn finish_counted(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        operation: &ThreadHistoryDiscardOperationRef,
        work: &mut AcknowledgementWork,
    ) -> Result<()> {
        anyhow::ensure!(
            cas_mutation_guard.is_exclusive(),
            "thread-history discard requires the exclusive CAS guard"
        );
        cas_mutation_guard.ensure_protects_pinned_runtime(&self.runtime_directory)?;
        operation.validate()?;
        let name = std::ffi::OsStr::new(THREAD_HISTORY_DISCARD_MARKER);
        let Some((pin, marker)) = pin_thread_history_discard_marker(&self.directory, work)? else {
            return Err(ThreadHistoryDiscardAcknowledgementRefused::Absent.into());
        };
        if &marker.operation != operation {
            return Err(ThreadHistoryDiscardAcknowledgementRefused::OtherOperation {
                acknowledged: marker.phase == ThreadHistoryDiscardPhase::Acknowledged,
                operation_id: marker.operation.operation_id,
            }
            .into());
        }
        match marker.phase {
            ThreadHistoryDiscardPhase::Acknowledged => {
                work.steps += 1;
                self.directory
                    .sync()
                    .context("sync acknowledged thread-history discard")
            }
            ThreadHistoryDiscardPhase::Active => {
                let acknowledged = ThreadHistoryDiscardMarker::for_operation(
                    operation,
                    ThreadHistoryDiscardPhase::Acknowledged,
                )?;
                work.memory_bytes += (std::mem::size_of::<ThreadHistoryDiscardMarker>()
                    + acknowledged.kind.capacity()
                    + acknowledged.operation.operation_id.capacity()
                    + acknowledged.operation.sha256.capacity()) as u64;
                let acknowledged = acknowledged.canonical_bytes()?;
                // The conditional write joins the directory path with the
                // marker name for its identity check, and encodes the name.
                let path = self.directory.path().as_os_str().len() as u64;
                anyhow::ensure!(
                    path <= ACK_DIRECTORY_PATH_MAX_BYTES,
                    "thread-history discard directory path exceeds the acknowledgement quote"
                );
                work.memory_bytes += acknowledged.capacity() as u64
                    + ACK_TEMP_NAME_SCRATCH_BYTES
                    + path
                    + ACK_MARKER_PATH_SCRATCH_BYTES;
                work.steps += ACK_ATOMIC_WRITE_STEPS;
                self.directory
                    .atomic_write_if_same(
                        name,
                        Some(&pin),
                        &acknowledged,
                        0o600,
                        crate::STATE_TEMP_PURPOSE,
                    )
                    .context("acknowledge completed thread-history discard")?;
                work.steps += 1;
                self.directory
                    .sync()
                    .context("sync completed thread-history discard acknowledgement")
            }
        }
    }
}

/// Bounded read, memory and filesystem steps of one ACK attempt.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AcknowledgementWork {
    pub(crate) read_bytes: u64,
    pub(crate) memory_bytes: u64,
    pub(crate) steps: u64,
}

/// Upper bound on a secure temporary's NUL-terminated name: the 12-byte
/// `.secure.tmp.` prefix, a purpose token of at most 16 bytes, a separator, a
/// decimal u32 pid (10), a separator, a decimal u64 sequence (20) and the NUL.
const ACK_TEMP_NAME_SCRATCH_BYTES: u64 = 12 + 16 + 1 + 10 + 1 + 20 + 1;

/// Largest recovery-directory path an ACK admits, the platform `PATH_MAX`.
/// The conditional write builds one joined path from it.
const ACK_DIRECTORY_PATH_MAX_BYTES: u64 = lillux::PATH_MAX_BYTES;

/// Path scratch of the conditional write beyond the directory path: the
/// separator and the marker name of the joined path, and the marker name
/// encoded once more with its NUL.
const ACK_MARKER_PATH_SCRATCH_BYTES: u64 =
    1 + THREAD_HISTORY_DISCARD_MARKER.len() as u64 + THREAD_HISTORY_DISCARD_MARKER.len() as u64 + 1;

/// Filesystem steps inside one conditional atomic write: temporary create,
/// mode set, write, fsync, interruption hook check, incumbent identity check,
/// rename and directory sync.
const ACK_ATOMIC_WRITE_STEPS: u64 = 8;

/// Filesystem steps of pinned marker intake: pin, presence probe, guarded
/// open and bounded read, pinned identity, and current-entry identity.
const MARKER_PINNED_INTAKE_STEPS: u64 = 5;

/// The prepaid bound of one ACK attempt that rewrites an Active marker to
/// Acknowledged. Reads: every admitted marker byte plus the one growth probe.
/// Memory: the read buffer, the decoded marker with its three strings, the
/// canonical re-encoding it is compared against, the Acknowledged marker
/// with its three strings, its encoded bytes, the temporary-name scratch and
/// the joined-path scratch of the conditional write. Steps: pinned intake,
/// the conditional atomic write and the final directory sync. A retry over an
/// Acknowledged marker does strictly less.
pub(crate) const fn thread_history_discard_acknowledgement_quote() -> AcknowledgementWork {
    let max = THREAD_HISTORY_DISCARD_MARKER_MAX_BYTES as u64;
    let marker = std::mem::size_of::<ThreadHistoryDiscardMarker>() as u64 + max;
    AcknowledgementWork {
        read_bytes: max + 1,
        memory_bytes: (max + 1)
            + marker
            + max
            + marker
            + max
            + ACK_TEMP_NAME_SCRATCH_BYTES
            + ACK_DIRECTORY_PATH_MAX_BYTES
            + ACK_MARKER_PATH_SCRATCH_BYTES,
        steps: MARKER_PINNED_INTAKE_STEPS + ACK_ATOMIC_WRITE_STEPS + 1,
    }
}

/// Pin the marker inode, read it through the guarded reader and prove the
/// read named the pinned inode. Absent markers return `None`.
fn pin_thread_history_discard_marker(
    directory: &lillux::PinnedDirectory,
    work: &mut AcknowledgementWork,
) -> Result<Option<(std::fs::File, ThreadHistoryDiscardMarker)>> {
    let name = std::ffi::OsStr::new(THREAD_HISTORY_DISCARD_MARKER);
    work.steps += 1;
    let Some(pin) = directory.open_mount_entry(name)? else {
        return Ok(None);
    };
    let marker = read_thread_history_discard_marker_in(None, directory, work)?
        .context("thread-history discard marker disappeared during pinned intake")?;
    work.steps += 2;
    let pinned = lillux::observe_open_file_identity(&pin)?;
    let current = directory
        .entry_no_follow(name)?
        .context("thread-history discard marker disappeared during pinned intake")?;
    anyhow::ensure!(
        current.entry_type == lillux::PinnedEntryType::Regular
            && current.containing_device == pinned.device()
            && current.inode == pinned.inode(),
        "thread-history discard marker changed during pinned intake"
    );
    Ok(Some((pin, marker)))
}

/// Thread-projection children that hold State's durable recovery records.
const THREAD_PROJECTION_RECORD_DIRECTORIES: [&str; 3] =
    ["pending", "staged-cas-roots", "durable-cas-uploads"];

/// Remove interrupted State-owned secure publications from State's recovery
/// directory, its thread-projection directory and that directory's record
/// children. The caller holds the exclusive runtime-state namespace lock, so
/// no State writer can still own one of these temporaries. Debris carrying
/// another owner's token is left in place; a prefixed name without an exact
/// owner token cannot be attributed and refuses startup.
pub(crate) fn sweep_interrupted_state_publications(
    recovery_parent: &lillux::PinnedDirectory,
    thread_projection: &lillux::PinnedDirectory,
) -> Result<usize> {
    let mut removed = sweep_state_temporaries_in(recovery_parent)?;
    removed += sweep_state_temporaries_in(thread_projection)?;
    for child in THREAD_PROJECTION_RECORD_DIRECTORIES {
        let directory = thread_projection.open_child_directory(std::ffi::OsStr::new(child))?;
        if let Some(directory) = directory {
            removed += sweep_state_temporaries_in(&directory)?;
        }
    }
    Ok(removed)
}

fn sweep_state_temporaries_in(directory: &lillux::PinnedDirectory) -> Result<usize> {
    let mut removed = 0;
    for name in directory.entry_names()? {
        let path = directory.path().join(&name);
        match lillux::classify_secure_temp_name(&name, crate::STATE_TEMP_PURPOSE) {
            lillux::SecureTempOwnership::NotTemp | lillux::SecureTempOwnership::Foreign => {}
            lillux::SecureTempOwnership::Own => {
                let file = directory.open_regular(&name, false)?.with_context(|| {
                    format!("interrupted State publication disappeared: {}", path.display())
                })?;
                directory.remove_if_same(&name, &file).with_context(|| {
                    format!("remove interrupted State publication {}", path.display())
                })?;
                removed += 1;
            }
            lillux::SecureTempOwnership::Malformed => anyhow::bail!(
                "State recovery directory holds a secure temporary without an exact owner token: {}; \
                 stop the node, remove that entry, then start it again",
                path.display()
            ),
        }
    }
    Ok(removed)
}

/// Guarded evidence read. With the owning native factory the read runs in
/// that factory's scope, so its own held projection leases answer for their
/// owner; without one the read refuses whenever any lease is listed.
pub(crate) fn guarded_evidence_read(
    factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    maximum_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    match factory {
        Some(factory) => factory.read_regular_without_sqlite_alias(directory, name, maximum_bytes),
        None => lillux_sqlite::read_regular_without_sqlite_alias(directory, name, maximum_bytes),
    }
}

fn read_thread_history_discard_operation(
    directory: &lillux::PinnedDirectory,
) -> Result<Option<ThreadHistoryDiscardOperationRef>> {
    read_thread_history_discard_operation_in(None, directory)
}

/// The in-progress operation: only an Active marker names one. An
/// Acknowledged marker is the completed record of the last discard.
fn read_thread_history_discard_operation_in(
    factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    directory: &lillux::PinnedDirectory,
) -> Result<Option<ThreadHistoryDiscardOperationRef>> {
    let mut work = AcknowledgementWork::default();
    Ok(
        read_thread_history_discard_marker_in(factory, directory, &mut work)?
            .filter(|marker| marker.phase == ThreadHistoryDiscardPhase::Active)
            .map(|marker| marker.operation),
    )
}

fn read_thread_history_discard_marker_in(
    factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    directory: &lillux::PinnedDirectory,
    work: &mut AcknowledgementWork,
) -> Result<Option<ThreadHistoryDiscardMarker>> {
    let name = std::ffi::OsStr::new(THREAD_HISTORY_DISCARD_MARKER);
    work.steps += 1;
    if directory.entry_no_follow(name)?.is_none() {
        return Ok(None);
    }
    work.steps += 1;
    let bytes = guarded_evidence_read(
        factory,
        directory,
        name,
        THREAD_HISTORY_DISCARD_MARKER_MAX_BYTES,
    )?
    .context("thread-history discard marker disappeared during intake")?;
    work.read_bytes += bytes.len() as u64 + 1;
    work.memory_bytes += bytes.capacity() as u64;
    ThreadHistoryDiscardMarker::refuse_retired_schema(&bytes)?;
    let marker: ThreadHistoryDiscardMarker =
        serde_json::from_slice(&bytes).context("decode strict thread-history discard marker")?;
    work.memory_bytes += (std::mem::size_of::<ThreadHistoryDiscardMarker>()
        + marker.kind.capacity()
        + marker.operation.operation_id.capacity()
        + marker.operation.sha256.capacity()) as u64;
    let canonical = marker.canonical_bytes()?;
    work.memory_bytes += canonical.capacity() as u64;
    anyhow::ensure!(
        canonical == bytes,
        "thread-history discard marker is not canonical"
    );
    Ok(Some(marker))
}

fn read_workspace_journal_key(file: &lillux::PinnedRegularFile) -> Result<[u8; 32]> {
    let bytes = file.read_bounded(33)?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "node-owned workspace journal key has invalid length {}",
            bytes.len()
        );
    }
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("workspace journal key length changed during validation"))
}

const THREAD_HISTORY_DISCARD_MARKER: &str = "thread-history-discard.json";
const THREAD_HISTORY_DISCARD_MARKER_MAX_BYTES: usize = 512;
const THREAD_HISTORY_DISCARD_MARKER_KIND: &str = "thread_history_discard";
const THREAD_HISTORY_DISCARD_MARKER_SCHEMA: u32 = 3;
/// The marker schema before the Active/Acknowledged phase existed. It is
/// refused, never upgraded.
const RETIRED_THREAD_HISTORY_DISCARD_MARKER_SCHEMA: u32 = 2;

/// The explicit repair that clears execution-history reset scratch and a
/// retired thread-history discard marker.
pub const EXECUTION_HISTORY_SCRATCH_REPAIR: &str =
    "ryeos node reset execution-history-scratch --confirm";

/// A thread-history discard marker written under a retired schema. Startup
/// and reset refuse it; only the explicit scratch repair clears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredThreadHistoryDiscardMarker {
    pub schema: u32,
}

impl std::fmt::Display for RetiredThreadHistoryDiscardMarker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "thread-history discard marker uses retired schema {}; stop the node and run `{}` to clear it",
            self.schema, EXECUTION_HISTORY_SCRATCH_REPAIR
        )
    }
}

impl std::error::Error for RetiredThreadHistoryDiscardMarker {}

/// A thread-history discard acknowledgement that no retry can complete: the
/// marker it would acknowledge is gone, or names another operation. Retrying
/// the same acknowledgement only repeats the refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadHistoryDiscardAcknowledgementRefused {
    /// No discard marker exists to acknowledge.
    Absent,
    /// The marker belongs to another operation.
    OtherOperation {
        /// Whether that marker is the acknowledged record of a completed
        /// discard rather than an active one.
        acknowledged: bool,
        operation_id: String,
    },
}

impl std::fmt::Display for ThreadHistoryDiscardAcknowledgementRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => formatter
                .write_str("thread-history discard acknowledgement found no marker to acknowledge"),
            Self::OtherOperation { acknowledged, operation_id } => write!(
                formatter,
                "thread-history discard acknowledgement differs from its marker's operation (the {} marker names {operation_id})",
                if *acknowledged { "acknowledged" } else { "active" }
            ),
        }
    }
}

impl std::error::Error for ThreadHistoryDiscardAcknowledgementRefused {}

/// Opaque activation reference. Shape validation is not operation authority:
/// App must authenticate the staged signed bytes, options and original owners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadHistoryDiscardOperationRef {
    pub operation_id: String,
    pub sha256: String,
}

impl ThreadHistoryDiscardOperationRef {
    pub fn validate(&self) -> Result<()> {
        for (label, value, length) in [
            ("operation id", self.operation_id.as_str(), 32),
            ("signed operation digest", self.sha256.as_str(), 64),
        ] {
            anyhow::ensure!(
                value.len() == length
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "invalid thread-history discard {label}"
            );
        }
        Ok(())
    }
}

/// Whether a discard is still in progress or is the acknowledged record of
/// the last completed discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ThreadHistoryDiscardPhase {
    Active,
    Acknowledged,
}

impl ThreadHistoryDiscardPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Acknowledged => "acknowledged",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadHistoryDiscardMarker {
    kind: String,
    schema: u32,
    phase: ThreadHistoryDiscardPhase,
    operation: ThreadHistoryDiscardOperationRef,
}

/// Only the schema number of a marker of any generation.
#[derive(Deserialize)]
struct ThreadHistoryDiscardMarkerSchemaProbe {
    schema: u32,
}

impl ThreadHistoryDiscardMarker {
    fn for_operation(
        operation: &ThreadHistoryDiscardOperationRef,
        phase: ThreadHistoryDiscardPhase,
    ) -> Result<Self> {
        operation.validate()?;
        Ok(Self {
            kind: THREAD_HISTORY_DISCARD_MARKER_KIND.to_owned(),
            schema: THREAD_HISTORY_DISCARD_MARKER_SCHEMA,
            phase,
            operation: operation.clone(),
        })
    }

    /// Refuse a retired-schema marker with its typed error before strict
    /// decoding reports it as merely malformed.
    fn refuse_retired_schema(bytes: &[u8]) -> Result<()> {
        if let Ok(probe) = serde_json::from_slice::<ThreadHistoryDiscardMarkerSchemaProbe>(bytes)
            && probe.schema == RETIRED_THREAD_HISTORY_DISCARD_MARKER_SCHEMA
        {
            return Err(RetiredThreadHistoryDiscardMarker {
                schema: probe.schema,
            }
            .into());
        }
        Ok(())
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        anyhow::ensure!(
            self.kind == THREAD_HISTORY_DISCARD_MARKER_KIND
                && self.schema == THREAD_HISTORY_DISCARD_MARKER_SCHEMA,
            "unsupported thread-history discard marker"
        );
        self.operation.validate()?;
        // This validated schema has only a fixed ASCII kind, schema and phase
        // and two exact hexadecimal strings. Write its canonical sorted-key
        // JSON directly into one bounded buffer, without intermediate maps.
        let mut bytes = Vec::with_capacity(THREAD_HISTORY_DISCARD_MARKER_MAX_BYTES);
        bytes.extend_from_slice(b"{\"kind\":\"");
        bytes.extend_from_slice(THREAD_HISTORY_DISCARD_MARKER_KIND.as_bytes());
        bytes.extend_from_slice(b"\",\"operation\":{\"operation_id\":\"");
        bytes.extend_from_slice(self.operation.operation_id.as_bytes());
        bytes.extend_from_slice(b"\",\"sha256\":\"");
        bytes.extend_from_slice(self.operation.sha256.as_bytes());
        bytes.extend_from_slice(b"\"},\"phase\":\"");
        bytes.extend_from_slice(self.phase.as_str().as_bytes());
        bytes.extend_from_slice(b"\",\"schema\":");
        bytes.extend_from_slice(THREAD_HISTORY_DISCARD_MARKER_SCHEMA.to_string().as_bytes());
        bytes.push(b'}');
        anyhow::ensure!(
            bytes.len() <= THREAD_HISTORY_DISCARD_MARKER_MAX_BYTES,
            "thread-history discard marker exceeds its byte bound"
        );
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagedCasRootsRecord {
    schema: u32,
    lease_id: String,
    owner_pid: u32,
    purpose: String,
    created_at: String,
    object_hashes: BTreeSet<String>,
    blob_hashes: BTreeSet<String>,
    large_object_hashes: BTreeSet<String>,
}

/// Typed, canonical publication namespace bound to one durable upload stage.
/// The wire deliberately stores the ref components separately rather than an
/// opaque delimiter-encoded string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableCasPublicationKey {
    ProjectHead {
        principal_key: String,
        project_hash: String,
    },
    ExternalContentImport {
        request_digest: String,
    },
    ExternalCandidateOccurrence {
        binding_digest: String,
        occurrence_digest: String,
    },
    /// Finite Q evidence retention, never a candidate snapshot/export receipt.
    QFixtureOccurrence {
        binding_digest: String,
        completion_frame_digest: String,
    },
    /// Exact controller-local source preparation. This is neither an
    /// execution capture nor an external-content import; the coordinate must
    /// bind recipe, installed source and admitted operator before publication.
    GuestRuntimeMaterialization {
        binding_digest: String,
        source_coordinate_digest: String,
    },
    /// Exact signed lifecycle adapter generation held for historical cleanup.
    /// Publication does not admit this generation for fresh placement work.
    RetainedLifecycleArtifacts {
        bundle_manifest_digest: String,
        /// Digest of the complete captured declaration: every executable,
        /// signed executor proof, and provider/snapshot spec. One member's
        /// digest cannot stand in for the historical cleanup generation.
        artifact_set_digest: String,
    },
    /// One bundle-catalog upload/admission decision. This is deliberately
    /// distinct from project publication: a catalog never fabricates a project
    /// path and stale policy authority cannot be replayed after reconfiguration.
    BundleCatalog {
        publisher_fingerprint: String,
        catalog_namespace: String,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        observed_predecessor: Option<String>,
        policy_section_digest: String,
        node_policy_generation_digest: String,
    },
}

impl DurableCasPublicationKey {
    pub fn project_head(principal_key: &str, project_hash: &str) -> Result<Self> {
        let key = Self::ProjectHead {
            principal_key: principal_key.to_string(),
            project_hash: project_hash.to_string(),
        };
        key.validate()?;
        Ok(key)
    }

    pub fn external_content_import(request_digest: &str) -> Result<Self> {
        let key = Self::ExternalContentImport {
            request_digest: request_digest.to_owned(),
        };
        key.validate()?;
        Ok(key)
    }

    pub fn external_candidate_occurrence(
        binding_digest: &str,
        occurrence_digest: &str,
    ) -> Result<Self> {
        let key = Self::ExternalCandidateOccurrence {
            binding_digest: binding_digest.to_owned(),
            occurrence_digest: occurrence_digest.to_owned(),
        };
        key.validate()?;
        Ok(key)
    }

    pub fn qualification_fixture_occurrence(
        binding_digest: &str,
        completion_frame_digest: &str,
    ) -> Result<Self> {
        let key = Self::QFixtureOccurrence {
            binding_digest: binding_digest.into(),
            completion_frame_digest: completion_frame_digest.into(),
        };
        key.validate()?;
        Ok(key)
    }

    pub fn guest_runtime_materialization(
        binding_digest: &str,
        source_coordinate_digest: &str,
    ) -> Result<Self> {
        let key = Self::GuestRuntimeMaterialization {
            binding_digest: binding_digest.to_owned(),
            source_coordinate_digest: source_coordinate_digest.to_owned(),
        };
        key.validate()?;
        Ok(key)
    }

    pub fn retained_lifecycle_artifacts(
        bundle_manifest_digest: &str,
        artifact_set_digest: &str,
    ) -> Result<Self> {
        let key = Self::RetainedLifecycleArtifacts {
            bundle_manifest_digest: bundle_manifest_digest.to_owned(),
            artifact_set_digest: artifact_set_digest.to_owned(),
        };
        key.validate()?;
        Ok(key)
    }

    pub fn bundle_catalog(
        publisher_fingerprint: &str,
        catalog_namespace: &str,
        observed_predecessor: Option<&str>,
        policy_section_digest: &str,
        node_policy_generation_digest: &str,
    ) -> Result<Self> {
        let key = Self::BundleCatalog {
            publisher_fingerprint: publisher_fingerprint.to_owned(),
            catalog_namespace: catalog_namespace.to_owned(),
            observed_predecessor: observed_predecessor.map(str::to_owned),
            policy_section_digest: policy_section_digest.to_owned(),
            node_policy_generation_digest: node_policy_generation_digest.to_owned(),
        };
        key.validate()?;
        Ok(key)
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::ProjectHead {
                principal_key,
                project_hash,
            } => {
                validate_hash("durable upload project principal key", principal_key)?;
                validate_hash("durable upload project hash", project_hash)
            }
            Self::ExternalContentImport { request_digest } => {
                validate_hash("external-content import request digest", request_digest)
            }
            Self::ExternalCandidateOccurrence {
                binding_digest,
                occurrence_digest,
            } => {
                validate_hash("external-candidate binding digest", binding_digest)?;
                validate_hash("external-candidate occurrence digest", occurrence_digest)
            }
            Self::QFixtureOccurrence {
                binding_digest,
                completion_frame_digest,
            } => {
                validate_hash("Q fixture binding", binding_digest)?;
                validate_hash("Q fixture completion", completion_frame_digest)
            }
            Self::GuestRuntimeMaterialization {
                binding_digest,
                source_coordinate_digest,
            } => {
                validate_hash(
                    "guest-runtime materialization binding digest",
                    binding_digest,
                )?;
                validate_hash(
                    "guest-runtime materialization source coordinate digest",
                    source_coordinate_digest,
                )
            }
            Self::RetainedLifecycleArtifacts {
                bundle_manifest_digest,
                artifact_set_digest,
            } => {
                validate_hash("retained lifecycle bundle manifest", bundle_manifest_digest)?;
                validate_hash(
                    "retained lifecycle complete artifact set",
                    artifact_set_digest,
                )
            }
            Self::BundleCatalog {
                publisher_fingerprint,
                catalog_namespace,
                observed_predecessor,
                policy_section_digest,
                node_policy_generation_digest,
            } => {
                validate_hash(
                    "bundle-catalog publisher fingerprint",
                    publisher_fingerprint,
                )?;
                validate_catalog_namespace(catalog_namespace)?;
                if let Some(predecessor) = observed_predecessor {
                    validate_hash("bundle-catalog observed predecessor", predecessor)?;
                }
                validate_hash(
                    "bundle-catalog policy-section digest",
                    policy_section_digest,
                )?;
                validate_hash(
                    "bundle-catalog node-policy generation digest",
                    node_policy_generation_digest,
                )
            }
        }
    }
}

fn validate_catalog_namespace(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        anyhow::bail!("bundle-catalog namespace must be a bounded lowercase identifier");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableCasUploadRecord {
    schema: u32,
    staging_id: String,
    owner_principal: String,
    purpose: String,
    publication_key: DurableCasPublicationKey,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    expected_previous_hash: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    admitted_target_hash: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    admitted_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    retained_at: Option<String>,
    created_at: String,
    object_hashes: BTreeSet<String>,
    blob_hashes: BTreeSet<String>,
    large_object_hashes: BTreeSet<String>,
}

/// Immutable coordinates of a retained external-candidate occurrence. The
/// corresponding recovery record remains a GC root until exact disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableExternalCandidateReceipt {
    staging_id: String,
    owner_principal: String,
    publication_key: DurableCasPublicationKey,
    snapshot_hash: String,
    object_hashes: BTreeSet<String>,
    blob_hashes: BTreeSet<String>,
    large_object_hashes: BTreeSet<String>,
}

impl DurableExternalCandidateReceipt {
    pub fn staging_id(&self) -> &str {
        &self.staging_id
    }

    pub fn owner_principal(&self) -> &str {
        &self.owner_principal
    }

    pub fn publication_key(&self) -> &DurableCasPublicationKey {
        &self.publication_key
    }

    pub fn snapshot_hash(&self) -> &str {
        &self.snapshot_hash
    }

    pub fn object_hashes(&self) -> &BTreeSet<String> {
        &self.object_hashes
    }

    pub fn blob_hashes(&self) -> &BTreeSet<String> {
        &self.blob_hashes
    }

    pub fn large_object_hashes(&self) -> &BTreeSet<String> {
        &self.large_object_hashes
    }
}

/// Exact non-expiring two-root Q receipt. It conveys no ordinary publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableQFixtureReceipt {
    pub staging_id: String,
    pub owner_principal: String,
    pub publication_key: DurableCasPublicationKey,
    pub metadata_hash: String,
    pub blob_hash: String,
}

#[derive(Debug, Clone, Default)]
pub struct StagedCasRootHashes {
    pub object_hashes: Vec<String>,
    pub blob_hashes: Vec<String>,
    pub large_object_hashes: Vec<String>,
}

/// Durable temporary GC roots for async CAS staging. The OS lock remains held
/// across awaits; hashes are added durably before their first CAS write.
pub struct StagedCasRootLease {
    recovery: RecoveryStore,
    directory: lillux::PinnedDirectory,
    record: StagedCasRootsRecord,
    lock_file: Option<File>,
    publication_uncertain: Cell<bool>,
}

/// Descriptor-owned temporary bytes associated with one staged-root lease.
/// Crash cleanup is keyed by the lease ID, so partial remote reads cannot
/// accumulate outside recovery's authoritative abandoned-lease scan.
pub struct StagedBlobPart {
    directory: lillux::PinnedDirectory,
    name: std::ffi::OsString,
    file: File,
}

impl StagedBlobPart {
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

/// Re-openable, principal-bound GC roots for a multi-request CAS publication.
/// Each handle owns only the per-stage filesystem lock for one synchronous
/// operation; dropping it keeps the durable roots live until explicit consume
/// or signed-policy retirement. An admitted stage becomes a durable receipt.
pub struct DurableCasUploadStage {
    recovery: RecoveryStore,
    directory: lillux::PinnedDirectory,
    record: DurableCasUploadRecord,
    lock_file: Option<File>,
    publication_uncertain: Cell<bool>,
}

impl DurableCasUploadStage {
    fn persist_record(&self, record: &DurableCasUploadRecord) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        let bytes = encode_durable_upload(record)?;
        // Validation refusals are reusable. Once I/O starts, even a failed
        // final directory sync may have published the new record. Keep the
        // lock but fence this handle; normal reopen resolves actual state.
        publish_staging_record(&self.directory, &record.staging_id, &bytes)
            .inspect_err(|_| self.publication_uncertain.set(true))
    }

    pub fn staging_id(&self) -> &str {
        &self.record.staging_id
    }

    pub fn owner_principal(&self) -> &str {
        &self.record.owner_principal
    }

    pub fn expected_previous_hash(&self) -> Option<&str> {
        self.record.expected_previous_hash.as_deref()
    }

    pub fn admitted_target_hash(&self) -> Option<&str> {
        self.record.admitted_target_hash.as_deref()
    }

    /// Exact currently protected roots. These are recovery evidence only;
    /// callers must still validate the referenced CAS closure independently.
    pub fn protected_object_hashes(&self) -> &BTreeSet<String> {
        &self.record.object_hashes
    }

    pub fn protected_blob_hashes(&self) -> &BTreeSet<String> {
        &self.record.blob_hashes
    }

    pub fn protected_large_object_hashes(&self) -> &BTreeSet<String> {
        &self.record.large_object_hashes
    }

    pub fn ensure_publication_contract(
        &self,
        publication_key: &DurableCasPublicationKey,
        expected_previous_hash: Option<&str>,
    ) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        if self.record.publication_key != *publication_key {
            anyhow::bail!("durable upload stage is bound to another publication target");
        }
        if self.record.expected_previous_hash.as_deref() != expected_previous_hash {
            anyhow::bail!("durable upload stage publication expectation does not match");
        }
        Ok(())
    }

    pub fn store_object(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        cas: &lillux::cas::CasStore,
        value: &serde_json::Value,
    ) -> Result<String> {
        self.ensure_active()?;
        let canonical =
            lillux::canonical_json(value).context("canonicalize durable staged CAS object")?;
        let expected = lillux::sha256_hex(canonical.as_bytes());
        self.recovery.ensure_guard(cas_mutation_guard)?;
        self.protect_object_hash_admitted(&expected)?;
        let stored = cas.store_object(value)?;
        if stored != expected {
            anyhow::bail!("durable staged object hash mismatch: expected {expected}, got {stored}");
        }
        Ok(stored)
    }

    pub fn store_blob(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        cas: &lillux::cas::CasStore,
        bytes: &[u8],
    ) -> Result<String> {
        self.ensure_active()?;
        let expected = lillux::sha256_hex(bytes);
        self.recovery.ensure_guard(cas_mutation_guard)?;
        self.protect_blob_hash_admitted(&expected)?;
        let stored = cas.store_blob(bytes)?;
        if stored != expected {
            anyhow::bail!("durable staged blob hash mismatch: expected {expected}, got {stored}");
        }
        Ok(stored)
    }

    /// Append one bounded chunk to a principal-bound durable blob upload.
    /// Chunks are sequential and retry-idempotent. The completed temporary
    /// file is streamed into CAS and its full digest is checked before the
    /// blob becomes a protected publication root.
    pub fn store_blob_chunk(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        cas: &lillux::cas::CasStore,
        expected_hash: &str,
        total_size: u64,
        offset: u64,
        bytes: &[u8],
    ) -> Result<bool> {
        self.ensure_active()?;
        validate_hash("durable staged blob", expected_hash)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        if let Some((mut existing, size)) = cas.open_blob(expected_hash)? {
            if size != total_size {
                anyhow::bail!(
                    "existing CAS blob {expected_hash} has size {size}, expected {total_size}"
                );
            }
            let mut digest = Sha256::new();
            let mut observed = 0_u64;
            let mut buffer = [0_u8; 128 * 1024];
            loop {
                let read = existing.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                observed = observed
                    .checked_add(u64::try_from(read)?)
                    .ok_or_else(|| anyhow::anyhow!("existing CAS blob size overflow"))?;
                digest.update(&buffer[..read]);
            }
            let actual = format!("{:x}", digest.finalize());
            if observed != total_size || actual != expected_hash {
                anyhow::bail!("existing CAS blob {expected_hash} failed full content verification");
            }
            self.protect_blob_hash_admitted(expected_hash)?;
            return Ok(true);
        }
        if bytes.is_empty() || offset > total_size {
            anyhow::bail!("durable blob chunk has an invalid empty/range boundary");
        }
        let chunk_size = u64::try_from(bytes.len()).context("blob chunk size overflow")?;
        let chunk_end = offset
            .checked_add(chunk_size)
            .ok_or_else(|| anyhow::anyhow!("blob chunk range overflow"))?;
        if chunk_end > total_size {
            anyhow::bail!("durable blob chunk extends beyond declared total size");
        }

        let parts_root = self
            .recovery
            .open_or_create_child_directory("durable-cas-upload-parts")?;
        let parts = parts_root
            .open_or_create_child(std::ffi::OsStr::new(&self.record.staging_id), 0o700)?;
        let mut part =
            parts.open_regular_create(std::ffi::OsStr::new(expected_hash), true, false, 0o600)?;
        let current = part.metadata()?.len();
        if current > offset {
            if current < chunk_end {
                anyhow::bail!("durable blob retry overlaps an incomplete prior chunk");
            }
            part.seek(std::io::SeekFrom::Start(offset))?;
            let mut prior = vec![0_u8; bytes.len()];
            part.read_exact(&mut prior)?;
            if prior != bytes {
                anyhow::bail!("durable blob retry bytes differ from the committed chunk");
            }
        } else if current == offset {
            part.seek(std::io::SeekFrom::End(0))?;
            part.write_all(bytes)?;
            part.sync_data()?;
            if current == 0 {
                // The first acknowledged chunk must include the newly-created
                // directory entry, not only its file data.
                parts.sync()?;
            }
        } else {
            anyhow::bail!(
                "durable blob chunk is out of order: expected offset {current}, got {offset}"
            );
        }

        if part.metadata()?.len() != total_size {
            return Ok(false);
        }
        part.seek(std::io::SeekFrom::Start(0))?;
        let completed_part = part.try_clone()?;
        let outcome = cas.put_blob_from_open_regular(part, &parts.path().join(expected_hash))?;
        if outcome.hash != expected_hash || outcome.size != total_size {
            anyhow::bail!(
                "durable staged blob mismatch: expected {expected_hash}/{total_size}, got {}/{}",
                outcome.hash,
                outcome.size
            );
        }
        self.protect_blob_hash_admitted(expected_hash)?;
        parts.remove_if_same(std::ffi::OsStr::new(expected_hash), &completed_part)?;
        Ok(true)
    }

    pub fn ensure_protects_object(&self, hash: &str) -> Result<()> {
        self.ensure_active()?;
        if !self.record.object_hashes.contains(hash) {
            anyhow::bail!("publication root {hash} was not uploaded under this staging capability");
        }
        Ok(())
    }

    pub fn ensure_protects_large_object(&self, hash: &str) -> Result<()> {
        self.ensure_active()?;
        validate_hash("durable staged large-object root", hash)?;
        if !self.record.large_object_hashes.contains(hash) {
            anyhow::bail!(
                "large-object root {hash} was not imported under this staging capability"
            );
        }
        Ok(())
    }

    pub fn protect_cas_closure<'a, O, B>(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        object_hashes: O,
        blob_hashes: B,
    ) -> Result<()>
    where
        O: IntoIterator<Item = &'a str>,
        B: IntoIterator<Item = &'a str>,
    {
        self.ensure_active()?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        let objects = object_hashes.into_iter().collect::<Vec<_>>();
        let blobs = blob_hashes.into_iter().collect::<Vec<_>>();
        for hash in &objects {
            validate_hash("durable staged object root", hash)?;
        }
        for hash in &blobs {
            validate_hash("durable staged blob root", hash)?;
        }
        let mut candidate = self.record.clone();
        let mut changed = false;
        for hash in objects {
            changed |= candidate.object_hashes.insert(hash.to_string());
        }
        for hash in blobs {
            changed |= candidate.blob_hashes.insert(hash.to_string());
        }
        if changed {
            self.persist_record(&candidate)?;
            self.record = candidate;
        }
        Ok(())
    }

    pub fn protect_large_object_hash(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        hash: &str,
    ) -> Result<()> {
        self.ensure_active()?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        validate_hash("durable staged large-object root", hash)?;
        if self.record.large_object_hashes.insert(hash.to_owned()) {
            if let Err(error) = self.persist_record(&self.record) {
                self.record.large_object_hashes.remove(hash);
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn finish_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        admitted_target_hash: &str,
    ) -> Result<()> {
        self.ensure_active()?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        if matches!(
            self.record.publication_key,
            DurableCasPublicationKey::QFixtureOccurrence { .. }
        ) {
            anyhow::bail!("Q fixture evidence requires retained two-root custody");
        }
        validate_hash("admitted durable upload target", admitted_target_hash)?;
        if !self.record.object_hashes.contains(admitted_target_hash) {
            anyhow::bail!("admitted publication target was not protected by the upload stage");
        }
        let mut candidate = self.record.clone();
        candidate.admitted_target_hash = Some(admitted_target_hash.to_string());
        candidate.admitted_at = Some(lillux::time::iso8601_now());
        candidate.retained_at = None;
        candidate.object_hashes.clear();
        candidate.blob_hashes.clear();
        candidate.large_object_hashes.clear();
        self.persist_record(&candidate)?;
        self.record = candidate;
        if let Err(error) = self
            .recovery
            .retire_durable_blob_parts(&self.record.staging_id)
        {
            tracing::warn!(
                staging_id = %self.record.staging_id,
                %error,
                "admitted upload retained empty blob-part staging for maintenance cleanup"
            );
        }
        drop(self.lock_file.take());
        Ok(())
    }

    /// Preserve exactly the Q metadata object and blob as non-expiring roots.
    /// This must precede committing a guest settlement row or signed frame.
    pub fn retain_qualification_fixture(
        &mut self,
        guard: &CasMutationGuard,
        reference: &crate::objects::qualification_fixture::QFixtureCaptureReference,
    ) -> Result<DurableQFixtureReceipt> {
        self.ensure_active()?;
        self.recovery.ensure_guard(guard)?;
        reference.validate()?;
        if !matches!(
            self.record.publication_key,
            DurableCasPublicationKey::QFixtureOccurrence { .. }
        ) || self.record.object_hashes != BTreeSet::from([reference.metadata_hash.clone()])
            || self.record.blob_hashes != BTreeSet::from([reference.blob_hash.clone()])
            || !self.record.large_object_hashes.is_empty()
            || self.record.expected_previous_hash.is_some()
        {
            anyhow::bail!("Q fixture retention changed its exact two-root contract");
        }
        let now = lillux::time::iso8601_now();
        let mut candidate = self.record.clone();
        candidate.admitted_target_hash = Some(reference.metadata_hash.clone());
        candidate.admitted_at = Some(now.clone());
        candidate.retained_at = Some(now);
        self.persist_record(&candidate)?;
        self.record = candidate;
        let receipt = self.qualification_fixture_receipt()?;
        drop(self.lock_file.take());
        Ok(receipt)
    }

    pub fn qualification_fixture_receipt(&self) -> Result<DurableQFixtureReceipt> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        validate_durable_upload_record(&self.record)?;
        if !matches!(
            self.record.publication_key,
            DurableCasPublicationKey::QFixtureOccurrence { .. }
        ) || self.record.retained_at.is_none()
        {
            anyhow::bail!("upload is not retained Q fixture evidence");
        }
        Ok(DurableQFixtureReceipt {
            staging_id: self.record.staging_id.clone(),
            owner_principal: self.record.owner_principal.clone(),
            publication_key: self.record.publication_key.clone(),
            metadata_hash: self
                .record
                .admitted_target_hash
                .clone()
                .context("Q fixture metadata is absent")?,
            blob_hash: self
                .record
                .blob_hashes
                .first()
                .context("Q fixture blob is absent")?
                .clone(),
        })
    }

    /// Convert an external-candidate staging capability into an immutable,
    /// non-expiring occurrence receipt while preserving its exact GC roots.
    /// Only an explicit, authenticated disposition may remove this receipt.
    pub fn retain_external_candidate_occurrence(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        snapshot_hash: &str,
    ) -> Result<DurableExternalCandidateReceipt> {
        self.ensure_active()?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        validate_hash("retained external-candidate snapshot", snapshot_hash)?;
        if !matches!(
            self.record.publication_key,
            DurableCasPublicationKey::ExternalCandidateOccurrence { .. }
        ) {
            anyhow::bail!("durable upload is not an external-candidate occurrence");
        }
        if !self.record.object_hashes.contains(snapshot_hash) {
            anyhow::bail!(
                "retained external-candidate snapshot was not protected by the occurrence stage"
            );
        }
        let retained_at = lillux::time::iso8601_now();
        let mut candidate = self.record.clone();
        candidate.admitted_target_hash = Some(snapshot_hash.to_owned());
        candidate.admitted_at = Some(retained_at.clone());
        candidate.retained_at = Some(retained_at);
        self.persist_record(&candidate)?;
        self.record = candidate;
        let receipt = self.external_candidate_receipt()?;
        if let Err(error) = self
            .recovery
            .retire_durable_blob_parts(&self.record.staging_id)
        {
            tracing::warn!(
                staging_id = %self.record.staging_id,
                %error,
                "retained external candidate kept empty blob-part staging for maintenance cleanup"
            );
        }
        drop(self.lock_file.take());
        Ok(receipt)
    }

    /// Reconstruct the exact immutable receipt after reopening it for the
    /// authenticated owner. Active uploads and ordinary admitted publications
    /// cannot masquerade as retained external candidates.
    pub fn external_candidate_receipt(&self) -> Result<DurableExternalCandidateReceipt> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        if !matches!(
            self.record.publication_key,
            DurableCasPublicationKey::ExternalCandidateOccurrence { .. }
        ) || self.record.retained_at.is_none()
        {
            anyhow::bail!("durable upload is not a retained external-candidate occurrence");
        }
        let snapshot_hash = self
            .record
            .admitted_target_hash
            .clone()
            .context("retained external-candidate occurrence has no snapshot")?;
        Ok(DurableExternalCandidateReceipt {
            staging_id: self.record.staging_id.clone(),
            owner_principal: self.record.owner_principal.clone(),
            publication_key: self.record.publication_key.clone(),
            snapshot_hash,
            object_hashes: self.record.object_hashes.clone(),
            blob_hashes: self.record.blob_hashes.clone(),
            large_object_hashes: self.record.large_object_hashes.clone(),
        })
    }

    /// Remove an exact retained occurrence after a durable downstream owner
    /// has accepted the same receipt. Consuming the handle prevents reuse.
    pub fn dispose_external_candidate_occurrence(
        mut self,
        cas_mutation_guard: &CasMutationGuard,
        expected: &DurableExternalCandidateReceipt,
    ) -> Result<()> {
        self.recovery.ensure_guard(cas_mutation_guard)?;
        let actual = self.external_candidate_receipt()?;
        if actual != *expected {
            anyhow::bail!("external-candidate disposition receipt does not match");
        }
        let record_name = format!("{}.json", self.record.staging_id);
        let lock_name = format!("{}.lock", self.record.staging_id);
        let record_file = self
            .directory
            .open_pinned_regular(std::ffi::OsStr::new(&record_name), false)?
            .context("retained external-candidate record is absent")?;
        let lock_file = self
            .directory
            .open_pinned_regular(std::ffi::OsStr::new(&lock_name), true)?
            .context("retained external-candidate lock is absent")?;
        self.recovery
            .retire_durable_blob_parts(&self.record.staging_id)?;
        self.directory
            .remove_pinned_regular_if_same(&record_file)
            .context("dispose retained external-candidate record")?;
        self.directory
            .remove_pinned_regular_if_same(&lock_file)
            .context("dispose retained external-candidate lock")?;
        self.directory.sync()?;
        drop(self.lock_file.take());
        Ok(())
    }

    fn ensure_active(&self) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        if let Some(target) = self.record.admitted_target_hash.as_deref() {
            anyhow::bail!(
                "durable upload stage is an admitted receipt for publication target {target}"
            );
        }
        Ok(())
    }

    fn protect_object_hash_admitted(&mut self, hash: &str) -> Result<()> {
        self.ensure_active()?;
        validate_hash("durable staged object root", hash)?;
        if self.record.object_hashes.insert(hash.to_string()) {
            if let Err(error) = self.persist_record(&self.record) {
                self.record.object_hashes.remove(hash);
                return Err(error);
            }
        }
        Ok(())
    }

    fn protect_blob_hash_admitted(&mut self, hash: &str) -> Result<()> {
        self.ensure_active()?;
        validate_hash("durable staged blob root", hash)?;
        if self.record.blob_hashes.insert(hash.to_string()) {
            if let Err(error) = self.persist_record(&self.record) {
                self.record.blob_hashes.remove(hash);
                return Err(error);
            }
        }
        Ok(())
    }
}

impl Drop for DurableCasUploadStage {
    fn drop(&mut self) {
        drop(self.lock_file.take());
    }
}

impl StagedCasRootLease {
    fn persist_record(&self, record: &StagedCasRootsRecord) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        let bytes = encode_staged_roots(record)?;
        publish_staging_record(&self.directory, &record.lease_id, &bytes)
            .inspect_err(|_| self.publication_uncertain.set(true))
    }

    pub fn open_blob_part(&self, expected_hash: &str) -> Result<StagedBlobPart> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        validate_hash("staged blob part", expected_hash)?;
        let root = self
            .recovery
            .open_or_create_child_directory("staged-cas-root-parts")?;
        let directory =
            root.open_or_create_child(std::ffi::OsStr::new(&self.record.lease_id), 0o700)?;
        let name = std::ffi::OsString::from(expected_hash);
        let file = directory.open_regular_create(&name, true, true, 0o600)?;
        Ok(StagedBlobPart {
            directory,
            name,
            file,
        })
    }

    pub fn publish_blob_part_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        cas: &lillux::cas::CasStore,
        mut part: StagedBlobPart,
        expected_hash: &str,
        expected_size: u64,
    ) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        part.file.sync_all()?;
        part.file.seek(std::io::SeekFrom::Start(0))?;
        let opened = part.file.try_clone()?;
        let outcome =
            cas.put_blob_from_open_regular(part.file, &part.directory.path().join(&part.name))?;
        if outcome.hash != expected_hash || outcome.size != expected_size {
            anyhow::bail!(
                "staged blob mismatch: expected {expected_hash}/{expected_size}, got {}/{}",
                outcome.hash,
                outcome.size
            );
        }
        self.protect_blob_hash_admitted(cas_mutation_guard, expected_hash)?;
        part.directory.remove_if_same(&part.name, &opened)?;
        Ok(())
    }

    pub fn store_object_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        cas: &lillux::cas::CasStore,
        value: &serde_json::Value,
    ) -> Result<String> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        let canonical = lillux::canonical_json(value).context("canonicalize staged CAS object")?;
        let expected = lillux::sha256_hex(canonical.as_bytes());
        self.protect_object_hash_admitted(cas_mutation_guard, &expected)?;
        let stored = cas.store_object(value)?;
        if stored != expected {
            anyhow::bail!("staged CAS object hash mismatch: expected {expected}, got {stored}");
        }
        Ok(stored)
    }

    /// Stage exact bounded canonical object bytes. The exact bytes whose hash
    /// is protected are the bytes persisted by CAS.
    pub fn store_object_bytes_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        cas: &lillux::cas::CasStore,
        canonical_bytes: &[u8],
        maximum_bytes: usize,
    ) -> Result<String> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        if canonical_bytes.len() > maximum_bytes {
            anyhow::bail!("staged canonical CAS object exceeds {maximum_bytes} bytes");
        }
        let expected = lillux::sha256_hex(canonical_bytes);
        self.protect_object_hash_admitted(cas_mutation_guard, &expected)?;
        let stored = cas.store_canonical_object_bytes_bounded(canonical_bytes, maximum_bytes)?;
        if stored != expected {
            anyhow::bail!("staged CAS object hash mismatch: expected {expected}, got {stored}");
        }
        Ok(stored)
    }

    pub fn store_blob_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        cas: &lillux::cas::CasStore,
        bytes: &[u8],
    ) -> Result<String> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        let expected = lillux::sha256_hex(bytes);
        self.protect_blob_hash_admitted(cas_mutation_guard, &expected)?;
        let stored = cas.store_blob(bytes)?;
        if stored != expected {
            anyhow::bail!("staged CAS blob hash mismatch: expected {expected}, got {stored}");
        }
        Ok(stored)
    }

    pub fn protect_object_hash_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        hash: &str,
    ) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        validate_hash("staged CAS object root", hash)?;
        if self.record.object_hashes.insert(hash.to_string()) {
            if let Err(error) = self.persist_record(&self.record) {
                self.record.object_hashes.remove(hash);
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn protect_blob_hash_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        hash: &str,
    ) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        validate_hash("staged CAS blob root", hash)?;
        if self.record.blob_hashes.insert(hash.to_string()) {
            if let Err(error) = self.persist_record(&self.record) {
                self.record.blob_hashes.remove(hash);
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn protect_large_object_hash_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        hash: &str,
    ) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        validate_hash("staged large-object root", hash)?;
        if self.record.large_object_hashes.insert(hash.to_owned()) {
            if let Err(error) = self.persist_record(&self.record) {
                self.record.large_object_hashes.remove(hash);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Durably protect a complete verified object/blob closure in one record
    /// publication. Callers that verified a head closure while holding the CAS
    /// mutation guard use this before releasing that guard, so omitted but
    /// already-local descendants cannot be swept between staging and publish.
    pub fn protect_cas_closure_admitted<'a, O, B>(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        object_hashes: O,
        blob_hashes: B,
    ) -> Result<()>
    where
        O: IntoIterator<Item = &'a str>,
        B: IntoIterator<Item = &'a str>,
    {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        self.recovery.ensure_guard(cas_mutation_guard)?;
        let object_hashes = object_hashes.into_iter().collect::<Vec<_>>();
        let blob_hashes = blob_hashes.into_iter().collect::<Vec<_>>();
        for hash in &object_hashes {
            validate_hash("staged CAS object root", hash)?;
        }
        for hash in &blob_hashes {
            validate_hash("staged CAS blob root", hash)?;
        }

        let mut candidate = self.record.clone();
        let mut changed = false;
        for hash in object_hashes {
            changed |= candidate.object_hashes.insert(hash.to_string());
        }
        for hash in blob_hashes {
            changed |= candidate.blob_hashes.insert(hash.to_string());
        }
        if changed {
            self.persist_record(&candidate)?;
            self.record = candidate;
        }
        Ok(())
    }

    /// Release this lease using an already-held CAS mutation guard. This is
    /// the admitted path for callers that also hold a later lock in the global
    /// mutation hierarchy (for example the daemon StateStore mutex).
    pub fn finish_admitted(&mut self, cas_mutation_guard: &CasMutationGuard) -> Result<()> {
        ensure_staging_publication_certain(&self.publication_uncertain)?;
        if let Err(error) = self.recovery.ensure_guard(cas_mutation_guard) {
            // Never let Drop reacquire the first lock while a caller may hold
            // later hierarchy locks. The durable record becomes an abandoned
            // lease and the next exclusive GC pass reclaims it safely.
            drop(self.lock_file.take());
            return Err(error);
        }
        let result = self.release_inner_admitted();
        if result.is_err() {
            // As above, preserve any remaining record/lock paths for the
            // recovery scanner instead of retrying through Drop.
            drop(self.lock_file.take());
        }
        result
    }

    fn release_inner_admitted(&mut self) -> Result<()> {
        if self.lock_file.is_none() {
            return Ok(());
        }
        self.recovery
            .retire_staged_blob_parts(&self.record.lease_id)?;
        let record_name = format!("{}.json", self.record.lease_id);
        if let Some(record_file) = self
            .directory
            .open_regular(std::ffi::OsStr::new(&record_name), false)?
        {
            self.directory
                .remove_if_same(std::ffi::OsStr::new(&record_name), &record_file)
                .context("release staged CAS roots")?;
        }
        let lock_name = format!("{}.lock", self.record.lease_id);
        let lock_file = self.lock_file.take().expect("lease lock checked above");
        self.directory
            .remove_if_same(std::ffi::OsStr::new(&lock_name), &lock_file)
            .context("remove staged CAS root lock")?;
        drop(lock_file);
        Ok(())
    }
}

impl Drop for StagedCasRootLease {
    fn drop(&mut self) {
        // Dropping a lease never reopens a path and never removes its durable
        // roots. Without an explicitly admitted finish, leave the record for
        // the exclusive GC recovery pass and only release the live lease lock.
        drop(self.lock_file.take());
    }
}

/// Deterministic scan of the pending-transition set. Startup recovery
/// processes canonical filenames in lexical chain-ID order; it never inherits
/// filesystem `read_dir` ordering.
pub struct PendingTransitionCursor {
    directory: Option<lillux::PinnedDirectory>,
    entries: VecDeque<std::ffi::OsString>,
    operation: Option<HeadOperation>,
}

impl PendingTransitionCursor {
    pub fn next_transition(&mut self) -> Result<Option<PendingChainHeadTransition>> {
        self.next_transition_checked(|| Ok(()))
    }

    pub(crate) fn entry_count(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn next_transition_checked(
        &mut self,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<Option<PendingChainHeadTransition>> {
        loop {
            check()?;
            let Some(name) = self.entries.pop_front() else {
                return Ok(None);
            };
            let directory = self
                .directory
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("pending cursor lost its pinned directory"))?;
            let path = directory.path().join(&name);
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                anyhow::bail!("unexpected pending transition file: {}", path.display());
            }
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 pending transition filename"))?;
            validate_chain_root_id(stem)?;
            let Some(file) = directory.open_regular(&name, false)? else {
                // A concurrent compare-ack may remove an entry after read_dir
                // yielded it. That transition is already complete.
                continue;
            };
            let record = decode_pending_file(file, stem)?;
            if self
                .operation
                .is_none_or(|operation| operation == record.operation)
            {
                return Ok(Some(record));
            }
        }
    }
}

/// Cross-process exclusion between immutable-CAS/head publishers (shared) and
/// mark-and-sweep (exclusive). Publishers acquire this before their first CAS
/// write and hold it through durable head publication.
pub struct CasMutationGuard {
    _file: Rc<File>,
    // A CAS guard owns a descriptor-backed flock. Keep direct-attachment
    // forks quiesced until the last clone of that lock authority is released,
    // regardless of which application layer acquired the guard.
    _fork_sensitive_descriptors: lillux::ForkSensitiveDescriptorLease,
    exclusive: bool,
    depth: GuardDepth,
    _not_send: std::marker::PhantomData<Rc<()>>,
}

impl CasMutationGuard {
    /// Initialize the minimum recovery generation in a fresh, unpublished
    /// private runtime without acquiring process-local guard authority. This
    /// includes both the mutation-lock anchor and the descriptor-pinned
    /// thread-projection root used by durable CAS publication stages.
    ///
    /// This operation exists for descriptor-held runtime construction: the
    /// caller exclusively owns a runtime that has not yet been handed to any
    /// reader or publisher, while it may already hold a guard for a distinct
    /// source runtime. Published runtimes must use the ordinary guard
    /// acquisition APIs instead.
    pub fn initialize_fresh_recovery_in_pinned_runtime(
        runtime: &lillux::PinnedDirectory,
    ) -> Result<()> {
        runtime.require_owner_private_directory()?;
        let recovery_name = std::ffi::OsStr::new("recovery");
        let recovery = runtime.create_child(recovery_name, 0o700)?;
        let result = (|| -> Result<()> {
            let lock = recovery.open_regular_create(
                std::ffi::OsStr::new(CAS_MUTATION_LOCK_FILENAME),
                true,
                true,
                0o600,
            )?;
            let thread_projection =
                recovery.create_child(std::ffi::OsStr::new("thread-projection"), 0o700)?;
            lock.sync_all()?;
            thread_projection.sync()?;
            recovery.sync()?;
            runtime.sync()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = recovery.remove_contents_recursive();
            let _ = runtime
                .remove_empty_child_if_same(recovery_name, &recovery)
                .map(|receipt| receipt.is_some());
        }
        result
    }

    #[track_caller]
    pub(crate) fn acquire_existing_shared_in_pinned_runtime(
        runtime: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        let fork_sensitive_descriptors = lillux::try_retain_fork_sensitive_descriptors()?;
        let exclusive_depth = EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get);
        if exclusive_depth != 0 {
            let file = current_guard_file_for_pinned_runtime(runtime)?;
            EXCLUSIVE_CAS_GUARD_DEPTH.with(|depth| depth.set(exclusive_depth + 1));
            return Ok(Self {
                _file: file,
                _fork_sensitive_descriptors: fork_sensitive_descriptors,
                exclusive: false,
                depth: GuardDepth::Exclusive,
                _not_send: std::marker::PhantomData,
            });
        }
        let shared_depth = SHARED_CAS_GUARD_DEPTH.with(Cell::get);
        if shared_depth != 0 {
            let file = current_guard_file_for_pinned_runtime(runtime)?;
            SHARED_CAS_GUARD_DEPTH.with(|depth| depth.set(shared_depth + 1));
            return Ok(Self {
                _file: file,
                _fork_sensitive_descriptors: fork_sensitive_descriptors,
                exclusive: false,
                depth: GuardDepth::Shared,
                _not_send: std::marker::PhantomData,
            });
        }

        let recovery = runtime
            .open_child_directory(std::ffi::OsStr::new("recovery"))?
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock directory is absent"))?;
        let file = recovery
            .open_regular(std::ffi::OsStr::new(CAS_MUTATION_LOCK_FILENAME), true)?
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock is absent"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("acquire pinned CAS mutation/sweep lock");
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("CAS mutation/sweep locking is unavailable on this platform");
        let file = Rc::new(file);
        CAS_GUARD_FILE.with(|current| {
            let previous = current.borrow_mut().replace(Rc::clone(&file));
            debug_assert!(previous.is_none());
        });
        SHARED_CAS_GUARD_DEPTH.with(|depth| depth.set(1));
        Ok(Self {
            _file: file,
            _fork_sensitive_descriptors: fork_sensitive_descriptors,
            exclusive: false,
            depth: GuardDepth::Shared,
            _not_send: std::marker::PhantomData,
        })
    }

    /// The existing pinned shared authority, bounded by one original caller
    /// deadline. No path-based anchor, lock-content mutation or Send guard.
    #[track_caller]
    pub(crate) fn acquire_existing_shared_in_pinned_runtime_until(
        runtime: &lillux::PinnedDirectory,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Self> {
        let check_deadline = || -> Result<()> {
            if deadline.has_elapsed() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "pinned CAS shared guard deadline elapsed",
                )
                .into());
            }
            Ok(())
        };
        check_deadline()?;
        let fork_sensitive_descriptors = lillux::retain_fork_sensitive_descriptors_until(deadline)
            .context("retain pinned CAS guard descriptors before deadline")?;
        check_deadline()?;
        let exclusive_depth = EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get);
        if exclusive_depth != 0 {
            let file = current_guard_file_for_pinned_runtime(runtime)?;
            check_deadline()?;
            EXCLUSIVE_CAS_GUARD_DEPTH.with(|depth| depth.set(exclusive_depth + 1));
            let guard = Self {
                _file: file,
                _fork_sensitive_descriptors: fork_sensitive_descriptors,
                exclusive: false,
                depth: GuardDepth::Exclusive,
                _not_send: std::marker::PhantomData,
            };
            check_deadline()?;
            return Ok(guard);
        }
        let shared_depth = SHARED_CAS_GUARD_DEPTH.with(Cell::get);
        if shared_depth != 0 {
            let file = current_guard_file_for_pinned_runtime(runtime)?;
            check_deadline()?;
            SHARED_CAS_GUARD_DEPTH.with(|depth| depth.set(shared_depth + 1));
            let guard = Self {
                _file: file,
                _fork_sensitive_descriptors: fork_sensitive_descriptors,
                exclusive: false,
                depth: GuardDepth::Shared,
                _not_send: std::marker::PhantomData,
            };
            check_deadline()?;
            return Ok(guard);
        }

        let recovery = runtime
            .open_child_directory(std::ffi::OsStr::new("recovery"))?
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock directory is absent"))?;
        check_deadline()?;
        let file = recovery
            .open_regular(std::ffi::OsStr::new(CAS_MUTATION_LOCK_FILENAME), true)?
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock is absent"))?;
        check_deadline()?;
        let file = lillux::locks::acquire_existing_shared_file_until(file, deadline)
            .context("acquire pinned CAS mutation/sweep lock before deadline")?;
        check_deadline()?;
        let file = Rc::new(file);
        CAS_GUARD_FILE.with(|current| {
            let previous = current.borrow_mut().replace(Rc::clone(&file));
            debug_assert!(previous.is_none());
        });
        SHARED_CAS_GUARD_DEPTH.with(|depth| depth.set(1));
        let guard = Self {
            _file: file,
            _fork_sensitive_descriptors: fork_sensitive_descriptors,
            exclusive: false,
            depth: GuardDepth::Shared,
            _not_send: std::marker::PhantomData,
        };
        check_deadline()?;
        Ok(guard)
    }

    #[track_caller]
    pub(crate) fn acquire_exclusive_in_pinned_runtime(
        runtime: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        Self::acquire_exclusive_in_pinned_runtime_mode(runtime, true)
    }

    #[track_caller]
    pub(crate) fn acquire_existing_exclusive_in_pinned_runtime(
        runtime: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        Self::acquire_exclusive_in_pinned_runtime_mode(runtime, false)
    }

    #[track_caller]
    fn acquire_exclusive_in_pinned_runtime_mode(
        runtime: &lillux::PinnedDirectory,
        create: bool,
    ) -> Result<Self> {
        let fork_sensitive_descriptors = lillux::try_retain_fork_sensitive_descriptors()?;
        if EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get) != 0
            || SHARED_CAS_GUARD_DEPTH.with(Cell::get) != 0
        {
            anyhow::bail!("pinned CAS mutation guard cannot be acquired reentrantly");
        }
        let recovery_name = std::ffi::OsStr::new("recovery");
        let recovery = if create {
            runtime.open_or_create_child(recovery_name, 0o700)?
        } else {
            runtime
                .open_child_directory(recovery_name)?
                .ok_or_else(|| anyhow::anyhow!("CAS mutation lock directory is absent"))?
        };
        let lock_name = std::ffi::OsStr::new(CAS_MUTATION_LOCK_FILENAME);
        let file = if create {
            let file = recovery.open_regular_create(lock_name, true, false, 0o600)?;
            recovery.sync()?;
            file
        } else {
            recovery
                .open_regular(lock_name, false)?
                .ok_or_else(|| anyhow::anyhow!("CAS mutation lock is absent"))?
        };
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("acquire pinned CAS mutation/sweep lock");
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("CAS mutation/sweep locking is unavailable on this platform");
        let file = Rc::new(file);
        CAS_GUARD_FILE.with(|current| {
            let previous = current.borrow_mut().replace(Rc::clone(&file));
            debug_assert!(previous.is_none());
        });
        EXCLUSIVE_CAS_GUARD_DEPTH.with(|depth| depth.set(1));
        Ok(Self {
            _file: file,
            _fork_sensitive_descriptors: fork_sensitive_descriptors,
            exclusive: true,
            depth: GuardDepth::Exclusive,
            _not_send: std::marker::PhantomData,
        })
    }

    /// Establish the immutable-CAS/head mutation lock during ordinary mutable
    /// startup so later read-only maintenance can acquire it without creating
    /// authority state.
    pub fn ensure_anchor(runtime_state_dir: &Path) -> Result<()> {
        drop(Self::acquire(runtime_state_dir, false, true)?);
        Ok(())
    }

    #[track_caller]
    pub fn acquire_shared(runtime_state_dir: &Path) -> Result<Self> {
        Self::acquire(runtime_state_dir, false, true)
    }

    /// Acquire the established guard in shared mode without creating recovery
    /// state. GC dry-runs use this for per-chain consistency checks before the
    /// later exclusive sweep inspection.
    #[track_caller]
    pub fn acquire_existing_shared(runtime_state_dir: &Path) -> Result<Self> {
        Self::acquire(runtime_state_dir, false, false)
    }

    #[track_caller]
    pub fn acquire_exclusive(runtime_state_dir: &Path) -> Result<Self> {
        Self::acquire(runtime_state_dir, true, true)
    }

    /// Acquire the established guard without creating recovery state. Used by
    /// strict read-only verification, where absence is an error rather than an
    /// invitation to mutate the node merely by inspecting it.
    #[track_caller]
    pub fn acquire_existing_exclusive(runtime_state_dir: &Path) -> Result<Self> {
        Self::acquire(runtime_state_dir, true, false)
    }

    #[track_caller]
    pub fn shared_from_cas_root(cas_root: &Path) -> Result<Self> {
        let runtime_state_dir = cas_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("CAS root has no runtime-state parent"))?;
        Self::acquire_shared(runtime_state_dir)
    }

    #[track_caller]
    pub fn exclusive_from_cas_root(cas_root: &Path) -> Result<Self> {
        let runtime_state_dir = cas_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("CAS root has no runtime-state parent"))?;
        Self::acquire_exclusive(runtime_state_dir)
    }

    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }

    /// Prove that this already-held guard protects the StateDb owning
    /// `cas_root`. App-layer writers use this to preserve the global lock order
    /// instead of reacquiring the CAS/GC guard after taking their store mutex.
    pub fn ensure_protects_cas_root(&self, cas_root: &Path) -> Result<()> {
        let runtime_state_dir = cas_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("CAS root has no runtime-state parent"))?;
        self.ensure_protects_runtime_state_dir(runtime_state_dir)
    }

    pub(crate) fn ensure_protects_pinned_runtime(
        &self,
        runtime: &lillux::PinnedDirectory,
    ) -> Result<()> {
        let recovery = runtime
            .open_child_directory(std::ffi::OsStr::new("recovery"))?
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock directory is absent"))?;
        let expected = recovery
            .open_regular(std::ffi::OsStr::new(CAS_MUTATION_LOCK_FILENAME), false)?
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock is absent"))?;
        let expected = expected.metadata()?;
        let held = self._file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if expected.dev() != held.dev() || expected.ino() != held.ino() {
                anyhow::bail!(
                    "held CAS mutation guard does not protect pinned runtime {}",
                    runtime.path().display()
                );
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("CAS mutation/sweep locking is unavailable on this platform");
        Ok(())
    }

    fn ensure_protects_runtime_state_dir(&self, runtime_state_dir: &Path) -> Result<()> {
        let expected_path = runtime_state_dir.join(RECOVERY_DIRECTORY_NAME).join(CAS_MUTATION_LOCK_FILENAME);
        let expected = secure_open_regular(&expected_path, false)?
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock is absent"))?;
        let expected = expected
            .metadata()
            .with_context(|| format!("inspect CAS mutation lock {}", expected_path.display()))?;
        let held = self
            ._file
            .metadata()
            .context("inspect held CAS mutation lock")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if expected.dev() != held.dev() || expected.ino() != held.ino() {
                anyhow::bail!(
                    "held CAS mutation guard does not protect {}",
                    runtime_state_dir.display()
                );
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("CAS mutation/sweep locking is unavailable on this platform");
        Ok(())
    }

    #[track_caller]
    fn acquire(runtime_state_dir: &Path, exclusive: bool, create_if_missing: bool) -> Result<Self> {
        let fork_sensitive_descriptors = lillux::try_retain_fork_sensitive_descriptors()?;
        if exclusive {
            let exclusive_depth = EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get);
            if exclusive_depth != 0 {
                let file = current_guard_file_for(runtime_state_dir)?;
                EXCLUSIVE_CAS_GUARD_DEPTH.with(|depth| depth.set(exclusive_depth + 1));
                return Ok(Self {
                    _file: file,
                    _fork_sensitive_descriptors: fork_sensitive_descriptors,
                    exclusive: true,
                    depth: GuardDepth::Exclusive,
                    _not_send: std::marker::PhantomData,
                });
            }
            let shared_held = SHARED_CAS_GUARD_DEPTH.with(|depth| depth.get() != 0);
            if shared_held {
                anyhow::bail!(
                    "cannot acquire exclusive CAS sweep guard while shared guard is held"
                );
            }
        } else {
            let exclusive_depth = EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get);
            if exclusive_depth != 0 {
                let file = current_guard_file_for(runtime_state_dir)?;
                EXCLUSIVE_CAS_GUARD_DEPTH.with(|depth| depth.set(exclusive_depth + 1));
                return Ok(Self {
                    _file: file,
                    _fork_sensitive_descriptors: fork_sensitive_descriptors,
                    exclusive: false,
                    depth: GuardDepth::Exclusive,
                    _not_send: std::marker::PhantomData,
                });
            }
            let shared_depth = SHARED_CAS_GUARD_DEPTH.with(Cell::get);
            if shared_depth != 0 {
                let file = current_guard_file_for(runtime_state_dir)?;
                SHARED_CAS_GUARD_DEPTH.with(|depth| depth.set(shared_depth + 1));
                return Ok(Self {
                    _file: file,
                    _fork_sensitive_descriptors: fork_sensitive_descriptors,
                    exclusive: false,
                    depth: GuardDepth::Shared,
                    _not_send: std::marker::PhantomData,
                });
            }
        }

        let lock_path = runtime_state_dir.join(RECOVERY_DIRECTORY_NAME).join(CAS_MUTATION_LOCK_FILENAME);
        let parent_path = lock_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock has no parent"))?;
        let parent = if create_if_missing {
            lillux::PinnedDirectory::open_or_create(parent_path)?
        } else {
            lillux::PinnedDirectory::open(parent_path)?
                .ok_or_else(|| anyhow::anyhow!("CAS mutation lock directory is absent"))?
        };
        let file = if create_if_missing {
            parent.open_regular_create(
                std::ffi::OsStr::new(CAS_MUTATION_LOCK_FILENAME),
                true,
                false,
                0o600,
            )?
        } else {
            parent
                .open_regular(std::ffi::OsStr::new(CAS_MUTATION_LOCK_FILENAME), true)?
                .ok_or_else(|| anyhow::anyhow!("CAS mutation lock is absent"))?
        };
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let operation = if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            };
            if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("acquire CAS mutation/sweep lock");
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("CAS mutation/sweep locking is unavailable on this platform");
        let file = Rc::new(file);
        CAS_GUARD_FILE.with(|current| {
            let previous = current.borrow_mut().replace(Rc::clone(&file));
            debug_assert!(previous.is_none());
        });
        let depth = if exclusive {
            EXCLUSIVE_CAS_GUARD_DEPTH.with(|depth| depth.set(1));
            GuardDepth::Exclusive
        } else {
            SHARED_CAS_GUARD_DEPTH.with(|depth| depth.set(1));
            GuardDepth::Shared
        };
        Ok(Self {
            _file: file,
            _fork_sensitive_descriptors: fork_sensitive_descriptors,
            exclusive,
            depth,
            _not_send: std::marker::PhantomData,
        })
    }
}

impl Drop for CasMutationGuard {
    fn drop(&mut self) {
        let counter = match self.depth {
            GuardDepth::Shared => &SHARED_CAS_GUARD_DEPTH,
            GuardDepth::Exclusive => &EXCLUSIVE_CAS_GUARD_DEPTH,
        };
        counter.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0);
            let next = current.saturating_sub(1);
            depth.set(next);
            if next == 0 {
                CAS_GUARD_FILE.with(|file| {
                    file.borrow_mut().take();
                });
            }
        });
    }
}

fn current_guard_file_for(runtime_state_dir: &Path) -> Result<Rc<File>> {
    let held = CAS_GUARD_FILE.with(|file| {
        file.borrow()
            .as_ref()
            .cloned()
            .expect("reentrant CAS guard depth without lock file")
    });
    let expected_path = runtime_state_dir.join(RECOVERY_DIRECTORY_NAME).join(CAS_MUTATION_LOCK_FILENAME);
    let expected = secure_open_regular(&expected_path, false)?
        .ok_or_else(|| anyhow::anyhow!("CAS mutation lock is absent"))?;
    let expected = expected
        .metadata()
        .with_context(|| format!("inspect CAS mutation lock {}", expected_path.display()))?;
    let held_metadata = held
        .metadata()
        .context("inspect reentrant CAS mutation lock")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if expected.dev() != held_metadata.dev() || expected.ino() != held_metadata.ino() {
            anyhow::bail!(
                "nested CAS mutation guard targets a different runtime state root: {}",
                runtime_state_dir.display()
            );
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("CAS mutation/sweep locking is unavailable on this platform");
    Ok(held)
}

fn current_guard_file_for_pinned_runtime(runtime: &lillux::PinnedDirectory) -> Result<Rc<File>> {
    let held = CAS_GUARD_FILE.with(|file| {
        file.borrow()
            .as_ref()
            .cloned()
            .expect("reentrant CAS guard depth without lock file")
    });
    let recovery = runtime
        .open_child_directory(std::ffi::OsStr::new("recovery"))?
        .ok_or_else(|| anyhow::anyhow!("CAS mutation lock directory is absent"))?;
    let expected = recovery
        .open_regular(std::ffi::OsStr::new(CAS_MUTATION_LOCK_FILENAME), false)?
        .ok_or_else(|| anyhow::anyhow!("CAS mutation lock is absent"))?;
    let expected = expected
        .metadata()
        .context("inspect pinned CAS mutation lock")?;
    let held_metadata = held.metadata().context("inspect held CAS mutation lock")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if expected.dev() != held_metadata.dev() || expected.ino() != held_metadata.ino() {
            anyhow::bail!(
                "held CAS mutation guard does not protect pinned runtime {}",
                runtime.path().display()
            );
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("CAS mutation/sweep locking is unavailable on this platform");
    Ok(held)
}

impl RecoveryStore {
    /// Borrow the SAME pinned directory allocations while preserving the
    /// typed semantic recovery owner. This cannot acknowledge recovery or
    /// authorize descriptor retirement. Caller funds counted alias work first.
    pub(crate) fn borrow_original_directory_custody(&self)
        -> [std::sync::Arc<lillux::PinnedDirectory>; 2] {
        [std::sync::Arc::clone(&self.runtime_directory), std::sync::Arc::clone(&self.directory)]
    }

    /// Move the real pinned owners after the original controller has completed
    /// semantic recovery. No reopen, duplicate, close or retirement receipt.
    pub(crate) fn into_original_directory_custody(
        self,
    ) -> [std::sync::Arc<lillux::PinnedDirectory>; 2] {
        [self.runtime_directory, self.directory]
    }

    pub fn from_runtime_state_dir(runtime_state_dir: &Path) -> Result<Self> {
        let runtime = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
            .context("pin recovery runtime-state directory")?;
        let recovery = runtime
            .open_or_create_child(std::ffi::OsStr::new("recovery"), 0o700)
            .context("pin recovery parent directory")?;
        let directory = recovery
            .open_or_create_child(std::ffi::OsStr::new("thread-projection"), 0o700)
            .context("pin thread-projection recovery directory")?;
        Self::from_pinned_directories(runtime, directory)
    }

    pub(crate) fn from_pinned_directories(
        runtime_directory: lillux::PinnedDirectory,
        directory: lillux::PinnedDirectory,
    ) -> Result<Self> {
        let root = directory.path().to_path_buf();
        let expected_name = root.file_name().and_then(|name| name.to_str());
        if expected_name != Some("thread-projection") {
            anyhow::bail!(
                "invalid thread-projection recovery directory: {}",
                root.display()
            );
        }
        Ok(Self {
            root,
            runtime_directory: std::sync::Arc::new(runtime_directory),
            directory: std::sync::Arc::new(directory),
        })
    }

    pub fn from_refs_root(refs_root: &Path) -> Result<Self> {
        let runtime_state_dir = refs_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("refs root has no runtime-state parent"))?;
        let runtime = lillux::PinnedDirectory::open(runtime_state_dir)?
            .ok_or_else(|| anyhow::anyhow!("runtime-state directory is absent"))?;
        let recovery = runtime
            .open_child_directory(std::ffi::OsStr::new("recovery"))?
            .ok_or_else(|| anyhow::anyhow!("recovery parent directory is absent"))?;
        let directory = recovery
            .open_child_directory(std::ffi::OsStr::new("thread-projection"))?
            .ok_or_else(|| anyhow::anyhow!("thread-projection recovery directory is absent"))?;
        Self::from_pinned_directories(runtime, directory)
    }

    fn ensure_chain_lock(
        &self,
        chain_lock: &crate::chain::ChainLock,
        chain_root_id: &str,
    ) -> Result<()> {
        chain_lock.ensure_protects(&self.runtime_directory.path().join("refs"), chain_root_id)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Node-owned authentication key for workspace recovery journals. The
    /// journal itself lives beside the workspace bytes it describes so it can
    /// survive a daemon crash, but it is never trusted unless its canonical
    /// body authenticates under this key held outside project space.
    pub fn workspace_journal_auth_key(&self) -> Result<[u8; 32]> {
        use rand::RngCore as _;

        let name = std::ffi::OsStr::new(WORKSPACE_JOURNAL_KEY);
        if let Some(file) = self.directory.open_pinned_regular(name, false)? {
            return read_workspace_journal_key(&file);
        }

        let mut generated = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut generated);
        if self
            .directory
            .atomic_create_pinned_regular(name, &generated, 0o600, crate::STATE_TEMP_PURPOSE)?
            .is_some()
        {
            return Ok(generated);
        }
        let existing = self
            .directory
            .open_pinned_regular(name, false)?
            .ok_or_else(|| anyhow::anyhow!("workspace journal key creation raced with removal"))?;
        read_workspace_journal_key(&existing)
    }

    pub(crate) fn runtime_directory(&self) -> &lillux::PinnedDirectory {
        self.runtime_directory.as_ref()
    }

    pub fn ensure_guard(&self, cas_mutation_guard: &CasMutationGuard) -> Result<()> {
        cas_mutation_guard.ensure_protects_pinned_runtime(self.runtime_directory())
    }

    pub fn pending_dir(&self) -> PathBuf {
        self.root.join("pending")
    }

    pub fn generation_path(&self) -> PathBuf {
        self.root.join("generation.json")
    }

    /// Read the strict active operation locator, without opening an observer
    /// data descriptor that could close a live SQLite alias. This reference
    /// locates evidence; it does not authenticate App policy or signatures.
    pub fn thread_history_discard_operation(
        &self,
    ) -> Result<Option<ThreadHistoryDiscardOperationRef>> {
        read_thread_history_discard_operation(&self.directory)
    }

    /// The same locator read inside the owning native factory's scope, for
    /// callers that may hold projection leases.
    pub fn thread_history_discard_operation_scoped(
        &self,
        factory: &lillux_sqlite::OriginalNativeFactory,
    ) -> Result<Option<ThreadHistoryDiscardOperationRef>> {
        self.thread_history_discard_operation_in(Some(factory))
    }

    pub(crate) fn thread_history_discard_operation_in(
        &self,
        factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<Option<ThreadHistoryDiscardOperationRef>> {
        read_thread_history_discard_operation_in(factory, &self.directory)
    }

    /// Ordinary startup refuses an Active marker and any malformed,
    /// unsupported or retired-schema evidence. An Acknowledged marker is the
    /// record of a completed discard and does not block startup. Re-entry must
    /// verify the exact staged operation before a projection preview can
    /// manufacture new accounting.
    pub fn thread_history_discard_in_progress(&self) -> Result<bool> {
        Ok(self.thread_history_discard_operation()?.is_some())
    }

    /// The operation of the last completed discard, when the marker records
    /// one as Acknowledged. An Active marker returns `None`.
    pub fn last_acknowledged_thread_history_discard(
        &self,
    ) -> Result<Option<ThreadHistoryDiscardOperationRef>> {
        let mut work = AcknowledgementWork::default();
        Ok(
            read_thread_history_discard_marker_in(None, &self.directory, &mut work)?
                .filter(|marker| marker.phase == ThreadHistoryDiscardPhase::Acknowledged)
                .map(|marker| marker.operation),
        )
    }

    /// Remove a thread-history discard marker written under the retired
    /// schema, which every reader refuses. Returns that schema when such a
    /// marker is present; a current or absent marker is left alone and
    /// returns `None`. With `dry_run` nothing is removed.
    pub fn clear_retired_thread_history_discard_marker(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        dry_run: bool,
    ) -> Result<Option<u32>> {
        self.ensure_guard(cas_mutation_guard)?;
        let name = std::ffi::OsStr::new(THREAD_HISTORY_DISCARD_MARKER);
        let Some(pin) = self.directory.open_mount_entry(name)? else {
            return Ok(None);
        };
        let Some(bytes) =
            guarded_evidence_read(None, &self.directory, name, THREAD_HISTORY_DISCARD_MARKER_MAX_BYTES)?
        else {
            return Ok(None);
        };
        let retired = serde_json::from_slice::<ThreadHistoryDiscardMarkerSchemaProbe>(&bytes)
            .is_ok_and(|probe| probe.schema == RETIRED_THREAD_HISTORY_DISCARD_MARKER_SCHEMA);
        if !retired {
            return Ok(None);
        }
        if !dry_run {
            anyhow::ensure!(
                cas_mutation_guard.is_exclusive(),
                "clearing a retired thread-history discard marker requires the exclusive CAS guard"
            );
            self.directory
                .unlink_regular_if_same(name, &pin)
                .context("remove retired thread-history discard marker")?;
        }
        Ok(Some(RETIRED_THREAD_HISTORY_DISCARD_MARKER_SCHEMA))
    }

    /// Activate the already-staged immutable operation after actual CAS
    /// admission. An uncertain acknowledgement retries this same reference.
    /// The Acknowledged record of an earlier discard is replaced in one
    /// same-coordinate rename; a different Active operation is refused.
    pub(crate) fn begin_thread_history_discard(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        operation: &ThreadHistoryDiscardOperationRef,
    ) -> Result<()> {
        anyhow::ensure!(
            cas_mutation_guard.is_exclusive(),
            "thread-history discard requires the exclusive CAS guard"
        );
        self.ensure_guard(cas_mutation_guard)?;
        let bytes = ThreadHistoryDiscardMarker::for_operation(
            operation,
            ThreadHistoryDiscardPhase::Active,
        )?
        .canonical_bytes()?;
        let name = std::ffi::OsStr::new(THREAD_HISTORY_DISCARD_MARKER);
        let mut work = AcknowledgementWork::default();
        if let Some((pin, marker)) = pin_thread_history_discard_marker(&self.directory, &mut work)? {
            match marker.phase {
                ThreadHistoryDiscardPhase::Active => {
                    anyhow::ensure!(
                        &marker.operation == operation,
                        "thread-history discard marker activates a different operation"
                    );
                    return self
                        .directory
                        .sync()
                        .context("sync retained thread-history discard activation");
                }
                ThreadHistoryDiscardPhase::Acknowledged => {
                    // An acknowledged operation is finished and never revived.
                    anyhow::ensure!(
                        &marker.operation != operation,
                        "thread-history discard operation was already acknowledged"
                    );
                    self.directory
                        .atomic_write_if_same(
                            name,
                            Some(&pin),
                            &bytes,
                            0o600,
                            crate::STATE_TEMP_PURPOSE,
                        )
                        .context("replace acknowledged thread-history discard with activation")?;
                    return self
                        .directory
                        .sync()
                        .context("sync thread-history discard activation");
                }
            }
        }
        // The no-replace publisher never overwrites another operation. A
        // publication collision is accepted only after exact guarded intake.
        if self
            .directory
            .atomic_create_regular(name, &bytes, 0o600, crate::STATE_TEMP_PURPOSE)?
            .is_none()
        {
            anyhow::ensure!(
                self.thread_history_discard_operation()?.as_ref() == Some(operation),
                "thread-history discard marker appeared with different evidence"
            );
        }
        self.directory
            .sync()
            .context("sync thread-history discard activation")
    }

    /// Acknowledge only this exact operation after every participating owner
    /// is terminal. Keep the staged operation until the Acknowledged marker
    /// is durable. Its marker read is unscoped, so no projection lease may be
    /// held; outside tests the reset acknowledges through a closed State's
    /// final acknowledgement custody instead.
    #[cfg(test)]
    pub(crate) fn finish_thread_history_discard(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        operation: &ThreadHistoryDiscardOperationRef,
    ) -> Result<()> {
        self.final_ack_custody()
            .finish(cas_mutation_guard, operation)
    }

    /// Retain only the SAME real recovery/runtime directory owners for the
    /// bounded ACK effect. No File duplication or diagnostic path allocation.
    pub(crate) fn final_ack_custody(&self) -> RecoveryAcknowledgementCustody {
        RecoveryAcknowledgementCustody {
            runtime_directory: self.runtime_directory.clone(),
            directory: self.directory.clone(),
        }
    }

    /// Drop every chain-head recovery record as one part of an explicitly
    /// authorized all-history discard. The global state lock and exclusive CAS
    /// guard replace per-chain recovery semantics for this operation.
    pub(crate) fn discard_all_pending_transitions(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        dry_run: bool,
    ) -> Result<usize> {
        if !cas_mutation_guard.is_exclusive() {
            anyhow::bail!("thread-history discard requires the exclusive CAS guard");
        }
        self.ensure_guard(cas_mutation_guard)?;
        let Some(directory) = self.open_child_directory("pending")? else {
            return Ok(0);
        };
        let entries = directory.regular_files()?;
        for entry in &entries {
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                anyhow::bail!(
                    "unexpected pending transition file: {}",
                    entry.path().display()
                );
            }
            let chain_root_id = entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 pending transition filename"))?;
            validate_chain_root_id(chain_root_id)?;
        }
        if !dry_run {
            for entry in &entries {
                directory
                    .remove_pinned_regular_if_same(entry)
                    .with_context(|| {
                        format!("discard pending transition {}", entry.path().display())
                    })?;
            }
        }
        Ok(entries.len())
    }

    fn open_child_directory(&self, name: &str) -> Result<Option<lillux::PinnedDirectory>> {
        self.directory
            .open_child_directory(std::ffi::OsStr::new(name))
    }

    fn open_or_create_child_directory(&self, name: &str) -> Result<lillux::PinnedDirectory> {
        self.directory
            .open_or_create_child(std::ffi::OsStr::new(name), 0o700)
    }

    pub fn begin_staged_cas_roots_admitted(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        purpose: &str,
    ) -> Result<StagedCasRootLease> {
        if purpose.is_empty()
            || purpose.len() > 128
            || purpose.bytes().any(|byte| byte.is_ascii_control())
        {
            anyhow::bail!("invalid staged CAS root purpose");
        }
        self.ensure_guard(cas_mutation_guard)?;
        let staged_directory = self
            .open_or_create_child_directory("staged-cas-roots")
            .context("create staged CAS root directory")?;
        let lease_id = unique_id("staged");
        let lock_name = format!("{lease_id}.lock");
        let lock_file = staged_directory.open_regular_create(
            std::ffi::OsStr::new(&lock_name),
            true,
            true,
            0o600,
        )?;
        staged_directory.sync()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error()).context("lock staged CAS root lease");
            }
        }
        #[cfg(not(unix))]
        anyhow::bail!("staged CAS root locking is unavailable on this platform");
        let record = StagedCasRootsRecord {
            schema: STAGED_ROOTS_SCHEMA,
            lease_id,
            owner_pid: std::process::id(),
            purpose: purpose.to_string(),
            created_at: lillux::time::iso8601_now(),
            object_hashes: BTreeSet::new(),
            blob_hashes: BTreeSet::new(),
            large_object_hashes: BTreeSet::new(),
        };
        self.write_staged_roots(&staged_directory, &record)?;
        Ok(StagedCasRootLease {
            recovery: self.clone(),
            directory: staged_directory,
            record,
            lock_file: Some(lock_file),
            publication_uncertain: Cell::new(false),
        })
    }

    /// Mint a re-openable, principal-bound upload capability. Its empty root
    /// record is durable before the caller receives the ID.
    pub fn begin_durable_cas_upload_admitted(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        owner_principal: &str,
        purpose: &str,
        publication_key: &DurableCasPublicationKey,
        expected_previous_hash: Option<&str>,
    ) -> Result<DurableCasUploadStage> {
        validate_upload_owner(owner_principal)?;
        validate_staging_purpose(purpose)?;
        publication_key.validate()?;
        if let Some(hash) = expected_previous_hash {
            validate_hash("durable upload expected previous hash", hash)?;
        }
        self.ensure_guard(cas_mutation_guard)?;
        let durable_directory = self
            .open_or_create_child_directory("durable-cas-uploads")
            .context("create durable CAS upload directory")?;
        let staging_id = unique_id("upload");
        let lock_name = format!("{staging_id}.lock");
        let lock_file = durable_directory.open_regular_create(
            std::ffi::OsStr::new(&lock_name),
            true,
            true,
            0o600,
        )?;
        durable_directory.sync()?;
        lock_exclusive(&lock_file, "lock durable CAS upload")?;
        let record = DurableCasUploadRecord {
            schema: DURABLE_UPLOAD_SCHEMA,
            staging_id,
            owner_principal: owner_principal.to_string(),
            purpose: purpose.to_string(),
            publication_key: publication_key.clone(),
            expected_previous_hash: expected_previous_hash.map(str::to_string),
            admitted_target_hash: None,
            admitted_at: None,
            retained_at: None,
            created_at: lillux::time::iso8601_now(),
            object_hashes: BTreeSet::new(),
            blob_hashes: BTreeSet::new(),
            large_object_hashes: BTreeSet::new(),
        };
        self.write_durable_upload(&durable_directory, &record)?;
        Ok(DurableCasUploadStage {
            recovery: self.clone(),
            directory: durable_directory,
            record,
            lock_file: Some(lock_file),
            publication_uncertain: Cell::new(false),
        })
    }

    /// Re-open an existing upload capability for its authenticated owner. The
    /// per-stage lock serializes upload/finalize and idempotent retry requests.
    pub fn open_durable_cas_upload_admitted(
        &self,
        cas_mutation_guard: &CasMutationGuard,
        staging_id: &str,
        owner_principal: &str,
    ) -> Result<DurableCasUploadStage> {
        validate_instance_id(staging_id)?;
        validate_upload_owner(owner_principal)?;
        self.ensure_guard(cas_mutation_guard)?;
        let directory = self
            .open_child_directory("durable-cas-uploads")?
            .ok_or_else(|| anyhow::anyhow!("durable CAS upload directory is absent"))?;
        let lock_name = format!("{staging_id}.lock");
        let lock_file = directory
            .open_regular(std::ffi::OsStr::new(&lock_name), true)?
            .ok_or_else(|| anyhow::anyhow!("durable CAS upload lock is absent"))?;
        lock_exclusive(&lock_file, "lock durable CAS upload")?;
        let record_name = format!("{staging_id}.json");
        let record_path = directory.path().join(&record_name);
        let record_file = directory
            .open_regular(std::ffi::OsStr::new(&record_name), false)?
            .ok_or_else(|| anyhow::anyhow!("durable CAS upload record is absent"))?;
        let record = self.read_durable_upload_file(record_file, &record_path)?;
        if record.owner_principal != owner_principal {
            anyhow::bail!("durable CAS upload capability belongs to another principal");
        }
        Ok(DurableCasUploadStage {
            recovery: self.clone(),
            directory,
            record,
            lock_file: Some(lock_file),
            publication_uncertain: Cell::new(false),
        })
    }

    /// Retire abandoned durable upload stages selected by an authoritative
    /// maintenance policy. There is deliberately no built-in age: callers must
    /// pass the signed policy's canonical cutoff, and omission means retain.
    pub fn retire_durable_cas_uploads_created_before(
        &self,
        created_before: &str,
        cas_mutation_guard: &CasMutationGuard,
    ) -> Result<usize> {
        crate::objects::parse_canonical_timestamp(created_before)
            .context("durable upload retirement cutoff is not canonical")?;
        if !cas_mutation_guard.is_exclusive() {
            anyhow::bail!("durable upload retirement requires the exclusive CAS guard");
        }
        self.ensure_guard(cas_mutation_guard)?;
        let Some(directory) = self.open_child_directory("durable-cas-uploads")? else {
            return Ok(0);
        };
        let mut records = Vec::new();
        let mut locks = BTreeMap::new();
        for entry in directory.regular_files()? {
            match entry.path().extension().and_then(|value| value.to_str()) {
                Some("json") => records.push(entry),
                Some("lock") => {
                    locks.insert(entry.name().to_os_string(), entry);
                }
                _ => anyhow::bail!(
                    "unexpected durable CAS upload entry: {}",
                    entry.path().display()
                ),
            }
        }
        let mut retired = 0;
        for record_entry in records {
            let record = self.read_durable_upload_pinned(&record_entry)?;
            let lock_name = std::ffi::OsString::from(format!("{}.lock", record.staging_id));
            let lock_entry = locks.remove(&lock_name).ok_or_else(|| {
                anyhow::anyhow!("durable CAS upload {} has no lock file", record.staging_id)
            })?;
            if record.created_at.as_str() >= created_before {
                continue;
            }
            if record.retained_at.is_some() {
                continue;
            }
            if !lock_entry
                .try_lock_exclusive()
                .context("lock durable upload for retirement")?
            {
                tracing::debug!(
                    staging_id = %record.staging_id,
                    "preserving active durable CAS upload past the retirement cutoff"
                );
                continue;
            }
            self.retire_durable_blob_parts(&record.staging_id)?;
            directory
                .remove_pinned_regular_if_same(&record_entry)
                .context("retire durable CAS upload record")?;
            directory
                .remove_pinned_regular_if_same(&lock_entry)
                .context("retire durable CAS upload lock")?;
            retired += 1;
        }
        if let Some((_name, orphan)) = locks.into_iter().next() {
            anyhow::bail!(
                "orphan durable CAS upload lock: {}",
                orphan.path().display()
            );
        }
        Ok(retired)
    }

    fn retire_durable_blob_parts(&self, staging_id: &str) -> Result<()> {
        let Some(parts_root) = self.open_child_directory("durable-cas-upload-parts")? else {
            return Ok(());
        };
        let Some(parts) = parts_root.open_child_directory(std::ffi::OsStr::new(staging_id))? else {
            return Ok(());
        };
        parts.remove_contents_recursive()?;
        if !parts_root
            .remove_empty_child_if_same(std::ffi::OsStr::new(staging_id), &parts)
            .map(|receipt| receipt.is_some())?
        {
            anyhow::bail!("durable blob-parts directory changed during retirement");
        }
        Ok(())
    }

    fn retire_staged_blob_parts(&self, lease_id: &str) -> Result<()> {
        let Some(parts_root) = self.open_child_directory("staged-cas-root-parts")? else {
            return Ok(());
        };
        let Some(parts) = parts_root.open_child_directory(std::ffi::OsStr::new(lease_id))? else {
            return Ok(());
        };
        parts.remove_contents_recursive()?;
        if !parts_root
            .remove_empty_child_if_same(std::ffi::OsStr::new(lease_id), &parts)
            .map(|receipt| receipt.is_some())?
        {
            anyhow::bail!("staged blob-parts directory changed during retirement");
        }
        Ok(())
    }

    /// Enumerate active async staging roots and reclaim records whose owner
    /// lock disappeared after a crash. Caller holds the exclusive CAS guard.
    pub fn active_staged_cas_root_hashes(&self) -> Result<StagedCasRootHashes> {
        let directory = self
            .open_or_create_child_directory("staged-cas-roots")
            .context("create staged CAS root scan directory")?;
        let mut records = Vec::new();
        let mut locks = BTreeMap::new();
        for entry in directory.regular_files()? {
            match entry.path().extension().and_then(|value| value.to_str()) {
                Some("json") => records.push(entry),
                Some("lock") => {
                    locks.insert(entry.name().to_os_string(), entry);
                }
                _ => anyhow::bail!(
                    "unexpected staged CAS root entry: {}",
                    entry.path().display()
                ),
            }
        }
        let mut object_hashes = BTreeSet::new();
        let mut blob_hashes = BTreeSet::new();
        let mut large_object_hashes = BTreeSet::new();
        for record_entry in records {
            let lease_id = record_entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 staged CAS root filename"))?;
            validate_instance_id(lease_id)?;
            let lock_name = std::ffi::OsString::from(format!("{lease_id}.lock"));
            let lock_entry = locks
                .remove(&lock_name)
                .ok_or_else(|| anyhow::anyhow!("staged CAS root {lease_id} has no lock file"))?;
            let active = !lock_entry
                .try_lock_exclusive()
                .context("inspect staged CAS root lease lock")?;
            if !active {
                self.retire_staged_blob_parts(lease_id)?;
                directory
                    .remove_pinned_regular_if_same(&record_entry)
                    .context("remove abandoned staged CAS roots")?;
                directory
                    .remove_pinned_regular_if_same(&lock_entry)
                    .context("remove abandoned staged CAS root lock")?;
                continue;
            }
            let record = self.read_staged_roots_pinned(&record_entry)?;
            object_hashes.extend(record.object_hashes);
            blob_hashes.extend(record.blob_hashes);
            large_object_hashes.extend(record.large_object_hashes);
        }
        for (_name, lock_entry) in locks {
            if lock_entry
                .try_lock_exclusive()
                .context("inspect orphan staged CAS root lock")?
            {
                directory
                    .remove_pinned_regular_if_same(&lock_entry)
                    .context("remove orphan staged CAS root lock")?;
            }
        }
        self.collect_durable_upload_roots(
            &mut object_hashes,
            &mut blob_hashes,
            &mut large_object_hashes,
        )?;
        Ok(StagedCasRootHashes {
            object_hashes: object_hashes.into_iter().collect(),
            blob_hashes: blob_hashes.into_iter().collect(),
            large_object_hashes: large_object_hashes.into_iter().collect(),
        })
    }

    /// Inspect every staged and durable upload root without repairing the
    /// namespace. Mutation-free GC dry-runs call this while holding the
    /// exclusive CAS guard: abandoned records remain conservative roots, and
    /// malformed records or orphan lock anchors fail closed instead of being
    /// deleted as a side effect of inspection.
    pub fn inspect_staged_cas_root_hashes_read_only(&self) -> Result<StagedCasRootHashes> {
        let mut object_hashes = BTreeSet::new();
        let mut blob_hashes = BTreeSet::new();
        let mut large_object_hashes = BTreeSet::new();
        let Some(directory) = self.open_child_directory("staged-cas-roots")? else {
            self.collect_durable_upload_roots(
                &mut object_hashes,
                &mut blob_hashes,
                &mut large_object_hashes,
            )?;
            return Ok(StagedCasRootHashes {
                object_hashes: object_hashes.into_iter().collect(),
                blob_hashes: blob_hashes.into_iter().collect(),
                large_object_hashes: large_object_hashes.into_iter().collect(),
            });
        };

        let mut records = Vec::new();
        let mut locks = BTreeMap::new();
        for entry in directory.regular_files()? {
            match entry.path().extension().and_then(|value| value.to_str()) {
                Some("json") => records.push(entry),
                Some("lock") => {
                    locks.insert(entry.name().to_os_string(), entry);
                }
                _ => anyhow::bail!(
                    "unexpected staged CAS root entry: {}",
                    entry.path().display()
                ),
            }
        }
        for entry in records {
            let record = self.read_staged_roots_pinned(&entry)?;
            let lock_name = std::ffi::OsString::from(format!("{}.lock", record.lease_id));
            if locks.remove(&lock_name).is_none() {
                anyhow::bail!("staged CAS root {} has no lock file", record.lease_id);
            }
            object_hashes.extend(record.object_hashes);
            blob_hashes.extend(record.blob_hashes);
            large_object_hashes.extend(record.large_object_hashes);
        }
        if let Some((_name, orphan)) = locks.into_iter().next() {
            anyhow::bail!("orphan staged CAS root lock: {}", orphan.path().display());
        }
        self.collect_durable_upload_roots(
            &mut object_hashes,
            &mut blob_hashes,
            &mut large_object_hashes,
        )?;
        Ok(StagedCasRootHashes {
            object_hashes: object_hashes.into_iter().collect(),
            blob_hashes: blob_hashes.into_iter().collect(),
            large_object_hashes: large_object_hashes.into_iter().collect(),
        })
    }

    /// Unscoped read, for processes that hold no projection lease. Any
    /// listed lease refuses it; lease holders use `read_generation_scoped`.
    pub fn read_generation(&self) -> Result<Option<ProjectionRecoveryGeneration>> {
        self.read_generation_in(None)
    }

    /// The generation read inside the owning native factory's scope, for
    /// callers that may hold that factory's projection leases.
    pub fn read_generation_scoped(
        &self,
        factory: &lillux_sqlite::OriginalNativeFactory,
    ) -> Result<Option<ProjectionRecoveryGeneration>> {
        self.read_generation_in(Some(factory))
    }

    pub(crate) fn read_generation_in(
        &self,
        factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<Option<ProjectionRecoveryGeneration>> {
        // Intake remains bounded and the native alias gate spans every data
        // open/read/close. Metadata observations alone cannot safely read JSON.
        let Some(bytes) = guarded_evidence_read(
            factory,
            &self.directory,
            std::ffi::OsStr::new("generation.json"),
            65_536,
        )
        .context("read bounded recovery generation without SQLite alias")?
        else {
            return Ok(None);
        };
        let generation: ProjectionRecoveryGeneration =
            serde_json::from_slice(&bytes).context("decode recovery generation")?;
        generation.validate()?;
        Ok(Some(generation))
    }

    pub(crate) fn write_generation(&self, generation: &ProjectionRecoveryGeneration) -> Result<()> {
        generation.validate()?;
        let value = serde_json::to_value(generation)?;
        let bytes = lillux::canonical_json(&value).context("canonicalize recovery generation")?;
        let name = std::ffi::OsStr::new("generation.json");
        let expected = self.directory.open_mount_entry(name)?;
        if let Some(file) = &expected {
            anyhow::ensure!(
                file.metadata()?.is_file(),
                "recovery generation target is not regular"
            );
            self.directory.ensure_open_regular_same_mount(file)?;
        }
        self.directory
            .atomic_write_if_same(
                name,
                expected.as_ref(),
                bytes.as_bytes(),
                0o600,
                crate::STATE_TEMP_PURPOSE,
            )
            .context("durably write recovery generation")
    }

    pub fn read_pending(&self, chain_root_id: &str) -> Result<Option<PendingChainHeadTransition>> {
        Ok(self
            .read_pending_with_file(chain_root_id)?
            .map(|(record, _file)| record))
    }

    fn read_pending_with_file(
        &self,
        chain_root_id: &str,
    ) -> Result<Option<(PendingChainHeadTransition, File)>> {
        validate_chain_root_id(chain_root_id)?;
        let Some(directory) = self.open_child_directory("pending")? else {
            return Ok(None);
        };
        let name = format!("{chain_root_id}.json");
        let Some(mut file) = directory.open_regular(std::ffi::OsStr::new(&name), false)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .context("read pending chain transition")?;
        let record = decode_pending_bytes(&bytes, chain_root_id)?;
        Ok(Some((record, file)))
    }

    pub fn list_pending(&self) -> Result<Vec<PendingChainHeadTransition>> {
        let Some(directory) = self.open_child_directory("pending")? else {
            return Ok(Vec::new());
        };
        let entries = directory.regular_files()?;
        let mut records = Vec::with_capacity(entries.len());
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                anyhow::bail!("unexpected pending transition file: {}", path.display());
            }
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 pending transition filename"))?;
            validate_chain_root_id(stem)?;
            records.push(decode_pending_pinned(&entry, stem)?);
        }
        records.sort_by(|left, right| left.chain_root_id.cmp(&right.chain_root_id));
        Ok(records)
    }

    pub fn pending_cursor(
        &self,
        operation: Option<HeadOperation>,
    ) -> Result<PendingTransitionCursor> {
        self.pending_cursor_checked(operation, || Ok(()))
    }

    pub(crate) fn pending_cursor_checked(
        &self,
        operation: Option<HeadOperation>,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<PendingTransitionCursor> {
        let mut entries = Vec::new();
        check()?;
        let directory = self.open_child_directory("pending")?;
        if let Some(directory) = directory.as_ref() {
            for entry in directory.regular_files()? {
                check()?;
                entries.push(entry.name().to_os_string());
            }
        }
        check()?;
        Ok(PendingTransitionCursor {
            directory,
            entries: entries.into(),
            operation,
        })
    }

    /// Prepare a Set while the caller holds the chain lock.
    ///
    /// No existing transition is overwritten. A prior Set must be repaired and
    /// compare-acknowledged before the next head advance can prepare its Set.
    pub(crate) fn prepare_set(
        &self,
        chain_lock: &crate::chain::ChainLock,
        chain_root_id: &str,
        expected_previous_hash: Option<&str>,
        target_hash: &str,
    ) -> Result<PendingChainHeadTransition> {
        self.ensure_chain_lock(chain_lock, chain_root_id)?;
        validate_chain_root_id(chain_root_id)?;
        validate_optional_hash("expected_previous_hash", expected_previous_hash)?;
        validate_hash("target_hash", target_hash)?;

        if let Some((existing, _existing_file)) = self.read_pending_with_file(chain_root_id)? {
            match existing.operation {
                HeadOperation::Remove => anyhow::bail!(
                    "chain {chain_root_id} has a pending Remove; recover it before publishing Set"
                ),
                HeadOperation::Set => {
                    if existing.target_hash.as_deref() == Some(target_hash)
                        && existing.expected_previous_hash.as_deref() == expected_previous_hash
                    {
                        return Ok(existing);
                    }
                    anyhow::bail!(
                        "chain {chain_root_id} has a pending Set; repair and acknowledge it before publishing another Set"
                    );
                }
            }
        }

        let record = PendingChainHeadTransition {
            schema: PENDING_SCHEMA,
            transition_id: unique_id("transition"),
            chain_root_id: chain_root_id.to_string(),
            operation: HeadOperation::Set,
            phase: TransitionPhase::Prepared,
            expected_previous_hash: expected_previous_hash.map(str::to_owned),
            target_hash: Some(target_hash.to_string()),
            prepared_at: lillux::time::iso8601_now(),
        };
        self.write_pending(&record, None)?;
        Ok(record)
    }

    /// Prepare a Remove while the caller holds the chain lock.
    pub(crate) fn prepare_remove(
        &self,
        chain_lock: &crate::chain::ChainLock,
        chain_root_id: &str,
        expected_previous_hash: &str,
    ) -> Result<PendingChainHeadTransition> {
        self.ensure_chain_lock(chain_lock, chain_root_id)?;
        validate_chain_root_id(chain_root_id)?;
        validate_hash("expected_previous_hash", expected_previous_hash)?;

        if let Some((existing, _existing_file)) = self.read_pending_with_file(chain_root_id)? {
            return match existing.operation {
                HeadOperation::Set => anyhow::bail!(
                    "chain {chain_root_id} has a pending Set; project and acknowledge it before Remove"
                ),
                HeadOperation::Remove
                    if existing.expected_previous_hash.as_deref()
                        == Some(expected_previous_hash) =>
                {
                    Ok(existing)
                }
                HeadOperation::Remove => {
                    anyhow::bail!("chain {chain_root_id} already has a different pending Remove")
                }
            };
        }

        let record = PendingChainHeadTransition {
            schema: PENDING_SCHEMA,
            transition_id: unique_id("transition"),
            chain_root_id: chain_root_id.to_string(),
            operation: HeadOperation::Remove,
            phase: TransitionPhase::Prepared,
            expected_previous_hash: Some(expected_previous_hash.to_string()),
            target_hash: None,
            prepared_at: lillux::time::iso8601_now(),
        };
        self.write_pending(&record, None)?;
        Ok(record)
    }

    /// Atomically retire one stale Remove journal slot into a published Set
    /// for the signed head that superseded it. The caller holds the exact
    /// chain lock, and the transition ID comparison prevents an observed
    /// Remove from replacing newer recovery work.
    ///
    /// This is deliberately one durable overwrite rather than an
    /// acknowledge-then-prepare pair: a crash can therefore leave either the
    /// old Remove or the repair Set, never an unjournaled visible head.
    pub(crate) fn replace_remove_with_published_set(
        &self,
        chain_lock: &crate::chain::ChainLock,
        chain_root_id: &str,
        remove_transition_id: &str,
        target_hash: &str,
    ) -> Result<PendingChainHeadTransition> {
        self.ensure_chain_lock(chain_lock, chain_root_id)?;
        validate_chain_root_id(chain_root_id)?;
        validate_hash("target_hash", target_hash)?;

        let (current, current_file) = self
            .read_pending_with_file(chain_root_id)?
            .ok_or_else(|| anyhow::anyhow!("pending Remove disappeared"))?;
        if current.transition_id != remove_transition_id {
            anyhow::bail!("pending Remove transition ID changed before repair Set publication");
        }
        if current.operation != HeadOperation::Remove {
            anyhow::bail!("pending transition is no longer a Remove");
        }
        let expected_previous_hash = current
            .expected_previous_hash
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("pending Remove has no expected head hash"))?;
        if expected_previous_hash == target_hash {
            anyhow::bail!("pending Remove still names the visible head; it is not stale");
        }

        let replacement = PendingChainHeadTransition {
            schema: PENDING_SCHEMA,
            transition_id: unique_id("transition"),
            chain_root_id: chain_root_id.to_string(),
            operation: HeadOperation::Set,
            // The replacement target is already the visible signed head.
            phase: TransitionPhase::HeadPublished,
            expected_previous_hash: Some(expected_previous_hash.to_string()),
            target_hash: Some(target_hash.to_string()),
            prepared_at: lillux::time::iso8601_now(),
        };
        self.write_pending(&replacement, Some(&current_file))?;
        Ok(replacement)
    }

    pub(crate) fn acknowledge(
        &self,
        chain_lock: &crate::chain::ChainLock,
        chain_root_id: &str,
        transition_id: &str,
    ) -> Result<bool> {
        self.ensure_chain_lock(chain_lock, chain_root_id)?;
        let (current, current_file) = match self.read_pending_with_file(chain_root_id)? {
            Some(current) => current,
            None => return Ok(false),
        };
        if current.transition_id != transition_id {
            return Ok(false);
        }
        let directory = self
            .open_child_directory("pending")?
            .ok_or_else(|| anyhow::anyhow!("pending transition directory disappeared"))?;
        let name = format!("{chain_root_id}.json");
        directory
            .remove_if_same(std::ffi::OsStr::new(&name), &current_file)
            .with_context(|| format!("durably acknowledge transition for {chain_root_id}"))?;
        Ok(true)
    }

    /// Durably advance a transition phase using transition-id and current-phase
    /// comparison. Phases never move backward and cannot skip boundaries.
    pub(crate) fn advance_phase(
        &self,
        chain_lock: &crate::chain::ChainLock,
        chain_root_id: &str,
        transition_id: &str,
        expected: TransitionPhase,
        next: TransitionPhase,
    ) -> Result<PendingChainHeadTransition> {
        self.ensure_chain_lock(chain_lock, chain_root_id)?;
        let allowed = matches!(
            (expected, next),
            (TransitionPhase::Prepared, TransitionPhase::HeadPublished)
                | (
                    TransitionPhase::HeadPublished,
                    TransitionPhase::ApplyingProjection
                )
        );
        if !allowed {
            anyhow::bail!("invalid recovery phase transition: {expected:?} -> {next:?}");
        }
        let (mut current, current_file) = self
            .read_pending_with_file(chain_root_id)?
            .ok_or_else(|| anyhow::anyhow!("pending transition disappeared"))?;
        if current.transition_id != transition_id {
            anyhow::bail!("pending transition ID changed before phase advance");
        }
        if current.phase != expected {
            anyhow::bail!(
                "pending transition phase changed: expected {expected:?}, got {:?}",
                current.phase
            );
        }
        current.phase = next;
        self.write_pending(&current, Some(&current_file))?;
        Ok(current)
    }

    /// Object closures temporarily protected by every unresolved head change.
    /// A Set protects its not-yet-published target; a Remove protects the old
    /// closure after unlink until runtime/projection cleanup is acknowledged.
    pub fn pending_transition_object_roots(&self) -> Result<Vec<String>> {
        self.list_pending()?
            .into_iter()
            .map(|record| match record.operation {
                HeadOperation::Set => record
                    .target_hash
                    .ok_or_else(|| anyhow::anyhow!("pending Set is missing target_hash")),
                HeadOperation::Remove => record.expected_previous_hash.ok_or_else(|| {
                    anyhow::anyhow!("pending Remove is missing expected_previous_hash")
                }),
            })
            .collect()
    }

    pub(crate) fn new_projection_instance_id(&self) -> String {
        unique_id("projection")
    }

    fn write_pending(
        &self,
        record: &PendingChainHeadTransition,
        expected: Option<&File>,
    ) -> Result<()> {
        record.validate()?;
        let value = serde_json::to_value(record)?;
        let bytes =
            lillux::canonical_json(&value).context("canonicalize pending chain-head transition")?;
        let parent = self.open_or_create_child_directory("pending")?;
        let name = format!("{}.json", record.chain_root_id);
        parent
            .atomic_write_if_same(
                std::ffi::OsStr::new(&name),
                expected,
                bytes.as_bytes(),
                0o600,
                crate::STATE_TEMP_PURPOSE,
            )
            .with_context(|| {
                format!(
                    "durably write pending transition for {}",
                    record.chain_root_id
                )
            })
    }

    fn collect_durable_upload_roots(
        &self,
        object_hashes: &mut BTreeSet<String>,
        blob_hashes: &mut BTreeSet<String>,
        large_object_hashes: &mut BTreeSet<String>,
    ) -> Result<()> {
        let Some(directory) = self.open_child_directory("durable-cas-uploads")? else {
            return Ok(());
        };
        let mut records = Vec::new();
        let mut locks = BTreeMap::new();
        for entry in directory.regular_files()? {
            match entry.path().extension().and_then(|value| value.to_str()) {
                Some("json") => records.push(entry),
                Some("lock") => {
                    locks.insert(entry.name().to_os_string(), entry);
                }
                _ => anyhow::bail!(
                    "unexpected durable CAS upload entry: {}",
                    entry.path().display()
                ),
            }
        }
        for entry in records {
            let record = self.read_durable_upload_pinned(&entry)?;
            let lock_name = std::ffi::OsString::from(format!("{}.lock", record.staging_id));
            if locks.remove(&lock_name).is_none() {
                anyhow::bail!("durable CAS upload {} has no lock file", record.staging_id);
            }
            object_hashes.extend(record.object_hashes);
            blob_hashes.extend(record.blob_hashes);
            large_object_hashes.extend(record.large_object_hashes);
        }
        if let Some((_name, orphan)) = locks.into_iter().next() {
            anyhow::bail!(
                "orphan durable CAS upload lock: {}",
                orphan.path().display()
            );
        }
        Ok(())
    }

    fn write_staged_roots(
        &self,
        directory: &lillux::PinnedDirectory,
        record: &StagedCasRootsRecord,
    ) -> Result<()> {
        let bytes = encode_staged_roots(record)?;
        publish_staging_record(directory, &record.lease_id, &bytes)
    }

    fn read_staged_roots_pinned(
        &self,
        file: &lillux::PinnedRegularFile,
    ) -> Result<StagedCasRootsRecord> {
        let bytes = file.read_bounded(MAX_STAGING_RECORD_BYTES)?;
        decode_staged_roots_bytes(&bytes, file.path())
    }

    fn write_durable_upload(
        &self,
        directory: &lillux::PinnedDirectory,
        record: &DurableCasUploadRecord,
    ) -> Result<()> {
        let bytes = encode_durable_upload(record)?;
        publish_staging_record(directory, &record.staging_id, &bytes)
    }

    fn read_durable_upload_file(&self, file: File, path: &Path) -> Result<DurableCasUploadRecord> {
        decode_durable_upload(file, path)
    }

    fn read_durable_upload_pinned(
        &self,
        file: &lillux::PinnedRegularFile,
    ) -> Result<DurableCasUploadRecord> {
        let bytes = file.read_bounded(MAX_STAGING_RECORD_BYTES)?;
        decode_durable_upload_bytes(&bytes, file.path())
    }
}

fn encode_staged_roots(record: &StagedCasRootsRecord) -> Result<String> {
    validate_staged_roots_record(record)?;
    let bytes = lillux::canonical_json(&serde_json::to_value(record)?)?;
    validate_staging_record_bytes(bytes.len())?;
    Ok(bytes)
}

fn encode_durable_upload(record: &DurableCasUploadRecord) -> Result<String> {
    validate_durable_upload_record(record)?;
    let bytes = lillux::canonical_json(&serde_json::to_value(record)?)?;
    validate_staging_record_bytes(bytes.len())?;
    Ok(bytes)
}

fn publish_staging_record(
    directory: &lillux::PinnedDirectory,
    id: &str,
    bytes: &str,
) -> Result<()> {
    let name = format!("{id}.json");
    let name = std::ffi::OsStr::new(&name);
    let expected = directory.open_regular(name, false)?;
    directory
        .atomic_write_if_same(
            name,
            expected.as_ref(),
            bytes.as_bytes(),
            0o600,
            crate::STATE_TEMP_PURPOSE,
        )
        .context("durably write staging roots")?;
    #[cfg(test)]
    if FAIL_AFTER_STAGING_PUBLICATION.with(|fail| fail.replace(false)) {
        anyhow::bail!("injected post-publication sync failure");
    }
    Ok(())
}

fn ensure_staging_publication_certain(uncertain: &Cell<bool>) -> Result<()> {
    if uncertain.get() {
        anyhow::bail!(
            "staging publication outcome is uncertain; drop this handle and reopen recovery state"
        );
    }
    Ok(())
}

fn decode_staged_roots_bytes(bytes: &[u8], path: &Path) -> Result<StagedCasRootsRecord> {
    validate_staging_record_bytes(bytes.len())?;
    let record: StagedCasRootsRecord =
        serde_json::from_slice(bytes).context("decode staged CAS roots")?;
    validate_staged_roots_record(&record)?;
    if path.file_stem().and_then(|value| value.to_str()) != Some(record.lease_id.as_str()) {
        anyhow::bail!("staged CAS root path/record mismatch");
    }
    Ok(record)
}

fn validate_staged_roots_record(record: &StagedCasRootsRecord) -> Result<()> {
    if record.schema != STAGED_ROOTS_SCHEMA {
        anyhow::bail!("unsupported staged CAS roots schema: {}", record.schema);
    }
    validate_instance_id(&record.lease_id)?;
    validate_staging_purpose(&record.purpose)?;
    if record.owner_pid == 0 {
        anyhow::bail!("invalid staged CAS root record");
    }
    crate::objects::parse_canonical_timestamp(&record.created_at)
        .context("staged CAS root created_at is not canonical")?;
    validate_staging_roots(
        &record.object_hashes,
        &record.blob_hashes,
        &record.large_object_hashes,
    )
}

fn decode_durable_upload(file: File, path: &Path) -> Result<DurableCasUploadRecord> {
    let bytes = lillux::read_open_regular_file_bounded(file, MAX_STAGING_RECORD_BYTES)?;
    decode_durable_upload_bytes(&bytes, path)
}

fn decode_durable_upload_bytes(bytes: &[u8], path: &Path) -> Result<DurableCasUploadRecord> {
    validate_staging_record_bytes(bytes.len())?;
    let record: DurableCasUploadRecord =
        serde_json::from_slice(bytes).context("decode durable CAS upload stage")?;
    validate_durable_upload_record(&record)?;
    if path.file_stem().and_then(|value| value.to_str()) != Some(record.staging_id.as_str()) {
        anyhow::bail!("durable CAS upload path/record mismatch");
    }
    Ok(record)
}

fn validate_chain_root_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        anyhow::bail!("invalid chain_root_id for recovery path: {value:?}");
    }
    Ok(())
}

fn validate_hash(label: &str, hash: &str) -> Result<()> {
    if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
        anyhow::bail!("invalid {label}: {hash}");
    }
    Ok(())
}

fn validate_staging_purpose(purpose: &str) -> Result<()> {
    if purpose.is_empty()
        || purpose.len() > 128
        || purpose.bytes().any(|byte| byte.is_ascii_control())
    {
        anyhow::bail!("invalid staged CAS root purpose");
    }
    Ok(())
}

fn validate_upload_owner(owner: &str) -> Result<()> {
    if owner.is_empty() || owner.len() > 512 || owner.bytes().any(|byte| byte.is_ascii_control()) {
        anyhow::bail!("invalid durable CAS upload owner");
    }
    Ok(())
}

fn validate_durable_upload_record(record: &DurableCasUploadRecord) -> Result<()> {
    if record.schema != DURABLE_UPLOAD_SCHEMA {
        anyhow::bail!("invalid durable CAS upload record");
    }
    validate_instance_id(&record.staging_id)?;
    validate_upload_owner(&record.owner_principal)?;
    validate_staging_purpose(&record.purpose)?;
    record.publication_key.validate()?;
    if let Some(hash) = record.expected_previous_hash.as_deref() {
        validate_hash("durable upload expected previous hash", hash)?;
    }
    match (
        record.admitted_target_hash.as_deref(),
        record.admitted_at.as_deref(),
        record.retained_at.as_deref(),
    ) {
        (None, None, None) => {}
        (Some(hash), Some(admitted_at), None) => {
            if matches!(
                record.publication_key,
                DurableCasPublicationKey::QFixtureOccurrence { .. }
            ) {
                anyhow::bail!("Q fixture receipt cannot discard its retained roots");
            }
            validate_hash("durable upload admitted target", hash)?;
            crate::objects::parse_canonical_timestamp(admitted_at)
                .context("durable upload admitted_at is not canonical")?;
            if !record.object_hashes.is_empty()
                || !record.blob_hashes.is_empty()
                || !record.large_object_hashes.is_empty()
            {
                anyhow::bail!("admitted durable upload receipt still carries staging roots");
            }
        }
        (Some(hash), Some(admitted_at), Some(retained_at)) => {
            validate_hash("retained occurrence target", hash)?;
            crate::objects::parse_canonical_timestamp(admitted_at)
                .context("retained occurrence admitted_at is not canonical")?;
            crate::objects::parse_canonical_timestamp(retained_at)
                .context("retained occurrence retained_at is not canonical")?;
            if admitted_at != retained_at {
                anyhow::bail!("retained occurrence timestamps do not match");
            }
            match record.publication_key {
                DurableCasPublicationKey::ExternalCandidateOccurrence { .. } => {}
                DurableCasPublicationKey::QFixtureOccurrence { .. } => {
                    if record.object_hashes.len() != 1
                        || record.blob_hashes.len() != 1
                        || !record.large_object_hashes.is_empty()
                        || record.expected_previous_hash.is_some()
                    {
                        anyhow::bail!(
                            "retained Q fixture must have exactly its metadata and blob roots"
                        );
                    }
                }
                _ => anyhow::bail!("only candidate or Q fixture occurrences may retain CAS roots"),
            }
            if !record.object_hashes.contains(hash) {
                anyhow::bail!("retained occurrence target is not a protected root");
            }
        }
        _ => anyhow::bail!("durable upload admission receipt is incomplete"),
    }
    crate::objects::parse_canonical_timestamp(&record.created_at)
        .context("durable CAS upload created_at is not canonical")?;
    validate_staging_roots(
        &record.object_hashes,
        &record.blob_hashes,
        &record.large_object_hashes,
    )
}

fn validate_staging_roots(
    objects: &BTreeSet<String>,
    blobs: &BTreeSet<String>,
    large_objects: &BTreeSet<String>,
) -> Result<()> {
    // Coordinates in different stores remain distinct even with equal hashes.
    let count = objects
        .len()
        .checked_add(blobs.len())
        .and_then(|n| n.checked_add(large_objects.len()));
    if count.is_none_or(|count| count > MAX_STAGING_ROOTS) {
        anyhow::bail!("staging roots exceed the recovery record root capacity {MAX_STAGING_ROOTS}");
    }
    for hash in objects.iter().chain(blobs).chain(large_objects) {
        validate_hash("staged root", hash)?;
    }
    Ok(())
}

fn validate_staging_record_bytes(len: usize) -> Result<()> {
    if len as u64 > MAX_STAGING_RECORD_BYTES {
        anyhow::bail!("staging record exceeds readable record bound {MAX_STAGING_RECORD_BYTES}");
    }
    Ok(())
}

fn lock_exclusive(file: &File, label: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| label.to_string());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (file, label);
        anyhow::bail!("staged CAS root locking is unavailable on this platform")
    }
}

fn secure_open_regular(path: &Path, writable: bool) -> Result<Option<File>> {
    let parent_path = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("recovery path has no parent: {}", path.display()))?;
    let Some(parent) = lillux::PinnedDirectory::open(parent_path)? else {
        return Ok(None);
    };
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("recovery path has no filename: {}", path.display()))?;
    parent.open_regular(name, writable)
}

fn decode_pending_file(file: File, chain_root_id: &str) -> Result<PendingChainHeadTransition> {
    let mut bytes = Vec::new();
    file.take(MAX_RECOVERY_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read pending chain transition")?;
    if bytes.len() as u64 > MAX_RECOVERY_RECORD_BYTES {
        anyhow::bail!("pending chain transition exceeds byte limit");
    }
    decode_pending_bytes(&bytes, chain_root_id)
}

fn decode_pending_pinned(
    file: &lillux::PinnedRegularFile,
    chain_root_id: &str,
) -> Result<PendingChainHeadTransition> {
    let bytes = file
        .read_bounded(MAX_RECOVERY_RECORD_BYTES)
        .context("read pending chain transition")?;
    decode_pending_bytes(&bytes, chain_root_id)
}

fn decode_pending_bytes(bytes: &[u8], chain_root_id: &str) -> Result<PendingChainHeadTransition> {
    let record: PendingChainHeadTransition =
        serde_json::from_slice(bytes).context("decode pending chain transition")?;
    record.validate()?;
    if record.chain_root_id != chain_root_id {
        anyhow::bail!(
            "pending transition path/chain mismatch: expected {chain_root_id}, got {}",
            record.chain_root_id
        );
    }
    Ok(record)
}

fn validate_optional_hash(label: &str, hash: Option<&str>) -> Result<()> {
    if let Some(hash) = hash {
        validate_hash(label, hash)?;
    }
    Ok(())
}

fn validate_instance_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("invalid projection_instance_id");
    }
    Ok(())
}

fn validate_projection_file(value: &str, instance_id: &str) -> Result<()> {
    let expected = format!("projection.{instance_id}.sqlite3");
    if value != expected
        || Path::new(value).file_name().and_then(|name| name.to_str()) != Some(value)
    {
        anyhow::bail!("invalid projection_file for instance {instance_id}");
    }
    Ok(())
}

fn unique_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = UNIQUE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{:x}-{sequence:x}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn hash(byte: &str) -> String {
        byte.repeat(64)
    }

    fn chain_lock(runtime_state_dir: &Path, chain_root_id: &str) -> crate::chain::ChainLock {
        crate::chain::ChainLock::acquire(&runtime_state_dir.join("refs"), chain_root_id).unwrap()
    }

    fn durable_upload_wire() -> serde_json::Value {
        serde_json::json!({
            "schema": DURABLE_UPLOAD_SCHEMA,
            "staging_id": "stage-1",
            "owner_principal": format!("fp:{}", hash("a")),
            "purpose": "project-push",
            "publication_key": {
                "kind": "project_head",
                "principal_key": hash("a"),
                "project_hash": hash("b"),
            },
            "expected_previous_hash": null,
            "admitted_target_hash": null,
            "admitted_at": null,
            "retained_at": null,
            "created_at": "2026-07-14T12:00:00Z",
            "object_hashes": [],
            "blob_hashes": [],
            "large_object_hashes": [],
        })
    }

    #[test]
    fn q_fixture_receipt_retains_exact_roots_across_retirement_and_reopen() {
        use crate::objects::qualification_fixture::QFixtureCaptureReference;
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let key =
            DurableCasPublicationKey::qualification_fixture_occurrence(&hash("a"), &hash("b"))
                .unwrap();
        let reference = QFixtureCaptureReference {
            metadata_hash: hash("d"),
            blob_hash: hash("e"),
            size_bytes: 5,
        };
        let owner = hash("c");
        let receipt;
        {
            let guard = CasMutationGuard::acquire_shared(temp.path()).unwrap();
            let mut stage = store
                .begin_durable_cas_upload_admitted(&guard, &owner, "q-fixture", &key, None)
                .unwrap();
            assert!(
                stage
                    .retain_qualification_fixture(&guard, &reference)
                    .is_err()
            );
            stage
                .protect_cas_closure(
                    &guard,
                    [reference.metadata_hash.as_str()],
                    [reference.blob_hash.as_str()],
                )
                .unwrap();
            assert!(
                stage
                    .finish_admitted(&guard, &reference.metadata_hash)
                    .is_err()
            );
            receipt = stage
                .retain_qualification_fixture(&guard, &reference)
                .unwrap();
            assert!(stage.external_candidate_receipt().is_err());
            assert!(
                stage
                    .protect_cas_closure(&guard, [hash("f").as_str()], [])
                    .is_err()
            );
        }
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        assert_eq!(
            store
                .retire_durable_cas_uploads_created_before("9999-12-31T23:59:59Z", &guard)
                .unwrap(),
            0
        );
        let roots = store.active_staged_cas_root_hashes().unwrap();
        assert!(roots.object_hashes.contains(&reference.metadata_hash));
        assert!(roots.blob_hashes.contains(&reference.blob_hash));
        let stage = store
            .open_durable_cas_upload_admitted(&guard, &receipt.staging_id, &owner)
            .unwrap();
        assert_eq!(stage.qualification_fixture_receipt().unwrap(), receipt);
        let other =
            DurableCasPublicationKey::qualification_fixture_occurrence(&hash("a"), &hash("f"))
                .unwrap();
        assert!(stage.ensure_publication_contract(&other, None).is_err());
        let mut malformed = stage.record.clone();
        malformed.retained_at = None;
        malformed.object_hashes.clear();
        malformed.blob_hashes.clear();
        assert!(validate_durable_upload_record(&malformed).is_err());
        malformed = stage.record.clone();
        malformed.blob_hashes.insert(hash("f"));
        assert!(validate_durable_upload_record(&malformed).is_err());
        malformed = stage.record.clone();
        malformed.publication_key =
            DurableCasPublicationKey::external_content_import(&hash("a")).unwrap();
        assert!(validate_durable_upload_record(&malformed).is_err());
    }

    #[test]
    fn q_fixture_unretained_stage_expiry_never_reopens_a_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let key =
            DurableCasPublicationKey::qualification_fixture_occurrence(&hash("a"), &hash("b"))
                .unwrap();
        let owner = hash("c");
        let original_id = {
            let guard = CasMutationGuard::acquire_shared(temp.path()).unwrap();
            let stage = store
                .begin_durable_cas_upload_admitted(
                    &guard,
                    &owner,
                    "qualification-fixture-receiver",
                    &key,
                    None,
                )
                .unwrap();
            stage.staging_id().to_owned()
        };
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        assert_eq!(
            store
                .retire_durable_cas_uploads_created_before("9999-12-31T23:59:59Z", &guard,)
                .unwrap(),
            1
        );
        assert!(
            store
                .open_durable_cas_upload_admitted(&guard, &original_id, &owner,)
                .is_err()
        );
        // Even a fresh capability with the same publication key does not
        // restore an immutable receiver row's selected stage identity.
        let replacement = store
            .begin_durable_cas_upload_admitted(
                &guard,
                &owner,
                "qualification-fixture-receiver",
                &key,
                None,
            )
            .unwrap();
        assert_ne!(replacement.staging_id(), original_id);
        assert!(replacement.qualification_fixture_receipt().is_err());
        assert!(
            store
                .open_durable_cas_upload_admitted(&guard, &original_id, &owner,)
                .is_err()
        );
    }

    #[test]
    fn guest_runtime_materialization_has_a_distinct_exact_publication_key() {
        let key = DurableCasPublicationKey::guest_runtime_materialization(&hash("a"), &hash("b"))
            .unwrap();
        assert_eq!(
            serde_json::to_value(&key).unwrap(),
            serde_json::json!({
                "kind": "guest_runtime_materialization",
                "binding_digest": hash("a"),
                "source_coordinate_digest": hash("b"),
            })
        );
        assert!(
            DurableCasPublicationKey::guest_runtime_materialization(&hash("a"), "pending").is_err()
        );
    }

    #[test]
    fn retained_lifecycle_artifacts_have_a_cleanup_only_exact_key() {
        let key =
            DurableCasPublicationKey::retained_lifecycle_artifacts(&hash("a"), &hash("b")).unwrap();
        assert_eq!(
            serde_json::to_value(&key).unwrap(),
            serde_json::json!({
                "kind": "retained_lifecycle_artifacts",
                "bundle_manifest_digest": hash("a"),
                "artifact_set_digest": hash("b"),
            })
        );
        assert!(
            DurableCasPublicationKey::retained_lifecycle_artifacts(&hash("a"), "pending").is_err()
        );
    }

    #[test]
    fn durable_upload_wire_requires_explicit_nullable_fields_and_typed_publication_key() {
        let record: DurableCasUploadRecord =
            serde_json::from_value(durable_upload_wire()).expect("current wire must decode");
        validate_durable_upload_record(&record).expect("current wire must validate");

        for field in [
            "expected_previous_hash",
            "admitted_target_hash",
            "admitted_at",
            "retained_at",
        ] {
            let mut wire = durable_upload_wire();
            wire.as_object_mut()
                .expect("fixture is an object")
                .remove(field);
            assert!(serde_json::from_value::<DurableCasUploadRecord>(wire).is_err());
        }

        let mut wire = durable_upload_wire();
        wire["publication_key"] =
            serde_json::Value::String(format!("project-head:{}:{}", hash("a"), hash("b")));
        assert!(serde_json::from_value::<DurableCasUploadRecord>(wire).is_err());
    }

    #[test]
    fn staging_record_envelope_reopens_large_current_upload_and_preserves_roots() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime(store.runtime_directory())
                .unwrap();
        let key = DurableCasPublicationKey::external_content_import(&hash("a")).unwrap();
        let mut stage = store
            .begin_durable_cas_upload_admitted(&guard, &hash("c"), "large-import", &key, None)
            .unwrap();
        let blobs = (0..20_000).map(|n| format!("{n:064x}")).collect::<Vec<_>>();
        stage
            .protect_cas_closure(
                &guard,
                [hash("b").as_str()],
                blobs.iter().map(String::as_str),
            )
            .unwrap();
        stage.protect_large_object_hash(&guard, &hash("d")).unwrap();
        let bytes = lillux::canonical_json(&serde_json::to_value(&stage.record).unwrap()).unwrap();
        assert!(bytes.len() as u64 > MAX_RECOVERY_RECORD_BYTES);
        let id = stage.staging_id().to_owned();
        drop(stage);
        let mut reopened = store
            .open_durable_cas_upload_admitted(&guard, &id, &hash("c"))
            .unwrap();
        assert_eq!(reopened.record.blob_hashes.len(), blobs.len());
        let roots = store.inspect_staged_cas_root_hashes_read_only().unwrap();
        assert_eq!(roots.blob_hashes.len(), blobs.len());
        assert_eq!(roots.large_object_hashes, vec![hash("d")]);
        reopened.finish_admitted(&guard, &hash("b")).unwrap();
        assert!(
            store
                .inspect_staged_cas_root_hashes_read_only()
                .unwrap()
                .blob_hashes
                .is_empty()
        );
    }

    #[test]
    fn retained_external_candidate_is_a_non_expiring_root_until_exact_disposition() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let owner = hash("c");
        let key = DurableCasPublicationKey::external_candidate_occurrence(&hash("a"), &hash("b"))
            .unwrap();
        let snapshot = hash("d");
        let evidence = hash("e");
        let staging_id;
        let receipt;
        {
            let guard = CasMutationGuard::acquire_existing_shared_in_pinned_runtime(
                store.runtime_directory(),
            )
            .unwrap();
            let mut stage = store
                .begin_durable_cas_upload_admitted(
                    &guard,
                    &owner,
                    "external-candidate-capture",
                    &key,
                    None,
                )
                .unwrap();
            stage
                .protect_cas_closure(&guard, [snapshot.as_str()], [evidence.as_str()])
                .unwrap();
            receipt = stage
                .retain_external_candidate_occurrence(&guard, &snapshot)
                .unwrap();
            staging_id = receipt.staging_id().to_owned();
            assert_eq!(receipt.publication_key(), &key);
            assert_eq!(receipt.snapshot_hash(), snapshot);
            assert!(receipt.object_hashes().contains(&snapshot));
            assert!(receipt.blob_hashes().contains(&evidence));
        }

        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let roots = store.active_staged_cas_root_hashes().unwrap();
        assert!(roots.object_hashes.contains(&snapshot));
        assert!(roots.blob_hashes.contains(&evidence));
        assert_eq!(
            store
                .retire_durable_cas_uploads_created_before("9999-12-31T23:59:59Z", &guard)
                .unwrap(),
            0,
            "retained occurrence receipts are never age-retired"
        );
        let reopened = store
            .open_durable_cas_upload_admitted(&guard, &staging_id, &owner)
            .unwrap();
        assert_eq!(reopened.external_candidate_receipt().unwrap(), receipt);
        reopened
            .dispose_external_candidate_occurrence(&guard, &receipt)
            .unwrap();
        let roots = store.active_staged_cas_root_hashes().unwrap();
        assert!(!roots.object_hashes.contains(&snapshot));
        assert!(!roots.blob_hashes.contains(&evidence));
    }

    #[test]
    fn staging_record_capacity_refusal_keeps_upload_and_lease_reusable() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime(store.runtime_directory())
                .unwrap();
        let key = DurableCasPublicationKey::external_content_import(&hash("a")).unwrap();
        let mut stage = store
            .begin_durable_cas_upload_admitted(&guard, &hash("c"), "bounded-import", &key, None)
            .unwrap();
        let mut lease = store
            .begin_staged_cas_roots_admitted(&guard, "bounded-lease")
            .unwrap();
        let blobs = (0..=MAX_STAGING_ROOTS)
            .map(|n| format!("{n:064x}"))
            .collect::<Vec<_>>();
        assert!(
            stage
                .protect_cas_closure(&guard, [], blobs.iter().map(String::as_str))
                .unwrap_err()
                .to_string()
                .contains("root capacity")
        );
        assert!(
            lease
                .protect_cas_closure_admitted(&guard, [], blobs.iter().map(String::as_str))
                .is_err()
        );
        assert!(stage.record.blob_hashes.is_empty());
        assert!(lease.record.blob_hashes.is_empty());
        assert!(
            store
                .inspect_staged_cas_root_hashes_read_only()
                .unwrap()
                .blob_hashes
                .is_empty()
        );
        stage
            .protect_cas_closure(&guard, [hash("b").as_str()], [])
            .unwrap();
        lease
            .protect_object_hash_admitted(&guard, &hash("b"))
            .unwrap();
        stage.finish_admitted(&guard, &hash("b")).unwrap();
        lease.finish_admitted(&guard).unwrap();
    }

    #[test]
    fn staging_record_ambiguous_publication_fences_handle_until_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime(store.runtime_directory())
                .unwrap();
        let key = DurableCasPublicationKey::external_content_import(&hash("a")).unwrap();
        let mut stage = store
            .begin_durable_cas_upload_admitted(&guard, &hash("c"), "ambiguous-import", &key, None)
            .unwrap();
        stage
            .protect_cas_closure(&guard, [hash("b").as_str()], [])
            .unwrap();
        FAIL_AFTER_STAGING_PUBLICATION.with(|fail| fail.set(true));
        assert!(stage.finish_admitted(&guard, &hash("b")).is_err());
        assert!(stage.lock_file.is_some());
        assert!(stage.ensure_protects_object(&hash("b")).is_err());
        assert!(stage.ensure_publication_contract(&key, None).is_err());
        assert!(
            stage
                .protect_cas_closure(&guard, [hash("b").as_str()], [])
                .is_err()
        );
        assert!(stage.finish_admitted(&guard, &hash("b")).is_err());
        let id = stage.staging_id().to_owned();
        drop(stage);
        let reopened = store
            .open_durable_cas_upload_admitted(&guard, &id, &hash("c"))
            .unwrap();
        assert_eq!(reopened.admitted_target_hash(), Some(hash("b").as_str()));

        let mut lease = store
            .begin_staged_cas_roots_admitted(&guard, "ambiguous-lease")
            .unwrap();
        FAIL_AFTER_STAGING_PUBLICATION.with(|fail| fail.set(true));
        assert!(
            lease
                .protect_object_hash_admitted(&guard, &hash("d"))
                .is_err()
        );
        assert!(lease.protect_cas_closure_admitted(&guard, [], []).is_err());
        assert!(lease.finish_admitted(&guard).is_err());
        assert!(lease.lock_file.is_some());
        drop(lease);
        // Drop must not erase the successfully published but unacknowledged
        // lease. Existing abandoned-lease recovery owns its eventual retirement.
        assert_eq!(
            store
                .inspect_staged_cas_root_hashes_read_only()
                .unwrap()
                .object_hashes,
            vec![hash("d")]
        );
    }

    #[test]
    fn staging_record_readers_and_writers_share_byte_and_hash_validation() {
        let mut record: DurableCasUploadRecord =
            serde_json::from_value(durable_upload_wire()).unwrap();
        record.blob_hashes = (0..=MAX_STAGING_ROOTS)
            .map(|n| format!("{n:064x}"))
            .collect();
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(decode_durable_upload_bytes(&bytes, Path::new("stage-1.json")).is_err());
        assert!(validate_staging_record_bytes(MAX_STAGING_RECORD_BYTES as usize).is_ok());
        assert!(validate_staging_record_bytes(MAX_STAGING_RECORD_BYTES as usize + 1).is_err());
        let invalid = StagedCasRootsRecord {
            schema: STAGED_ROOTS_SCHEMA,
            lease_id: "lease-1".into(),
            owner_pid: 1,
            purpose: "test".into(),
            created_at: "2026-07-14T12:00:00Z".into(),
            object_hashes: BTreeSet::new(),
            blob_hashes: BTreeSet::new(),
            large_object_hashes: BTreeSet::from(["not-a-hash".into()]),
        };
        assert!(validate_staged_roots_record(&invalid).is_err());
        assert!(
            decode_staged_roots_bytes(
                &serde_json::to_vec(&invalid).unwrap(),
                Path::new("lease-1.json")
            )
            .is_err()
        );
    }

    #[test]
    fn durable_upload_can_settle_an_already_current_publication_root() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime(store.runtime_directory())
                .unwrap();
        let publication_key =
            DurableCasPublicationKey::external_content_import(&hash("a")).unwrap();
        let target_hash = hash("b");
        let mut stage = store
            .begin_durable_cas_upload_admitted(
                &guard,
                &hash("c"),
                "idempotent-publication",
                &publication_key,
                None,
            )
            .unwrap();
        let staging_id = stage.staging_id().to_owned();

        assert!(stage.finish_admitted(&guard, &target_hash).is_err());
        stage
            .protect_cas_closure(
                &guard,
                std::iter::once(target_hash.as_str()),
                std::iter::empty(),
            )
            .unwrap();
        stage.finish_admitted(&guard, &target_hash).unwrap();
        drop(stage);

        let reopened = store
            .open_durable_cas_upload_admitted(&guard, &staging_id, &hash("c"))
            .unwrap();
        reopened
            .ensure_publication_contract(&publication_key, None)
            .unwrap();
        assert_eq!(reopened.admitted_target_hash(), Some(target_hash.as_str()));
        assert!(
            reopened
                .ensure_protects_object(&target_hash)
                .unwrap_err()
                .to_string()
                .contains("admitted receipt")
        );
    }

    #[test]
    fn recovery_wire_timestamps_require_canonical_whole_second_utc() {
        let pending = PendingChainHeadTransition {
            schema: PENDING_SCHEMA,
            transition_id: "transition-1".into(),
            chain_root_id: "T-root".into(),
            operation: HeadOperation::Set,
            phase: TransitionPhase::Prepared,
            expected_previous_hash: None,
            target_hash: Some(hash("a")),
            prepared_at: "2026-07-14T12:00:00+00:00".into(),
        };
        assert!(pending.validate().is_err());

        let mut generation = ProjectionRecoveryGeneration::new(
            1,
            "instance-1".into(),
            "projection.instance-1.sqlite3".into(),
        )
        .unwrap();
        generation.established_at = "2026-07-14T12:00:00.1Z".into();
        assert!(generation.validate().is_err());
    }

    #[test]
    fn pending_transition_wire_requires_both_nullable_hash_keys() {
        let pending = PendingChainHeadTransition {
            schema: PENDING_SCHEMA,
            transition_id: "transition-1".into(),
            chain_root_id: "T-root".into(),
            operation: HeadOperation::Set,
            phase: TransitionPhase::Prepared,
            expected_previous_hash: None,
            target_hash: Some(hash("a")),
            prepared_at: "2026-07-14T12:00:00Z".into(),
        };
        let wire = serde_json::to_value(&pending).expect("encode current pending wire");
        assert_eq!(wire["expected_previous_hash"], serde_json::Value::Null);

        for field in ["expected_previous_hash", "target_hash"] {
            let mut missing = wire.clone();
            missing
                .as_object_mut()
                .expect("pending wire is an object")
                .remove(field);
            assert!(
                serde_json::from_value::<PendingChainHeadTransition>(missing).is_err(),
                "missing nullable pending field {field} must fail strict decoding"
            );
        }

        let remove = PendingChainHeadTransition {
            operation: HeadOperation::Remove,
            expected_previous_hash: Some(hash("b")),
            target_hash: None,
            ..pending
        };
        let decoded: PendingChainHeadTransition = serde_json::from_value(
            serde_json::to_value(remove).expect("encode current Remove wire"),
        )
        .expect("decode current Remove wire");
        decoded.validate().expect("current Remove wire validates");
    }

    #[test]
    fn startup_sweep_removes_only_state_owned_secure_temporaries() {
        let temp = tempfile::tempdir().unwrap();
        let recovery_path = temp.path().join("recovery");
        let projection_path = recovery_path.join("thread-projection");
        fs::create_dir_all(projection_path.join("pending")).unwrap();
        let own = |pid: u32| format!(".secure.tmp.{}.{pid}.1", crate::STATE_TEMP_PURPOSE.as_str());
        let owned = [
            recovery_path.join(own(11)),
            projection_path.join(own(12)),
            projection_path.join("pending").join(own(13)),
        ];
        let retained = [
            recovery_path.join(".secure.tmp.reset.14.1"),
            projection_path.join(".secure.tmp.scheduler.15.1"),
            projection_path.join("generation.json"),
        ];
        for path in owned.iter().chain(&retained) {
            fs::write(path, b"bytes").unwrap();
        }
        let parent = lillux::PinnedDirectory::open(&recovery_path).unwrap().unwrap();
        let projection = lillux::PinnedDirectory::open(&projection_path).unwrap().unwrap();

        assert_eq!(sweep_interrupted_state_publications(&parent, &projection).unwrap(), 3);
        for path in &owned {
            assert!(!path.exists(), "{}", path.display());
        }
        for path in &retained {
            assert!(path.is_file(), "{}", path.display());
        }

        let retired_format = projection_path.join(".secure.tmp.16.1");
        fs::write(&retired_format, b"unattributed").unwrap();
        let error = sweep_interrupted_state_publications(&parent, &projection).unwrap_err();
        assert!(error.to_string().contains("without an exact owner token"), "{error:#}");
        assert!(retired_format.is_file());
    }

    #[test]
    fn thread_history_discard_marker_is_durable_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();

        let operation = ThreadHistoryDiscardOperationRef {
            operation_id: "a".repeat(32),
            sha256: "b".repeat(64),
        };
        assert!(!store.thread_history_discard_in_progress().unwrap());
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        assert!(store.thread_history_discard_in_progress().unwrap());

        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        assert!(!store.thread_history_discard_in_progress().unwrap());
        assert_eq!(
            store.last_acknowledged_thread_history_discard().unwrap(),
            Some(operation)
        );
    }

    fn discard_operation(id: char, digest: char) -> ThreadHistoryDiscardOperationRef {
        ThreadHistoryDiscardOperationRef {
            operation_id: id.to_string().repeat(32),
            sha256: digest.to_string().repeat(64),
        }
    }

    fn marker_bytes(
        operation: &ThreadHistoryDiscardOperationRef,
        phase: ThreadHistoryDiscardPhase,
    ) -> Vec<u8> {
        ThreadHistoryDiscardMarker::for_operation(operation, phase)
            .unwrap()
            .canonical_bytes()
            .unwrap()
    }

    fn recovery_entry_names(store: &RecoveryStore) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(store.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn discard_marker_schema_three_bytes_are_canonical_sorted_json() {
        let operation = discard_operation('a', 'b');
        let expected = format!(
            "{{\"kind\":\"thread_history_discard\",\"operation\":{{\"operation_id\":\"{}\",\"sha256\":\"{}\"}},\"phase\":\"acknowledged\",\"schema\":3}}",
            operation.operation_id, operation.sha256
        );
        let marker = ThreadHistoryDiscardMarker::for_operation(
            &operation,
            ThreadHistoryDiscardPhase::Acknowledged,
        )
        .unwrap();
        assert_eq!(marker.canonical_bytes().unwrap(), expected.as_bytes());
        let value = serde_json::to_value(&marker).unwrap();
        assert_eq!(
            lillux::canonical_json(&value).unwrap().into_bytes(),
            expected.into_bytes()
        );
    }

    #[test]
    fn acknowledgement_rewrites_the_marker_in_place_and_retries_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let operation = discard_operation('a', 'b');
        let marker = store.root().join(THREAD_HISTORY_DISCARD_MARKER);
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            marker_bytes(&operation, ThreadHistoryDiscardPhase::Active)
        );
        assert_eq!(store.last_acknowledged_thread_history_discard().unwrap(), None);

        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        let acknowledged = marker_bytes(&operation, ThreadHistoryDiscardPhase::Acknowledged);
        assert_eq!(std::fs::read(&marker).unwrap(), acknowledged);
        let marker_inode = || {
            store
                .directory
                .entry_no_follow(std::ffi::OsStr::new(THREAD_HISTORY_DISCARD_MARKER))
                .unwrap()
                .unwrap()
                .inode
        };
        let inode = marker_inode();
        assert_eq!(store.thread_history_discard_operation().unwrap(), None);
        assert!(!store.thread_history_discard_in_progress().unwrap());
        assert_eq!(
            store.last_acknowledged_thread_history_discard().unwrap(),
            Some(operation.clone())
        );

        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), acknowledged);
        assert_eq!(marker_inode(), inode);
        assert_eq!(
            recovery_entry_names(&store),
            vec![THREAD_HISTORY_DISCARD_MARKER.to_owned()]
        );
    }

    #[test]
    fn acknowledgement_refuses_a_different_acknowledged_operation() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let operation = discard_operation('a', 'b');
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        let marker = store.root().join(THREAD_HISTORY_DISCARD_MARKER);
        let before = std::fs::read(&marker).unwrap();
        for other in [discard_operation('c', 'b'), discard_operation('a', 'd')] {
            let error = store
                .finish_thread_history_discard(&guard, &other)
                .unwrap_err();
            assert!(
                error.to_string().contains("differs from its marker's operation"),
                "{error:#}"
            );
            assert_eq!(
                error.downcast_ref::<ThreadHistoryDiscardAcknowledgementRefused>(),
                Some(&ThreadHistoryDiscardAcknowledgementRefused::OtherOperation {
                    acknowledged: true,
                    operation_id: operation.operation_id.clone(),
                }),
                "a permanent refusal must be typed: {error:#}"
            );
            assert_eq!(std::fs::read(&marker).unwrap(), before);
        }
        assert_eq!(
            store.last_acknowledged_thread_history_discard().unwrap(),
            Some(operation)
        );
    }

    #[test]
    fn acknowledgement_refuses_an_absent_marker() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let error = store
            .finish_thread_history_discard(&guard, &discard_operation('a', 'b'))
            .unwrap_err();
        assert!(error.to_string().contains("found no marker"), "{error:#}");
        assert_eq!(
            error.downcast_ref::<ThreadHistoryDiscardAcknowledgementRefused>(),
            Some(&ThreadHistoryDiscardAcknowledgementRefused::Absent)
        );
        assert!(recovery_entry_names(&store).is_empty());
    }

    #[test]
    fn activation_replaces_an_acknowledged_marker_and_refuses_a_different_active_one() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let first = discard_operation('a', 'b');
        let second = discard_operation('c', 'd');
        store.begin_thread_history_discard(&guard, &first).unwrap();
        store.finish_thread_history_discard(&guard, &first).unwrap();

        store.begin_thread_history_discard(&guard, &second).unwrap();
        let marker = store.root().join(THREAD_HISTORY_DISCARD_MARKER);
        let active = marker_bytes(&second, ThreadHistoryDiscardPhase::Active);
        assert_eq!(std::fs::read(&marker).unwrap(), active);
        assert!(store.thread_history_discard_in_progress().unwrap());
        assert_eq!(
            store.thread_history_discard_operation().unwrap(),
            Some(second.clone())
        );
        assert_eq!(store.last_acknowledged_thread_history_discard().unwrap(), None);

        let error = store
            .begin_thread_history_discard(&guard, &first)
            .unwrap_err();
        assert!(error.to_string().contains("different operation"), "{error:#}");
        assert!(store.finish_thread_history_discard(&guard, &first).is_err());
        assert_eq!(std::fs::read(&marker).unwrap(), active);
        assert_eq!(
            recovery_entry_names(&store),
            vec![THREAD_HISTORY_DISCARD_MARKER.to_owned()]
        );
    }

    #[test]
    fn retired_schema_two_marker_is_refused_naming_the_scratch_repair() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let operation = discard_operation('a', 'b');
        let retired = format!(
            "{{\"kind\":\"thread_history_discard\",\"operation\":{{\"operation_id\":\"{}\",\"sha256\":\"{}\"}},\"schema\":2}}",
            operation.operation_id, operation.sha256
        );
        let marker = store.root().join(THREAD_HISTORY_DISCARD_MARKER);
        std::fs::write(&marker, retired.as_bytes()).unwrap();
        let refusals = [
            store.thread_history_discard_operation().map(drop),
            store.thread_history_discard_in_progress().map(drop),
            store.last_acknowledged_thread_history_discard().map(drop),
            store.begin_thread_history_discard(&guard, &operation),
            store.finish_thread_history_discard(&guard, &operation),
        ];
        for refusal in refusals {
            let error = refusal.unwrap_err();
            let retired = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<RetiredThreadHistoryDiscardMarker>())
                .unwrap_or_else(|| panic!("untyped refusal: {error:#}"));
            assert_eq!(retired.schema, 2);
            assert!(
                format!("{error:#}").contains(EXECUTION_HISTORY_SCRATCH_REPAIR),
                "{error:#}"
            );
        }
        assert_eq!(std::fs::read(&marker).unwrap(), retired.into_bytes());
        assert_eq!(
            EXECUTION_HISTORY_SCRATCH_REPAIR,
            "ryeos node reset execution-history-scratch --confirm"
        );
    }

    #[test]
    fn acknowledgement_work_stays_within_its_prepaid_quote() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let operation = discard_operation('a', 'b');
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        let quote = thread_history_discard_acknowledgement_quote();
        let custody = store.final_ack_custody();
        let within = |work: AcknowledgementWork| {
            work.read_bytes <= quote.read_bytes
                && work.memory_bytes <= quote.memory_bytes
                && work.steps <= quote.steps
        };

        let mut rewrite = AcknowledgementWork::default();
        custody
            .finish_counted(&guard, &operation, &mut rewrite)
            .unwrap();
        assert!(within(rewrite), "{rewrite:?} exceeds {quote:?}");
        // The rewrite is the most expensive attempt: it takes every step.
        assert_eq!(rewrite.steps, quote.steps);
        assert_eq!(
            rewrite.read_bytes,
            marker_bytes(&operation, ThreadHistoryDiscardPhase::Active).len() as u64 + 1
        );

        let mut retry = AcknowledgementWork::default();
        custody.finish_counted(&guard, &operation, &mut retry).unwrap();
        assert!(within(retry), "{retry:?} exceeds {quote:?}");
        assert!(retry.steps < rewrite.steps);

        // The temporary-name scratch bounds the longest name a State writer
        // can generate: maximal pid and sequence digits.
        let longest = format!(
            ".secure.tmp.{}.{}.{}",
            "x".repeat(16),
            u32::MAX,
            u64::MAX
        );
        assert!(longest.len() as u64 + 1 <= ACK_TEMP_NAME_SCRATCH_BYTES);
        // The joined-path term is the directory's own path plus the name.
        let joined = store.root().join(THREAD_HISTORY_DISCARD_MARKER);
        assert!(
            joined.as_os_str().len() as u64
                <= store.root().as_os_str().len() as u64 + ACK_MARKER_PATH_SCRATCH_BYTES
        );
        assert!(store.root().as_os_str().len() as u64 <= ACK_DIRECTORY_PATH_MAX_BYTES);
        assert_eq!(quote.read_bytes, THREAD_HISTORY_DISCARD_MARKER_MAX_BYTES as u64 + 1);
    }

    #[test]
    fn discard_activation_rejects_different_operation_without_replacing_marker() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let operation = ThreadHistoryDiscardOperationRef {
            operation_id: "a".repeat(32),
            sha256: "b".repeat(64),
        };
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        let before = std::fs::read(store.root().join(THREAD_HISTORY_DISCARD_MARKER)).unwrap();
        let other = ThreadHistoryDiscardOperationRef {
            operation_id: operation.operation_id.clone(),
            sha256: "c".repeat(64),
        };
        assert!(store.begin_thread_history_discard(&guard, &other).is_err());
        assert!(store.finish_thread_history_discard(&guard, &other).is_err());
        assert_eq!(
            store.thread_history_discard_operation().unwrap(),
            Some(operation.clone())
        );
        assert_eq!(
            std::fs::read(store.root().join(THREAD_HISTORY_DISCARD_MARKER)).unwrap(),
            before
        );
        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        assert_eq!(store.thread_history_discard_operation().unwrap(), None);
    }

    #[test]
    fn discard_activation_refuses_unbound_malformed_and_noncanonical_markers() {
        let valid = ThreadHistoryDiscardMarker::for_operation(
            &ThreadHistoryDiscardOperationRef {
                operation_id: "a".repeat(32),
                sha256: "b".repeat(64),
            },
            ThreadHistoryDiscardPhase::Active,
        )
        .unwrap();
        let value = serde_json::to_value(&valid).unwrap();
        let mut cases = vec![
            br#"{"kind":"thread_history_discard","schema":1}"#.to_vec(),
            vec![b'x'; THREAD_HISTORY_DISCARD_MARKER_MAX_BYTES + 1],
        ];
        for key in ["kind", "schema", "phase", "operation"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            cases.push(lillux::canonical_json(&missing).unwrap().into_bytes());
        }
        for key in ["operation_id", "sha256"] {
            let mut missing = value.clone();
            missing["operation"].as_object_mut().unwrap().remove(key);
            cases.push(lillux::canonical_json(&missing).unwrap().into_bytes());
        }
        let mut extra = value.clone();
        extra["operation"]["authority"] = serde_json::json!(true);
        cases.push(lillux::canonical_json(&extra).unwrap().into_bytes());
        let mut unknown_phase = value.clone();
        unknown_phase["phase"] = serde_json::json!("retired");
        cases.push(lillux::canonical_json(&unknown_phase).unwrap().into_bytes());
        let mut wrong_id = value.clone();
        wrong_id["operation"]["operation_id"] = serde_json::json!("A".repeat(32));
        cases.push(lillux::canonical_json(&wrong_id).unwrap().into_bytes());
        let mut whitespace = valid.canonical_bytes().unwrap();
        whitespace.push(10);
        cases.push(whitespace);
        for bytes in cases {
            let temp = tempfile::tempdir().unwrap();
            let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
            let marker = store.root().join(THREAD_HISTORY_DISCARD_MARKER);
            std::fs::write(&marker, &bytes).unwrap();
            assert!(store.thread_history_discard_operation().is_err());
            assert!(store.thread_history_discard_in_progress().is_err());
            assert_eq!(std::fs::read(marker).unwrap(), bytes);
        }
    }

    #[test]
    fn pending_remove_cannot_be_overwritten_by_set() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let lock = chain_lock(temp.path(), "T-root");
        store.prepare_remove(&lock, "T-root", &hash("a")).unwrap();
        let error = store
            .prepare_set(&lock, "T-root", None, &hash("b"))
            .unwrap_err();
        assert!(error.to_string().contains("pending Remove"));
    }

    #[test]
    fn stale_remove_is_atomically_replaced_by_exact_published_set() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let lock = chain_lock(temp.path(), "T-root");
        let remove = store.prepare_remove(&lock, "T-root", &hash("a")).unwrap();

        assert!(
            store
                .replace_remove_with_published_set(
                    &lock,
                    "T-root",
                    "different-transition",
                    &hash("b"),
                )
                .is_err()
        );
        assert_eq!(
            store.read_pending("T-root").unwrap().unwrap(),
            remove,
            "a transition-ID mismatch must leave the Remove intact"
        );

        let set = store
            .replace_remove_with_published_set(&lock, "T-root", &remove.transition_id, &hash("b"))
            .unwrap();
        assert_ne!(set.transition_id, remove.transition_id);
        assert_eq!(set.operation, HeadOperation::Set);
        assert_eq!(set.phase, TransitionPhase::HeadPublished);
        assert_eq!(
            set.expected_previous_hash.as_deref(),
            Some(hash("a").as_str())
        );
        assert_eq!(set.target_hash.as_deref(), Some(hash("b").as_str()));
        assert_eq!(store.read_pending("T-root").unwrap().unwrap(), set);
    }

    #[test]
    fn pending_set_must_be_acknowledged_before_next_set() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let lock = chain_lock(temp.path(), "T-root");
        let first = store
            .prepare_set(&lock, "T-root", None, &hash("a"))
            .unwrap();
        assert!(
            store
                .prepare_set(&lock, "T-root", Some(&hash("a")), &hash("b"))
                .is_err()
        );
        store
            .acknowledge(&lock, "T-root", &first.transition_id)
            .unwrap();
        store
            .prepare_set(&lock, "T-root", Some(&hash("a")), &hash("b"))
            .unwrap();
    }

    #[test]
    fn acknowledgement_compares_transition_id() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let lock = chain_lock(temp.path(), "T-root");
        let record = store
            .prepare_set(&lock, "T-root", None, &hash("a"))
            .unwrap();
        assert!(!store.acknowledge(&lock, "T-root", "newer").unwrap());
        assert!(store.read_pending("T-root").unwrap().is_some());
        assert!(
            store
                .acknowledge(&lock, "T-root", &record.transition_id)
                .unwrap()
        );
        assert!(store.read_pending("T-root").unwrap().is_none());
    }

    #[test]
    fn transition_phases_are_strict_and_durable() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let lock = chain_lock(temp.path(), "T-root");
        let record = store
            .prepare_set(&lock, "T-root", None, &hash("a"))
            .unwrap();
        assert!(
            store
                .advance_phase(
                    &lock,
                    "T-root",
                    &record.transition_id,
                    TransitionPhase::Prepared,
                    TransitionPhase::ApplyingProjection,
                )
                .is_err()
        );
        let published = store
            .advance_phase(
                &lock,
                "T-root",
                &record.transition_id,
                TransitionPhase::Prepared,
                TransitionPhase::HeadPublished,
            )
            .unwrap();
        assert_eq!(published.phase, TransitionPhase::HeadPublished);
        assert_eq!(
            store.read_pending("T-root").unwrap().unwrap().phase,
            TransitionPhase::HeadPublished
        );
    }

    #[test]
    fn generation_binds_instance_filename() {
        let instance = "projection-abc".to_string();
        let file = format!("projection.{instance}.sqlite3");
        assert!(ProjectionRecoveryGeneration::new(7, instance.clone(), file).is_ok());
        assert!(
            ProjectionRecoveryGeneration::new(7, instance, "projection.sqlite3".to_string(),)
                .is_err()
        );
    }

    #[test]
    fn traversal_chain_id_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        assert!(store.read_pending("../escape").is_err());
    }

    #[test]
    fn pending_cursor_filters_in_one_bounded_pass() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let set_lock = chain_lock(temp.path(), "T-set");
        let remove_lock = chain_lock(temp.path(), "T-remove");
        store
            .prepare_set(&set_lock, "T-set", None, &hash("a"))
            .unwrap();
        store
            .prepare_remove(&remove_lock, "T-remove", &hash("b"))
            .unwrap();

        let mut cursor = store.pending_cursor(Some(HeadOperation::Remove)).unwrap();
        let remove = cursor.next_transition().unwrap().unwrap();
        assert_eq!(remove.chain_root_id, "T-remove");
        assert_eq!(remove.operation, HeadOperation::Remove);
        assert!(cursor.next_transition().unwrap().is_none());
    }

    #[test]
    fn pending_cursor_checked_can_cancel_directory_enumeration() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let set_lock = chain_lock(temp.path(), "T-set");
        let second_lock = chain_lock(temp.path(), "T-second");
        store
            .prepare_set(&set_lock, "T-set", None, &hash("a"))
            .unwrap();
        store
            .prepare_set(&second_lock, "T-second", None, &hash("b"))
            .unwrap();
        let mut checks = 0usize;

        let error = store
            .pending_cursor_checked(None, || {
                checks += 1;
                if checks > 2 {
                    anyhow::bail!("recovery cancelled")
                }
                Ok(())
            })
            .err()
            .expect("checked construction must surface cancellation");

        assert!(error.to_string().contains("recovery cancelled"));
    }

    #[test]
    fn staged_cas_roots_are_active_until_finished() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime(store.runtime_directory())
                .unwrap();
        let mut lease = store
            .begin_staged_cas_roots_admitted(&guard, "test-import")
            .unwrap();
        lease
            .protect_object_hash_admitted(&guard, &hash("a"))
            .unwrap();
        lease
            .protect_blob_hash_admitted(&guard, &hash("b"))
            .unwrap();

        let active = store.active_staged_cas_root_hashes().unwrap();
        assert_eq!(active.object_hashes, vec![hash("a")]);
        assert_eq!(active.blob_hashes, vec![hash("b")]);

        lease.finish_admitted(&guard).unwrap();
        let active = store.active_staged_cas_root_hashes().unwrap();
        assert!(active.object_hashes.is_empty());
        assert!(active.blob_hashes.is_empty());
    }

    #[test]
    fn staged_cas_root_read_only_inspection_preserves_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime(store.runtime_directory())
                .unwrap();
        let mut lease = store
            .begin_staged_cas_roots_admitted(&guard, "test-dry-run")
            .unwrap();
        lease
            .protect_object_hash_admitted(&guard, &hash("a"))
            .unwrap();
        lease
            .protect_blob_hash_admitted(&guard, &hash("b"))
            .unwrap();
        let staged_dir = temp
            .path()
            .join("recovery/thread-projection/staged-cas-roots");
        let before = fs::read_dir(&staged_dir).unwrap().count();

        let roots = store.inspect_staged_cas_root_hashes_read_only().unwrap();

        assert_eq!(roots.object_hashes, vec![hash("a")]);
        assert_eq!(roots.blob_hashes, vec![hash("b")]);
        assert_eq!(fs::read_dir(&staged_dir).unwrap().count(), before);
        lease.finish_admitted(&guard).unwrap();
    }

    #[test]
    fn cas_mutation_guard_is_reentrant_at_the_same_or_weaker_level() {
        let temp = tempfile::tempdir().unwrap();

        let shared = CasMutationGuard::acquire_shared(temp.path()).unwrap();
        let nested_shared = CasMutationGuard::acquire_shared(temp.path()).unwrap();
        drop(nested_shared);
        drop(shared);

        let exclusive = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let nested_exclusive = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let nested_shared = CasMutationGuard::acquire_shared(temp.path()).unwrap();
        drop(nested_shared);
        drop(nested_exclusive);
        drop(exclusive);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_pinned_shared_guard_preserves_nested_original_file() {
        let temp = tempfile::tempdir().unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let runtime = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1));
        let outer =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime_until(&runtime, deadline)
                .unwrap();
        let nested =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime_until(&runtime, deadline)
                .unwrap();
        assert!(Rc::ptr_eq(&outer._file, &nested._file));
        assert_eq!(SHARED_CAS_GUARD_DEPTH.with(Cell::get), 2);
        drop(outer);
        assert_eq!(SHARED_CAS_GUARD_DEPTH.with(Cell::get), 1);
        nested.ensure_protects_pinned_runtime(&runtime).unwrap();
        drop(nested);
        assert_eq!(SHARED_CAS_GUARD_DEPTH.with(Cell::get), 0);
        assert!(CAS_GUARD_FILE.with(|file| file.borrow().is_none()));

        let exclusive =
            CasMutationGuard::acquire_existing_exclusive_in_pinned_runtime(&runtime).unwrap();
        let nested =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime_until(&runtime, deadline)
                .unwrap();
        assert!(Rc::ptr_eq(&exclusive._file, &nested._file));
        assert_eq!(EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get), 2);
        assert!(!nested.is_exclusive());
        drop(exclusive);
        nested.ensure_protects_pinned_runtime(&runtime).unwrap();
        drop(nested);
        assert_eq!(EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get), 0);
        assert!(CAS_GUARD_FILE.with(|file| file.borrow().is_none()));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_pinned_shared_guard_refuses_expired_reentry_without_depth_change() {
        let temp = tempfile::tempdir().unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let runtime = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let original =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime(&runtime).unwrap();
        let error = match CasMutationGuard::acquire_existing_shared_in_pinned_runtime_until(
            &runtime,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
        ) {
            Ok(_) => panic!("expired nested CAS guard unexpectedly acquired"),
            Err(error) => error,
        };
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(SHARED_CAS_GUARD_DEPTH.with(Cell::get), 1);
        assert!(
            CAS_GUARD_FILE
                .with(|file| Rc::ptr_eq(file.borrow().as_ref().unwrap(), &original._file))
        );
        original.ensure_protects_pinned_runtime(&runtime).unwrap();
        drop(original);
        assert_eq!(SHARED_CAS_GUARD_DEPTH.with(Cell::get), 0);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_pinned_shared_guard_refuses_foreign_runtime_without_depth_change() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        CasMutationGuard::ensure_anchor(first.path()).unwrap();
        CasMutationGuard::ensure_anchor(second.path()).unwrap();
        let runtime = lillux::PinnedDirectory::open(first.path())
            .unwrap()
            .unwrap();
        let foreign = lillux::PinnedDirectory::open(second.path())
            .unwrap()
            .unwrap();
        let original =
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime(&runtime).unwrap();
        let error = match CasMutationGuard::acquire_existing_shared_in_pinned_runtime_until(
            &foreign,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1)),
        ) {
            Ok(_) => panic!("foreign nested CAS guard unexpectedly acquired"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}")
                .contains("held CAS mutation guard does not protect pinned runtime")
        );
        assert_eq!(SHARED_CAS_GUARD_DEPTH.with(Cell::get), 1);
        original.ensure_protects_pinned_runtime(&runtime).unwrap();
        drop(original);
        assert_eq!(SHARED_CAS_GUARD_DEPTH.with(Cell::get), 0);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_pinned_shared_guard_does_not_create_missing_anchor() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(1));
        assert!(
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime_until(&runtime, deadline)
                .is_err()
        );
        assert!(!temp.path().join("recovery").exists());
        fs::create_dir(temp.path().join("recovery")).unwrap();
        assert!(
            CasMutationGuard::acquire_existing_shared_in_pinned_runtime_until(&runtime, deadline)
                .is_err()
        );
        assert_eq!(
            fs::read_dir(temp.path().join("recovery")).unwrap().count(),
            0
        );
        assert_eq!(SHARED_CAS_GUARD_DEPTH.with(Cell::get), 0);
        assert!(CAS_GUARD_FILE.with(|file| file.borrow().is_none()));
    }

    #[test]
    fn existing_shared_cas_guard_is_mutation_free() {
        let temp = tempfile::tempdir().unwrap();
        let recovery_dir = temp.path().join("recovery");

        assert!(CasMutationGuard::acquire_existing_shared(temp.path()).is_err());
        assert!(!recovery_dir.exists());

        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let before = fs::read_dir(&recovery_dir).unwrap().count();
        drop(CasMutationGuard::acquire_existing_shared(temp.path()).unwrap());
        assert_eq!(fs::read_dir(&recovery_dir).unwrap().count(), before);
    }

    #[test]
    fn cas_mutation_guard_rejects_shared_to_exclusive_upgrade() {
        let temp = tempfile::tempdir().unwrap();
        let _shared = CasMutationGuard::acquire_shared(temp.path()).unwrap();
        let error = match CasMutationGuard::acquire_exclusive(temp.path()) {
            Ok(_) => panic!("shared-to-exclusive upgrade unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("while shared guard is held"));
    }

    #[test]
    fn nested_cas_mutation_guard_rejects_a_different_runtime_root() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        CasMutationGuard::ensure_anchor(first.path()).unwrap();
        CasMutationGuard::ensure_anchor(second.path()).unwrap();
        let _shared = CasMutationGuard::acquire_existing_shared(first.path()).unwrap();
        let error = match CasMutationGuard::acquire_existing_shared(second.path()) {
            Ok(_) => panic!("cross-root nested guard unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("different runtime state root"));
    }

    #[test]
    fn cas_mutation_guard_state_survives_out_of_order_drop() {
        let temp = tempfile::tempdir().unwrap();
        let outer = CasMutationGuard::acquire_shared(temp.path()).unwrap();
        let nested = CasMutationGuard::acquire_shared(temp.path()).unwrap();
        drop(outer);
        drop(nested);

        let exclusive = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        drop(exclusive);
    }
}
