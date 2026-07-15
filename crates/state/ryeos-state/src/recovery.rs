//! Durable recovery protocol for chain-head transitions and projection baselines.
//!
//! The journal is deliberately local operational state: signed heads and CAS
//! objects remain authoritative. One record exists per chain, and transitions
//! are never overwritten. A pending transition must be explicitly repaired or
//! cancelled and compare-acknowledged before another transition is prepared.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
const PENDING_SCHEMA: u32 = 1;
const GENERATION_SCHEMA: u32 = 1;
const STAGED_ROOTS_SCHEMA: u32 = 1;
const DURABLE_UPLOAD_SCHEMA: u32 = 1;
static UNIQUE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static SHARED_CAS_GUARD_DEPTH: Cell<usize> = const { Cell::new(0) };
    static EXCLUSIVE_CAS_GUARD_DEPTH: Cell<usize> = const { Cell::new(0) };
    static CAS_GUARD_FILE: RefCell<Option<Rc<File>>> = const { RefCell::new(None) };
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

    fn validate(&self) -> Result<()> {
        match self {
            Self::ProjectHead {
                principal_key,
                project_hash,
            } => {
                validate_hash("durable upload project principal key", principal_key)?;
                validate_hash("durable upload project hash", project_hash)
            }
        }
    }
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
    created_at: String,
    object_hashes: BTreeSet<String>,
    blob_hashes: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StagedCasRootHashes {
    pub object_hashes: Vec<String>,
    pub blob_hashes: Vec<String>,
}

/// Durable temporary GC roots for async CAS staging. The OS lock remains held
/// across awaits; hashes are added durably before their first CAS write.
pub struct StagedCasRootLease {
    recovery: RecoveryStore,
    directory: lillux::PinnedDirectory,
    record: StagedCasRootsRecord,
    lock_file: Option<File>,
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
}

impl DurableCasUploadStage {
    pub fn staging_id(&self) -> &str {
        &self.record.staging_id
    }

    pub fn expected_previous_hash(&self) -> Option<&str> {
        self.record.expected_previous_hash.as_deref()
    }

    pub fn admitted_target_hash(&self) -> Option<&str> {
        self.record.admitted_target_hash.as_deref()
    }

    pub fn ensure_publication_contract(
        &self,
        publication_key: &DurableCasPublicationKey,
        expected_previous_hash: Option<&str>,
    ) -> Result<()> {
        if self.record.publication_key != publication_key {
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
        let canonical = lillux::canonical_json(value)
            .context("canonicalize durable staged CAS object")?;
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

    pub fn ensure_protects_object(&self, hash: &str) -> Result<()> {
        self.ensure_active()?;
        if !self.record.object_hashes.contains(hash) {
            anyhow::bail!("publication root {hash} was not uploaded under this staging capability");
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
        let mut changed = false;
        for hash in objects {
            changed |= self.record.object_hashes.insert(hash.to_string());
        }
        for hash in blobs {
            changed |= self.record.blob_hashes.insert(hash.to_string());
        }
        if changed {
            self.recovery
                .write_durable_upload(&self.directory, &self.record)?;
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
        validate_hash("admitted durable upload target", admitted_target_hash)?;
        if !self.record.object_hashes.contains(admitted_target_hash) {
            anyhow::bail!("admitted publication target was not protected by the upload stage");
        }
        self.record.admitted_target_hash = Some(admitted_target_hash.to_string());
        self.record.admitted_at = Some(lillux::time::iso8601_now());
        self.record.object_hashes.clear();
        self.record.blob_hashes.clear();
        self.recovery
            .write_durable_upload(&self.directory, &self.record)?;
        drop(self.lock_file.take());
        Ok(())
    }

    fn ensure_active(&self) -> Result<()> {
        if let Some(target) = self.record.admitted_target_hash.as_deref() {
            anyhow::bail!(
                "durable upload stage is an admitted receipt for publication target {target}"
            );
        }
        Ok(())
    }

    fn protect_object_hash_admitted(&mut self, hash: &str) -> Result<()> {
        validate_hash("durable staged object root", hash)?;
        if self.record.object_hashes.insert(hash.to_string()) {
            self.recovery
                .write_durable_upload(&self.directory, &self.record)?;
        }
        Ok(())
    }

    fn protect_blob_hash_admitted(&mut self, hash: &str) -> Result<()> {
        validate_hash("durable staged blob root", hash)?;
        if self.record.blob_hashes.insert(hash.to_string()) {
            self.recovery
                .write_durable_upload(&self.directory, &self.record)?;
        }
        Ok(())
    }

    fn runtime_state_dir(&self) -> Result<&Path> {
        self.recovery
            .root
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow::anyhow!("recovery root has no runtime-state parent"))
    }
}

impl Drop for DurableCasUploadStage {
    fn drop(&mut self) {
        drop(self.lock_file.take());
    }
}

impl StagedCasRootLease {
    pub fn store_object_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        cas: &lillux::cas::CasStore,
        value: &serde_json::Value,
    ) -> Result<String> {
        self.recovery.ensure_guard(cas_mutation_guard)?;
        let canonical =
            lillux::canonical_json(value).context("canonicalize staged CAS object")?;
        let expected = lillux::sha256_hex(canonical.as_bytes());
        self.protect_object_hash_admitted(cas_mutation_guard, &expected)?;
        let stored = cas.store_object(value)?;
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
        self.recovery.ensure_guard(cas_mutation_guard)?;
        validate_hash("staged CAS object root", hash)?;
        if self.record.object_hashes.insert(hash.to_string()) {
            self.recovery
                .write_staged_roots(&self.directory, &self.record)?;
        }
        Ok(())
    }

    pub fn protect_blob_hash_admitted(
        &mut self,
        cas_mutation_guard: &CasMutationGuard,
        hash: &str,
    ) -> Result<()> {
        self.recovery.ensure_guard(cas_mutation_guard)?;
        validate_hash("staged CAS blob root", hash)?;
        if self.record.blob_hashes.insert(hash.to_string()) {
            self.recovery
                .write_staged_roots(&self.directory, &self.record)?;
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
        self.recovery.ensure_guard(cas_mutation_guard)?;
        let object_hashes = object_hashes.into_iter().collect::<Vec<_>>();
        let blob_hashes = blob_hashes.into_iter().collect::<Vec<_>>();
        for hash in &object_hashes {
            validate_hash("staged CAS object root", hash)?;
        }
        for hash in &blob_hashes {
            validate_hash("staged CAS blob root", hash)?;
        }

        let mut changed = false;
        for hash in object_hashes {
            changed |= self.record.object_hashes.insert(hash.to_string());
        }
        for hash in blob_hashes {
            changed |= self.record.blob_hashes.insert(hash.to_string());
        }
        if changed {
            self.recovery
                .write_staged_roots(&self.directory, &self.record)?;
        }
        Ok(())
    }

    /// Release this lease using an already-held CAS mutation guard. This is
    /// the admitted path for callers that also hold a later lock in the global
    /// mutation hierarchy (for example the daemon StateStore mutex).
    pub fn finish_admitted(&mut self, cas_mutation_guard: &CasMutationGuard) -> Result<()> {
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
                .map_or(true, |operation| operation == record.operation)
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
    exclusive: bool,
    depth: GuardDepth,
    _not_send: std::marker::PhantomData<Rc<()>>,
}

impl CasMutationGuard {
    pub(crate) fn acquire_existing_shared_in_pinned_runtime(
        runtime: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        let exclusive_depth = EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get);
        if exclusive_depth != 0 {
            let file = current_guard_file_for_pinned_runtime(runtime)?;
            EXCLUSIVE_CAS_GUARD_DEPTH.with(|depth| depth.set(exclusive_depth + 1));
            return Ok(Self {
                _file: file,
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
                exclusive: false,
                depth: GuardDepth::Shared,
                _not_send: std::marker::PhantomData,
            });
        }

        let recovery = runtime
            .open_child_directory(std::ffi::OsStr::new("recovery"))?
            .ok_or_else(|| anyhow::anyhow!("CAS mutation lock directory is absent"))?;
        let file = recovery
            .open_regular(std::ffi::OsStr::new("cas-mutation.lock"), true)?
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
            exclusive: false,
            depth: GuardDepth::Shared,
            _not_send: std::marker::PhantomData,
        })
    }

    pub(crate) fn acquire_exclusive_in_pinned_runtime(
        runtime: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        Self::acquire_exclusive_in_pinned_runtime_mode(runtime, true)
    }

    pub(crate) fn acquire_existing_exclusive_in_pinned_runtime(
        runtime: &lillux::PinnedDirectory,
    ) -> Result<Self> {
        Self::acquire_exclusive_in_pinned_runtime_mode(runtime, false)
    }

    fn acquire_exclusive_in_pinned_runtime_mode(
        runtime: &lillux::PinnedDirectory,
        create: bool,
    ) -> Result<Self> {
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
        let lock_name = std::ffi::OsStr::new("cas-mutation.lock");
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

    pub fn acquire_shared(runtime_state_dir: &Path) -> Result<Self> {
        Self::acquire(runtime_state_dir, false, true)
    }

    /// Acquire the established guard in shared mode without creating recovery
    /// state. GC dry-runs use this for per-chain consistency checks before the
    /// later exclusive sweep inspection.
    pub fn acquire_existing_shared(runtime_state_dir: &Path) -> Result<Self> {
        Self::acquire(runtime_state_dir, false, false)
    }

    pub fn acquire_exclusive(runtime_state_dir: &Path) -> Result<Self> {
        Self::acquire(runtime_state_dir, true, true)
    }

    /// Acquire the established guard without creating recovery state. Used by
    /// strict read-only verification, where absence is an error rather than an
    /// invitation to mutate the node merely by inspecting it.
    pub fn acquire_existing_exclusive(runtime_state_dir: &Path) -> Result<Self> {
        Self::acquire(runtime_state_dir, true, false)
    }

    pub fn shared_from_cas_root(cas_root: &Path) -> Result<Self> {
        let runtime_state_dir = cas_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("CAS root has no runtime-state parent"))?;
        Self::acquire_shared(runtime_state_dir)
    }

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
            .open_regular(std::ffi::OsStr::new("cas-mutation.lock"), false)?
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
        let expected_path = runtime_state_dir.join("recovery/cas-mutation.lock");
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

    fn acquire(runtime_state_dir: &Path, exclusive: bool, create_if_missing: bool) -> Result<Self> {
        if exclusive {
            let exclusive_depth = EXCLUSIVE_CAS_GUARD_DEPTH.with(Cell::get);
            if exclusive_depth != 0 {
                let file = current_guard_file_for(runtime_state_dir)?;
                EXCLUSIVE_CAS_GUARD_DEPTH.with(|depth| depth.set(exclusive_depth + 1));
                return Ok(Self {
                    _file: file,
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
                    exclusive: false,
                    depth: GuardDepth::Shared,
                    _not_send: std::marker::PhantomData,
                });
            }
        }

        let lock_path = runtime_state_dir.join("recovery/cas-mutation.lock");
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
                std::ffi::OsStr::new("cas-mutation.lock"),
                true,
                false,
                0o600,
            )?
        } else {
            parent
                .open_regular(std::ffi::OsStr::new("cas-mutation.lock"), true)?
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
    let expected_path = runtime_state_dir.join("recovery/cas-mutation.lock");
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
        .open_regular(std::ffi::OsStr::new("cas-mutation.lock"), false)?
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
        };
        self.write_staged_roots(&staged_directory, &record)?;
        Ok(StagedCasRootLease {
            recovery: self.clone(),
            directory: staged_directory,
            record,
            lock_file: Some(lock_file),
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
            created_at: lillux::time::iso8601_now(),
            object_hashes: BTreeSet::new(),
            blob_hashes: BTreeSet::new(),
        };
        self.write_durable_upload(&durable_directory, &record)?;
        Ok(DurableCasUploadStage {
            recovery: self.clone(),
            directory: durable_directory,
            record,
            lock_file: Some(lock_file),
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
            match entry.path.extension().and_then(|value| value.to_str()) {
                Some("json") => records.push(entry),
                Some("lock") => {
                    locks.insert(entry.name.clone(), entry);
                }
                _ => anyhow::bail!(
                    "unexpected durable CAS upload entry: {}",
                    entry.path.display()
                ),
            }
        }
        let mut retired = 0;
        for record_entry in records {
            let record =
                self.read_durable_upload_file(record_entry.file.try_clone()?, &record_entry.path)?;
            let lock_name = std::ffi::OsString::from(format!("{}.lock", record.staging_id));
            let lock_entry = locks.remove(&lock_name).ok_or_else(|| {
                anyhow::anyhow!("durable CAS upload {} has no lock file", record.staging_id)
            })?;
            if record.created_at.as_str() >= created_before {
                continue;
            }
            if !try_lock_exclusive(&lock_entry.file, "lock durable upload for retirement")? {
                tracing::debug!(
                    staging_id = %record.staging_id,
                    "preserving active durable CAS upload past the retirement cutoff"
                );
                continue;
            }
            directory
                .remove_if_same(&record_entry.name, &record_entry.file)
                .context("retire durable CAS upload record")?;
            directory
                .remove_if_same(&lock_entry.name, &lock_entry.file)
                .context("retire durable CAS upload lock")?;
            retired += 1;
        }
        if let Some((_name, orphan)) = locks.into_iter().next() {
            anyhow::bail!("orphan durable CAS upload lock: {}", orphan.path.display());
        }
        Ok(retired)
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
            match entry.path.extension().and_then(|value| value.to_str()) {
                Some("json") => records.push(entry),
                Some("lock") => {
                    locks.insert(entry.name.clone(), entry);
                }
                _ => anyhow::bail!("unexpected staged CAS root entry: {}", entry.path.display()),
            }
        }
        let mut object_hashes = BTreeSet::new();
        let mut blob_hashes = BTreeSet::new();
        for record_entry in records {
            let lease_id = record_entry
                .path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 staged CAS root filename"))?;
            validate_instance_id(lease_id)?;
            let lock_name = std::ffi::OsString::from(format!("{lease_id}.lock"));
            let lock_entry = locks
                .remove(&lock_name)
                .ok_or_else(|| anyhow::anyhow!("staged CAS root {lease_id} has no lock file"))?;
            #[cfg(unix)]
            let active = {
                use std::os::fd::AsRawFd;
                let result = unsafe {
                    libc::flock(lock_entry.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
                };
                if result == 0 {
                    false
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        true
                    } else {
                        return Err(error).context("inspect staged CAS root lease lock");
                    }
                }
            };
            #[cfg(not(unix))]
            anyhow::bail!("staged CAS root locking is unavailable on this platform");
            if !active {
                directory
                    .remove_if_same(&record_entry.name, &record_entry.file)
                    .context("remove abandoned staged CAS roots")?;
                directory
                    .remove_if_same(&lock_entry.name, &lock_entry.file)
                    .context("remove abandoned staged CAS root lock")?;
                continue;
            }
            let record =
                self.read_staged_roots_file(record_entry.file.try_clone()?, &record_entry.path)?;
            object_hashes.extend(record.object_hashes);
            blob_hashes.extend(record.blob_hashes);
        }
        for (_name, lock_entry) in locks {
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                if unsafe {
                    libc::flock(lock_entry.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
                } == 0
                {
                    directory
                        .remove_if_same(&lock_entry.name, &lock_entry.file)
                        .context("remove orphan staged CAS root lock")?;
                }
            }
            #[cfg(not(unix))]
            anyhow::bail!("staged CAS root locking is unavailable on this platform");
        }
        self.collect_durable_upload_roots(&mut object_hashes, &mut blob_hashes)?;
        Ok(StagedCasRootHashes {
            object_hashes: object_hashes.into_iter().collect(),
            blob_hashes: blob_hashes.into_iter().collect(),
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
        let Some(directory) = self.open_child_directory("staged-cas-roots")? else {
            self.collect_durable_upload_roots(&mut object_hashes, &mut blob_hashes)?;
            return Ok(StagedCasRootHashes {
                object_hashes: object_hashes.into_iter().collect(),
                blob_hashes: blob_hashes.into_iter().collect(),
            });
        };

        let mut records = Vec::new();
        let mut locks = BTreeMap::new();
        for entry in directory.regular_files()? {
            match entry.path.extension().and_then(|value| value.to_str()) {
                Some("json") => records.push(entry),
                Some("lock") => {
                    locks.insert(entry.name.clone(), entry);
                }
                _ => anyhow::bail!("unexpected staged CAS root entry: {}", entry.path.display()),
            }
        }
        for entry in records {
            let record = self.read_staged_roots_file(entry.file, &entry.path)?;
            let lock_name = std::ffi::OsString::from(format!("{}.lock", record.lease_id));
            if locks.remove(&lock_name).is_none() {
                anyhow::bail!("staged CAS root {} has no lock file", record.lease_id);
            }
            object_hashes.extend(record.object_hashes);
            blob_hashes.extend(record.blob_hashes);
        }
        if let Some((_name, orphan)) = locks.into_iter().next() {
            anyhow::bail!("orphan staged CAS root lock: {}", orphan.path.display());
        }
        self.collect_durable_upload_roots(&mut object_hashes, &mut blob_hashes)?;
        Ok(StagedCasRootHashes {
            object_hashes: object_hashes.into_iter().collect(),
            blob_hashes: blob_hashes.into_iter().collect(),
        })
    }

    pub fn read_generation(&self) -> Result<Option<ProjectionRecoveryGeneration>> {
        let Some(mut file) = self
            .directory
            .open_regular(std::ffi::OsStr::new("generation.json"), false)?
        else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .context("read recovery generation")?;
        let generation: ProjectionRecoveryGeneration =
            serde_json::from_slice(&bytes).context("decode recovery generation")?;
        generation.validate()?;
        Ok(Some(generation))
    }

    pub(crate) fn write_generation(&self, generation: &ProjectionRecoveryGeneration) -> Result<()> {
        generation.validate()?;
        let value = serde_json::to_value(generation)?;
        let bytes = lillux::canonical_json(&value)
            .context("canonicalize recovery generation")?;
        let name = std::ffi::OsStr::new("generation.json");
        let expected = self.directory.open_regular(name, false)?;
        self.directory
            .atomic_write_if_same(name, expected.as_ref(), bytes.as_bytes(), 0o600)
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
            let path = &entry.path;
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                anyhow::bail!("unexpected pending transition file: {}", path.display());
            }
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 pending transition filename"))?;
            validate_chain_root_id(stem)?;
            records.push(decode_pending_file(entry.file, stem)?);
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
                entries.push(entry.name);
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
                HeadOperation::Remove => anyhow::bail!(
                    "chain {chain_root_id} already has a different pending Remove"
                ),
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
        let bytes = lillux::canonical_json(&value)
            .context("canonicalize pending chain-head transition")?;
        let parent = self.open_or_create_child_directory("pending")?;
        let name = format!("{}.json", record.chain_root_id);
        parent
            .atomic_write_if_same(
                std::ffi::OsStr::new(&name),
                expected,
                bytes.as_bytes(),
                0o600,
            )
            .with_context(|| {
                format!(
                    "durably write pending transition for {}",
                    record.chain_root_id
                )
            })
    }

    fn runtime_state_dir(&self) -> Result<&Path> {
        Ok(self.runtime_directory.path())
    }

    fn collect_durable_upload_roots(
        &self,
        object_hashes: &mut BTreeSet<String>,
        blob_hashes: &mut BTreeSet<String>,
    ) -> Result<()> {
        let Some(directory) = self.open_child_directory("durable-cas-uploads")? else {
            return Ok(());
        };
        let mut records = Vec::new();
        let mut locks = BTreeMap::new();
        for entry in directory.regular_files()? {
            match entry.path.extension().and_then(|value| value.to_str()) {
                Some("json") => records.push(entry),
                Some("lock") => {
                    locks.insert(entry.name.clone(), entry);
                }
                _ => anyhow::bail!(
                    "unexpected durable CAS upload entry: {}",
                    entry.path.display()
                ),
            }
        }
        for entry in records {
            let record = self.read_durable_upload_file(entry.file, &entry.path)?;
            let lock_name = std::ffi::OsString::from(format!("{}.lock", record.staging_id));
            if locks.remove(&lock_name).is_none() {
                anyhow::bail!("durable CAS upload {} has no lock file", record.staging_id);
            }
            object_hashes.extend(record.object_hashes);
            blob_hashes.extend(record.blob_hashes);
        }
        if let Some((_name, orphan)) = locks.into_iter().next() {
            anyhow::bail!("orphan durable CAS upload lock: {}", orphan.path.display());
        }
        Ok(())
    }

    fn write_staged_roots(
        &self,
        directory: &lillux::PinnedDirectory,
        record: &StagedCasRootsRecord,
    ) -> Result<()> {
        if record.schema != STAGED_ROOTS_SCHEMA
            || record.owner_pid == 0
            || record.purpose.is_empty()
        {
            anyhow::bail!("invalid staged CAS root record");
        }
        crate::objects::parse_canonical_timestamp(&record.created_at)
            .context("staged CAS root created_at is not canonical")?;
        validate_instance_id(&record.lease_id)?;
        for hash in &record.object_hashes {
            validate_hash("staged CAS object root", hash)?;
        }
        for hash in &record.blob_hashes {
            validate_hash("staged CAS blob root", hash)?;
        }
        let value = serde_json::to_value(record)?;
        let bytes = lillux::canonical_json(&value)
            .context("canonicalize staged CAS roots")?;
        let name = format!("{}.json", record.lease_id);
        let name = std::ffi::OsStr::new(&name);
        let expected = directory.open_regular(name, false)?;
        directory
            .atomic_write_if_same(name, expected.as_ref(), bytes.as_bytes(), 0o600)
            .context("durably write staged CAS roots")
    }

    fn read_staged_roots_file(&self, file: File, path: &Path) -> Result<StagedCasRootsRecord> {
        decode_staged_roots(file, path)
    }

    fn write_durable_upload(
        &self,
        directory: &lillux::PinnedDirectory,
        record: &DurableCasUploadRecord,
    ) -> Result<()> {
        validate_durable_upload_record(record)?;
        let value = serde_json::to_value(record)?;
        let bytes = lillux::canonical_json(&value)
            .context("canonicalize durable CAS upload stage")?;
        let name = format!("{}.json", record.staging_id);
        let name = std::ffi::OsStr::new(&name);
        let expected = directory.open_regular(name, false)?;
        directory
            .atomic_write_if_same(name, expected.as_ref(), bytes.as_bytes(), 0o600)
            .context("durably write CAS upload stage")
    }

    fn read_durable_upload_file(&self, file: File, path: &Path) -> Result<DurableCasUploadRecord> {
        decode_durable_upload(file, path)
    }
}

fn decode_staged_roots(mut file: File, path: &Path) -> Result<StagedCasRootsRecord> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let record: StagedCasRootsRecord =
        serde_json::from_slice(&bytes).context("decode staged CAS roots")?;
    if record.schema != STAGED_ROOTS_SCHEMA {
        anyhow::bail!("unsupported staged CAS roots schema: {}", record.schema);
    }
    validate_instance_id(&record.lease_id)?;
    if path.file_stem().and_then(|value| value.to_str()) != Some(record.lease_id.as_str()) {
        anyhow::bail!("staged CAS root path/record mismatch");
    }
    if record.owner_pid == 0 || record.purpose.is_empty() {
        anyhow::bail!("invalid staged CAS root record");
    }
    crate::objects::parse_canonical_timestamp(&record.created_at)
        .context("staged CAS root created_at is not canonical")?;
    for hash in &record.object_hashes {
        validate_hash("staged CAS object root", hash)?;
    }
    for hash in &record.blob_hashes {
        validate_hash("staged CAS blob root", hash)?;
    }
    Ok(record)
}

fn decode_durable_upload(mut file: File, path: &Path) -> Result<DurableCasUploadRecord> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let record: DurableCasUploadRecord =
        serde_json::from_slice(&bytes).context("decode durable CAS upload stage")?;
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
    ) {
        (None, None) => {}
        (Some(hash), Some(admitted_at)) => {
            validate_hash("durable upload admitted target", hash)?;
            crate::objects::parse_canonical_timestamp(admitted_at)
                .context("durable upload admitted_at is not canonical")?;
            if !record.object_hashes.is_empty() || !record.blob_hashes.is_empty() {
                anyhow::bail!("admitted durable upload receipt still carries staging roots");
            }
        }
        _ => anyhow::bail!("durable upload admission receipt is incomplete"),
    }
    crate::objects::parse_canonical_timestamp(&record.created_at)
        .context("durable CAS upload created_at is not canonical")?;
    for hash in &record.object_hashes {
        validate_hash("durable staged object root", hash)?;
    }
    for hash in &record.blob_hashes {
        validate_hash("durable staged blob root", hash)?;
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

fn try_lock_exclusive(file: &File, label: &str) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(false)
        } else {
            Err(error).with_context(|| label.to_string())
        }
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

fn decode_pending_file(mut file: File, chain_root_id: &str) -> Result<PendingChainHeadTransition> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
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
            "created_at": "2026-07-14T12:00:00+00:00",
            "object_hashes": [],
            "blob_hashes": [],
        })
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

        assert!(store
            .replace_remove_with_published_set(&lock, "T-root", "different-transition", &hash("b"),)
            .is_err());
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
        assert!(store
            .prepare_set(&lock, "T-root", Some(&hash("a")), &hash("b"))
            .is_err());
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
        assert!(store
            .acknowledge(&lock, "T-root", &record.transition_id)
            .unwrap());
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
        assert!(store
            .advance_phase(
                &lock,
                "T-root",
                &record.transition_id,
                TransitionPhase::Prepared,
                TransitionPhase::ApplyingProjection,
            )
            .is_err());
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
        let staged_dir = temp.path().join("recovery/staged-cas-roots");
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
        let _shared = CasMutationGuard::acquire_shared(first.path()).unwrap();
        let error = match CasMutationGuard::acquire_shared(second.path()) {
            Ok(_) => panic!("cross-root nested guard unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("different runtime state root"));
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
