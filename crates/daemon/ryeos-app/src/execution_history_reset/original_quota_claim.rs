//! Durable local quota reservation. Publication does not supply native
//! acquisition, work metering, financial funding, or physical cleanup evidence.

use std::ffi::OsStr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, ensure};
use lillux_sqlite::SqliteInspectionCopyWork as _;
use ryeos_scheduler_contract::OfflineResetQuota;
use ryeos_state::ThreadHistoryDiscardOperationRef;
use serde::{Deserialize, Serialize};

use super::creation_evidence::{CreatedDirectoryRecord, evidence_name};
use super::inspection_destination::{
    InspectionAccountingRequirement, InspectionDestinationPlan, OriginalInspectionMode,
    OriginalInspectionPlan, destination_plans, validate_destinations,
};
use super::inspection_plan::{
    InspectionSourcePlan, OperationalSourcePins, OperationalSourcePlan, capture_sources,
    validate_sources,
};
use super::offline_policy::OfflineResetPolicyOwner;
use super::original_operation::OriginalOperation;
use ryeos_node_runtime::identity::NodeIdentity;

use lillux_sqlite::original_host_owner::region::NativeHostRegion;
pub(super) const HOST_RELEASE_ATTEMPTS: u64 = 8;

const PREFIX: &str = "reserved.";
const MAX_RECORD_BYTES: usize = 8192;
const MAX_CLAIMS: usize = 64;

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_PUBLICATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_CREATED_EVIDENCE_ACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn fail_next_publication_ack() {
    FAIL_AFTER_PUBLICATION.with(|slot| slot.set(true));
}

#[cfg(test)]
pub(super) fn fail_next_created_evidence_ack() {
    FAIL_CREATED_EVIDENCE_ACK.with(|slot| slot.set(true));
}

fn required_nullable<'de, D>(
    d: D,
) -> std::result::Result<Option<ThreadHistoryDiscardOperationRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<ThreadHistoryDiscardOperationRef>::deserialize(d)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimRecord {
    schema: u32,
    purpose: String,
    operation_id: String,
    claim_nonce: String,
    signer: String,
    policy_digest: String,
    app_root_identity: String,
    runtime_identity: String,
    #[serde(deserialize_with = "required_nullable")]
    prior_operation: Option<ThreadHistoryDiscardOperationRef>,
    reserved: OfflineResetQuota,
    resident_process: u32,
    resident_generation: u64,
    resident_bytes: u64,
    occupancy: lillux::time::OccupancyLimit,
    clock_binding: String,
    sources: [InspectionSourcePlan; 2],
    operational_source: OperationalSourcePlan,
    copy_root_identity: String,
    destinations: [InspectionDestinationPlan; 2],
    inspection: OriginalInspectionPlan,
}

impl ClaimRecord {
    fn verified_original(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        ensure!(!bytes.is_empty() && bytes.len() <= MAX_RECORD_BYTES,
            "original signed claim exceeds recovery bound");
        let raw = std::str::from_utf8(bytes)?;
        let (body, header) = lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)?;
        let header = header.context("original recovery claim is unsigned")?;
        ensure!(lillux::signature::is_valid_signature_for(
            &header.content_hash, &header.signature_b64, &header.signer_fingerprint,
            &body, identity.verifying_key(), identity.fingerprint()),
            "original recovery claim signature is invalid");
        let record: Self = serde_json::from_str(&body)?;
        record.validate()?;
        ensure!(record.signer == identity.fingerprint()
            && lillux::canonical_json(&serde_json::to_value(&record)?)? == body,
            "original recovery claim has foreign signer or noncanonical body");
        Ok(record)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 9 && self.purpose == "offline_reset_local_quota_reservation_v9",
            "unsupported offline quota claim"
        );
        for (name, value, length) in [
            ("operation", self.operation_id.as_str(), 32),
            ("claim nonce", self.claim_nonce.as_str(), 32),
            ("policy", self.policy_digest.as_str(), 64),
            ("clock binding", self.clock_binding.as_str(), 64),
        ] {
            ensure!(
                value.len() == length
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid offline quota {name}"
            );
        }
        if let Some(prior) = &self.prior_operation {
            prior.validate()?;
            ensure!(
                prior.operation_id == self.operation_id,
                "quota claim changes the prior operation"
            );
        }
        self.reserved.validate()?;
        let resident = lillux_sqlite::original_host_owner::backing::HostStorage::quote_native_region()
            .map_err(anyhow::Error::msg)?;
        ensure!(self.resident_process != 0 && self.resident_generation != 0
            && self.resident_bytes == resident.bytes() as u64
            && self.resident_bytes.checked_add(self.reserved.max_native_storage_bytes)
                .is_some_and(|bytes| bytes <= self.reserved.max_memory_bytes),
            "offline claim lacks exact resident incarnation/memory debit");
        validate_sources(&self.sources)?;
        self.operational_source
            .validate_with_copies(&self.sources)?;
        self.inspection
            .validate(self.prior_operation.as_ref(), &self.sources)?;
        ensure!(
            !self.copy_root_identity.is_empty(),
            "copy reservation lacks original destination parent"
        );
        validate_destinations(&self.destinations, &self.operation_id, &self.sources)?;
        ensure!(
            self.reserved.max_source_roles >= ryeos_scheduler_contract::OFFLINE_RESET_SOURCE_ROLES,
            "offline quota cannot hold its thirteen fixed source roles"
        );
        let bytes = self.sources.iter().try_fold(0u64, |total, source| {
            total
                .checked_add(source.total_bytes()?)
                .context("inspection aggregate source overflow")
        })?;
        ensure!(
            bytes <= self.reserved.max_copied_bytes
                && bytes <= self.reserved.max_retained_destination_bytes,
            "planned source extent exceeds original copy/destination reservation"
        );
        self.occupancy.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            self.occupancy.maximum_occupancy_ns == occupancy_nanoseconds(&self.reserved)?,
            "offline quota clock limit differs from its signed service/cleanup bounds"
        );
        Ok(())
    }
}

fn occupancy_nanoseconds(quota: &OfflineResetQuota) -> Result<u64> {
    quota
        .max_operation_millis
        .checked_add(quota.max_cleanup_millis)
        .and_then(|millis| millis.checked_mul(1_000_000))
        .context("offline quota service/cleanup occupancy overflow")
}

/// Same original signed reservation and descriptor owners on publication
/// uncertainty. No Drop release: retained deposits remain charged until a
/// separately verified physical-close and durable cleanup path is implemented.
pub(super) struct OriginalQuotaClaim {
    record: ClaimRecord,
    clock: lillux::time::RetainedOccupancyClock,
    native_service_deadline: lillux::time::MonotonicDeadline,
    native_cleanup_deadline: lillux::time::MonotonicDeadline,
    signer: NodeIdentity,
    signed: Vec<u8>,
    app_root: lillux::PinnedDirectory,
    runtime: lillux::PinnedDirectory,
    ledger: lillux::PinnedDirectory,
    copy_root: lillux::PinnedDirectory,
    inspection_namespace: Mutex<Option<lillux::PinnedDirectoryLock>>,
    publication_entered: AtomicBool,
    publication_acknowledged: AtomicBool,
    finalizer_master_entered: AtomicBool,
    reservation_publication: Mutex<Option<Arc<lillux_sqlite::RetainedOriginalWitnessFile>>>,
    copy_usage: Mutex<CopyUsage>,
    production: Mutex<OriginalProductionState>,
    copy_issuer: Mutex<Option<std::sync::Weak<OriginalCopyWork>>>,
    native_factory: Mutex<Option<std::sync::Weak<lillux_sqlite::OriginalNativeFactory>>>,
    native_admission: Mutex<Option<std::sync::Weak<OriginalNativeQuotaAdmission>>>,
    resident: Mutex<NativeHostRegion>,
    _prior_operation: Option<Arc<OriginalOperation>>,
    _source_roots: [lillux::PinnedDirectory; 2],
    _source_pins: Vec<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>,
    operational_source_pins: OperationalSourcePins,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuotaFailurePhase {
    Publication,
    Admission,
    Acquisition,
    NativeClose,
}

pub(super) struct QuotaCustodyFailure {
    // Close/retain the actual underlying failure owner before its claim.
    error: anyhow::Error,
    claim: Arc<OriginalQuotaClaim>,
    phase: QuotaFailurePhase,
}
impl QuotaCustodyFailure {
    #[cfg(test)]
    pub(super) fn claim(&self) -> &Arc<OriginalQuotaClaim> {
        &self.claim
    }
    #[cfg(test)]
    pub(super) fn phase(&self) -> QuotaFailurePhase {
        self.phase
    }
    #[cfg(test)]
    pub(super) fn retains(&self, claim: &Arc<OriginalQuotaClaim>) -> bool {
        Arc::ptr_eq(&self.claim, claim)
    }
}
impl QuotaCustodyFailure {
    pub(super) fn retry_native_preparation(&mut self) -> Result<bool> {
        if let Some(preparation) = self.error.downcast_mut::<OriginalNativeQuotaPreparationFailure>() {
            preparation.retry_resident_release()?;
            return Ok(true);
        }
        if let Some(admission) = self.error.downcast_ref::<OriginalNativeQuotaAdmissionFailure>() {
            admission.retry_preinstallation_retirement()?;
            return Ok(true);
        }
        Ok(false)
    }
}
impl std::fmt::Debug for QuotaCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaCustodyFailure")
            .field("error", &self.error)
            .field("phase", &self.phase)
            .field("operation", &self.claim.record.operation_id)
            .finish()
    }
}
impl std::fmt::Display for QuotaCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "original offline quota {:?} retains its reservation: {}",
            self.phase, self.error
        )
    }
}
impl std::error::Error for QuotaCustodyFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginalReservationPhase {
    PreparedOriginal,
    TerminalAcknowledgedReservationRetained,
    PostAckFinalizerMasterRetained,
}
struct OriginalSettlementAdmission {
    claim: Arc<OriginalQuotaClaim>,
    original: Arc<OriginalCopyWork>,
    deadline: lillux::time::MonotonicDeadline,
    ledger_lock: lillux::PinnedDirectoryLock,
    reservation_publication: Arc<lillux_sqlite::RetainedOriginalWitnessFile>,
    reservation_identity: lillux::OpenFileIdentity,
    phase: Mutex<OriginalReservationPhase>,
    finalizer_master: Mutex<Option<std::sync::Weak<OriginalFinalizerMaster>>>,
}
impl OriginalSettlementAdmission {
    fn checkpoint_original(&self) -> Result<()> {
        let phase = *self
            .phase
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original reservation phase busy or poisoned"))?;
        if phase == OriginalReservationPhase::PostAckFinalizerMasterRetained {
            let master = self
                .finalizer_master
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original finalizer master lookup busy or poisoned"))?
                .as_ref()
                .context("authentic finalizer master not registered")?
                .upgrade()
                .context("authentic finalizer master custody absent")?;
            return master.checkpoint_original();
        }
        ensure!(
            !self.deadline.has_elapsed(),
            "original settlement deadline expired; custody retained"
        );
        self.original
            .state_lifetime
            .ensure_protects_app_root(self.claim.app_root.path())?;
        self.original
            .namespace
            .ensure_protects(&self.claim.runtime)?;
        self.original
            .inspection_namespace
            .ensure_protects(&self.claim.copy_root)?;
        self.ledger_lock.ensure_protects(&self.claim.ledger)?;
        self.claim.app_root.ensure_path_binding()?;
        self.claim.runtime.ensure_path_binding()?;
        self.claim.copy_root.ensure_path_binding()?;
        self.claim.ledger.ensure_path_binding()?;
        for root in &self.claim._source_roots {
            root.ensure_path_binding()?;
        }
        ensure!(
            self.claim.record.clock_binding == self.claim.clock.binding_digest()
                && self.original.policy.generation_digest() == self.claim.record.policy_digest,
            "original settlement clock/policy changed"
        );
        original_cleanup_remaining(
            self.claim
                .clock
                .window(
                    &self.claim.record.occupancy,
                    std::time::Duration::from_millis(self.claim.record.reserved.max_cleanup_millis),
                )
                .map_err(anyhow::Error::msg)?,
            std::time::Duration::from_millis(self.claim.record.reserved.max_cleanup_millis),
        )?;
        let phase = *self
            .phase
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original reservation phase busy or poisoned"))?;
        match phase {
            OriginalReservationPhase::PreparedOriginal
            | OriginalReservationPhase::TerminalAcknowledgedReservationRetained => {
                // Pending subsidiary evidence does not authorize unlinking the
                // reservation. Normal CopyWork custody is never relaxed.
                ensure!(
                    self.reservation_publication.identity_original()? == self.reservation_identity,
                    "original reservation publication descriptor changed"
                );
                let current = self
                    .claim
                    .ledger
                    .entry_no_follow(OsStr::new(&self.original.reservation_name))?
                    .context("original charged reservation disappeared")?;
                ensure!(
                    current.entry_type == lillux::PinnedEntryType::Regular
                        && current.containing_device == self.reservation_identity.device()
                        && current.inode == self.reservation_identity.inode(),
                    "original charged reservation coordinate changed"
                );
                ensure!(
                    self.original.original_reservation_identity()?
                        == self.original.reservation_identity,
                    "original issued reservation pin changed"
                );
            }
            OriginalReservationPhase::PostAckFinalizerMasterRetained => {
                anyhow::bail!("original finalizer phase changed during custody validation")
            }
        }
        ensure!(
            !self.deadline.has_elapsed(),
            "original settlement deadline expired during validation"
        );
        Ok(())
    }
    fn debit_original(&self, steps: u64) -> Result<()> {
        self.checkpoint_original()?;
        ensure!(steps > 0, "original settlement debit empty");
        let mut used = self
            .claim
            .copy_usage
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original settlement counter busy or poisoned"))?;
        let total = used
            .cleanup_steps
            .checked_add(steps)
            .context("original settlement debit overflow")?;
        ensure!(
            total <= self.claim.record.reserved.max_cleanup_steps,
            "original shared settlement cleanup steps exhausted"
        );
        used.cleanup_steps = total;
        Ok(())
    }
    fn debit_original_read(&self, bytes: u64) -> Result<()> {
        self.checkpoint_original()?;
        ensure!(bytes > 0, "original retirement read debit empty");
        let mut used =
            self.claim.copy_usage.try_lock().map_err(|_| {
                anyhow::anyhow!("original retirement read counter busy or poisoned")
            })?;
        let read = used
            .read_bytes
            .checked_add(bytes)
            .context("original retirement read overflow")?;
        let memory = used
            .buffer_bytes
            .checked_add(bytes)
            .context("original retirement read memory overflow")?;
        ensure!(
            read <= self.claim.record.reserved.max_read_bytes
                && memory
                    .checked_add(used.native_storage_bytes)
                    .and_then(|bytes| bytes.checked_add(used.resident_storage_bytes))
                    .is_some_and(|total| total <= self.claim.record.reserved.max_memory_bytes),
            "original retirement read exceeds shared envelope"
        );
        used.read_bytes = read;
        used.buffer_bytes = memory;
        drop(used);
        self.checkpoint_original()
    }
    fn remaining_steps(&self) -> Result<u64> {
        self.checkpoint_original()?;
        let used = self
            .claim
            .copy_usage
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original settlement counter busy or poisoned"))?;
        self.claim
            .record
            .reserved
            .max_cleanup_steps
            .checked_sub(used.cleanup_steps)
            .context("original cleanup counter exceeds reservation")
    }
    fn reserve_records(&self, bytes: u64) -> Result<()> {
        self.checkpoint_original()?;
        let mut used =
            self.claim.copy_usage.try_lock().map_err(|_| {
                anyhow::anyhow!("original settlement record debit busy or poisoned")
            })?;
        let retained = used
            .retained_evidence_bytes
            .checked_add(bytes)
            .context("original settlement record debit overflow")?;
        ensure!(
            retained
                .checked_add(self.claim.signed.len() as u64)
                .is_some_and(|total| total <= self.claim.record.reserved.max_recovery_record_bytes),
            "original recovery record reservation exhausted before signing/publication"
        );
        ensure!(
            used.buffer_bytes
                .checked_add(used.native_storage_bytes)
                .and_then(|total| total.checked_add(used.resident_storage_bytes))
                .and_then(|total| total.checked_add(bytes))
                .is_some_and(|total| total <= self.claim.record.reserved.max_memory_bytes),
            "original record working storage exceeds local memory envelope"
        );
        used.retained_evidence_bytes = retained;
        used.buffer_bytes = used
            .buffer_bytes
            .checked_add(bytes)
            .context("original record memory debit overflow")?;
        Ok(())
    }
}
impl lillux_sqlite::OriginalWitnessCleanupWork for OriginalSettlementAdmission {
    fn retain_original_witness_progress(&self, progress: &lillux_sqlite::OriginalWitnessRemovalProgress) -> Result<()> {
        ensure!(self.claim.record.resident_process == std::process::id(), "foreign original account removal observer");
        if !progress.is_original_owner(&self.reservation_publication)? { return Ok(()); }
        let master = self.finalizer_master.try_lock()
            .map_err(|_| anyhow::anyhow!("original account phase observer lookup busy or poisoned"))?
            .as_ref().and_then(std::sync::Weak::upgrade).context("actual original release master absent")?;
        ensure!(std::ptr::eq(self, master.admission.as_ref()), "account phase observer selects another admission");
        master.retain_original_removal_progress(progress)
    }
    fn retain_original_removed_witness(&self, removed: &lillux_sqlite::RemovedOriginalWitnessFile) -> Result<()> {
        ensure!(self.claim.record.resident_process == std::process::id(), "foreign original durable removal observer");
        // Ordinary inspection witnesses use the same account but do not enter
        // reservation release. Match the actual owner before invoking its tail.
        if !removed.is_original_owner(&self.reservation_publication)? { return Ok(()); }
        let master = self.finalizer_master.try_lock()
            .map_err(|_| anyhow::anyhow!("original durable removal observer lookup busy or poisoned"))?
            .as_ref().and_then(std::sync::Weak::upgrade).context("actual original durable release master absent")?;
        ensure!(std::ptr::eq(self, master.admission.as_ref()), "durable account observer selects another admission");
        master.retain_original_removal(removed)
    }
    fn prepay_original_removal_parent(&self, bytes: u64, steps: u64) -> Result<()> {
        ensure!(bytes > 0 && steps == 2, "invalid original witness parent quote");
        self.debit_original(steps)?;
        let mut used = self.claim.copy_usage.try_lock()
            .map_err(|_| anyhow::anyhow!("original witness parent memory debit busy or poisoned"))?;
        let memory = used.buffer_bytes.checked_add(bytes)
            .context("original witness parent memory overflow")?;
        ensure!(memory.checked_add(used.native_storage_bytes)
            .and_then(|total| total.checked_add(used.resident_storage_bytes))
            .is_some_and(|total| total <= self.claim.record.reserved.max_memory_bytes),
            "original witness parent exceeds same signed memory envelope");
        used.buffer_bytes = memory;
        drop(used);
        self.checkpoint_original()
    }
    fn checkpoint(&self) -> Result<()> {
        self.checkpoint_original()
    }
    fn require_namespace(&self, parent: &lillux::PinnedDirectory) -> Result<()> {
        if parent.is_same_directory(&self.claim.copy_root)? {
            self.original.inspection_namespace.ensure_protects(parent)
        } else if parent.is_same_directory(&self.claim.ledger)? {
            self.ledger_lock.ensure_protects(parent)
        } else {
            anyhow::bail!("foreign original witness namespace")
        }
    }
    fn debit_steps(&self, steps: u64) -> Result<()> {
        self.debit_original(steps)
    }
    fn debit_retirement(&self, maximum: u32) -> Result<()> {
        ensure!(
            maximum > 0 && maximum <= i32::MAX as u32,
            "invalid original witness retirement bound"
        );
        self.debit_original(
            u64::from(maximum)
                .checked_add(1)
                .context("witness retirement debit overflow")?,
        )
    }
}
#[derive(Debug)]
struct OriginalStoredPublicationFailure {
    original: Arc<anyhow::Error>,
}
impl std::fmt::Display for OriginalStoredPublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.original.as_ref(), f)
    }
}
impl std::error::Error for OriginalStoredPublicationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.original.as_ref().as_ref())
    }
}

#[derive(Default)]
struct OriginalRecordPublication {
    attempted: bool,
    owner: Option<Arc<lillux_sqlite::RetainedOriginalWitnessFile>>,
    failure: Option<Arc<anyhow::Error>>,
    acknowledged: bool,
}
impl OriginalSettlementAdmission {
    fn publish_record(
        &self,
        name: &str,
        signed: &[u8],
        publication: &mut OriginalRecordPublication,
    ) -> Result<()> {
        self.checkpoint_original()?;
        if let Some(original) = &publication.failure {
            return Err(anyhow::Error::new(OriginalStoredPublicationFailure {
                original: original.clone(),
            }));
        }
        if !publication.attempted {
            let parent_identity = self.claim.ledger.identity()?;
            let coordinate = std::ffi::OsString::from(name);
            self.debit_original(1)?;
            publication.attempted = true;
            match self.claim
                .ledger
                .atomic_create_regular(
                    OsStr::new(name),
                    signed,
                    0o600,
                    crate::execution_history_reset::RESET_TEMP_PURPOSE,
                )
            {
                Ok(Some(file)) => {
                    publication.owner = Some(Arc::new(
                        lillux_sqlite::RetainedOriginalWitnessFile::created(
                            file,
                            parent_identity,
                            coordinate,
                        ),
                    ))
                }
                Ok(None) => {
                    publication.failure = Some(Arc::new(anyhow::anyhow!(
                        "original durable record coordinate occupied; no replacement"
                    )));
                    return Err(anyhow::Error::new(OriginalStoredPublicationFailure {
                        original: publication
                            .failure
                            .as_ref()
                            .expect("retained original collision")
                            .clone(),
                    }));
                }
                Err(error) => {
                    publication.failure = Some(Arc::new(error));
                    return Err(anyhow::Error::new(OriginalStoredPublicationFailure {
                        original: publication
                            .failure
                            .as_ref()
                            .expect("retained original publication failure")
                            .clone(),
                    }));
                }
            }
        }
        let owner = publication
            .owner
            .as_ref()
            .context("original record publication lacks actual File")?;
        self.debit_original(4)?;
        let identity = owner.identity_original()?;
        owner.require_original(&self.claim.ledger, OsStr::new(name))?;
        self
            .debit_original_read((MAX_RECORD_BYTES + 1) as u64)?;
        ensure!(
            owner.read_original(MAX_RECORD_BYTES)? == signed,
            "original durable record bytes changed"
        );
        let entry = self.claim
            .ledger
            .entry_no_follow(OsStr::new(name))?
            .context("original durable record coordinate disappeared")?;
        ensure!(
            entry.entry_type == lillux::PinnedEntryType::Regular
                && entry.containing_device == identity.device()
                && entry.inode == identity.inode(),
            "original durable record inode changed"
        );
        self.debit_original(1)?;
        self.claim.ledger.sync()?;
        publication.acknowledged = true;
        self.checkpoint_original()?;
        Ok(())
    }
}
struct OriginalWitnessDisposition {
    evidence: Option<Arc<CreatedDirectoryEvidence>>,
    removed: Option<Arc<lillux_sqlite::RemovedOriginalWitnessFile>>,
    retired: Option<Arc<lillux_sqlite::RetiredOriginalWitnessFile>>,
}
struct OriginalSettlementProgress {
    final_ack: Option<Arc<OriginalReadyForFinalAck>>,
    post_ack: Option<Arc<OriginalPostAckFinalization>>,
    disposition: OriginalRecordPublication,
    terminal: OriginalRecordPublication,
    witnesses: [OriginalWitnessDisposition; 2],
}
/// Complete two-slot ORIGINAL resource custody. This is not successful accounting.
pub(super) struct OriginalInspectionSettlement {
    seal: Arc<OriginalProductionSeal>,
    _disposals: Mutex<Option<std::mem::ManuallyDrop<Vec<Arc<crate::runtime_db::InspectionCopyDisposal>>>>> ,
    admission: Arc<OriginalSettlementAdmission>,
    disposition: super::creation_evidence::InspectionDispositionRecord,
    signed_disposition: Vec<u8>,
    terminal: super::creation_evidence::InspectionTerminalRecord,
    signed_terminal: Vec<u8>,
    progress: Mutex<OriginalSettlementProgress>,
}
impl std::fmt::Debug for OriginalInspectionSettlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalInspectionSettlement")
            .field("operation", &self.seal.claim.record.operation_id)
            .field("remaining", &self.disposition.remaining)
            .finish_non_exhaustive()
    }
}
#[derive(Debug)]
struct OriginalInspectionSettlementFailure {
    error: anyhow::Error,
    _owner: Arc<OriginalInspectionSettlement>,
}
impl std::fmt::Display for OriginalInspectionSettlementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalInspectionSettlementFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
#[derive(Debug)]
struct OriginalSealCustodyFailure {
    error: anyhow::Error,
    _seal: Arc<OriginalProductionSeal>,
    _disposals: Vec<Arc<crate::runtime_db::InspectionCopyDisposal>>,
}
impl std::fmt::Display for OriginalSealCustodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalSealCustodyFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
impl OriginalQuotaClaim {
    fn debit_original_record_working_storage(&self, bytes: u64) -> Result<()> {
        ensure!(self.record.resident_process == std::process::id() && bytes > 0,
            "foreign or empty original record working quote");
        let mut used = self.copy_usage.try_lock()
            .map_err(|_| anyhow::anyhow!("original record working debit busy or poisoned"))?;
        let total = used.buffer_bytes.checked_add(bytes)
            .context("original record working memory overflow")?;
        ensure!(total.checked_add(used.native_storage_bytes)
            .and_then(|memory| memory.checked_add(used.resident_storage_bytes))
            .is_some_and(|memory| memory <= self.record.reserved.max_memory_bytes),
            "original record working quote exceeds same signed memory envelope");
        used.buffer_bytes = total;
        Ok(())
    }

    pub(super) fn begin_inspection_settlement(
        self: &Arc<Self>,
        seal: Arc<OriginalProductionSeal>,
        disposals: Vec<Arc<crate::runtime_db::InspectionCopyDisposal>>,
    ) -> Result<Arc<OriginalInspectionSettlement>> {
        let prepared = (|| -> Result<Arc<OriginalInspectionSettlement>> {
            let work = seal.work.clone();
            seal.require_original(self, &work)?;
            let original = self.original_copy_issuer()?;
            work.cleanup_checkpoint()?;
            let deadline = self.native_cleanup_deadline;
            let ledger_lock = self
                .ledger
                .try_lock_exclusive()?
                .context("original settlement ledger exclusion busy; custody retained")?;
            let publication = self
                .reservation_publication
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original reservation publication busy or poisoned"))?
                .clone()
                .context("original reservation publication descriptor missing")?;
            publication.require_original(&self.ledger, OsStr::new(&original.reservation_name))?;
            let reservation_identity = publication.identity_original()?;
            ensure!(
                publication.read_original(MAX_RECORD_BYTES)? == self.signed,
                "original reservation publication bytes changed"
            );
            let admission = Arc::new(OriginalSettlementAdmission {
                claim: self.clone(),
                original,
                deadline,
                ledger_lock,
                reservation_publication: publication,
                reservation_identity,
                phase: Mutex::new(OriginalReservationPhase::PreparedOriginal),
                finalizer_master: Mutex::new(None),
            });
            admission.debit_original(8)?;
            let (roots, evidence, admitted, outputs) = {
                let used = self
                    .copy_usage
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("original aggregate slots busy or poisoned"))?;
                (
                    used.destination_roots.clone(),
                    used.created_evidence.clone(),
                    used.admitted,
                    used.destination_roles.clone(),
                )
            };
            let mut matched = [false; 2];
            let mut witnesses = Vec::with_capacity(2);
            let mut stores = Vec::with_capacity(2);
            for index in 0..2 {
                let slot = seal.slot(index)?;
                if seal.never_started[index] {
                    admission.debit_original(4)?;
                    let created = super::creation_evidence::evidence_name(&slot.directory_name);
                    for name in [
                        &slot.directory_name,
                        &created,
                        &format!("cleanup.{created}"),
                    ] {
                        ensure!(
                            self.copy_root.entry_no_follow(OsStr::new(name))?.is_none(),
                            "never-started slot has unexplained original namespace deposit"
                        );
                    }
                    ensure!(
                        roots[index].is_none() && evidence[index].is_none(),
                        "never-started slot retains acquired owner"
                    );
                    stores.push(
                        super::creation_evidence::InspectionStoreDispositionRecord::NeverStarted {
                            purpose: slot.purpose,
                            destination_name: slot.directory_name,
                            original_present: slot.expected_present,
                        },
                    );
                    witnesses.push(OriginalWitnessDisposition {
                        evidence: None,
                        removed: None,
                        retired: None,
                    });
                    continue;
                }
                let root = roots[index]
                    .as_ref()
                    .context("attempted original store lacks actual root custody")?;
                let witness = evidence[index]
                    .as_ref()
                    .context("attempted original store lacks actual creation witness custody")?;
                ensure!(
                    witness.acknowledged.load(Ordering::Acquire),
                    "attempted original witness acknowledgement remains unknown"
                );
                for role in 0..4 {
                    ensure!(
                        admitted[index * 4 + role]
                            && outputs[index * 4 + role].is_some() == slot.expected_present[role],
                        "original role is neither authenticated absent nor retained present"
                    );
                }
                let mut selected = None;
                for (candidate, proof) in disposals.iter().enumerate() {
                    if proof.matches_original_slot(&work, OsStr::new(&slot.directory_name)) {
                        ensure!(
                            selected.replace(candidate).is_none(),
                            "duplicate original completed store disposition"
                        );
                    }
                }
                let selected = selected.context(
                    "attempted original store lacks matching actual terminal/root/role disposal",
                )?;
                ensure!(!matched[index], "original store disposed twice");
                matched[index] = true;
                disposals[selected].require_original(
                    &work,
                    &self.copy_root,
                    OsStr::new(&slot.directory_name),
                    slot.expected_present,
                )?;
                let receipt = disposals[selected].removed_root()?;
                ensure!(
                    root.is_same_directory(receipt.child())?,
                    "original disposition substitutes retained created root"
                );
                admission.debit_original(4)?;
                admission.original.verify_created_evidence_binding(
                    index,
                    receipt.child(),
                    witness,
                )?;
                let identities = disposals[selected].role_identities()?;
                ensure!(
                    std::array::from_fn::<_, 4, _>(|role| identities[role].is_some())
                        == slot.expected_present,
                    "retired output evidence changes authenticated role presence"
                );
                stores.push(
                    super::creation_evidence::InspectionStoreDispositionRecord::Removed {
                        purpose: slot.purpose,
                        destination_name: slot.directory_name,
                        created_identity: receipt.child().identity()?,
                        witness_sha256: lillux::sha256_hex(&witness.signed),
                        retired_outputs: identities,
                    },
                );
                witnesses.push(OriginalWitnessDisposition {
                    evidence: Some(witness.clone()),
                    removed: None,
                    retired: None,
                });
            }
            ensure!(
                disposals.len() == matched.iter().filter(|matched| **matched).count(),
                "extra or foreign original completed store disposition"
            );
            use super::creation_evidence::OriginalLocalSettlementObligation as Obligation;
            use super::creation_evidence::OriginalRetainedLocalAuthority as Authority;
            let mut remaining = vec![
                Obligation::ReservationPublicationDescriptor,
                Obligation::IssuedReservationPin,
            ];
            let retained_source_pins = self
                ._source_pins
                .len()
                .checked_add(self.operational_source_pins.retained_count())
                .context("original source pin obligation overflow")?;
            if retained_source_pins > 0 {
                remaining.push(Obligation::SourcePins {
                    count: u32::try_from(retained_source_pins)?,
                });
            }
            if let Some(ancestor) = &self._prior_operation {
                remaining.push(Obligation::AncestorOperation {
                    reference: ancestor.reference().clone(),
                });
            }
            let native_count = self
                .production
                .try_lock()
                .map_err(|_| {
                    anyhow::anyhow!("original native allocation inventory busy or poisoned")
                })?
                .native_entered
                .iter()
                .filter(|entered| **entered)
                .count();
            if native_count > 0 {
                remaining.push(Obligation::NativeAllocationAccounting {
                    attempted: u32::try_from(native_count)?,
                });
            }
            let disposition = super::creation_evidence::InspectionDispositionRecord {
                schema: 2,
                purpose: "offline_reset_inspection_disposition_v2".into(),
                signer: self.record.signer.clone(),
                operation_id: self.record.operation_id.clone(),
                claim_nonce: self.record.claim_nonce.clone(),
                claim_sha256: lillux::sha256_hex(&self.signed),
                policy_digest: self.record.policy_digest.clone(),
                clock_binding: self.record.clock_binding.clone(),
                parent_identity: self.copy_root.identity()?,
                inspection: self.record.inspection.clone(),
                native_quota: self.native_quota_evidence()?,
                stores: stores
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("original aggregate store cardinality changed"))?,
                remaining,
                retained_authorities: [
                    Authority::SourceDirectoryOwners,
                    Authority::InspectionParentOwner,
                    Authority::OriginalNamespaceAndStateLifelines,
                    Authority::OriginalClockAndPolicyOwner,
                ],
            };
            // Reserve two bounded record buffers BEFORE signing or publication;
            // retain the full irreversible allowance even if exact bytes are less.
            admission.reserve_records((2 * MAX_RECORD_BYTES) as u64)?;
            admission.debit_original(2)?;
            admission.debit_original(1)?;
            let storage = lillux::cas::canonical_record_storage_quote(&disposition, MAX_RECORD_BYTES)?;
            self.debit_original_record_working_storage(storage.working_bytes())?;
            let signed_disposition = disposition.signed_bytes(&self.signer, &storage)?;
            let terminal = super::creation_evidence::InspectionTerminalRecord {
                schema: 2,
                purpose: "offline_reset_inspection_terminal_pending_v2".into(),
                disposition: disposition.clone(),
                disposition_sha256: lillux::sha256_hex(&signed_disposition),
                witness_retired: std::array::from_fn(|index| witnesses[index].evidence.is_some()),
                reservation_retained: true,
            };
            admission.debit_original(1)?;
            let storage = lillux::cas::canonical_record_storage_quote(&terminal, MAX_RECORD_BYTES)?;
            self.debit_original_record_working_storage(storage.working_bytes())?;
            let signed_terminal = terminal.signed_bytes(&self.signer, &storage)?;
            Ok(Arc::new(OriginalInspectionSettlement {
                seal: seal.clone(),
                _disposals: Mutex::new(Some(std::mem::ManuallyDrop::new(disposals.clone()))),
                admission,
                disposition,
                signed_disposition,
                terminal,
                signed_terminal,
                progress: Mutex::new(OriginalSettlementProgress {
                    final_ack: None,
                    post_ack: None,
                    disposition: Default::default(),
                    terminal: Default::default(),
                    witnesses: witnesses
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("original witness cardinality changed"))?,
                }),
            }))
        })();
        prepared.map_err(|error| {
            anyhow::Error::new(OriginalSealCustodyFailure {
                error,
                _seal: seal,
                _disposals: disposals,
            })
        })
    }
}
impl OriginalInspectionSettlement {
    pub(super) fn require_original(
        &self,
        claim: &Arc<OriginalQuotaClaim>,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    ) -> Result<()> {
        self.seal.require_original(claim, work)
    }
    fn retain_error(self: &Arc<Self>, error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(OriginalInspectionSettlementFailure {
            error,
            _owner: self.clone(),
        })
    }
    fn publish_record(&self, name: &str, signed: &[u8], publication: &mut OriginalRecordPublication) -> Result<()> {
        self.admission.publish_record(name, signed, publication)
    }
    pub(super) fn disposition_record(self: &Arc<Self>) -> Result<()> {
        let result = (|| -> Result<()> {
            let mut progress = self
                .progress
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
            self.publish_record(
                &super::inspection_destination::disposition_name(
                    &self.seal.claim.record.operation_id,
                ),
                &self.signed_disposition,
                &mut progress.disposition,
            )
        })();
        result.map_err(|error| self.retain_error(error))
    }
    pub(super) fn dispose_witnesses(self: &Arc<Self>) -> Result<()> {
        let result = (|| -> Result<()> {
            let mut progress = self
                .progress
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
            ensure!(
                progress.disposition.acknowledged,
                "witness disposal requires actual durable original aggregate acknowledgement"
            );
            let work: Arc<dyn lillux_sqlite::OriginalWitnessCleanupWork> = self.admission.clone();
            for witness in &mut progress.witnesses {
                let Some(evidence) = &witness.evidence else {
                    continue;
                };
                if let Some(retired) = &witness.retired {
                    retired.require_original(&evidence.pin)?;
                    self.require_witness_absence(evidence)?;
                    continue;
                }
                if witness.removed.is_none() {
                    let removed = evidence
                        .pin
                        .remove_original_planned(
                            work.clone(),
                            &self.seal.claim.copy_root,
                            self.admission.deadline,
                        )
                        .map_err(anyhow::Error::new)?;
                    witness.removed = Some(Arc::new(removed));
                }
                let remaining = self.admission.remaining_steps()?;
                let maximum = u32::try_from(
                    (remaining / 4)
                        .checked_sub(1)
                        .filter(|bound| *bound > 0)
                        .context("original witness retirement lacks finite admitted scan")?
                        .min(i32::MAX as u64),
                )?;
                let retired = evidence.pin.retire_removed_original(
                    witness
                        .removed
                        .as_ref()
                        .expect("actual original witness removal"),
                    maximum,
                )?;
                // Store actual success before subsequent window/sibling checks.
                witness.retired = Some(Arc::new(retired));
                self.admission.checkpoint_original()?;
            }
            Ok(())
        })();
        result.map_err(|error| self.retain_error(error))
    }
    fn require_witness_absence(&self, evidence: &CreatedDirectoryEvidence) -> Result<()> {
        self.admission.debit_original(3)?;
        let record = CreatedDirectoryRecord::verified(&evidence.signed, &self.seal.claim.signer)?;
        let name = super::creation_evidence::evidence_name(&record.destination_name);
        ensure!(
            self.seal
                .claim
                .copy_root
                .entry_no_follow(OsStr::new(&name))?
                .is_none()
                && self
                    .seal
                    .claim
                    .copy_root
                    .entry_no_follow(OsStr::new(&format!("cleanup.{name}")))?
                    .is_none(),
            "retired original witness coordinate reappeared"
        );
        Ok(())
    }

    pub(super) fn prepare_terminal(self: &Arc<Self>) -> Result<()> {
        ensure!(
            !self
                .seal
                .claim
                .finalizer_master_entered
                .load(Ordering::Acquire),
            "old pending terminal cannot replace actual finalizer master phase"
        );
        let result =
            (|| -> Result<()> {
                let mut progress = self.progress.try_lock().map_err(|_| {
                    anyhow::anyhow!("original settlement progress busy or poisoned")
                })?;
                ensure!(
                    progress.disposition.acknowledged,
                    "terminal preparation lacks durable aggregate acknowledgement"
                );
                for witness in &progress.witnesses {
                    if let Some(evidence) = &witness.evidence {
                        witness
                            .retired
                            .as_ref()
                            .context("original witness descriptor retirement pending")?
                            .require_original(&evidence.pin)?;
                        self.require_witness_absence(evidence)?;
                    }
                }
                self.publish_record(
                    &super::inspection_destination::terminal_name(
                        &self.seal.claim.record.operation_id,
                    ),
                    &self.signed_terminal,
                    &mut progress.terminal,
                )?;
                *self.admission.phase.try_lock().map_err(|_| {
                    anyhow::anyhow!("original reservation phase busy or poisoned")
                })? = OriginalReservationPhase::TerminalAcknowledgedReservationRetained;
                Ok(())
            })();
        result.map_err(|error| self.retain_error(error))
    }
    pub(super) fn require_completed(self: &Arc<Self>) -> Result<()> {
        let result = (|| -> Result<()> {
            self.admission.checkpoint_original()?;
            self.seal
                .require_original(&self.seal.claim, &self.seal.work)?;
            let progress = self
                .progress
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
            ensure!(
                progress.disposition.acknowledged && progress.terminal.acknowledged,
                "original local durable terminal acknowledgement incomplete"
            );
            for witness in &progress.witnesses {
                if let Some(evidence) = &witness.evidence {
                    witness
                        .retired
                        .as_ref()
                        .context("original witness close pending")?
                        .require_original(&evidence.pin)?;
                    self.require_witness_absence(evidence)?;
                }
            }
            self.seal.claim.resident.try_lock()
                .map_err(|_| anyhow::anyhow!("actual offline resident disposition busy"))?.require_released()?;
            // Actual held objects are reflected in the authenticated record.
            // No unrelated process/account financial receipt applies to this CLI.
            ensure!(
                self.disposition.remaining.is_empty(),
                "original local settlement retains concrete obligations: {:?}",
                self.disposition.remaining
            );
            ensure!(
                !self.terminal.reservation_retained,
                "original local reservation remains charged"
            );
            Ok(())
        })();
        result.map_err(|error| self.retain_error(error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginalProductionPhase {
    Producing,
    Draining,
    Sealed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginalProducerKind {
    Issuer,
    NativeQuotaIssuer,
    NativeConnection(u32),
    NativeLease(u32),
    Namespace(usize),
    Copy(usize),
    SqlIssue(usize),
    Native(usize),
}
struct OriginalProductionState {
    phase: OriginalProductionPhase,
    next: u64,
    active: std::collections::BTreeMap<u64, OriginalProducerKind>,
    pending: std::collections::BTreeMap<u64, OriginalProducerKind>,
    issuer_entered: bool,
    native_quota_entered: bool,
    namespace_entered: [bool; 2],
    copy_entered: [bool; 8],
    native_entered: [bool; 2],
    sql_issue_entered: [bool; 2],
    seal: Option<std::sync::Weak<OriginalProductionSeal>>,
}
impl Default for OriginalProductionState {
    fn default() -> Self {
        Self {
            phase: OriginalProductionPhase::Producing,
            next: 0,
            active: Default::default(),
            pending: Default::default(),
            issuer_entered: false,
            native_quota_entered: false,
            namespace_entered: [false; 2],
            copy_entered: [false; 8],
            native_entered: [false; 2],
            sql_issue_entered: [false; 2],
            seal: None,
        }
    }
}
struct OriginalProductionControl {
    claim: Arc<OriginalQuotaClaim>,
    copy: Option<Arc<OriginalCopyWork>>,
    ticket: u64,
    kind: OriginalProducerKind,
    completed: AtomicBool,
}
impl lillux_sqlite::SqliteInspectionProductionControl for OriginalProductionControl {
    fn checkpoint(&self) -> Result<()> {
        {
            let state = self
                .claim
                .production
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original producer state busy or poisoned"))?;
            ensure!(
                !self.completed.load(Ordering::Acquire)
                    && state.active.get(&self.ticket) == Some(&self.kind)
                    && state.phase != OriginalProductionPhase::Sealed,
                "original producer interval is not active"
            );
        }
        if let Some(copy) = &self.copy {
            lillux_sqlite::SqliteInspectionCopyWork::checkpoint(copy.as_ref())?;
        } else {
            self.claim.require_service_window()?;
        }
        Ok(())
    }
    fn complete(&self) -> Result<()> {
        let mut state =
            self.claim.production.try_lock().map_err(|_| {
                anyhow::anyhow!("original producer acknowledgement busy or poisoned")
            })?;
        if self.completed.load(Ordering::Acquire) {
            return Ok(());
        }
        ensure!(
            state.active.get(&self.ticket) == Some(&self.kind),
            "original producer acknowledgement lost its ticket"
        );
        state.active.remove(&self.ticket);
        self.completed.store(true, Ordering::Release);
        Ok(())
    }
    fn abandoned(&self) {
        if self.completed.load(Ordering::Acquire) {
            return;
        }
        // Bookkeeping preservation only. Poison can never mint a seal.
        // Never block/spin in Drop. If bookkeeping is busy/poisoned, the
        // untouched active ticket itself remains an unresolved seal blocker.
        let Ok(mut state) = self.claim.production.try_lock() else {
            return;
        };
        if state.active.remove(&self.ticket).is_some() {
            state.pending.insert(self.ticket, self.kind);
        }
        state.phase = OriginalProductionPhase::Draining;
    }
}
/// Authentic original quota subreservation only. No native factory/allocation
/// exists yet; this owner intentionally cannot acknowledge native terminal.
/// Retain the original registered lock through both service and outside
/// custody. Conversion only moves the lease's same Arc; it never closes the
/// fork-registered file or creates stopped-node authority.
struct OriginalAccountStateLock {
    owner: Mutex<OriginalAccountStateLockOwner>,
}
enum OriginalAccountStateLockOwner {
    Lease(std::mem::ManuallyDrop<ryeos_node_runtime::state_lock::StateLockLease>),
    Retained(std::mem::ManuallyDrop<Arc<lillux::ExactExclusiveFileLock>>),
    Joined(std::sync::Weak<lillux::ExactExclusiveFileLock>),
    InFlight,
}
impl OriginalAccountStateLock {
    fn new(lease: ryeos_node_runtime::state_lock::StateLockLease) -> Self {
        Self { owner: Mutex::new(OriginalAccountStateLockOwner::Lease(std::mem::ManuallyDrop::new(lease))) }
    }
    fn retain_original_owner(&self) -> Result<Arc<lillux::ExactExclusiveFileLock>> {
        let mut custody = self.owner.try_lock()
            .map_err(|_| anyhow::anyhow!("original account State lock custody busy or poisoned"))?;
        if let OriginalAccountStateLockOwner::Retained(owner) = &*custody {
            owner.require_original_process()?;
            return Ok(Arc::clone(owner));
        }
        if let OriginalAccountStateLockOwner::Joined(original) = &*custody {
            let owner = original.upgrade().context("actual canonical account State lock absent")?;
            owner.require_original_process()?;
            return Ok(owner);
        }
        ensure!(matches!(&*custody, OriginalAccountStateLockOwner::Lease(_)),
            "original account State lock transfer unresolved");
        let OriginalAccountStateLockOwner::Lease(lease) =
            std::mem::replace(&mut *custody, OriginalAccountStateLockOwner::InFlight) else { unreachable!() };
        // Pure copied-PID check and same-Arc move only, under this gate.
        // No filesystem effect, caller callback or descriptor close occurs.
        match std::mem::ManuallyDrop::into_inner(lease).into_original_owner() {
            Ok(owner) => {
                *custody = OriginalAccountStateLockOwner::Retained(std::mem::ManuallyDrop::new(owner));
                let OriginalAccountStateLockOwner::Retained(owner) = &*custody else { unreachable!() };
                Ok(Arc::clone(owner))
            }
            Err((lease, error)) => {
                *custody = OriginalAccountStateLockOwner::Lease(std::mem::ManuallyDrop::new(lease));
                Err(error)
            }
        }
    }
    fn join_original_owner(&self, canonical: &Arc<lillux::ExactExclusiveFileLock>) -> Result<()> {
        canonical.require_original_process()?;
        let mut custody = self.owner.try_lock()
            .map_err(|_| anyhow::anyhow!("original account State lock alias join busy or poisoned"))?;
        match &*custody {
            OriginalAccountStateLockOwner::Joined(original) => {
                ensure!(std::sync::Weak::ptr_eq(original, &Arc::downgrade(canonical)),
                    "original account State lock join selects another allocation");
                return Ok(());
            }
            OriginalAccountStateLockOwner::Retained(original) => {
                ensure!(Arc::ptr_eq(original, canonical),
                    "original account State lock alias differs from canonical owner");
            }
            _ => anyhow::bail!("original account State lock not retained before alias join"),
        }
        // The caller holds the actual canonical owner and has cached it in
        // the original account parent. End only this exact duplicate alias.
        let original = std::mem::replace(&mut *custody,
            OriginalAccountStateLockOwner::Joined(Arc::downgrade(canonical)));
        let OriginalAccountStateLockOwner::Retained(original) = original else { unreachable!() };
        drop(custody);
        drop(std::mem::ManuallyDrop::into_inner(original));
        Ok(())
    }
    fn ensure_protects_app_root(&self, app_root: &std::path::Path) -> Result<()> {
        // The actual owner remains cached; pathname verification is outside
        // the bookkeeping gate and retains the existing service semantics.
        self.retain_original_owner()?.ensure_path_binding(
            &ryeos_node_runtime::state_lock::default_lock_path(app_root))
    }
}
struct OriginalNativeAdmissionLifelines {
    state_lifetime: OriginalAccountStateLock,
    namespace: lillux::PinnedDirectoryLock,
    policy: ryeos_node_runtime::node_policy::NodePolicySnapshot,
}
pub(super) struct OriginalNativeQuotaAdmission {
    claim: Arc<OriginalQuotaClaim>,
    lifelines: Mutex<Option<OriginalNativeAdmissionLifelines>>,
    _production: Mutex<Option<Arc<OriginalProductionControl>>>,
    region: Mutex<Option<lillux_sqlite::original_host_owner::OriginalNativeRegionHandle>>,
    issuer_index: Mutex<Option<lillux_sqlite::OriginalNativeProducerCellIndex>>,
    issuer_publication_failure: Mutex<Option<lillux_sqlite::SqliteInspectionProductionPublicationFailure>>,
    preparations: Mutex<Vec<OfflineProducerPreparation>>,
    budget: Arc<lillux_sqlite::OriginalNativeBudgetOwner>,
}
struct OfflineProducerPreparation {
    core: Option<lillux_sqlite::OriginalNativeProducerControl>,
    ticket: Option<u32>,
    publication: Option<lillux_sqlite::SqliteInspectionProductionPublicationFailure>,
    callback: Option<Arc<dyn lillux_sqlite::SqliteInspectionProductionControl>>,
    terminal: Option<Arc<lillux_sqlite::ReleasedOriginalNativeAcquisition>>,
}
impl OriginalNativeQuotaAdmission {
    fn with_region<T>(&self,
        apply: impl FnOnce(&lillux_sqlite::original_host_owner::OriginalNativeRegionHandle) -> Result<T>) -> Result<T> {
        ensure!(self.claim.record.resident_process == std::process::id(), "foreign actual offline native admission");
        let original = self.region.try_lock()
            .map_err(|_| anyhow::anyhow!("actual offline native region custody busy or poisoned"))?;
        apply(original.as_ref().context("actual offline native admission handle consumed")?)
        // Only fixed local handle/control operations run here, never C effects
        // or authority callbacks. The outside gate retains the actual handle.
    }
    pub(super) fn retire_preparation_terminal_alias(
        &self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        canonical: &lillux_sqlite::OriginalNativeConnectionTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.claim.record.resident_process == std::process::id(), "foreign actual offline preparation aliases");
        aggregate.require_original(factory)?;
        canonical.require_original(factory)?;
        let count = self.preparations.try_lock()
            .map_err(|_| anyhow::anyhow!("actual offline preparation quote busy or poisoned"))?.len();
        let steps = u64::try_from(count)?.checked_mul(4).and_then(|steps| steps.checked_add(4))
            .context("actual offline preparation alias work overflow")?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| prepay(0, steps))?;
        let mut preparations = self.preparations.try_lock()
            .map_err(|_| anyhow::anyhow!("actual offline preparation aliases busy or poisoned"))?;
        ensure!(preparations.len() == count, "actual preparation inventory changed after original quote");
        for preparation in &mut *preparations {
            let matches = preparation.terminal.as_ref().is_some_and(|original| {
                matches!(canonical, lillux_sqlite::OriginalNativeConnectionTerminal::Released(canonical)
                    if Arc::ptr_eq(original, canonical))
            });
            if matches {
                let core = preparation.core.as_ref().context("actual completed preparation core absent")?;
                let observation = core.observation()?;
                ensure!(observation.phase == 2 && observation.reference_retired,
                    "actual preparation producer remains unresolved");
                // Canonical actual receipt/resources remain retained by caller;
                // this drops only a redundant same-Arc terminal alias.
                preparation.terminal.take();
            }
        }
        Ok(())
    }
    pub(super) fn retire_native_region_handles(self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<lillux_sqlite::OriginalNativeBudgetSnapshot> {
        self.claim.require_destination_outside_transfer(factory, aggregate)?;
        let issued = self.claim.native_admission.try_lock()
            .map_err(|_| anyhow::anyhow!("actual native admission retirement lookup busy or poisoned"))?
            .as_ref().and_then(std::sync::Weak::upgrade)
            .context("actual registered original native admission absent")?;
        ensure!(Arc::ptr_eq(self, &issued), "native handle retirement selects foreign actual admission");
        ensure!(self._production.try_lock().map_err(|_| anyhow::anyhow!("actual issuer custody busy"))?.is_none()
            && self.issuer_publication_failure.try_lock().map_err(|_| anyhow::anyhow!("actual issuer refusal custody busy"))?.is_none(),
            "actual original issuer interval still retained");
        let mut preparations = self.preparations.try_lock()
            .map_err(|_| anyhow::anyhow!("actual native preparation retirement busy or poisoned"))?;
        for preparation in &*preparations {
            ensure!(preparation.publication.is_none() && preparation.terminal.is_none(),
                "actual preparation publication/terminal aliases not transferred");
            let observation = preparation.core.as_ref().context("actual original preparation core absent")?.observation()?;
            ensure!(observation.phase == 2 && observation.reference_retired,
                "actual preparation producer has not ended");
        }
        let completed = std::mem::take(&mut *preparations);
        drop(preparations);
        drop(completed); // Completed control references only; no physical/account disposal.
        let snapshot = self.budget.retire_original_handle(factory, aggregate)?;
        let mut region = self.region.try_lock()
            .map_err(|_| anyhow::anyhow!("actual admission region retirement busy or poisoned"))?;
        let original = region.take();
        drop(region);
        drop(original);
        Ok(snapshot)
    }
    pub(super) fn retire_preparations(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>)
        -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
        self.checkpoint(true)?;
        let tickets = u64::from(factory.maximum_resource_tickets()?);
        let bytes = tickets.checked_mul(
            u64::try_from(std::mem::size_of::<lillux_sqlite::OriginalNativeConnectionTerminal>())?
        ).context("offline preparation census storage overflow")?;
        {
            let mut used = self.claim.copy_usage.try_lock()
                .map_err(|_| anyhow::anyhow!("offline preparation census debit busy or poisoned"))?;
            let buffers = used.buffer_bytes.checked_add(bytes)
                .context("offline preparation census memory overflow")?;
            ensure!(buffers.checked_add(used.native_storage_bytes)
                .and_then(|total| total.checked_add(used.resident_storage_bytes))
                .is_some_and(|total| total <= self.claim.record.reserved.max_memory_bytes),
                "offline preparation census exceeds original memory envelope");
            used.buffer_bytes = buffers;
        }
        let mut retained = std::mem::take(&mut *self.preparations.try_lock()
            .map_err(|_| anyhow::anyhow!("offline preparation drain busy or poisoned"))?);
        // No preparation/controller mutex spans C effects or callback ACK.
        let result = (|| -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
            let mut terminals = Vec::with_capacity(usize::try_from(tickets)?);
            for preparation in &mut retained {
                if let Some(failure) = preparation.publication.take() {
                    match failure.into_parts() {
                        Ok((_, callback, core)) => { preparation.callback = Some(callback); preparation.core = Some(core); }
                        Err(failure) => { preparation.publication = Some(failure); anyhow::bail!("offline publication custody extraction refused"); }
                    }
                }
                let core = preparation.core.as_ref().context("actual offline preparation core absent")?;
                if let Some(ticket) = preparation.ticket {
                    if core.observation()?.ticket == 0 { core.bind_ticket(ticket)?; }
                    if preparation.terminal.is_none() {
                        // Cache real native success before fallible outside ACK.
                        preparation.terminal = Some(factory.retire_unpublished_ticket(core)?);
                    }
                    if let Some(callback) = &preparation.callback { callback.complete()?; }
                    terminals.push(lillux_sqlite::OriginalNativeConnectionTerminal::Released(
                        preparation.terminal.as_ref().context("actual preparation release receipt absent")?.clone()));
                } else {
                    if core.observation()?.phase != 2 { core.retire_unbound_preparation()?; }
                }
            }
            Ok(terminals)
        })();
        // Preserve all actual cores/callbacks/receipts, including completed
        // prefixes, before returning a late error. Final consuming retirement
        // clears these only after the full backend receipt has been joined.
        self.preparations.lock().unwrap_or_else(|p| p.into_inner()).extend(retained);
        result
    }
    fn begin_resident_native_producer(&self, lease: bool) -> Result<(u32, lillux_sqlite::SqliteInspectionProductionPermit)> {
        self.checkpoint(false)?;
        // Retain failure custody before claiming a cell or debiting a ticket.
        let mut preparations = self.preparations.try_lock()
            .map_err(|_| anyhow::anyhow!("offline preparation custody busy or poisoned"))?;
        let mut state = self.claim.production.try_lock()
            .map_err(|_| anyhow::anyhow!("offline native producer issuer busy or poisoned"))?;
        ensure!(state.phase == OriginalProductionPhase::Producing, "offline native issuer draining/sealed");
        let producer_ticket = state.next.checked_add(1).context("offline producer ticket overflow")?;
        let native_kind = if lease { lillux_sqlite::SqliteInspectionProductionKind::ProjectionLease }
            else { lillux_sqlite::SqliteInspectionProductionKind::Native };
        let core = self.with_region(|region| Ok(region.claim_producer_control(native_kind)?))?;
        let ticket = match self.budget.debit_connection_ticket() {
            Ok(ticket) => ticket,
            Err(error) => {
                if core.retire_unbound_preparation().is_err() {
                    preparations.push(OfflineProducerPreparation { core: Some(core), ticket: None, publication: None, callback: None, terminal: None });
                }
                return Err(error);
            }
        };
        if let Err(error) = core.bind_ticket(ticket) {
            preparations.push(OfflineProducerPreparation { core: Some(core), ticket: Some(ticket), publication: None, callback: None, terminal: None });
            return Err(anyhow::Error::new(error));
        }
        let kind = if lease { OriginalProducerKind::NativeLease(ticket) } else { OriginalProducerKind::NativeConnection(ticket) };
        state.next = producer_ticket; state.active.insert(producer_ticket, kind);
        drop(state);
        let callback: Arc<dyn lillux_sqlite::SqliteInspectionProductionControl> = Arc::new(OriginalProductionControl {
            claim: self.claim.clone(), copy: None, ticket: producer_ticket, kind, completed: AtomicBool::new(false),
        });
        // SAFETY: this actual offline original claim issued the same fixed
        // cell and irreversibly debited its exact sole resident counter ticket.
        let producer = match unsafe { lillux_sqlite::SqliteInspectionProductionPermit::issued_from_region(native_kind, callback, core) } {
            Ok(producer) => producer,
            Err(publication) => {
                preparations.push(OfflineProducerPreparation { core: None, ticket: Some(ticket), publication: Some(publication), callback: None, terminal: None });
                anyhow::bail!("offline native producer publication refused; original custody retained");
            }
        };
        Ok((ticket, producer))
    }
    fn require_custody(&self) -> Result<()> {
        ensure!(self.claim.record.resident_process == std::process::id(),
            "inherited offline native admission refused before custody traversal");
        ensure!(
            !self.claim.finalizer_master_entered.load(Ordering::Acquire),
            "original finalizer master entered; native admission callbacks refused"
        );
        // Actual lifeline ownership guard, not producer/counter bookkeeping.
        // Transferred admission callbacks refuse; no copied lease substitutes.
        let custody = self
            .lifelines
            .try_lock()
            .map_err(|_| anyhow::anyhow!("native admission lifeline owner busy or poisoned"))?;
        let lifelines = custody
            .as_ref()
            .context("native admission lifelines transferred; callbacks refused")?;
        lifelines
            .state_lifetime
            .ensure_protects_app_root(self.claim.app_root.path())?;
        lifelines.namespace.ensure_protects(&self.claim.runtime)?;
        self.claim.app_root.ensure_path_binding()?;
        self.claim.runtime.ensure_path_binding()?;
        ensure!(
            lifelines.policy.generation_digest() == self.claim.record.policy_digest
                && lifelines
                    .policy
                    .require::<ryeos_scheduler_contract::NodeMaintenancePolicy>()?
                    .offline_reset
                    .as_ref()
                    == Some(&self.claim.record.reserved),
            "native quota admission changed signed original policy"
        );
        ensure!(
            self.claim.publication_acknowledged.load(Ordering::Acquire),
            "native quota admission lacks actual original publication ACK"
        );
        self.claim.ledger.ensure_path_binding()?;
        let publication = self
            .claim
            .reservation_publication
            .try_lock()
            .map_err(|_| anyhow::anyhow!("native original publication busy or poisoned"))?
            .clone()
            .context("native original publication descriptor missing")?;
        let name = format!("{PREFIX}{}", self.claim.record.operation_id);
        publication.require_original(&self.claim.ledger, OsStr::new(&name))?;
        let original = publication.identity_original()?;
        let entry = self
            .claim
            .ledger
            .entry_no_follow(OsStr::new(&name))?
            .context("native charged original reservation disappeared")?;
        ensure!(
            entry.entry_type == lillux::PinnedEntryType::Regular
                && entry.containing_device == original.device()
                && entry.inode == original.inode()
                && publication.read_original(MAX_RECORD_BYTES)? == self.claim.signed,
            "native original reservation incarnation or bytes changed"
        );
        Ok(())
    }
    fn checkpoint(&self, cleanup: bool) -> Result<()> {
        self.require_custody()?;
        if cleanup {
            self.claim.record.validate()?;
            ensure!(
                self.claim.record.clock_binding == self.claim.clock.binding_digest(),
                "native cleanup changed original clock"
            );
            let allowance =
                std::time::Duration::from_millis(self.claim.record.reserved.max_cleanup_millis);
            original_cleanup_remaining(
                self.claim
                    .clock
                    .window(&self.claim.record.occupancy, allowance)
                    .map_err(anyhow::Error::msg)?,
                allowance,
            )?;
        } else {
            self.claim.require_service_window()?;
        }
        Ok(())
    }
    /// Checked precharge before a future native allocation/scan effect. This
    /// is NOT a callback API: custody/clock validation runs outside native locks.
    pub(super) fn debit_allocator_steps(&self, steps: u64) -> Result<()> {
        self.checkpoint(false)?;
        self.budget.debit_steps(steps, false)
    }
    pub(super) fn debit_allocator_cleanup_steps(&self, steps: u64) -> Result<()> {
        self.checkpoint(true)?;
        self.budget.debit_steps(steps, true)
    }
    pub(super) fn debit_connection_ticket(
        self: &Arc<Self>,
    ) -> Result<OriginalNativeConnectionQuotaTicket> {
        self.checkpoint(false)?;
        let state = self
            .claim
            .production
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original native ticket issuer busy"))?;
        ensure!(
            state.phase == OriginalProductionPhase::Producing,
            "native producer draining/sealed; new ticket refused"
        );
        let ticket = self.budget.debit_connection_ticket()?;
        Ok(OriginalNativeConnectionQuotaTicket {
            _admission: self.clone(),
            _ticket: ticket,
        })
    }
}
impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for OriginalNativeQuotaAdmission {
    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>, cleanup: bool) -> Result<()> {
        self.checkpoint(cleanup)?;
        let original = self.claim.native_factory.try_lock()
            .map_err(|_| anyhow::anyhow!("original offline lease factory binding busy"))?
            .as_ref().and_then(std::sync::Weak::upgrade).context("actual offline lease factory absent")?;
        ensure!(Arc::ptr_eq(&original, factory), "offline lease selects foreign actual factory");
        Ok(())
    }
    fn fork_deadline(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>, cleanup: bool)
        -> Result<lillux::time::MonotonicDeadline> {
        lillux_sqlite::OriginalNativeProjectionLeaseAuthority::require_original(self, factory, cleanup)?;
        let deadline = if cleanup { self.claim.native_cleanup_deadline } else { self.claim.native_service_deadline };
        ensure!(!deadline.has_elapsed(), "original offline lease endpoint elapsed");
        Ok(deadline)
    }
}
impl OriginalQuotaClaim {
    pub(super) fn native_projection_lease_authority(self: &Arc<Self>, factory: &Arc<lillux_sqlite::OriginalNativeFactory>)
        -> Result<Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>> {
        ensure!(self.record.resident_process == std::process::id(), "foreign offline lease issuance");
        let admission = self.native_admission.try_lock().map_err(|_| anyhow::anyhow!("offline lease admission lookup busy"))?
            .as_ref().and_then(std::sync::Weak::upgrade).context("actual original offline admission absent")?;
        ensure!(Arc::ptr_eq(&admission.claim, self), "offline lease authority selects foreign claim");
        lillux_sqlite::OriginalNativeProjectionLeaseAuthority::require_original(admission.as_ref(), factory, false)?;
        Ok(admission)
    }
}
impl lillux_sqlite::OriginalNativeAdmissionAuthority for OriginalNativeQuotaAdmission {
    fn require_original(&self, cleanup: bool) -> Result<()> {
        self.checkpoint(cleanup)
    }
    fn require_runtime_binding(&self, runtime: &lillux::PinnedDirectory) -> Result<()> {
        self.checkpoint(false)?;
        ensure!(
            self.claim.runtime.is_same_directory(runtime)?,
            "native factory selects foreign original runtime"
        );
        runtime.ensure_path_binding()
    }
    fn budget(&self) -> Arc<lillux_sqlite::OriginalNativeBudgetOwner> {
        self.budget.clone()
    }
    fn debit_sql_vm_steps(&self, steps: u64) -> Result<()> {
        self.checkpoint(false)?;
        ensure!(steps > 0, "empty original SQL debit");
        let mut used = self
            .claim
            .copy_usage
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original SQL/native counter busy or poisoned"))?;
        let total = used
            .sql_steps
            .checked_add(steps)
            .context("original native SQL debit overflow")?;
        ensure!(
            total <= self.claim.record.reserved.max_sql_vm_steps,
            "original shared SQL quota exhausted"
        );
        used.sql_steps = total;
        Ok(())
    }
    fn begin_projection_lease(&self) -> Result<lillux_sqlite::OriginalNativeProjectionLeaseAdmission> {
        let (ticket, producer) = self.begin_resident_native_producer(true)?;
        // SAFETY: distinct real lease producer/ticket debited from this exact
        // authentic offline resident budget, retained before any lease effect.
        unsafe { lillux_sqlite::OriginalNativeProjectionLeaseAdmission::issued(self.budget.clone(), ticket, producer) }.map_err(anyhow::Error::new)
    }
    fn begin_connection(&self) -> Result<lillux_sqlite::OriginalNativeConnectionAdmission> {
        let (ticket, producer) = self.begin_resident_native_producer(false)?;
        // SAFETY: genuine offline producing gate, sole resident counter and
        // exact native producer interval; no service/live grant is substituted.
        unsafe { lillux_sqlite::OriginalNativeConnectionAdmission::issued(self.budget.clone(), ticket, producer) }.map_err(anyhow::Error::new)
    }
}

// Failed issuance retains authentic custody and its irreversible issuer
// interval even before the shared native budget has been constructed.
struct OriginalNativeQuotaPreparationFailure {
    error: anyhow::Error,
    _claim: std::mem::ManuallyDrop<Arc<OriginalQuotaClaim>>,
    _state_lifetime: std::mem::ManuallyDrop<ryeos_node_runtime::state_lock::StateLockLease>,
    _namespace: std::mem::ManuallyDrop<lillux::PinnedDirectoryLock>,
    _policy: std::mem::ManuallyDrop<ryeos_node_runtime::node_policy::NodePolicySnapshot>,
    _production: std::mem::ManuallyDrop<Arc<OriginalProductionControl>>,
    raw_budget: Mutex<Option<lillux_sqlite::OriginalNativeBudget>>,
}
impl OriginalNativeQuotaPreparationFailure {
    fn retry_resident_release(&mut self) -> Result<()> {
        ensure!(self._claim.record.resident_process == std::process::id(), "foreign offline preparation custody");
        self._state_lifetime.ensure_protects_app_root(self._claim.app_root.path())?;
        self._namespace.ensure_protects(&self._claim.runtime)?;
        ensure!(self._policy.generation_digest() == self._claim.record.policy_digest,
            "offline preparation recovery selects foreign policy");
        self._claim.require_resident_cleanup()?;
        self._claim.resident.try_lock().map_err(|_| anyhow::anyhow!("offline resident recovery owner busy"))?
            .release(|| self._claim.require_resident_cleanup())?;
        // Native issuance never occurred. End this actual retained original
        // upper interval after genuine physical absence, without backend ACK.
        lillux_sqlite::SqliteInspectionProductionControl::complete(self._production.as_ref())?;
        // The raw budget, if retained before controller transfer, has no C
        // borrowers or backing. This local counter is never reused/reissued.
        drop(self.raw_budget.get_mut().map_err(|_| anyhow::anyhow!("offline raw preparation custody poisoned"))?.take());
        Ok(())
    }
}
impl std::fmt::Debug for OriginalNativeQuotaPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeQuotaPreparationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeQuotaPreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalNativeQuotaPreparationFailure {}
#[derive(Debug)]
struct OriginalNativeQuotaAdmissionFailure {
    error: anyhow::Error,
    _admission: Arc<OriginalNativeQuotaAdmission>,
}
impl OriginalNativeQuotaAdmissionFailure {
    fn retry_preinstallation_retirement(&self) -> Result<()> {
        self._admission.require_custody()?;
        self._admission.claim.require_resident_cleanup()?;
        {
            let mut state = self._admission.claim.production.try_lock()
                .map_err(|_| anyhow::anyhow!("offline admission close gate busy"))?;
            state.phase = OriginalProductionPhase::Draining;
        }
        self._admission.claim.resident.try_lock()
            .map_err(|_| anyhow::anyhow!("offline region close custody busy"))?.begin_close()?;
        let failure = self._admission.issuer_publication_failure.try_lock()
            .map_err(|_| anyhow::anyhow!("offline issuer publication failure busy"))?;
        if let Some(failure) = failure.as_ref() { failure.retire_uninstalled_factory_issuer()?; }
        let index = *self._admission.issuer_index.try_lock()
            .map_err(|_| anyhow::anyhow!("offline actual issuer index busy"))?;
        if let Some(index) = index { self._admission.with_region(|region| Ok(region.retire_uninstalled_factory_issuer(index)?))?; }
        if let Some(control) = self._admission._production.try_lock()
            .map_err(|_| anyhow::anyhow!("offline retained issuer interval busy"))?.as_ref() {
            lillux_sqlite::SqliteInspectionProductionControl::complete(control.as_ref())?;
        }
        Ok(())
    }
}
impl std::fmt::Debug for OriginalNativeQuotaAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeQuotaAdmission")
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeQuotaAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalNativeQuotaAdmissionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
struct OriginalNativeFactoryIssuerFailure {
    error: anyhow::Error,
    _factory: Arc<lillux_sqlite::OriginalNativeFactory>,
}
impl std::fmt::Debug for OriginalNativeFactoryIssuerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeFactoryIssuerFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalNativeFactoryIssuerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalNativeFactoryIssuerFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
/// Retains the SAME original subreservation; no clone/terminal/ID constructor.
pub(super) struct OriginalNativeConnectionQuotaTicket {
    _admission: Arc<OriginalNativeQuotaAdmission>,
    _ticket: u32,
}
impl OriginalQuotaClaim {
    pub(super) fn issue_native_quota_admission(
        self: &Arc<Self>,
        owner: &OfflineResetPolicyOwner<'_>,
        identity: &NodeIdentity,
        storage_bytes: u64,
        cleanup_allocator_steps: u64,
    ) -> Result<Arc<OriginalNativeQuotaAdmission>> {
        self.require_published(owner, identity)?;
        self.require_service_window()?;
        ensure!(
            storage_bytes > 0
                && storage_bytes <= self.record.reserved.max_native_storage_bytes
                && cleanup_allocator_steps > 0
                && cleanup_allocator_steps < self.record.reserved.max_native_allocator_steps,
            "native original storage/cleanup request outside signed dimensions"
        );
        let (state_lifetime, namespace, policy) = owner.retain_copy_inputs()?;
        let production = self.enter_native_quota_interval()?;
        let charged = (|| -> Result<()> {
            let mut used = self
                .copy_usage
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original native subreservation busy"))?;
            ensure!(
                used.native_storage_bytes == 0 && used.native_budget.is_none(),
                "original native storage already reserved"
            );
            ensure!(
                used.buffer_bytes
                    .checked_add(storage_bytes)
                    .is_some_and(|total| total <= self.record.reserved.max_memory_bytes),
                "native storage and actual buffers exceed SAME original memory envelope"
            );
            let resident_bytes = self.record.resident_bytes;
            ensure!(used.resident_storage_bytes == 0
                && used.buffer_bytes.checked_add(storage_bytes)
                    .and_then(|bytes| bytes.checked_add(resident_bytes))
                    .is_some_and(|bytes| bytes <= self.record.reserved.max_memory_bytes),
                "native arena, resident graph and buffers exceed signed offline memory");
            used.native_storage_bytes = storage_bytes;
            used.resident_storage_bytes = resident_bytes;
            Ok(())
        })();
        if let Err(error) = charged {
            return Err(anyhow::Error::new(OriginalNativeQuotaPreparationFailure {
                error,
                _claim: std::mem::ManuallyDrop::new(self.clone()),
                _state_lifetime: std::mem::ManuallyDrop::new(state_lifetime),
                _namespace: std::mem::ManuallyDrop::new(namespace),
                _policy: std::mem::ManuallyDrop::new(policy),
                _production: std::mem::ManuallyDrop::new(production), raw_budget: Mutex::new(None),
            }));
        }
        // Safety: storage charged once above from real signed original quota;
        // no native effect occurs until caller transfers this genuine owner.
        let budget = match unsafe {
            lillux_sqlite::OriginalNativeBudget::prepaid_by_original_issuer(
                storage_bytes,
                self.record.reserved.max_native_allocator_steps,
                self.record.reserved.max_native_connections,
                cleanup_allocator_steps,
            )
        } {
            Ok(budget) => budget,
            Err(error) => {
                return Err(anyhow::Error::new(OriginalNativeQuotaPreparationFailure {
                    error: anyhow::Error::new(error),
                    _claim: std::mem::ManuallyDrop::new(self.clone()),
                    _state_lifetime: std::mem::ManuallyDrop::new(state_lifetime),
                    _namespace: std::mem::ManuallyDrop::new(namespace),
                    _policy: std::mem::ManuallyDrop::new(policy),
                    _production: std::mem::ManuallyDrop::new(production), raw_budget: Mutex::new(None),
                }));
            }
        };
        let mut resident = match self.resident.try_lock() {
            Ok(resident) => resident,
            Err(_) => return Err(anyhow::Error::new(OriginalNativeQuotaPreparationFailure {
                error: anyhow::anyhow!("offline resident controller busy or poisoned"),
                _claim: std::mem::ManuallyDrop::new(self.clone()), _state_lifetime: std::mem::ManuallyDrop::new(state_lifetime), _namespace: std::mem::ManuallyDrop::new(namespace),
                _policy: std::mem::ManuallyDrop::new(policy), _production: std::mem::ManuallyDrop::new(production), raw_budget: Mutex::new(Some(budget)),
            })),
        };
        let initialized = (|| -> Result<Arc<lillux_sqlite::OriginalNativeBudgetOwner>> {
            let mut checkpoint = || self.require_published(owner, identity);
            resident.enter(budget, HOST_RELEASE_ATTEMPTS, &mut checkpoint)?;
            resident.map_resident(&mut checkpoint)?;
            // SAFETY: the signed offline reservation durably debited this
            // exact resident extent within its memory cap; its generation was
            // bound at claim creation from a nonrecycled process counter.
            unsafe { resident.initialize(self.record.resident_generation) }
        })();
        let budget = match initialized {
            Ok(budget) => budget,
            Err(error) => return Err(anyhow::Error::new(OriginalNativeQuotaPreparationFailure {
                error, _claim: std::mem::ManuallyDrop::new(self.clone()), _state_lifetime: std::mem::ManuallyDrop::new(state_lifetime),
                _namespace: std::mem::ManuallyDrop::new(namespace), _policy: std::mem::ManuallyDrop::new(policy), _production: std::mem::ManuallyDrop::new(production), raw_budget: Mutex::new(None),
            })),
        };
        let region = match resident.handle() {
            Ok(region) => region,
            Err(error) => return Err(anyhow::Error::new(OriginalNativeQuotaPreparationFailure {
                error, _claim: std::mem::ManuallyDrop::new(self.clone()), _state_lifetime: std::mem::ManuallyDrop::new(state_lifetime),
                _namespace: std::mem::ManuallyDrop::new(namespace), _policy: std::mem::ManuallyDrop::new(policy), _production: std::mem::ManuallyDrop::new(production), raw_budget: Mutex::new(None),
            })),
        };
        drop(resident);
        let admission = Arc::new(OriginalNativeQuotaAdmission {
            claim: self.clone(),
            lifelines: Mutex::new(Some(OriginalNativeAdmissionLifelines {
                state_lifetime: OriginalAccountStateLock::new(state_lifetime),
                namespace,
                policy,
            })),
            _production: Mutex::new(Some(production)),
            region: Mutex::new(Some(region)),
            issuer_index: Mutex::new(None), issuer_publication_failure: Mutex::new(None),
            preparations: Mutex::new(Vec::new()),
            budget: budget.clone(),
        });
        let registered = (|| -> Result<()> {
            let mut used = self
                .copy_usage
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original native budget registration busy"))?;
            ensure!(
                used.native_budget.is_none(),
                "original native budget registration conflict"
            );
            used.native_budget = Some(budget);
            drop(used);
            let mut issued = self.native_admission.try_lock().map_err(|_| {
                anyhow::anyhow!("actual native admission registration busy or poisoned")
            })?;
            ensure!(
                issued.is_none(),
                "original native admission already registered"
            );
            *issued = Some(Arc::downgrade(&admission));
            Ok(())
        })();
        registered.map_err(|error| {
            anyhow::Error::new(OriginalNativeQuotaAdmissionFailure {
                error,
                _admission: admission.clone(),
            })
        })?;
        Ok(admission)
    }
    pub(super) fn issue_native_factory(
        self: &Arc<Self>,
        owner: &OfflineResetPolicyOwner<'_>,
        identity: &NodeIdentity,
    ) -> Result<Arc<lillux_sqlite::OriginalNativeFactory>> {
        // Authentic publication/funding and original metadata/content before
        // arena acquisition. Reading charges the SAME shared owner counters.
        self.require_published(owner, identity)?;
        self.record
            .operational_source
            .require_current(&self.runtime, &self.operational_source_pins)?;
        self.require_operational_marker()?;
        let quota = owner.quota()?;
        let cleanup = lillux_sqlite::OriginalNativeFactory::required_cleanup_steps(
            quota.max_native_storage_bytes,
            quota.max_native_connections,
        )?.checked_add(HOST_RELEASE_ATTEMPTS)
            .context("offline native/physical cleanup quote overflow")?;
        let admission = self.issue_native_quota_admission(
            owner,
            identity,
            quota.max_native_storage_bytes,
            cleanup,
        )?;
        let transfer = (|| -> Result<(lillux_sqlite::original_host_owner::OriginalNativeRegionHandle,
            lillux_sqlite::SqliteInspectionProductionPermit)> {
            let region = admission.with_region(|region| Ok(region.try_clone()?))?;
            let mut issuer_index = admission.issuer_index.try_lock()
                .map_err(|_| anyhow::anyhow!("offline issuer index busy"))?;
            let control = admission._production.try_lock()
                .map_err(|_| anyhow::anyhow!("offline issuer interval busy"))?
                .as_ref().context("offline issuer interval already transferred")?.clone();
            let core = admission.with_region(|region| Ok(region.claim_producer_control(lillux_sqlite::SqliteInspectionProductionKind::FactoryIssuer)?))?;
            *issuer_index = Some(core.index());
            drop(issuer_index);
            let callback: Arc<dyn lillux_sqlite::SqliteInspectionProductionControl> = control;
            // SAFETY: the same genuine signed offline claim owns the prepaid
            // resident graph and this actual nonrecycled issuer interval.
            let producer = match unsafe { lillux_sqlite::SqliteInspectionProductionPermit::issued_from_region(
                lillux_sqlite::SqliteInspectionProductionKind::FactoryIssuer, callback, core) } {
                Ok(producer) => producer,
                Err(failure) => {
                    *admission.issuer_publication_failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(failure);
                    anyhow::bail!("original offline issuer publication refused; custody retained");
                }
            };
            admission._production.lock().unwrap_or_else(|p| p.into_inner()).take();
            Ok((region, producer))
        })();
        let (region, producer) = transfer.map_err(|error| anyhow::Error::new(OriginalNativeQuotaAdmissionFailure {
            error, _admission: admission.clone(),
        }))?;
        let authority: Arc<dyn lillux_sqlite::OriginalNativeAdmissionAuthority> = admission.clone();
        // SAFETY: exact genuine offline claim/namespace/clock and resident
        // budget/issuer are retained above, before actual native installation.
        let factory = unsafe {
            lillux_sqlite::OriginalNativeFactory::install_from_original_issuer(region, authority, producer)
        }.map_err(anyhow::Error::new)?;
        let registered = (|| -> Result<()> {
            let mut issued = self
                .native_factory
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original native factory registration busy"))?;
            ensure!(
                issued.is_none(),
                "original native factory already registered"
            );
            *issued = Some(Arc::downgrade(&factory));
            Ok(())
        })();
        registered.map_err(|error| {
            anyhow::Error::new(OriginalNativeFactoryIssuerFailure {
                error,
                _factory: factory.clone(),
            })
        })?;
        // Real Ready handoff occurs only after actual arena/bootstrap ownership.
        factory
            .initialize_original_backend()
            .map_err(anyhow::Error::new)?;
        Ok(factory)
    }
    fn native_quota_evidence(
        &self,
    ) -> Result<super::creation_evidence::OriginalNativeQuotaEvidence> {
        let used = self
            .copy_usage
            .try_lock()
            .map_err(|_| anyhow::anyhow!("native quota evidence busy or poisoned"))?;
        let evidence = if let Some(budget) = &used.native_budget {
            let snapshot = budget.snapshot()?;
            super::creation_evidence::OriginalNativeQuotaEvidence {
                admitted: true,
                storage_bytes: snapshot.storage_bytes,
                allocator_steps: snapshot.allocator_steps,
                cleanup_reserved: snapshot.cleanup_reserved,
                cleanup_steps: snapshot.cleanup_steps,
                connections_issued: snapshot.connections_issued,
            }
        } else {
            ensure!(
                used.native_storage_bytes == 0,
                "native storage charged with unresolved original budget registration"
            );
            super::creation_evidence::OriginalNativeQuotaEvidence::unissued()
        };
        evidence.require_quota(&self.record.reserved)?;
        Ok(evidence)
    }
}

/// Issued only by a sealed, drained original producer. Held outside the claim.
pub(super) struct OriginalProductionSeal {
    claim: Arc<OriginalQuotaClaim>,
    work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    never_started: [bool; 2],
}
#[derive(Debug, Clone)]
pub(super) struct OriginalInspectionSlot {
    pub(super) purpose: String,
    pub(super) directory_name: String,
    pub(super) expected_present: [bool; 4],
    pub(super) accounting: InspectionAccountingRequirement,
    pub(super) never_started: bool,
}
enum OriginalOfflineDestinationSemantics {
    Operational,
    Runtime { name: std::ffi::OsString, main_identity: (u64, u64) },
}
/// This removes only a redundant account alias after the actual original
/// supervisor retains the canonical account. No account owner is disposed.
struct OriginalOfflineDestinationContinuation {
    leaf: lillux_sqlite::OriginalNativeOutsideLeafEvidence,
    process: u32,
    claim: std::sync::Weak<OriginalQuotaClaim>,
    semantics: OriginalOfflineDestinationSemantics,
}
// SAFETY: concrete original domain fields/account alias, copied issuer PID;
// directory/namespace owners are moved into the shared physical source token.
// The real account controller retains its own complete physical obligations.
unsafe impl lillux_sqlite::OriginalOutsideDestinationContinuation for OriginalOfflineDestinationContinuation {
    fn require_original_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original offline destination continuation");
        Ok(())
    }
    fn retire_native_references(&mut self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()?;
        self.leaf.require_original(factory, aggregate)?;
        let claim = self.claim.upgrade().context("actual canonical offline account owner absent")?;
        claim.require_destination_outside_transfer(factory, aggregate)
        // This context owns no factory, budget or region handle. The canonical
        // account's independently retained native controls still count and
        // MUST be consumed by that account controller before region sealing.
    }
    unsafe fn register_shared_roots(&mut self,
        _registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()
        // All actual roots/namespace owners of this concrete destination were
        // moved to OriginalDestinationPhysicalSource; none are hidden here.
    }
    fn physical_holders_retired(&self) -> Result<bool> {
        self.require_original_process()?;
        Ok(true) // No physical holder in this semantic-only weak alias.
    }
    fn physical_holders_retirement_quote(&self) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        self.require_original_process()?;
        Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 4 })
    }
}
impl OriginalQuotaClaim {
    fn require_destination_outside_transfer(self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal) -> Result<()> {
        ensure!(self.record.resident_process == std::process::id(), "foreign offline destination account");
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        let issued = self.native_factory.try_lock()
            .map_err(|_| anyhow::anyhow!("actual destination factory lookup busy or poisoned"))?
            .as_ref().and_then(std::sync::Weak::upgrade)
            .context("actual original destination factory absent")?;
        ensure!(Arc::ptr_eq(&issued, factory), "destination transfer selects another genuine factory");
        ensure!(self.finalizer_master_entered.load(Ordering::Acquire),
            "canonical original outside account handoff not entered");
        ensure!(!self.native_cleanup_deadline.has_elapsed(), "original destination cleanup deadline elapsed");
        let state = self.production.try_lock()
            .map_err(|_| anyhow::anyhow!("actual destination production seal busy or poisoned"))?;
        ensure!(state.phase == OriginalProductionPhase::Sealed && state.active.is_empty(),
            "original destination account producer not sealed");
        Ok(())
    }
}

// Genuine stable Operational source; same original funding and exclusions.
struct OriginalOperationalAuthority {
    claim: Arc<OriginalQuotaClaim>,
    factory: std::sync::Weak<lillux_sqlite::OriginalNativeFactory>,
    root: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
}
impl lillux_sqlite::OriginalNativeDestinationAuthority for OriginalOperationalAuthority {
    fn into_outside_custody(self: Arc<Self>, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<lillux_sqlite::OriginalDestinationOutsideCustody, lillux_sqlite::OriginalDestinationCustodyFailure> {
        let checked = (|| -> Result<()> {
            let actual: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> = self.clone();
            leaf.require_original_authority(&actual, factory, aggregate)?;
            drop(actual);
            self.claim.require_destination_outside_transfer(factory, aggregate)?;
            let original = self.factory.upgrade().context("actual original destination affinity absent")?;
            ensure!(Arc::ptr_eq(&original, factory), "outside destination selects foreign factory affinity");
            ensure!(Arc::strong_count(&self) == 1,
                "actual original destination authority still has aliases");
            let bytes = std::mem::size_of::<OriginalOfflineDestinationContinuation>()
                .checked_add(std::mem::size_of::<lillux_sqlite::OriginalDestinationOutsideCustody>())
                .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<lillux_sqlite::OriginalDestinationPhysicalSource>()))
                .context("outside destination concrete custody quote overflow")?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| prepay(u64::try_from(bytes)?, 16))?;
            self.claim.require_destination_outside_transfer(factory, aggregate)
        })();
        if let Err(error) = checked {
            return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(factory.clone(), self, error));
        }
        let original = match Arc::try_unwrap(self) {
            Ok(original) => original,
            Err(original) => return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
                factory.clone(), original, anyhow::anyhow!("actual original destination alias appeared during transfer"))),
        };
        let Self { claim, factory: _, root, namespace } = original;
        let continuation = Box::new(OriginalOfflineDestinationContinuation {
            leaf: leaf.clone(), process: claim.record.resident_process, claim: Arc::downgrade(&claim), semantics: OriginalOfflineDestinationSemantics::Operational,
        });
        let sources = vec![lillux_sqlite::OriginalDestinationPhysicalSource::Directory(root),
            lillux_sqlite::OriginalDestinationPhysicalSource::Namespace(namespace)];
        // SAFETY: actual canonical original account retained by entered master,
        // genuine aggregate and charged storage before any Vec/Box allocation.
        Ok(unsafe { lillux_sqlite::OriginalDestinationOutsideCustody::issued(sources, continuation) })
    }

    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>) -> Result<()> {
        let expected = self
            .factory
            .upgrade()
            .context("original Operational factory absent")?;
        ensure!(
            expected.same_original(factory),
            "Operational selects a foreign factory"
        );
        let issued = {
            self.claim
                .native_factory
                .try_lock()
                .map_err(|_| {
                    anyhow::anyhow!("original Operational factory lookup busy or poisoned")
                })?
                .as_ref()
                .context("Operational original native factory not issued")?
                .upgrade()
                .context("Operational original native factory custody absent")?
        };
        ensure!(
            issued.same_original(factory),
            "Operational factory differs from authentic claim issuer"
        );
        // Bookkeeping lookup guard is gone before any filesystem/native read.
        factory.require_runtime_binding(&self.claim.runtime)?;
        self.namespace.ensure_protects(&self.root)?;
        self.root.ensure_path_binding()?;
        ensure!(
            self.root.is_same_directory(&self.claim.runtime)?,
            "Operational root differs from original runtime namespace"
        );
        self.claim
            .record
            .operational_source
            .require_main_identity(&self.root, &self.claim.operational_source_pins)?;
        self.claim.require_operational_marker()
    }
}
impl OriginalQuotaClaim {
    fn debit_operational_marker(&self, read_bytes: u64, memory_bytes: u64) -> Result<()> {
        self.require_service_window()?;
        ensure!(
            read_bytes > 0 && memory_bytes > 0,
            "empty original Operational marker debit"
        );
        let mut used = self.copy_usage.try_lock().map_err(|_| {
            anyhow::anyhow!("original Operational marker counters busy or poisoned")
        })?;
        let read = used
            .read_bytes
            .checked_add(read_bytes)
            .context("Operational original read debit overflow")?;
        let buffers = used
            .buffer_bytes
            .checked_add(memory_bytes)
            .context("Operational original memory debit overflow")?;
        ensure!(
            read <= self.record.reserved.max_read_bytes
                && buffers
                    .checked_add(used.native_storage_bytes)
                    .and_then(|bytes| bytes.checked_add(used.resident_storage_bytes))
                    .is_some_and(|total| total <= self.record.reserved.max_memory_bytes),
            "Operational marker exceeds original shared read/memory envelope"
        );
        used.read_bytes = read;
        used.buffer_bytes = buffers;
        drop(used);
        self.require_service_window()?;
        Ok(())
    }
    fn require_operational_marker(&self) -> Result<()> {
        self.record.operational_source.require_marker(
            &self.runtime,
            &self.operational_source_pins,
            |read, memory| self.debit_operational_marker(read, memory),
        )?;
        self.require_service_window()?;
        Ok(())
    }
    /// Same genuine source checks as the bound Operational role; returns the
    /// destination authority both the bound role and its schema reference use.
    fn operational_reset_authority(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        root: &Arc<lillux::PinnedDirectory>,
        namespace: &lillux::PinnedDirectoryLock,
        target: &std::path::Path,
    ) -> Result<Arc<OriginalOperationalAuthority>> {
        self.require_service_window()?;
        namespace.ensure_protects(root)?;
        root.ensure_path_binding()?;
        ensure!(
            target.parent() == Some(root.path())
                && target.file_name()
                    == Some(OsStr::new(
                        ryeos_state::operational::OPERATIONAL_DB_FILENAME
                    ))
                && root.is_same_directory(&self.runtime)?,
            "Operational target differs from original protocol source"
        );
        self.record
            .operational_source
            .require_current(root, &self.operational_source_pins)?;
        let authority = Arc::new(OriginalOperationalAuthority {
            claim: self.clone(),
            factory: Arc::downgrade(factory),
            root: root.clone(),
            namespace: namespace.clone(),
        });
        lillux_sqlite::OriginalNativeDestinationAuthority::require_original(
            authority.as_ref(),
            factory,
        )?;
        Ok(authority)
    }

    /// One paid ticket for the current Operational schema reference that the
    /// original Operational owner matches against.
    pub(super) fn issue_operational_reference_inputs(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        root: Arc<lillux::PinnedDirectory>,
        namespace: &lillux::PinnedDirectoryLock,
        target: &std::path::Path,
    ) -> Result<lillux_sqlite::OriginalNativeTransientInputs> {
        let authority = self.operational_reset_authority(factory, &root, namespace, target)?;
        // SAFETY: same genuine signed source, retained pins/namespace/funding
        // as the bound Operational role; the fixed reference purpose is chosen here.
        unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::NodeOperationalSchemaReference,
                authority,
            )
        }
    }

    pub(super) fn issue_operational_reset_inputs(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        root: Arc<lillux::PinnedDirectory>,
        namespace: &lillux::PinnedDirectoryLock,
        target: &std::path::Path,
        read_only: bool,
    ) -> Result<lillux_sqlite::OriginalNativeBoundInputs> {
        let authority = self.operational_reset_authority(factory, &root, namespace, target)?;
        let (mode, domain) = if read_only {
            (
                lillux_sqlite::OriginalNativeBoundMode::ReadonlySource,
                lillux_sqlite::OriginalNativeDomain::OperationalInspection,
            )
        } else {
            (
                lillux_sqlite::OriginalNativeBoundMode::Existing,
                lillux_sqlite::OriginalNativeDomain::OperationalReplayReset,
            )
        };
        // Genuine signed schema8 source, real retained pins/namespace/funding;
        // no default quota, SQL, ticket or independent phase/mutation permit.
        unsafe {
            lillux_sqlite::OriginalNativeBoundInputs::issued(
                root,
                OsStr::new(ryeos_state::operational::OPERATIONAL_DB_FILENAME),
                mode,
                domain,
                authority,
                None,
            )
        }
    }
}

// Real original Runtime destination custody, never restored from path facts.
// The claim owns the original O_PATH main pin captured before first SQLite.
struct OriginalRuntimeMutationAuthority {
    claim: Arc<OriginalQuotaClaim>,
    factory: std::sync::Weak<lillux_sqlite::OriginalNativeFactory>,
    root: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    name: std::ffi::OsString,
    main_identity: (u64, u64),
}
impl lillux_sqlite::OriginalNativeDestinationAuthority for OriginalRuntimeMutationAuthority {
    fn into_outside_custody(self: Arc<Self>, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<lillux_sqlite::OriginalDestinationOutsideCustody, lillux_sqlite::OriginalDestinationCustodyFailure> {
        let checked = (|| -> Result<()> {
            let actual: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> = self.clone();
            leaf.require_original_authority(&actual, factory, aggregate)?;
            drop(actual);
            self.claim.require_destination_outside_transfer(factory, aggregate)?;
            let original = self.factory.upgrade().context("actual original destination affinity absent")?;
            ensure!(Arc::ptr_eq(&original, factory), "outside destination selects foreign factory affinity");
            ensure!(Arc::strong_count(&self) == 1,
                "actual original destination authority still has aliases");
            let bytes = std::mem::size_of::<OriginalOfflineDestinationContinuation>()
                .checked_add(std::mem::size_of::<lillux_sqlite::OriginalDestinationOutsideCustody>())
                .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<lillux_sqlite::OriginalDestinationPhysicalSource>()))
                .context("outside destination concrete custody quote overflow")?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| prepay(u64::try_from(bytes)?, 16))?;
            self.claim.require_destination_outside_transfer(factory, aggregate)
        })();
        if let Err(error) = checked {
            return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(factory.clone(), self, error));
        }
        let original = match Arc::try_unwrap(self) {
            Ok(original) => original,
            Err(original) => return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
                factory.clone(), original, anyhow::anyhow!("actual original destination alias appeared during transfer"))),
        };
        let Self { claim, factory: _, root, namespace, name, main_identity } = original;
        let continuation = Box::new(OriginalOfflineDestinationContinuation {
            leaf: leaf.clone(), process: claim.record.resident_process, claim: Arc::downgrade(&claim), semantics: OriginalOfflineDestinationSemantics::Runtime { name, main_identity },
        });
        let sources = vec![lillux_sqlite::OriginalDestinationPhysicalSource::Directory(root),
            lillux_sqlite::OriginalDestinationPhysicalSource::Namespace(namespace)];
        // SAFETY: actual canonical original account retained by entered master,
        // genuine aggregate and charged storage before any Vec/Box allocation.
        Ok(unsafe { lillux_sqlite::OriginalDestinationOutsideCustody::issued(sources, continuation) })
    }

    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>) -> Result<()> {
        let original = self
            .factory
            .upgrade()
            .context("original Runtime factory owner absent")?;
        ensure!(
            original.same_original(factory),
            "Runtime mutation selects a foreign factory"
        );
        let issued = {
            self.claim
                .native_factory
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original Runtime issuer lookup busy or poisoned"))?
                .as_ref()
                .context("Runtime mutation factory was not issued")?
                .upgrade()
                .context("original Runtime factory custody absent")?
        };
        ensure!(
            issued.same_original(factory),
            "Runtime mutation factory differs from genuine claim issuer"
        );
        // Funding/state/node-namespace/policy/publication remain the exact
        // actual factory admission. Source-root exclusion is retained below.
        factory.require_runtime_binding(&self.claim.runtime)?;
        self.namespace.ensure_protects(&self.root)?;
        self.root.ensure_path_binding()?;
        self.claim._source_roots[0].ensure_path_binding()?;
        ensure!(
            self.root.is_same_directory(&self.claim._source_roots[0])?
                && serde_json::to_string(&self.root.identity()?)?
                    == self.claim.record.sources[0].directory_identity
                && self.name.as_os_str() == OsStr::new(&self.claim.record.sources[0].database_name),
            "Runtime mutation source/root differs from original reservation"
        );
        ensure!(
            self.claim.record.sources[0].roles[0].is_some(),
            "original Runtime source main was absent"
        );
        // capture_sources stores runtime pins first in fixed role order; when
        // Main was present its authentic retained pin is therefore first.
        let pin = self
            .claim
            ._source_pins
            .first()
            .context("original Runtime main pin absent")?;
        let original_main = pin.observed_identity()?;
        ensure!(
            (original_main.device(), original_main.inode()) == self.main_identity,
            "Runtime mutation original main pin changed"
        );
        let current = self
            .root
            .entry_no_follow(&self.name)?
            .context("original Runtime main coordinate absent")?;
        ensure!(
            current.entry_type == lillux::PinnedEntryType::Regular
                && (current.containing_device, current.inode) == self.main_identity,
            "Runtime mutation selected main inode was replaced"
        );
        // Authorized mutation can change size/contents and WAL/SHM. Do not
        // demand an old digest/extent/presence snapshot after those writes.
        Ok(())
    }
}
struct OriginalNativeCleanupAdmission {
    seal: Arc<OriginalProductionSeal>,
    factory: std::sync::Weak<lillux_sqlite::OriginalNativeFactory>,
}
impl lillux_sqlite::OriginalNativeCleanupAuthority for OriginalNativeCleanupAdmission {
    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>) -> Result<()> {
        let original = self
            .factory
            .upgrade()
            .context("original cleanup factory owner absent")?;
        ensure!(
            original.same_original(factory),
            "foreign original cleanup factory"
        );
        // Each helper releases its bookkeeping guard before clock/native I/O.
        self.seal
            .require_original(&self.seal.claim, &self.seal.work)?;
        self.seal.work.cleanup_checkpoint()?;
        let issued = {
            self.seal
                .claim
                .native_factory
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original cleanup issuer lookup busy or poisoned"))?
                .as_ref()
                .context("original native factory was not issued")?
                .upgrade()
                .context("original issued native factory custody absent")?
        };
        ensure!(
            issued.same_original(factory),
            "cleanup factory differs from actual claim issuer"
        );
        Ok(())
    }
}
impl OriginalProductionSeal {
    pub(super) fn issue_native_cleanup(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: lillux_sqlite::OriginalNativeConnectionTerminal,
    ) -> Result<lillux_sqlite::OriginalNativeCleanupInputs> {
        self.require_original(&self.claim, &self.work)?;
        terminal.require_original(factory)?;
        let authority = Arc::new(OriginalNativeCleanupAdmission {
            seal: self.clone(),
            factory: Arc::downgrade(factory),
        });
        lillux_sqlite::OriginalNativeCleanupAuthority::require_original(
            authority.as_ref(),
            factory,
        )?;
        // Safety: genuine sealed/drained original producer, actual issued
        // factory/Closed-or-Released and SAME cleanup clock retained above.
        unsafe { lillux_sqlite::OriginalNativeCleanupInputs::issued(terminal, authority) }
    }
    pub(super) fn slot(&self, index: usize) -> Result<OriginalInspectionSlot> {
        ensure!(index < 2, "foreign original inspection slot");
        Ok(OriginalInspectionSlot {
            purpose: self.claim.record.destinations[index].purpose.clone(),
            directory_name: self.claim.record.destinations[index].directory_name.clone(),
            expected_present: std::array::from_fn(|role| {
                self.claim.record.sources[index].roles[role].is_some()
            }),
            accounting: self.claim.record.inspection.accounting[index].clone(),
            never_started: self.never_started[index],
        })
    }
    fn require_original(
        &self,
        claim: &Arc<OriginalQuotaClaim>,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    ) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.claim, claim) && Arc::ptr_eq(&self.work, work),
            "producer seal selects foreign original claim/work"
        );
        let state = self
            .claim
            .production
            .try_lock()
            .map_err(|_| anyhow::anyhow!("sealed original producer busy or poisoned"))?;
        ensure!(
            state.phase == OriginalProductionPhase::Sealed
                && state.active.is_empty()
                && state.pending.is_empty(),
            "original producer seal is not drained"
        );
        Ok(())
    }
}
impl std::fmt::Debug for OriginalProductionSeal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalProductionSeal")
            .field("never_started", &self.never_started)
            .finish_non_exhaustive()
    }
}
impl OriginalQuotaClaim {
    fn enter_native_quota_interval(self: &Arc<Self>) -> Result<Arc<OriginalProductionControl>> {
        self.require_service_window()?;
        let mut state = self.production.try_lock()
            .map_err(|_| anyhow::anyhow!("original offline quota issuer busy or poisoned"))?;
        ensure!(state.phase == OriginalProductionPhase::Producing && !state.native_quota_entered,
            "original offline native quota interval already entered/draining");
        let ticket = state.next.checked_add(1).context("offline issuer ticket overflow")?;
        state.next = ticket; state.native_quota_entered = true;
        state.active.insert(ticket, OriginalProducerKind::NativeQuotaIssuer);
        Ok(Arc::new(OriginalProductionControl { claim: self.clone(), copy: None,
            ticket, kind: OriginalProducerKind::NativeQuotaIssuer, completed: AtomicBool::new(false) }))
    }
    fn enter_production(
        self: &Arc<Self>,
        kind: OriginalProducerKind,
        copy: Option<Arc<OriginalCopyWork>>,
    ) -> Result<lillux_sqlite::SqliteInspectionProductionPermit> {
        self.require_service_window()?;
        let mut state = self
            .production
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original production admission busy or poisoned"))?;
        ensure!(
            state.phase == OriginalProductionPhase::Producing,
            "original producer is draining/sealed; new interval refused"
        );
        match kind {
            OriginalProducerKind::Issuer => {
                ensure!(
                    !state.issuer_entered,
                    "original copy issuer already entered"
                );
                state.issuer_entered = true;
            }
            OriginalProducerKind::NativeQuotaIssuer => {
                ensure!(
                    !state.native_quota_entered,
                    "original native quota issuer already entered"
                );
                state.native_quota_entered = true;
            }
            OriginalProducerKind::NativeConnection(_) | OriginalProducerKind::NativeLease(_) => {
                anyhow::bail!("native connection requires its actual signed ticket issuer")
            }
            OriginalProducerKind::Namespace(index) => {
                ensure!(
                    index < 2 && !state.namespace_entered[index],
                    "original namespace producer already attempted"
                );
                state.namespace_entered[index] = true;
            }
            OriginalProducerKind::Copy(index) => {
                ensure!(
                    index < 8 && !state.copy_entered[index],
                    "original role producer already attempted"
                );
                state.copy_entered[index] = true;
            }
            OriginalProducerKind::Native(index) => {
                ensure!(
                    index < 2 && !state.native_entered[index],
                    "original native producer already attempted"
                );
                state.native_entered[index] = true;
            }
            OriginalProducerKind::SqlIssue(index) => {
                ensure!(
                    index < 2 && state.namespace_entered[index] && !state.sql_issue_entered[index],
                    "SQL issuer lacks original namespace or was already attempted"
                );
                state.sql_issue_entered[index] = true;
            }
        }
        let ticket = state
            .next
            .checked_add(1)
            .context("original producer ticket overflow")?;
        state.next = ticket;
        state.active.insert(ticket, kind);
        drop(state);
        let native_kind = match kind {
            OriginalProducerKind::NativeQuotaIssuer => {
                lillux_sqlite::SqliteInspectionProductionKind::FactoryIssuer
            }
            OriginalProducerKind::Native(_) | OriginalProducerKind::NativeConnection(_) => {
                lillux_sqlite::SqliteInspectionProductionKind::Native
            }
            OriginalProducerKind::NativeLease(_) => lillux_sqlite::SqliteInspectionProductionKind::ProjectionLease,
            OriginalProducerKind::Copy(_) => lillux_sqlite::SqliteInspectionProductionKind::Copy,
            _ => lillux_sqlite::SqliteInspectionProductionKind::Namespace,
        };
        Ok(lillux_sqlite::SqliteInspectionProductionPermit::issued(
            native_kind,
            Arc::new(OriginalProductionControl {
                claim: self.clone(),
                copy,
                ticket,
                kind,
                completed: AtomicBool::new(false),
            }),
        ))
    }
    fn original_copy_issuer(&self) -> Result<Arc<OriginalCopyWork>> {
        self.copy_issuer
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original copy issuer custody busy or poisoned"))?
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .context("original copy issuer absent; no replacement issue")
    }
    pub(super) fn close_native_admission_and_retire_preparations(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>> {
        ensure!(self.record.resident_process == std::process::id(), "foreign offline producer drain");
        factory.require_original_process()?;
        let original_factory = self.native_factory.try_lock()
            .map_err(|_| anyhow::anyhow!("offline drain factory lookup busy or poisoned"))?
            .as_ref().and_then(std::sync::Weak::upgrade)
            .context("actual issued offline factory absent")?;
        ensure!(Arc::ptr_eq(&original_factory, factory), "offline drain selects another original factory");
        let admission = self.native_admission.try_lock()
            .map_err(|_| anyhow::anyhow!("offline drain admission lookup busy or poisoned"))?
            .as_ref().and_then(std::sync::Weak::upgrade)
            .context("actual original offline admission absent")?;
        ensure!(Arc::ptr_eq(&admission.claim, self), "offline drain selects another claim");
        {
            let mut production = self.production.try_lock()
                .map_err(|_| anyhow::anyhow!("offline permanent admission close busy or poisoned"))?;
            if production.phase == OriginalProductionPhase::Producing {
                production.phase = OriginalProductionPhase::Draining;
            }
        }
        // Permanently close this same original admission BEFORE taking any
        // preparation. No producer/controller gate spans real C effects or ACK.
        admission.retire_preparations(factory)
    }
    pub(super) fn inspection_plan(&self) -> &OriginalInspectionPlan {
        &self.record.inspection
    }
    pub(super) fn seal_production(
        self: &Arc<Self>,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    ) -> Result<Arc<OriginalProductionSeal>> {
        let original: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork> =
            self.original_copy_issuer()?;
        ensure!(
            Arc::ptr_eq(&original, work),
            "sealing foreign original copy issuer"
        );
        work.cleanup_checkpoint()?;
        let mut state = self
            .production
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original producer seal busy or poisoned"))?;
        if state.phase == OriginalProductionPhase::Sealed {
            return state
                .seal
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .context("original seal owner released; cannot recreate");
        }
        state.phase = OriginalProductionPhase::Draining;
        ensure!(
            state.active.is_empty() && state.pending.is_empty(),
            "original producer drain pending: {} active, {} unresolved",
            state.active.len(),
            state.pending.len()
        );
        let never_started = std::array::from_fn(|index| {
            !state.namespace_entered[index]
                && !state.native_entered[index]
                && !state.sql_issue_entered[index]
                && state.copy_entered[index * 4..index * 4 + 4]
                    .iter()
                    .all(|entered| !entered)
        });
        let used = self
            .copy_usage
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original seal role snapshot busy or poisoned"))?;
        for index in 0..2 {
            if never_started[index] {
                ensure!(
                    !used.destinations[index]
                        && !used.sql_admitted[index]
                        && used.destination_roots[index].is_none()
                        && used.created_evidence[index].is_none()
                        && used.admitted[index * 4..index * 4 + 4]
                            .iter()
                            .all(|admitted| !admitted)
                        && used.destination_roles[index * 4..index * 4 + 4]
                            .iter()
                            .all(Option::is_none),
                    "never-started slot retains original production obligations"
                );
            }
        }
        drop(used);
        let seal = Arc::new(OriginalProductionSeal {
            claim: self.clone(),
            work: work.clone(),
            never_started,
        });
        state.phase = OriginalProductionPhase::Sealed;
        state.seal = Some(Arc::downgrade(&seal));
        Ok(seal)
    }
}

impl OriginalQuotaClaim {
    pub(super) fn prepare(
        owner: &OfflineResetPolicyOwner<'_>,
        identity: &NodeIdentity,
        prior: Option<&Arc<OriginalOperation>>,
        mode: OriginalInspectionMode,
        scheduler_selected: bool,
    ) -> Result<Arc<Self>> {
        owner.require_held()?;
        ensure!(
            owner.signer_fingerprint()? == identity.fingerprint(),
            "quota issuer is not the original node operator"
        );
        let (app_root, runtime) = owner.directories()?;
        let reserved = owner.quota()?.clone();
        // Observe once before ledger creation/signing. Retry never resamples a
        // new start or reconstructs an expired original window.
        let clock = lillux::time::RetainedOccupancyClock::capture().map_err(anyhow::Error::msg)?;
        let occupancy = lillux::time::OccupancyLimit::new(
            clock.start().clone(),
            occupancy_nanoseconds(&reserved)?,
        )
        .map_err(anyhow::Error::msg)?;
        let (native_service_deadline, native_cleanup_deadline) = clock.original_local_deadlines(
            &occupancy, std::time::Duration::from_millis(reserved.max_cleanup_millis))
            .map_err(anyhow::Error::msg)?;
        ensure!(
            owner.quota()?.max_retained_operations as usize <= MAX_CLAIMS,
            "offline quota operation count exceeds the bounded reservation namespace"
        );
        if let Some(operation) = prior {
            operation.require_staged(runtime)?;
        }
        let (sources, source_roots, source_pins) = capture_sources(runtime, owner.db_target()?)?;
        let (_, operational_namespace, _) = owner.retain_copy_inputs()?;
        let (operational_source, operational_source_pins) =
            OperationalSourcePlan::capture(runtime, &operational_namespace, &sources)?;
        // One narrowly owned ledger outside the scheduler's native inventory.
        // Its entire namespace is bounded and authenticated before publication.
        let ai = app_root
            .open_child_directory(OsStr::new(".ai"))?
            .context("node .ai root absent")?;
        let node = ai
            .open_child_directory(OsStr::new("node"))?
            .context("node root absent")?;
        let ledger = match node.open_child_directory(OsStr::new("offline-reset-quota"))? {
            Some(ledger) => ledger,
            None => {
                let ledger = node.create_child(OsStr::new("offline-reset-quota"), 0o700)?;
                node.sync().context("sync original offline quota ledger")?;
                ledger
            }
        };
        let copy_root = match node.open_child_directory(OsStr::new("offline-reset-inspections"))? {
            Some(parent) => parent,
            None => {
                let parent = node.create_child(OsStr::new("offline-reset-inspections"), 0o700)?;
                node.sync()?;
                parent
            }
        };
        copy_root.ensure_path_binding()?;
        let operation_id = prior
            .map(|p| p.reference().operation_id.clone())
            .unwrap_or_else(|| format!("{:032x}", rand::random::<u128>()));
        let destinations = destination_plans(&operation_id);
        let resident_layout = lillux_sqlite::original_host_owner::backing::HostStorage::quote_native_region()
            .map_err(anyhow::Error::msg)?;
        // One process-wide counter shared with every other resident issuer.
        let resident_generation = lillux_sqlite::original_host_owner::region::next_resident_generation()?;
        let record = ClaimRecord {
            schema: 9,
            purpose: "offline_reset_local_quota_reservation_v9".into(),
            resident_process: std::process::id(), resident_generation,
            resident_bytes: resident_layout.bytes() as u64,
            operation_id,
            claim_nonce: format!("{:032x}", rand::random::<u128>()),
            signer: identity.fingerprint().to_owned(),
            policy_digest: owner.generation_digest()?.to_owned(),
            app_root_identity: serde_json::to_string(&app_root.identity()?)?,
            runtime_identity: serde_json::to_string(&runtime.identity()?)?,
            prior_operation: prior.map(|p| p.reference().clone()),
            reserved,
            occupancy,
            clock_binding: clock.binding_digest().to_owned(),
            inspection: OriginalInspectionPlan::original(
                mode,
                scheduler_selected,
                prior.map(|operation| operation.reference()),
                &sources,
            ),
            sources,
            operational_source,
            copy_root_identity: serde_json::to_string(&copy_root.identity()?)?,
            destinations,
        };
        record.validate()?;
        let body = lillux::canonical_json(&serde_json::to_value(&record)?)?;
        let signed =
            lillux::signature::sign_content(&body, identity.signing_key(), "#", None).into_bytes();
        ensure!(
            signed.len() <= MAX_RECORD_BYTES,
            "offline quota claim exceeds record bound"
        );
        Ok(Arc::new(Self {
            record,
            clock,
            native_service_deadline, native_cleanup_deadline,
            signer: identity.clone(),
            signed,
            app_root: app_root.try_clone()?,
            runtime: runtime.try_clone()?,
            ledger,
            copy_root,
            inspection_namespace: Mutex::new(None),
            publication_entered: AtomicBool::new(false),
            publication_acknowledged: AtomicBool::new(false),
            finalizer_master_entered: AtomicBool::new(false),
            reservation_publication: Mutex::new(None),
            copy_usage: Mutex::new(CopyUsage::default()),
            production: Mutex::new(OriginalProductionState::default()),
            copy_issuer: Mutex::new(None),
            native_factory: Mutex::new(None),
            native_admission: Mutex::new(None),
            resident: Mutex::new(NativeHostRegion::new(resident_layout)),
            _prior_operation: prior.cloned(),
            _source_roots: source_roots,
            // Safety: actual ordered O_PATH metadata owners captured under
            // original namespace, transferred without duplication/reopening.
            _source_pins: source_pins
                .into_iter()
                .map(|pin| unsafe { lillux_sqlite::RetainedOriginalMetadataPin::issued(pin) })
                .collect(),
            operational_source_pins,
        }))
    }

    /// Identity/custody join only. A prepared claim is not yet a work permit.
    pub(super) fn require_original_identity(
        &self,
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
        app_root: &std::path::Path,
    ) -> Result<&str> {
        self.record.validate()?;
        self.require_service_window()?;
        self.app_root.ensure_path_binding()?;
        self.runtime.ensure_path_binding()?;
        ensure!(
            self.app_root.path() == app_root
                && self.runtime.is_same_directory(runtime)?
                && self.record.signer == identity.fingerprint(),
            "original quota identity selects another app, runtime, or signer"
        );
        Ok(&self.record.operation_id)
    }

    fn require_resident_cleanup(&self) -> Result<()> {
        ensure!(self.record.resident_process == std::process::id(), "inherited offline physical cleanup refused");
        self.record.validate()?;
        ensure!(self.record.clock_binding == self.clock.binding_digest(), "offline resident cleanup changes original clock");
        self.ledger.ensure_path_binding()?;
        let publication = self.reservation_publication.try_lock()
            .map_err(|_| anyhow::anyhow!("offline resident reservation custody busy"))?
            .as_ref().context("original offline resident debit not published")?.clone();
        publication.require_original(&self.ledger, OsStr::new(&format!("{PREFIX}{}", self.record.operation_id)))?;
        ensure!(publication.read_original(MAX_RECORD_BYTES)? == self.signed, "offline resident durable debit changed");
        let allowance = std::time::Duration::from_millis(self.record.reserved.max_cleanup_millis);
        original_cleanup_remaining(self.clock.window(&self.record.occupancy, allowance)
            .map_err(anyhow::Error::msg)?, allowance)?;
        Ok(())
    }
    pub(super) fn require_service_window(&self) -> Result<std::time::Duration> {
        ensure!(
            !self.finalizer_master_entered.load(Ordering::Acquire),
            "original finalizer master entered; service/copy admission refused"
        );
        self.record.validate()?;
        ensure!(
            self.record.clock_binding == self.clock.binding_digest(),
            "original clock owner differs from signed reservation"
        );
        match self
            .clock
            .window(
                &self.record.occupancy,
                std::time::Duration::from_millis(self.record.reserved.max_cleanup_millis),
            )
            .map_err(anyhow::Error::msg)?
        {
            lillux::time::OccupancyWindowState::Service { remaining } => Ok(remaining),
            _ => anyhow::bail!(
                "original offline quota service window expired; charge remains retained"
            ),
        }
    }

    pub(super) fn operation_id(&self) -> &str {
        &self.record.operation_id
    }

    /// Same genuine source checks as the bound Runtime mutation; returns the
    /// destination authority both the bound role and its schema reference use.
    fn runtime_mutation_authority(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        root: &Arc<lillux::PinnedDirectory>,
        namespace: &lillux::PinnedDirectoryLock,
        target: &std::path::Path,
    ) -> Result<(Arc<OriginalRuntimeMutationAuthority>, std::ffi::OsString)> {
        self.require_service_window()?;
        namespace.ensure_protects(root)?;
        root.ensure_path_binding()?;
        ensure!(
            target.parent() == Some(root.path())
                && root.path() == self._source_roots[0].path()
                && root.is_same_directory(&self._source_roots[0])?,
            "Runtime mutation target/source differs from reserved Config target"
        );
        self.require_runtime_inspection_target(target)?;
        self.record.sources[0].require_current(root)?;
        ensure!(
            self.record.sources[0].roles[0].is_some(),
            "Existing Runtime source main was absent; no create fallback"
        );
        let pin = self
            ._source_pins
            .first()
            .context("original Runtime main pin absent")?;
        let main = pin.observed_identity()?;
        let name = target
            .file_name()
            .context("original Runtime target basename absent")?
            .to_owned();
        let authority = Arc::new(OriginalRuntimeMutationAuthority {
            claim: self.clone(),
            factory: Arc::downgrade(factory),
            root: root.clone(),
            namespace: namespace.clone(),
            name: name.clone(),
            main_identity: (main.device(), main.inode()),
        });
        lillux_sqlite::OriginalNativeDestinationAuthority::require_original(
            authority.as_ref(),
            factory,
        )?;
        Ok((authority, name))
    }

    /// One paid ticket for the current Runtime schema reference that the
    /// original Runtime mutation owner matches against.
    pub(super) fn issue_runtime_reference_inputs(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        root: Arc<lillux::PinnedDirectory>,
        namespace: &lillux::PinnedDirectoryLock,
        target: &std::path::Path,
    ) -> Result<lillux_sqlite::OriginalNativeTransientInputs> {
        let (authority, _name) = self.runtime_mutation_authority(factory, &root, namespace, target)?;
        // SAFETY: same genuine signed source, retained main pin, root,
        // namespace and funding as the bound Runtime mutation; the fixed
        // reference purpose is chosen here and builds only declared schema.
        unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference,
                authority,
            )
        }
    }

    /// Genuine once-claim destination inputs only; opening/ticket admission
    /// remains with the actual factory and original producer phase.
    pub(super) fn issue_runtime_mutation_inputs(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        root: Arc<lillux::PinnedDirectory>,
        namespace: &lillux::PinnedDirectoryLock,
        target: &std::path::Path,
    ) -> Result<lillux_sqlite::OriginalNativeBoundInputs> {
        let (authority, name) = self.runtime_mutation_authority(factory, &root, namespace, target)?;
        // Safety: exact signed claim source[0]/Config target, actual original
        // main pin, root/namespace and SAME real issued factory retained above.
        unsafe {
            lillux_sqlite::OriginalNativeBoundInputs::issued(
                root,
                &name,
                lillux_sqlite::OriginalNativeBoundMode::Existing,
                lillux_sqlite::OriginalNativeDomain::RuntimeMutation,
                authority,
                None,
            )
        }
    }

    /// Source selection check only, not a work or native allocation permit.
    pub(super) fn require_runtime_inspection_target(&self, target: &std::path::Path) -> Result<()> {
        self.require_service_window()?;
        let source = &self._source_roots[0];
        ensure!(
            target.parent() == Some(source.path())
                && target.file_name() == Some(OsStr::new(&self.record.sources[0].database_name)),
            "runtime inspection target differs from original reservation"
        );
        self.record.sources[0].require_current(source)
    }

    /// Join the original scheduler selection before absence probes or copy.
    /// This is source affinity only, not a native work permit.
    pub(super) fn require_scheduler_inspection_source(
        &self,
        source: &lillux::PinnedDirectory,
    ) -> Result<()> {
        self.require_service_window()?;
        ensure!(
            source.is_same_directory(&self._source_roots[1])?,
            "scheduler inspection selects another original namespace"
        );
        self.record.sources[1].require_current(source)
    }

    /// Only a retry witness for this original publisher. The actual publish
    /// path still performs the complete aggregate census and durable ack.
    pub(super) fn require_published(
        &self,
        owner: &OfflineResetPolicyOwner<'_>,
        identity: &NodeIdentity,
    ) -> Result<()> {
        owner.require_held()?;
        let (app_root, runtime) = owner.directories()?;
        self.require_original_identity(runtime, identity, app_root.path())?;
        ensure!(
            self.publication_entered.load(Ordering::Acquire)
                && owner.generation_digest()? == self.record.policy_digest
                && owner.quota()? == &self.record.reserved,
            "offline quota retry lacks its original policy/publication owner"
        );
        self.ledger.ensure_path_binding()?;
        self.copy_root.ensure_path_binding()?;
        ensure!(
            serde_json::to_string(&self.copy_root.identity()?)? == self.record.copy_root_identity,
            "inspection destination parent differs from original reservation"
        );
        let name = format!("{PREFIX}{}", self.record.operation_id);
        let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
            &self.ledger,
            OsStr::new(&name),
            MAX_RECORD_BYTES,
        )?
        .context("original offline quota reservation is absent")?;
        ensure!(
            bytes == self.signed,
            "original offline quota reservation changed"
        );
        self.require_service_window()?;
        Ok(())
    }

    pub(super) fn publish(
        self: &Arc<Self>,
        owner: &OfflineResetPolicyOwner<'_>,
        identity: &NodeIdentity,
    ) -> Result<()> {
        self.publish_inner(owner, identity)
            .map_err(|error| self.retain_failure(error, QuotaFailurePhase::Publication))?;
        self.publication_acknowledged.store(true, Ordering::Release);
        Ok(())
    }

    /// Cooperating inspection-parent writers share this exact original lock.
    /// This is not confinement against arbitrary same-UID host processes.
    fn retain_inspection_namespace(&self) -> Result<lillux::PinnedDirectoryLock> {
        self.copy_root.require_owner_private_directory()?;
        let mut original = self
            .inspection_namespace
            .lock()
            .map_err(|_| anyhow::anyhow!("original inspection namespace issuer poisoned"))?;
        if original.is_none() {
            let held = self
                .copy_root
                .try_lock_exclusive()?
                .context("inspection parent has another live namespace owner")?;
            held.ensure_protects(&self.copy_root)?;
            *original = Some(held);
        }
        let held = original.as_ref().unwrap().clone();
        drop(original);
        held.ensure_protects(&self.copy_root)?;
        self.copy_root.require_owner_private_directory()?;
        Ok(held)
    }

    /// Census joins an already-issued lock or obtains a temporary original
    /// lock. Publication alone cannot manufacture a persistent copy owner.
    fn inspection_census_namespace(&self) -> Result<lillux::PinnedDirectoryLock> {
        self.copy_root.require_owner_private_directory()?;
        let original = self
            .inspection_namespace
            .lock()
            .map_err(|_| anyhow::anyhow!("inspection namespace issuer poisoned"))?;
        let held = match original.as_ref() {
            Some(held) => held.clone(),
            None => self
                .copy_root
                .try_lock_exclusive()?
                .context("inspection census conflicts with a live namespace owner")?,
        };
        drop(original);
        held.ensure_protects(&self.copy_root)?;
        Ok(held)
    }

    /// Issue only after the original complete-census publication acknowledged.
    /// Every issued view shares counters and the original nonrenewable clock.
    pub(super) fn issue_copy_work(
        self: &Arc<Self>,
        owner: &OfflineResetPolicyOwner<'_>,
        identity: &NodeIdentity,
    ) -> Result<Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>> {
        let production = self.enter_production(OriginalProducerKind::Issuer, None)?;
        let reservation_name = format!("{PREFIX}{}", self.record.operation_id);
        let reservation_pin = self
            .ledger
            .open_mount_entry(OsStr::new(&reservation_name))?
            .context("issued copy work has no original reservation inode")?;
        ensure!(
            reservation_pin.metadata()?.is_file(),
            "reservation inode is not regular"
        );
        let reservation_identity = lillux::observe_open_file_identity(&reservation_pin)?;
        self.require_published(owner, identity)?;
        ensure!(
            lillux::observe_open_file_identity(&reservation_pin)? == reservation_identity,
            "reservation changed while original copy work was issued"
        );
        let current = self
            .ledger
            .entry_no_follow(OsStr::new(&reservation_name))?
            .context("original reservation disappeared during copy issue")?;
        ensure!(
            current.entry_type == lillux::PinnedEntryType::Regular
                && current.containing_device == reservation_identity.device()
                && current.inode == reservation_identity.inode(),
            "reservation issue selected another inode"
        );
        ensure!(
            self.publication_acknowledged.load(Ordering::Acquire),
            "copy work requires acknowledged original aggregate reservation"
        );
        let (state_lifetime, namespace, policy) = owner.retain_copy_inputs()?;
        let inspection_namespace = self.retain_inspection_namespace()?;
        self.require_service_window()?;
        let work = Arc::new(OriginalCopyWork {
            outside_master: Mutex::new(None),
            claim: Arc::clone(self),
            state_lifetime: OriginalAccountStateLock::new(state_lifetime),
            namespace,
            inspection_namespace,
            policy,
            reservation_name,
            // Safety: the actual issued O_PATH reservation pin transfers once,
            // never a duplicate/new source or reconstructed funding owner.
            reservation_pin: Mutex::new(Some(unsafe {
                lillux_sqlite::RetainedOriginalMetadataPin::issued(reservation_pin)
            })),
            reservation_identity,
        });
        *self
            .copy_issuer
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original copy issuer registration busy or poisoned"))? =
            Some(Arc::downgrade(&work));
        production.acknowledge_synchronous()?;
        Ok(work)
    }

    pub(super) fn retain_failure(
        self: &Arc<Self>,
        error: anyhow::Error,
        phase: QuotaFailurePhase,
    ) -> anyhow::Error {
        anyhow::Error::new(QuotaCustodyFailure {
            error,
            claim: Arc::clone(self),
            phase,
        })
    }

    fn publish_inner(
        &self,
        owner: &OfflineResetPolicyOwner<'_>,
        identity: &NodeIdentity,
    ) -> Result<()> {
        owner.require_held()?;
        self.app_root.ensure_path_binding()?;
        self.runtime.ensure_path_binding()?;
        self.ledger.ensure_path_binding()?;
        self.copy_root.ensure_path_binding()?;
        ensure!(
            serde_json::to_string(&self.copy_root.identity()?)? == self.record.copy_root_identity,
            "inspection destination parent differs from original reservation"
        );
        let (app_root, runtime) = owner.directories()?;
        ensure!(
            self.app_root.is_same_directory(app_root)?
                && self.runtime.is_same_directory(runtime)?,
            "quota publication selects another root or runtime"
        );
        ensure!(
            identity.fingerprint() == self.record.signer
                && owner.signer_fingerprint()? == self.record.signer
                && owner.generation_digest()? == self.record.policy_digest
                && owner.quota()? == &self.record.reserved,
            "quota publication changes original issuer or policy"
        );
        self.record.validate()?;
        self.require_service_window()?;
        // StateLock excludes other stopped-node processes, but its shared
        // lease alone cannot serialize two in-process publishers. Acquire an
        // independent OFD-backed ledger lock across scan, debit and fsync.
        let publication_lock = self.ledger.lock_exclusive_with_timeout(
            self.require_service_window()?
                .min(std::time::Duration::from_millis(1000)),
        )?;
        publication_lock.ensure_protects(&self.ledger)?;
        // Fixed order: ledger then inspection parent. Acquisition is
        // nonblocking, so a retained copy owner cannot cause a lock cycle.
        let inspection_namespace = self.inspection_census_namespace()?;
        inspection_namespace.ensure_protects(&self.copy_root)?;
        self.ledger.ensure_path_binding()?;
        self.copy_root.ensure_path_binding()?;
        ensure!(
            serde_json::to_string(&self.copy_root.identity()?)? == self.record.copy_root_identity,
            "inspection destination parent differs from original reservation"
        );
        self.require_service_window()?;
        let selected_name = format!("{PREFIX}{}", self.record.operation_id);
        let mut records = Vec::new();
        let mut expected_destinations = std::collections::BTreeMap::new();
        let mut expected_evidence = std::collections::BTreeMap::new();
        let mut original_present = false;
        let mut original_records = std::collections::BTreeMap::new();
        let mut disposition_records = std::collections::BTreeMap::new();
        let mut terminal_records = std::collections::BTreeMap::new();
        let mut terminal_record_hashes = std::collections::BTreeMap::new();
        let mut terminal_record_bytes = std::collections::BTreeMap::new();
        let mut prepared_records = std::collections::BTreeMap::new();
        let mut prepared_record_bytes = std::collections::BTreeMap::new();
        let mut account_intent_records = std::collections::BTreeMap::new();
        // Reservation + disposal + pending terminal + prepared + account intent;
        // the intent can preserve the authenticated original full charge.
        let names = self.ledger.entry_names_bounded(MAX_CLAIMS * 5 + 1)?;
        ensure!(
            names.len() <= MAX_CLAIMS * 5,
            "offline quota ledger exceeds complete inventory capacity"
        );
        // A result strictly below the enumeration stop establishes EOF; an
        // extra entry always refuses, so no truncated census authorizes debit.
        for name in names {
            let text = name
                .to_str()
                .context("offline quota ledger name is not UTF-8")?;
            if let Some(operation) = text.strip_prefix("account-intent.") {
                require_operation_suffix(operation)?;
                let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
                    &self.ledger, &name, MAX_RECORD_BYTES)?
                    .context("original account intent census record disappeared")?;
                let record = super::creation_evidence::OriginalAccountSettlementIntentRecord::verified(&bytes, identity)?;
                ensure!(record.prepared.operation_id == operation
                    && account_intent_records.insert(operation.to_owned(), record).is_none(),
                    "duplicate/foreign original account intent coordinate");
                continue;
            }
            if let Some(operation) = text.strip_prefix("disposed.") {
                require_operation_suffix(operation)?;
                let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
                    &self.ledger,
                    &name,
                    MAX_RECORD_BYTES,
                )?
                .context("original disposition census record disappeared")?;
                let record = super::creation_evidence::InspectionDispositionRecord::verified(
                    &bytes, identity,
                )?;
                ensure!(
                    record.operation_id == operation
                        && disposition_records
                            .insert(operation.to_owned(), (record, lillux::sha256_hex(&bytes)))
                            .is_none(),
                    "duplicate/foreign disposition census coordinate"
                );
                continue;
            }
            if let Some(operation) = text.strip_prefix("terminal.") {
                require_operation_suffix(operation)?;
                let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
                    &self.ledger,
                    &name,
                    MAX_RECORD_BYTES,
                )?
                .context("original terminal census record disappeared")?;
                let record =
                    super::creation_evidence::InspectionTerminalRecord::verified(&bytes, identity)?;
                terminal_record_hashes.insert(operation.to_owned(), lillux::sha256_hex(&bytes));
                ensure!(
                    record.disposition.operation_id == operation
                        && terminal_records
                            .insert(operation.to_owned(), record)
                            .is_none(),
                    "duplicate/foreign terminal census coordinate"
                );
                terminal_record_bytes.insert(operation.to_owned(), bytes);
                continue;
            }
            if let Some(operation) = text.strip_prefix("prepared.") {
                require_operation_suffix(operation)?;
                let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
                    &self.ledger,
                    &name,
                    MAX_RECORD_BYTES,
                )?
                .context("prepared retirement census record disappeared")?;
                let record =
                    super::creation_evidence::OriginalPreparedSourceRetirementRecord::verified(
                        &bytes, identity,
                    )?;
                ensure!(
                    record.operation_id == operation
                        && prepared_records
                            .insert(operation.to_owned(), record)
                            .is_none(),
                    "duplicate/foreign prepared retirement coordinate"
                );
                prepared_record_bytes.insert(operation.to_owned(), bytes);
                continue;
            }
            if let Some(operation) = text.strip_prefix("cleanup.reserved.") {
                require_operation_suffix(operation)?;
                anyhow::bail!(
                    "original reservation quarantine is pending; complete authenticated recovery required before new publication"
                );
            }
            let suffix = text
                .strip_prefix(PREFIX)
                .context("unclassified offline quota ledger deposit requires recovery")?;
            ensure!(
                suffix.len() == 32
                    && suffix
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "malformed offline quota claim name"
            );
            let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
                &self.ledger,
                &name,
                MAX_RECORD_BYTES,
            )?
            .context("offline quota claim disappeared")?;
            let raw = std::str::from_utf8(&bytes)?;
            let (body, header) =
                lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)?;
            let header = header.context("offline quota claim is unsigned")?;
            ensure!(
                lillux::signature::is_valid_signature_for(
                    &header.content_hash,
                    &header.signature_b64,
                    &header.signer_fingerprint,
                    &body,
                    identity.verifying_key(),
                    identity.fingerprint(),
                ),
                "offline quota claim signature is invalid"
            );
            let record: ClaimRecord = serde_json::from_str(&body)?;
            record.validate()?;
            ensure!(
                lillux::canonical_json(&serde_json::to_value(&record)?)? == body
                    && record.operation_id == suffix
                    && record.signer == self.record.signer
                    && record.app_root_identity == self.record.app_root_identity
                    && record.runtime_identity == self.record.runtime_identity
                    && record.copy_root_identity == self.record.copy_root_identity
                    && record.clock_binding == self.record.clock_binding,
                "offline quota claim has noncanonical or foreign ownership"
            );
            if text == selected_name {
                ensure!(
                    self.publication_entered.load(Ordering::Acquire) && bytes == self.signed,
                    "another offline reservation already owns this operation; authenticated recovery required"
                );
                original_present = true;
            }
            let claim_hash = lillux::sha256_hex(&bytes);
            ensure!(
                original_records
                    .insert(
                        record.operation_id.clone(),
                        (record.clone(), claim_hash.clone())
                    )
                    .is_none(),
                "duplicate original quota reservation"
            );
            for (index, plan) in record.destinations.iter().enumerate() {
                expected_destinations.insert(
                    plan.directory_name.clone(),
                    (record.clone(), claim_hash.clone(), index),
                );
                expected_evidence.insert(
                    evidence_name(&plan.directory_name),
                    plan.directory_name.clone(),
                );
            }
            records.push(record.reserved);
        }
        // A durable intent preserves the authentic original charge after its
        // reservation coordinate disappears. It never recreates old process
        // owners or supplies close authority to this scanner.
        for (operation, intent) in &account_intent_records {
            intent.require_original_chain(
                terminal_record_bytes.get(operation).context("account intent lacks exact pending bytes")?,
                prepared_record_bytes.get(operation).context("account intent lacks exact prepared bytes")?, identity)?;
            let original = ClaimRecord::verified_original(intent.original_claim.as_bytes(), identity)?;
            let hash = lillux::sha256_hex(intent.original_claim.as_bytes());
            ensure!(original.operation_id == *operation
                && original.signer == self.record.signer
                && original.app_root_identity == self.record.app_root_identity
                && original.runtime_identity == self.record.runtime_identity
                && original.copy_root_identity == self.record.copy_root_identity
                && original.clock_binding == self.record.clock_binding
                && original.policy_digest == intent.prepared.policy_digest
                && original.resident_generation == intent.prepared.resident_generation
                && original.resident_bytes == intent.prepared.resident_bytes
                && hash == intent.prepared.claim_sha256,
                "account intent changes authentic original claim ownership");
            intent.prepared.native_quota.require_quota(&original.reserved)?;
            if let Some((_, present_hash)) = original_records.get(operation) {
                ensure!(*present_hash == hash, "account intent selects another signed reservation");
            } else {
                for (index, plan) in original.destinations.iter().enumerate() {
                    ensure!(!expected_destinations.contains_key(&plan.directory_name),
                        "recovered original destination collides with another claim");
                    expected_destinations.insert(plan.directory_name.clone(), (original.clone(), hash.clone(), index));
                    expected_evidence.insert(evidence_name(&plan.directory_name), plan.directory_name.clone());
                }
                records.push(original.reserved.clone());
                original_records.insert(operation.clone(), (original, hash));
            }
        }
        ensure!(
            original_records.len() <= MAX_CLAIMS,
            "original reservation census exceeds claim capacity"
        );
        for (operation, (disposition, _)) in &disposition_records {
            let (original, hash) = original_records
                .get(operation)
                .context("disposition has no charged original reservation")?;
            require_disposition_claim(disposition, original, hash)?;
        }
        for (operation, terminal) in &terminal_records {
            let (disposition, hash) = disposition_records
                .get(operation)
                .context("terminal lacks original disposition record")?;
            ensure!(
                &terminal.disposition == disposition
                    && terminal.disposition_sha256 == *hash
                    && terminal.reservation_retained,
                "terminal contradicts charged original disposition"
            );
        }
        for (operation, prepared) in &prepared_records {
            let (original, hash) = original_records
                .get(operation)
                .context("prepared retirement lacks charged original reservation")?;
            terminal_records
                .get(operation)
                .context("prepared retirement lacks original pending terminal")?;
            prepared.native_quota.require_quota(&original.reserved)?;
            ensure!(
                prepared.claim_sha256 == *hash
                    && prepared.reservation_parent_identity == self.ledger.identity()?
                    && prepared.resident_generation == original.resident_generation
                    && prepared.resident_bytes == original.resident_bytes
                    && prepared.signer == original.signer
                    && prepared.clock_binding == original.clock_binding
                    && prepared.policy_digest == original.policy_digest
                    && prepared.pending_terminal_sha256
                        == *terminal_record_hashes
                            .get(operation)
                            .context("pending terminal byte hash absent")?
                    ,
                "prepared retirement contradicts original charged scope"
            );
            // Signed prepared evidence never supplies cold close/ACK authority,
            // removes an obligation or refunds this charged original envelope.
        }
        for (operation, intent) in &account_intent_records {
            original_records.get(operation)
                .context("account settlement intent lacks charged original reservation")?;
            intent.require_original_chain(
                terminal_record_bytes.get(operation).context("account intent lacks exact original pending bytes")?,
                prepared_record_bytes.get(operation).context("account intent lacks exact original prepared bytes")?,
                identity)?;
            // Intent is evidence of a pending transition only. The original
            // reservation stays in `records`, including every cleanup charge.
        }
        // Discover all destination deposits from authenticated reservation
        // intents. Unknown/colliding objects refuse; no cold owner is recreated.
        let copies = self.copy_root.entry_names_bounded(MAX_CLAIMS * 6 + 1)?;
        ensure!(
            copies.len() <= MAX_CLAIMS * 6,
            "inspection destination census is incomplete"
        );
        let mut observed_roots = std::collections::BTreeMap::new();
        let mut observed_evidence = std::collections::BTreeMap::new();
        for name in copies {
            let text = name
                .to_str()
                .context("inspection destination name is not UTF-8")?;
            let entry = self
                .copy_root
                .entry_no_follow(&name)?
                .context("inspection destination disappeared")?;
            if expected_destinations.contains_key(text) {
                ensure!(
                    entry.entry_type == lillux::PinnedEntryType::Directory,
                    "inspection destination is not a directory"
                );
                let root = self
                    .copy_root
                    .open_child_directory(&name)?
                    .context("inspection destination disappeared")?;
                root.ensure_path_binding()?;
                observed_roots.insert(text.to_owned(), root.identity()?);
            } else if let Some(destination) =
                expected_evidence.get(text.strip_prefix("cleanup.").unwrap_or(text))
            {
                ensure!(
                    entry.entry_type == lillux::PinnedEntryType::Regular,
                    "created evidence is not regular"
                );
                let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
                    &self.copy_root,
                    &name,
                    MAX_RECORD_BYTES,
                )?
                .context("created evidence disappeared")?;
                let evidence = CreatedDirectoryRecord::verified(&bytes, identity)?;
                let (record, claim_hash, index) = &expected_destinations[destination];
                ensure!(
                    evidence.operation_id == record.operation_id
                        && evidence.claim_nonce == record.claim_nonce
                        && evidence.claim_sha256 == *claim_hash
                        && evidence.policy_digest == record.policy_digest
                        && evidence.clock_binding == record.clock_binding
                        && evidence.source_purpose == record.sources[*index].purpose
                        && evidence.destination_name == *destination
                        && serde_json::to_string(&evidence.parent_identity)?
                            == record.copy_root_identity,
                    "created evidence differs from authenticated original reservation"
                );
                ensure!(
                    observed_evidence
                        .insert(destination.clone(), evidence.created_identity)
                        .is_none(),
                    "original creation witness has duplicate public/quarantine deposit"
                );
                if text.starts_with("cleanup.") {
                    ensure!(disposition_records.get(&evidence.operation_id).is_some_and(|(record, _)| record.stores.iter().any(|store| matches!(store, super::creation_evidence::InspectionStoreDispositionRecord::Removed { destination_name, witness_sha256, .. } if destination_name == destination && witness_sha256 == &lillux::sha256_hex(&bytes)))), "creation witness quarantine lacks original durable disposal join");
                }
            } else {
                anyhow::bail!("unclassified inspection destination requires recovery");
            }
        }
        for (operation, (disposition, _)) in &disposition_records {
            disposition
                .native_quota
                .require_quota(&original_records[operation].0.reserved)?;
            for (index, store) in disposition.stores.iter().enumerate() {
                let name = &original_records[operation].0.destinations[index].directory_name;
                ensure!(
                    !observed_roots.contains_key(name),
                    "durably disposed/never-started root coordinate reappeared"
                );
                if matches!(
                    store,
                    super::creation_evidence::InspectionStoreDispositionRecord::NeverStarted { .. }
                ) {
                    ensure!(
                        !observed_evidence.contains_key(name),
                        "never-started disposition retains creation witness"
                    );
                }
                if terminal_records
                    .get(operation)
                    .is_some_and(|terminal| terminal.witness_retired[index])
                {
                    ensure!(
                        !observed_evidence.contains_key(name),
                        "terminal witness retirement contradicts namespace deposit"
                    );
                }
            }
        }
        for (name, observed) in observed_roots {
            ensure!(
                observed_evidence.get(&name) == Some(&observed),
                "created directory lacks exact original inode evidence; recovery required"
            );
        }
        if !original_present {
            records.push(self.record.reserved.clone());
        }
        // Reserve the complete envelope conservatively. No concurrent/retained
        // owner can borrow unused terms, reset counters, or release on error.
        validate_aggregate(&records, owner.quota()?)?;
        self.require_service_window()?;
        if !original_present {
            let parent_identity = self.ledger.identity()?;
            let original_name = std::ffi::OsString::from(&selected_name);
            self.publication_entered.store(true, Ordering::Release);
            let file = self
                .ledger
                .atomic_create_regular(
                    OsStr::new(&selected_name),
                    &self.signed,
                    0o600,
                    crate::execution_history_reset::RESET_TEMP_PURPOSE,
                )?
                .context("offline quota publication raced another owner")?;
            let publication = Arc::new(lillux_sqlite::RetainedOriginalWitnessFile::created(
                file,
                parent_identity,
                original_name,
            ));
            let registration = (|| -> Result<()> {
                let mut slot = self.reservation_publication.try_lock().map_err(|_| {
                    anyhow::anyhow!("original reservation publisher custody busy or poisoned")
                })?;
                ensure!(
                    slot.is_none(),
                    "original reservation publisher already retained"
                );
                *slot = Some(publication.clone());
                drop(slot);
                publication.identity_original()?;
                Ok(())
            })();
            registration.map_err(|error| {
                anyhow::Error::new(OriginalWitnessPublicationFailure {
                    error,
                    _owner: publication,
                })
            })?;
        }
        #[cfg(test)]
        if FAIL_AFTER_PUBLICATION.with(|slot| slot.replace(false)) {
            anyhow::bail!("injected quota acknowledgement failure after publication");
        }
        self.require_service_window()?;
        self.ledger
            .sync()
            .context("sync charged original offline quota reservation")?;
        self.require_service_window()?;
        Ok(())
    }
}

struct InspectionNamespaceFailure {
    error: anyhow::Error,
    // Keep the actual original issuer, exclusion and planned destination parent
    // alive even when mkdir/open acknowledgement is uncertain. Never replace it.
    work: Arc<OriginalCopyWork>,
    created_root: Option<Arc<lillux::PinnedDirectory>>,
    _production: lillux_sqlite::SqliteInspectionProductionPermit,
}
impl std::fmt::Debug for InspectionNamespaceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectionNamespaceFailure")
            .field("error", &self.error)
            .field("operation", &self.work.claim.record.operation_id)
            .field("has_created_root", &self.created_root.is_some())
            .finish()
    }
}
impl std::fmt::Display for InspectionNamespaceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for InspectionNamespaceFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[derive(Default)]
struct CopyUsage {
    admitted: [bool; 8],
    destinations: [bool; 2],
    sql_admitted: [bool; 2],
    sql_steps: u64,
    cleanup_steps: u64,
    destination_roots: [Option<Arc<lillux::PinnedDirectory>>; 2],
    // Original outputs are retained before any metadata/body operation. Files
    // have no back-reference to this claim, unlike SQL/copy work owners.
    destination_roles: [Option<Arc<lillux_sqlite::RetainedSqliteDestinationRole>>; 8],
    created_evidence: [Option<Arc<CreatedDirectoryEvidence>>; 2],
    retained_evidence_bytes: u64,
    copied_bytes: u64,
    retained_bytes: u64,
    read_bytes: u64,
    buffer_bytes: u64,
    native_storage_bytes: u64,
    resident_storage_bytes: u64,
    native_budget: Option<Arc<lillux_sqlite::OriginalNativeBudgetOwner>>,
}

/// Failed output registration preserves the actual File even before a fixed
/// slot can be selected. It grants no disposal and owns no work/claim cycle.
struct DestinationRoleRetentionFailure {
    error: anyhow::Error,
    _file: Arc<lillux_sqlite::RetainedSqliteDestinationRole>,
}
impl std::fmt::Debug for DestinationRoleRetentionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestinationRoleRetentionFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for DestinationRoleRetentionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for DestinationRoleRetentionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[derive(Debug)]
struct OriginalWitnessPublicationFailure {
    error: anyhow::Error,
    _owner: Arc<lillux_sqlite::RetainedOriginalWitnessFile>,
}
impl std::fmt::Display for OriginalWitnessPublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for OriginalWitnessPublicationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

struct CreatedDirectoryEvidence {
    signed: Vec<u8>,
    pin: Arc<lillux_sqlite::RetainedOriginalWitnessFile>,
    identity: std::sync::OnceLock<lillux::OpenFileIdentity>,
    acknowledged: AtomicBool,
}

struct CreatedEvidenceFailure {
    error: anyhow::Error,
    evidence: Arc<CreatedDirectoryEvidence>,
}
impl std::fmt::Debug for CreatedEvidenceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedEvidenceFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for CreatedEvidenceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original created evidence retained: {}", self.error)
    }
}
impl std::error::Error for CreatedEvidenceFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

struct OriginalCopyWork {
    outside_master: Mutex<Option<std::sync::Weak<OriginalFinalizerMaster>>>,
    claim: Arc<OriginalQuotaClaim>,
    state_lifetime: OriginalAccountStateLock,
    namespace: lillux::PinnedDirectoryLock,
    inspection_namespace: lillux::PinnedDirectoryLock,
    policy: ryeos_node_runtime::node_policy::NodePolicySnapshot,
    reservation_name: String,
    reservation_pin: Mutex<Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>>,
    reservation_identity: lillux::OpenFileIdentity,
}

enum OriginalSqlRootCustody {
    Owned(Arc<lillux::PinnedDirectory>),
    InFlight,
    Registered(lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration),
}
struct OriginalSqlWork {
    copy: Arc<OriginalCopyWork>,
    store: usize,
    root: std::mem::ManuallyDrop<Mutex<OriginalSqlRootCustody>>,
}
impl OriginalSqlWork {
    fn service_root(&self) -> Result<Arc<lillux::PinnedDirectory>> {
        ensure!(self.copy.claim.record.resident_process == std::process::id(), "foreign original SQL root");
        let root = self.root.try_lock().map_err(|_| anyhow::anyhow!("original SQL root custody busy or poisoned"))?;
        match &*root {
            OriginalSqlRootCustody::Owned(root) => Ok(root.clone()),
            _ => anyhow::bail!("original SQL root transferred; service re-entry refused"),
        }
    }
}
impl lillux_sqlite::SqliteInspectionSqlWork for OriginalSqlWork {
    unsafe fn register_original_outside_roots(&self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.copy.claim.record.resident_process == std::process::id(), "foreign original SQL outside root");
        leaf.require_original(factory, aggregate)?;
        self.copy.require_original_outside_master(factory, aggregate)?;
        lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| prepay(0, 4))?;
        let original = {
            let mut root = self.root.try_lock()
                .map_err(|_| anyhow::anyhow!("actual SQL root checkout busy or poisoned"))?;
            match &*root {
                OriginalSqlRootCustody::Registered(binding) => { registry.require_membership(binding)?; return Ok(()); }
                OriginalSqlRootCustody::InFlight => anyhow::bail!("actual SQL root already transferring"),
                OriginalSqlRootCustody::Owned(_) => {},
            }
            std::mem::replace(&mut *root, OriginalSqlRootCustody::InFlight)
        };
        let OriginalSqlRootCustody::Owned(original) = original else { unreachable!() };
        // SAFETY: actual paid SQL source owner from the SAME original claim.
        let result = unsafe { registry.register_directory(original, prepay) };
        let (custody, error) = match result {
            Ok(binding) => (OriginalSqlRootCustody::Registered(binding), None),
            Err((original, error)) => (OriginalSqlRootCustody::Owned(original), Some(error)),
        };
        *self.root.lock().unwrap_or_else(|p| p.into_inner()) = custody;
        if let Some(error) = error { return Err(error); }
        Ok(())
    }
    fn original_outside_roots_retired(&self) -> Result<bool> {
        ensure!(self.copy.claim.record.resident_process == std::process::id(), "foreign original SQL root receipt");
        let root = self.root.try_lock().map_err(|_| anyhow::anyhow!("actual SQL root receipt busy or poisoned"))?;
        Ok(matches!(&*root, OriginalSqlRootCustody::Registered(binding) if binding.retired()))
    }

    fn begin_native_production(
        &self,
        root: &lillux::PinnedDirectory,
        name: &OsStr,
    ) -> Result<lillux_sqlite::SqliteInspectionProductionPermit> {
        self.require_destination(root)?;
        ensure!(
            name == OsStr::new(&self.copy.claim.record.sources[self.store].database_name),
            "native producer changes original basename"
        );
        self.copy.claim.enter_production(
            OriginalProducerKind::Native(self.store),
            Some(self.copy.clone()),
        )
    }
    fn require_copy_work(
        &self,
        supplied: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    ) -> Result<()> {
        let original: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork> = self.copy.clone();
        ensure!(
            Arc::ptr_eq(&original, supplied),
            "SQL disposal selects foreign original copy work"
        );
        Ok(())
    }
    fn require_destination(&self, destination: &lillux::PinnedDirectory) -> Result<()> {
        self.checkpoint()?;
        destination.ensure_path_binding()?;
        ensure!(
            self.service_root()?.is_same_directory(destination)?,
            "SQL open selects foreign original destination"
        );
        Ok(())
    }
    fn checkpoint(&self) -> Result<()> {
        lillux_sqlite::SqliteInspectionCopyWork::checkpoint(self.copy.as_ref())?;
        self.service_root()?.ensure_path_binding()?;
        Ok(())
    }
    fn debit_vm_steps(&self, steps: u64) -> Result<()> {
        ensure!(steps > 0, "SQL work debit is empty");
        {
            let producer = self
                .copy
                .claim
                .production
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original native interval busy or poisoned"))?;
            ensure!(
                producer
                    .active
                    .values()
                    .any(|kind| *kind == OriginalProducerKind::Native(self.store)),
                "SQL work lacks active original native interval"
            );
        }
        // No filesystem/signature/CAS work under the counter mutex. This
        // immutable clock was authenticated once; never resample its start.
        ensure!(
            matches!(
                self.copy
                    .claim
                    .clock
                    .window(
                        &self.copy.claim.record.occupancy,
                        std::time::Duration::from_millis(
                            self.copy.claim.record.reserved.max_cleanup_millis
                        )
                    )
                    .map_err(anyhow::Error::msg)?,
                lillux::time::OccupancyWindowState::Service { .. }
            ),
            "original SQL service window expired; charge retained"
        );
        let mut used = self
            .copy
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original SQL VM work poisoned"))?;
        let total = used
            .sql_steps
            .checked_add(steps)
            .context("SQL VM debit overflow")?;
        ensure!(
            total <= self.copy.claim.record.reserved.max_sql_vm_steps,
            "original aggregate SQL VM limit exhausted"
        );
        used.sql_steps = total;
        Ok(())
    }
}

impl OriginalCopyWork {
    fn original_reservation_identity(&self) -> Result<lillux::OpenFileIdentity> {
        let pin = self
            .reservation_pin
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original reservation pin owner busy or poisoned"))?
            .clone()
            .context("issued reservation pin transferred; old copy custody refused")?;
        pin.observed_identity()
    }
    fn require_custody(&self) -> Result<()> {
        ensure!(
            !self.claim.finalizer_master_entered.load(Ordering::Acquire),
            "original finalizer master entered; old copy custody refused"
        );
        self.state_lifetime
            .ensure_protects_app_root(self.claim.app_root.path())?;
        self.namespace.ensure_protects(&self.claim.runtime)?;
        self.inspection_namespace
            .ensure_protects(&self.claim.copy_root)?;
        self.claim.copy_root.require_owner_private_directory()?;
        self.claim.app_root.ensure_path_binding()?;
        self.claim.runtime.ensure_path_binding()?;
        self.claim.ledger.ensure_path_binding()?;
        self.claim.copy_root.ensure_path_binding()?;
        ensure!(
            serde_json::to_string(&self.claim.copy_root.identity()?)?
                == self.claim.record.copy_root_identity,
            "issued copy destination parent changed"
        );
        for root in &self.claim._source_roots {
            root.ensure_path_binding()?;
        }
        ensure!(
            self.original_reservation_identity()? == self.reservation_identity,
            "charged reservation inode changed during copy work"
        );
        let current = self
            .claim
            .ledger
            .entry_no_follow(OsStr::new(&self.reservation_name))?
            .context("charged reservation disappeared during copy work")?;
        ensure!(
            current.entry_type == lillux::PinnedEntryType::Regular
                && current.containing_device == self.reservation_identity.device()
                && current.inode == self.reservation_identity.inode(),
            "charged reservation name changed"
        );
        ensure!(
            self.claim.publication_acknowledged.load(Ordering::Acquire)
                && self.policy.generation_digest() == self.claim.record.policy_digest
                && self
                    .policy
                    .require::<ryeos_scheduler_contract::NodeMaintenancePolicy>()?
                    .offline_reset
                    .as_ref()
                    == Some(&self.claim.record.reserved),
            "copy work lost its original policy/publication owner"
        );
        Ok(())
    }

    fn publish_created_evidence(&self, index: usize, root: &lillux::PinnedDirectory) -> Result<()> {
        lillux_sqlite::SqliteInspectionCopyWork::checkpoint(self)?;
        root.ensure_path_binding()?;
        let record = CreatedDirectoryRecord {
            schema: 1,
            purpose: "offline_reset_inspection_created".into(),
            signer: self.claim.record.signer.clone(),
            operation_id: self.claim.record.operation_id.clone(),
            claim_nonce: self.claim.record.claim_nonce.clone(),
            claim_sha256: lillux::sha256_hex(&self.claim.signed),
            policy_digest: self.claim.record.policy_digest.clone(),
            clock_binding: self.claim.record.clock_binding.clone(),
            source_purpose: self.claim.record.sources[index].purpose.clone(),
            destination_name: self.claim.record.destinations[index].directory_name.clone(),
            parent_identity: self.claim.copy_root.identity()?,
            created_identity: root.identity()?,
        };
        self.debit_cleanup_steps(1)?;
        let storage = lillux::cas::canonical_record_storage_quote(&record, MAX_RECORD_BYTES)?;
        self.claim.debit_original_record_working_storage(storage.working_bytes())?;
        let mut used = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("created evidence debit poisoned"))?;
        let retained = used
            .retained_evidence_bytes
            .checked_add(MAX_RECORD_BYTES as u64)
            .context("created evidence debit overflow")?;
        ensure!(
            retained
                .checked_add(self.claim.signed.len() as u64)
                .is_some_and(|bytes| bytes <= self.claim.record.reserved.max_recovery_record_bytes),
            "created evidence exceeds original recovery record reservation"
        );
        used.retained_evidence_bytes = retained;
        drop(used);
        self.claim.debit_original_record_working_storage(MAX_RECORD_BYTES as u64)?;
        let signed = record.signed_bytes(&self.claim.signer, &storage)?;
        let name = evidence_name(&record.destination_name);
        lillux_sqlite::SqliteInspectionCopyWork::checkpoint(self)?;
        let parent_identity = self.claim.copy_root.identity()?;
        let original_name = std::ffi::OsString::from(&name);
        let pin = self
            .claim
            .copy_root
            .atomic_create_regular(
                OsStr::new(&name),
                &signed,
                0o600,
                crate::execution_history_reset::RESET_TEMP_PURPOSE,
            )?
            .context("created evidence publication collided; original custody retained")?;
        // Retain the actual file returned by publication, never a later lookup.
        let evidence = Arc::new(CreatedDirectoryEvidence {
            signed,
            pin: Arc::new(lillux_sqlite::RetainedOriginalWitnessFile::created(
                pin,
                parent_identity,
                original_name,
            )),
            identity: std::sync::OnceLock::new(),
            acknowledged: AtomicBool::new(false),
        });
        let acknowledged = (|| -> Result<()> {
            let identity = evidence.pin.identity_original()?;
            evidence
                .identity
                .set(identity)
                .map_err(|_| anyhow::anyhow!("created evidence identity already set"))?;
            let mut used = self
                .claim
                .copy_usage
                .lock()
                .map_err(|_| anyhow::anyhow!("created evidence registration poisoned"))?;
            ensure!(
                used.created_evidence[index].is_none(),
                "original created evidence already registered"
            );
            used.created_evidence[index] = Some(evidence.clone());
            drop(used);
            #[cfg(test)]
            if FAIL_CREATED_EVIDENCE_ACK.with(|slot| slot.replace(false)) {
                anyhow::bail!("injected created evidence acknowledgement failure");
            }
            lillux_sqlite::SqliteInspectionCopyWork::checkpoint(self)?;
            self.claim.copy_root.sync()?;
            lillux_sqlite::SqliteInspectionCopyWork::checkpoint(self)?;
            let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
                &self.claim.copy_root,
                OsStr::new(&name),
                MAX_RECORD_BYTES,
            )?
            .context("created evidence disappeared")?;
            ensure!(
                bytes == evidence.signed
                    && CreatedDirectoryRecord::verified(&bytes, &self.claim.signer)? == record,
                "created evidence changed before acknowledgement"
            );
            ensure!(
                evidence.pin.identity_original()? == identity,
                "created evidence inode changed during acknowledgement"
            );
            self.verify_created_evidence_binding(index, root, &evidence)?;
            lillux_sqlite::SqliteInspectionCopyWork::checkpoint(self)?;
            evidence.acknowledged.store(true, Ordering::Release);
            Ok(())
        })();
        acknowledged.map_err(|error| anyhow::Error::new(CreatedEvidenceFailure { error, evidence }))
    }
    fn require_created_evidence(&self, index: usize, root: &lillux::PinnedDirectory) -> Result<()> {
        let evidence = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("created evidence witness poisoned"))?
            .created_evidence[index]
            .clone()
            .context("native body work lacks acknowledged created evidence")?;
        ensure!(
            evidence.acknowledged.load(Ordering::Acquire),
            "created evidence publication acknowledgement uncertain"
        );
        self.verify_created_evidence_binding(index, root, &evidence)
    }
    fn verify_created_evidence_binding(
        &self,
        index: usize,
        root: &lillux::PinnedDirectory,
        evidence: &CreatedDirectoryEvidence,
    ) -> Result<()> {
        let identity = evidence
            .identity
            .get()
            .context("created evidence original inode missing")?;
        ensure!(
            evidence.pin.identity_original()? == *identity,
            "created evidence inode mutated"
        );
        let name = evidence_name(&self.claim.record.destinations[index].directory_name);
        let current = self
            .claim
            .copy_root
            .entry_no_follow(OsStr::new(&name))?
            .context("created evidence name disappeared")?;
        ensure!(
            current.entry_type == lillux::PinnedEntryType::Regular
                && current.containing_device == identity.device()
                && current.inode == identity.inode(),
            "created evidence name selects another inode"
        );
        let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
            &self.claim.copy_root,
            OsStr::new(&name),
            MAX_RECORD_BYTES,
        )?
        .context("created evidence disappeared")?;
        ensure!(
            bytes == evidence.signed,
            "original created evidence bytes changed"
        );
        let after = self
            .claim
            .copy_root
            .entry_no_follow(OsStr::new(&name))?
            .context("created evidence name disappeared during read")?;
        ensure!(
            after.entry_type == current.entry_type
                && after.containing_device == current.containing_device
                && after.inode == current.inode
                && evidence.pin.identity_original()? == *identity,
            "created evidence inode changed during read"
        );
        let record = CreatedDirectoryRecord::verified(&bytes, &self.claim.signer)?;
        ensure!(
            record.created_identity == root.identity()?
                && record.parent_identity == self.claim.copy_root.identity()?,
            "created evidence selects another native root"
        );
        Ok(())
    }
}

impl lillux_sqlite::SqliteInspectionCopyWork for OriginalCopyWork {
    fn debit_original_outside_steps(&self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        steps: u64) -> Result<()> {
        self.require_original_outside_master(factory, aggregate)?;
        ensure!(steps > 0, "original outside inspection debit is empty");
        let master = self.outside_master.try_lock()
            .map_err(|_| anyhow::anyhow!("original inspection debit master busy or poisoned"))?
            .as_ref().and_then(std::sync::Weak::upgrade)
            .context("actual original inspection debit master absent")?;
        master.admission.debit_original(steps)
    }

    fn require_original_outside_master(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<()> {
        ensure!(self.claim.record.resident_process == std::process::id(),
            "copy outside master belongs to another original process");
        self.claim.require_destination_outside_transfer(factory, aggregate)?;
        let master = self.outside_master.try_lock()
            .map_err(|_| anyhow::anyhow!("copy outside master lookup busy or poisoned"))?
            .as_ref().and_then(std::sync::Weak::upgrade)
            .context("original copy outside master not registered")?;
        ensure!(std::ptr::eq(master.admission.original.as_ref(), self)
            && Arc::ptr_eq(&master.admission.claim, &self.claim),
            "copy selects another original outside master");
        // The lookup gate is released before original account verification.
        // This consumes the existing cleanup allowance, never service credit.
        master.checkpoint_original()
    }

    fn require_original_native_factory(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<()> {
        self.checkpoint()?;
        let original = self.claim.original_copy_issuer()?;
        ensure!(
            std::ptr::eq(original.as_ref(), self),
            "native inspection selects a foreign original copy issuer"
        );
        let issued = {
            self.claim
                .native_factory
                .try_lock()
                .map_err(|_| {
                    anyhow::anyhow!("original inspection native issuer lookup busy or poisoned")
                })?
                .as_ref()
                .context("original inspection native factory was not issued")?
                .upgrade()
                .context("original inspection native factory custody absent")?
        };
        ensure!(
            issued.same_original(factory),
            "inspection work and native factory have different original claims"
        );
        // Released lookup guard precedes authentic funding/State/namespace/
        // policy/publication checks. No SQL, ticket or renewed permit is issued.
        factory.require_runtime_binding(&self.claim.runtime)?;
        self.checkpoint()
    }
    fn begin_copy_production(
        &self,
        source: &lillux::PinnedDirectory,
        destination: &lillux::PinnedDirectory,
        name: &OsStr,
    ) -> Result<lillux_sqlite::SqliteInspectionProductionPermit> {
        self.checkpoint()?;
        let original = self.claim.original_copy_issuer()?;
        ensure!(
            std::ptr::eq(original.as_ref(), self),
            "foreign original copy producer"
        );
        let roots = self
            .claim
            .copy_usage
            .try_lock()
            .map_err(|_| anyhow::anyhow!("copy producer root snapshot busy or poisoned"))?
            .destination_roots
            .clone();
        let mut selected = None;
        for store in 0..2 {
            if let Some(root) = &roots[store] {
                if root.is_same_directory(destination)?
                    && self.claim._source_roots[store].is_same_directory(source)?
                {
                    let names = super::inspection_plan::role_names(
                        &self.claim.record.sources[store].database_name,
                    )?;
                    if let Some(role) = names.iter().position(|candidate| candidate == name) {
                        ensure!(
                            selected.replace(store * 4 + role).is_none(),
                            "ambiguous original copy interval"
                        );
                    }
                }
            }
        }
        self.claim.enter_production(
            OriginalProducerKind::Copy(
                selected.context("copy producer selects foreign original coordinate")?,
            ),
            Some(original),
        )
    }
    fn retain_destination_role(
        &self,
        destination: &lillux::PinnedDirectory,
        name: &OsStr,
        file: Arc<lillux_sqlite::RetainedSqliteDestinationRole>,
    ) -> Result<()> {
        // Even coordinate selection/poison/duplicate failures keep this exact
        // output, rather than closing a partial body or replacing an old pin.
        let retained = (|| -> Result<()> {
            let mut used = self
                .claim
                .copy_usage
                .lock()
                .map_err(|_| anyhow::anyhow!("original destination role custody poisoned"))?;
            let mut selected = None;
            for (store, source) in self.claim.record.sources.iter().enumerate() {
                let Some(root) = &used.destination_roots[store] else {
                    continue;
                };
                // Coordinate selection only; physical root validation follows
                // retention and may refuse without losing the created file.
                if root.path() != destination.path() {
                    continue;
                }
                for (role, candidate) in super::inspection_plan::role_names(&source.database_name)?
                    .iter()
                    .enumerate()
                {
                    if candidate == name {
                        ensure!(
                            selected.replace(store * 4 + role).is_none(),
                            "ambiguous original output coordinate"
                        );
                    }
                }
            }
            let selected =
                selected.context("output is outside original fixed destination roles")?;
            ensure!(
                used.destination_roles[selected].is_none(),
                "original destination role already retained"
            );
            used.destination_roles[selected] = Some(file.clone());
            let expected_root = used.destination_roots[selected / 4]
                .clone()
                .context("retained output lacks original destination")?;
            let admitted = used.admitted[selected];
            drop(used);
            self.checkpoint()?;
            self.require_created_evidence(selected / 4, &expected_root)?;
            expected_root.ensure_path_binding()?;
            destination.ensure_path_binding()?;
            ensure!(
                expected_root.is_same_directory(destination)?
                    && admitted
                    && self.claim.record.sources[selected / 4].roles[selected % 4].is_some(),
                "output lacks its original admitted destination role"
            );
            file.require_original(destination, name)?;
            let entry = destination
                .entry_no_follow(name)?
                .context("created output disappeared")?;
            let metadata = file.metadata_original()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.is_file()
                        && metadata.len() == 0
                        && metadata.nlink() == 1
                        && entry.entry_type == lillux::PinnedEntryType::Regular
                        && entry.containing_device == metadata.dev()
                        && entry.inode == metadata.ino(),
                    "retained output is not its original exclusive empty regular inode"
                );
            }
            #[cfg(not(unix))]
            {
                let _ = (metadata, entry);
                anyhow::bail!("original output identity is unavailable");
            }
            self.checkpoint()
        })();
        retained.map_err(|error| {
            anyhow::Error::new(DestinationRoleRetentionFailure { error, _file: file })
        })
    }

    fn issue_sql_work(
        self: Arc<Self>,
        destination: &lillux::PinnedDirectory,
    ) -> Result<Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>> {
        self.checkpoint()?;
        destination.ensure_path_binding()?;
        let roots = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original SQL destination witness poisoned"))?
            .destination_roots
            .clone();
        let mut selected = None;
        for (index, root) in roots.iter().enumerate() {
            if let Some(root) = root {
                root.ensure_path_binding()?;
                if root.is_same_directory(destination)? {
                    ensure!(
                        selected.replace(index).is_none(),
                        "ambiguous original SQL destination"
                    );
                }
            }
        }
        let index = selected.context("SQL inspection selects foreign destination")?;
        self.require_created_evidence(index, destination)?;
        let production = self
            .claim
            .enter_production(OriginalProducerKind::SqlIssue(index), Some(self.clone()))?;
        let mut used = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original SQL work admission poisoned"))?;
        ensure!(
            !used.sql_admitted[index],
            "original SQL inspection already issued; recovery required"
        );
        ensure!(
            used.admitted[index * 4..index * 4 + 4]
                .iter()
                .all(|charged| *charged),
            "SQL inspection precedes original fixed copy role admission"
        );
        let controls = u64::try_from(std::mem::size_of::<OriginalSqlWork>() + 2 * std::mem::size_of::<usize>())?;
        let buffers = used.buffer_bytes.checked_add(controls).context("original SQL owner storage debit overflow")?;
        ensure!(buffers.checked_add(used.native_storage_bytes)
            .and_then(|n| n.checked_add(used.resident_storage_bytes))
            .is_some_and(|n| n <= self.claim.record.reserved.max_memory_bytes),
            "original SQL owner exceeds same reserved host memory");
        used.buffer_bytes = buffers;
        used.sql_admitted[index] = true;
        drop(used);
        production.acknowledge_synchronous()?;
        Ok(Arc::new(OriginalSqlWork {
            store: index,
            copy: self,
            root: std::mem::ManuallyDrop::new(Mutex::new(OriginalSqlRootCustody::Owned(roots[index].as_ref().unwrap().clone()))),
        }))
    }

    fn create_destination(
        self: Arc<Self>,
        source: &lillux::PinnedDirectory,
        database_name: &OsStr,
    ) -> Result<lillux_sqlite::SqliteInspectionCopyDestination> {
        self.checkpoint()?;
        let source_identity = serde_json::to_string(&source.identity()?)?;
        let store = self
            .claim
            .record
            .sources
            .iter()
            .position(|plan| {
                plan.directory_identity == source_identity
                    && OsStr::new(&plan.database_name) == database_name
            })
            .context("namespace producer selects foreign original store")?;
        let production = self
            .claim
            .enter_production(OriginalProducerKind::Namespace(store), Some(self.clone()))?;
        let mut created_root = None;
        let allocated = (|| -> Result<lillux_sqlite::SqliteInspectionCopyDestination> {
            self.checkpoint()?;
            let source_identity = serde_json::to_string(&source.identity()?)?;
            let index = self
                .claim
                .record
                .sources
                .iter()
                .position(|plan| {
                    plan.directory_identity == source_identity
                        && OsStr::new(&plan.database_name) == database_name
                })
                .context("inspection allocation selects another original store")?;
            self.claim.record.sources[index].require_current(source)?;
            // Clone the parent BEFORE creation; do not clone the new root FD.
            // The same inspection-parent lock excludes cooperating writers
            // through mkdir/open/sync. Native post-mkdir errors retain the
            // original parent/opened child; no hostile-host exclusion claimed.
            let parent = self.claim.copy_root.try_clone()?;
            let name =
                std::ffi::OsString::from(&self.claim.record.destinations[index].directory_name);
            let mut used = self
                .claim
                .copy_usage
                .lock()
                .map_err(|_| anyhow::anyhow!("original copy allocation is poisoned"))?;
            ensure!(
                !used.destinations[index],
                "original inspection namespace already allocated; recovery required"
            );
            used.destinations[index] = true;
            drop(used);
            self.checkpoint()?;
            let root = Arc::new(
                parent
                    .create_child(&name, 0o700)
                    .context("create original signed inspection destination")?,
            );
            created_root = Some(root.clone());
            let mut used =
                self.claim.copy_usage.lock().map_err(|_| {
                    anyhow::anyhow!("original destination registration is poisoned")
                })?;
            ensure!(
                used.destination_roots[index].is_none(),
                "original destination witness already exists"
            );
            used.destination_roots[index] = Some(root.clone());
            drop(used);
            self.publish_created_evidence(index, &root)?;
            Ok(lillux_sqlite::SqliteInspectionCopyDestination { parent, name, root })
        })();
        let allocated = allocated.and_then(|destination| {
            production.acknowledge_synchronous()?;
            Ok(destination)
        });
        allocated.map_err(|error| {
            anyhow::Error::new(InspectionNamespaceFailure {
                error,
                work: self,
                created_root,
                _production: production,
            })
        })
    }

    fn checkpoint(&self) -> Result<()> {
        self.require_custody()?;
        self.claim.require_service_window()?;
        Ok(())
    }

    fn cleanup_checkpoint(&self) -> Result<std::time::Duration> {
        self.require_custody()?;
        self.claim.record.validate()?;
        ensure!(
            self.claim.record.clock_binding == self.claim.clock.binding_digest(),
            "original cleanup clock differs from signed reservation"
        );
        let allowance =
            std::time::Duration::from_millis(self.claim.record.reserved.max_cleanup_millis);
        let window = self
            .claim
            .clock
            .window(&self.claim.record.occupancy, allowance)
            .map_err(anyhow::Error::msg)?;
        original_cleanup_remaining(window, allowance)
    }

    fn remaining_cleanup_steps(&self) -> Result<u64> {
        self.cleanup_checkpoint()?;
        let used = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original cleanup budget poisoned"))?;
        self.claim
            .record
            .reserved
            .max_cleanup_steps
            .checked_sub(used.cleanup_steps)
            .context("original cleanup counter exceeds reservation")
    }

    fn debit_destination_retirement(&self, maximum: u32) -> Result<()> {
        ensure!(
            maximum > 0 && maximum <= i32::MAX as u32,
            "invalid native retirement bound"
        );
        let steps = u64::from(maximum)
            .checked_add(1)
            .context("retirement debit overflow")?;
        lillux_sqlite::SqliteInspectionCopyWork::debit_cleanup_steps(self, steps)
    }

    fn debit_cleanup_steps(&self, steps: u64) -> Result<()> {
        self.cleanup_checkpoint()?;
        ensure!(steps > 0, "original cleanup debit is empty");
        // No filesystem/signature/clock work under the shared counter lock.
        let mut used = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original cleanup debit poisoned"))?;
        let total = used
            .cleanup_steps
            .checked_add(steps)
            .context("original cleanup debit overflow")?;
        ensure!(
            total <= self.claim.record.reserved.max_cleanup_steps,
            "original aggregate cleanup steps exhausted; charge remains retained"
        );
        used.cleanup_steps = total;
        Ok(())
    }

    fn retained_destination_role(
        &self,
        destination: &lillux::PinnedDirectory,
        name: &OsStr,
    ) -> Result<Option<Arc<lillux_sqlite::RetainedSqliteDestinationRole>>> {
        self.cleanup_checkpoint()?;
        let roots = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original cleanup role custody poisoned"))?
            .destination_roots
            .clone();
        let mut selected = None;
        for (store, root) in roots.iter().enumerate() {
            let Some(root) = root else { continue };
            if !root.is_same_directory(destination)? {
                continue;
            }
            for (role, candidate) in
                super::inspection_plan::role_names(&self.claim.record.sources[store].database_name)?
                    .iter()
                    .enumerate()
            {
                if candidate == name {
                    ensure!(
                        selected.replace(store * 4 + role).is_none(),
                        "ambiguous original cleanup role"
                    );
                }
            }
        }
        let selected =
            selected.context("cleanup role selects foreign original root or fixed name")?;
        destination.ensure_path_binding()?;
        self.require_created_evidence(
            selected / 4,
            roots[selected / 4]
                .as_ref()
                .expect("selected registered root"),
        )?;
        let used = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original cleanup pin custody poisoned"))?;
        ensure!(
            used.admitted[selected],
            "cleanup role was never originally admitted"
        );
        let file = used.destination_roles[selected].clone();
        let present = self.claim.record.sources[selected / 4].roles[selected % 4].is_some();
        ensure!(
            file.is_some() == present,
            "cleanup role lacks original present pin or retains unexpected absent pin"
        );
        drop(used);
        self.cleanup_checkpoint()?;
        // Return the SAME File; no lookup/reopen of the copied role, no refund.
        Ok(file)
    }

    fn remaining_service_window(&self) -> Result<std::time::Duration> {
        self.checkpoint()?;
        self.claim.require_service_window()
    }

    fn admit_role(
        &self,
        source: &lillux::PinnedDirectory,
        destination: &lillux::PinnedDirectory,
        name: &OsStr,
        original: Option<&lillux_sqlite::SqliteRoleIdentity>,
        maximum_read_bytes: u64,
        working_memory_bytes: u64,
    ) -> Result<()> {
        self.checkpoint()?;
        let mut selected = None;
        for (store, plan) in self.claim.record.sources.iter().enumerate() {
            if serde_json::to_string(&source.identity()?)? == plan.directory_identity
                && super::inspection_plan::role_names(&plan.database_name)?
                    .iter()
                    .any(|candidate| candidate == name)
            {
                let role = plan.require_role(source, name, original)?;
                ensure!(
                    selected.replace(store * 4 + role).is_none(),
                    "ambiguous original copy role"
                );
            }
        }
        let selected = selected.context("native copy is outside original signed source plan")?;
        let expected_destination = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original destination witness is poisoned"))?
            .destination_roots[selected / 4]
            .clone()
            .context("copy role lacks its original allocated destination")?;
        self.require_created_evidence(selected / 4, &expected_destination)?;
        expected_destination.ensure_path_binding()?;
        destination.ensure_path_binding()?;
        ensure!(
            expected_destination.is_same_directory(destination)?,
            "native copy selects a foreign destination allocation"
        );
        let bytes = original.map_or(0, |role| role.byte_length);
        let expected_read = if original.is_some() {
            bytes
                .checked_mul(2)
                .and_then(|n| n.checked_add(1))
                .context("copy read debit overflow")?
        } else {
            0
        };
        ensure!(
            maximum_read_bytes == expected_read
                && working_memory_bytes == if original.is_some() { 65_536 } else { 0 },
            "native copy work shape differs from bounded original request"
        );
        let mut used = self
            .claim
            .copy_usage
            .lock()
            .map_err(|_| anyhow::anyhow!("original copy debit is poisoned"))?;
        ensure!(
            !used.admitted[selected],
            "original copy role already charged; no retry refund"
        );
        let copied = used
            .copied_bytes
            .checked_add(bytes)
            .context("copy debit overflow")?;
        let retained = used
            .retained_bytes
            .checked_add(bytes)
            .context("retained copy debit overflow")?;
        let read = used
            .read_bytes
            .checked_add(maximum_read_bytes)
            .context("read debit overflow")?;
        let buffers = used
            .buffer_bytes
            .checked_add(working_memory_bytes)
            .context("copy buffer debit overflow")?;
        let quota = &self.claim.record.reserved;
        ensure!(
            copied <= quota.max_copied_bytes
                && retained <= quota.max_retained_destination_bytes
                && read <= quota.max_read_bytes
                && buffers
                    .checked_add(used.native_storage_bytes)
                    .and_then(|bytes| bytes.checked_add(used.resident_storage_bytes))
                    .is_some_and(|total| total <= quota.max_memory_bytes),
            "original copy resource reservation exhausted"
        );
        // The full envelope was durably charged before issue. These irreversible
        // same-owner subdebits prohibit warm replay; cold intake never recreates
        // this owner. Failure or Drop does not refund any subdebit/envelope.
        used.admitted[selected] = true;
        used.copied_bytes = copied;
        used.retained_bytes = retained;
        used.read_bytes = read;
        used.buffer_bytes = buffers;
        drop(used);
        self.checkpoint()
    }
}

fn require_operation_suffix(operation: &str) -> Result<()> {
    ensure!(
        operation.len() == 32
            && operation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "malformed original settlement coordinate"
    );
    Ok(())
}
fn require_disposition_claim(
    disposition: &super::creation_evidence::InspectionDispositionRecord,
    claim: &ClaimRecord,
    hash: &str,
) -> Result<()> {
    ensure!(
        disposition.operation_id == claim.operation_id
            && disposition.claim_nonce == claim.claim_nonce
            && disposition.claim_sha256 == hash
            && disposition.signer == claim.signer
            && disposition.policy_digest == claim.policy_digest
            && disposition.clock_binding == claim.clock_binding
            && disposition.inspection == claim.inspection
            && serde_json::to_string(&disposition.parent_identity)? == claim.copy_root_identity,
        "disposition census selects foreign original claim/mode/clock"
    );
    disposition.native_quota.require_quota(&claim.reserved)?;
    for (index, store) in disposition.stores.iter().enumerate() {
        let present =
            std::array::from_fn::<_, 4, _>(|role| claim.sources[index].roles[role].is_some());
        match store {
            super::creation_evidence::InspectionStoreDispositionRecord::NeverStarted {
                original_present,
                ..
            } => ensure!(
                *original_present == present,
                "never-started census changes original source selection"
            ),
            super::creation_evidence::InspectionStoreDispositionRecord::Removed {
                retired_outputs,
                ..
            } => ensure!(
                std::array::from_fn::<_, 4, _>(|role| retired_outputs[role].is_some()) == present,
                "retired census changes original role selection"
            ),
        }
    }
    Ok(())
}

fn original_cleanup_remaining(
    window: lillux::time::OccupancyWindowState,
    allowance: std::time::Duration,
) -> Result<std::time::Duration> {
    match window {
        lillux::time::OccupancyWindowState::Service { remaining } => remaining
            .checked_add(allowance)
            .context("original cleanup remaining interval overflow"),
        lillux::time::OccupancyWindowState::Cleanup { remaining } => Ok(remaining),
        lillux::time::OccupancyWindowState::Expired => {
            anyhow::bail!("original cleanup window expired; charge remains retained")
        }
    }
}

fn validate_aggregate(records: &[OfflineResetQuota], quota: &OfflineResetQuota) -> Result<()> {
    quota.validate()?;
    ensure!(
        records.len() <= quota.max_retained_operations as usize,
        "offline retained operation quota exhausted"
    );
    let mut roles = 0u64;
    let mut totals = [0u64; 10];
    for record in records {
        record.validate()?;
        roles = roles
            .checked_add(u64::from(record.max_source_roles))
            .context("offline role quota overflow")?;
        for (total, debit) in totals.iter_mut().zip([
            record.max_copied_bytes,
            record.max_retained_destination_bytes,
            record.max_read_bytes,
            record.max_sql_vm_steps,
            record.max_cleanup_steps,
            record.max_memory_bytes,
            record.max_recovery_record_bytes,
            record.max_native_storage_bytes,
            record.max_native_allocator_steps,
            u64::from(record.max_native_connections),
        ]) {
            *total = total
                .checked_add(debit)
                .context("offline aggregate quota overflow")?;
        }
    }
    ensure!(
        roles <= u64::from(quota.max_source_roles),
        "offline source-role quota exhausted"
    );
    for (used, limit) in totals.into_iter().zip([
        quota.max_copied_bytes,
        quota.max_retained_destination_bytes,
        quota.max_read_bytes,
        quota.max_sql_vm_steps,
        quota.max_cleanup_steps,
        quota.max_memory_bytes,
        quota.max_recovery_record_bytes,
        quota.max_native_storage_bytes,
        quota.max_native_allocator_steps,
        u64::from(quota.max_native_connections),
    ]) {
        ensure!(used <= limit, "offline aggregate resource quota exhausted");
    }
    Ok(())
}

#[cfg(test)]
mod clock_controls {
    use super::*;
    use lillux::PinnedDirectory;

    struct SealDuringPhysicalCopy {
        claim: Arc<OriginalQuotaClaim>,
        inner: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        checkpoints: std::sync::atomic::AtomicUsize,
        observed: AtomicBool,
    }
    impl lillux_sqlite::SqliteInspectionCopyWork for SealDuringPhysicalCopy {
        fn require_original_native_factory(
            &self,
            factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        ) -> Result<()> {
            self.inner.require_original_native_factory(factory)
        }
        fn begin_copy_production(
            &self,
            source: &PinnedDirectory,
            destination: &PinnedDirectory,
            name: &OsStr,
        ) -> Result<lillux_sqlite::SqliteInspectionProductionPermit> {
            self.inner.begin_copy_production(source, destination, name)
        }
        fn issue_sql_work(
            self: Arc<Self>,
            destination: &PinnedDirectory,
        ) -> Result<Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>> {
            self.inner.clone().issue_sql_work(destination)
        }
        fn create_destination(
            self: Arc<Self>,
            source: &PinnedDirectory,
            database_name: &OsStr,
        ) -> Result<lillux_sqlite::SqliteInspectionCopyDestination> {
            self.inner.clone().create_destination(source, database_name)
        }
        fn admit_role(
            &self,
            source: &PinnedDirectory,
            destination: &PinnedDirectory,
            name: &OsStr,
            original: Option<&lillux_sqlite::SqliteRoleIdentity>,
            maximum_read_bytes: u64,
            working_memory_bytes: u64,
        ) -> Result<()> {
            self.inner.admit_role(
                source,
                destination,
                name,
                original,
                maximum_read_bytes,
                working_memory_bytes,
            )
        }
        fn retain_destination_role(
            &self,
            destination: &PinnedDirectory,
            name: &OsStr,
            file: Arc<lillux_sqlite::RetainedSqliteDestinationRole>,
        ) -> Result<()> {
            self.inner.retain_destination_role(destination, name, file)
        }
        fn cleanup_checkpoint(&self) -> Result<std::time::Duration> {
            self.inner.cleanup_checkpoint()
        }
        fn remaining_cleanup_steps(&self) -> Result<u64> {
            self.inner.remaining_cleanup_steps()
        }
        fn debit_destination_retirement(&self, max_identity_checks: u32) -> Result<()> {
            self.inner.debit_destination_retirement(max_identity_checks)
        }
        fn debit_cleanup_steps(&self, steps: u64) -> Result<()> {
            self.inner.debit_cleanup_steps(steps)
        }
        fn retained_destination_role(
            &self,
            destination: &PinnedDirectory,
            name: &OsStr,
        ) -> Result<Option<Arc<lillux_sqlite::RetainedSqliteDestinationRole>>> {
            self.inner.retained_destination_role(destination, name)
        }
        fn remaining_service_window(&self) -> Result<std::time::Duration> {
            self.inner.remaining_service_window()
        }
        fn checkpoint(&self) -> Result<()> {
            // At the fourth copier checkpoint the actual output has already
            // been retained, before its first body write. Admission/debits use
            // the genuine issuer; this proxy adds only the competing seal.
            if self.checkpoints.fetch_add(1, Ordering::SeqCst) == 3 {
                assert!(self.claim.seal_production(&self.inner).is_err());
                self.observed.store(true, Ordering::SeqCst);
            }
            self.inner.checkpoint()
        }
    }

    // Genuine signed issuer controls. Authored source only; execution UNRUN.
    #[test]
    fn genuine_native_interval_blocks_seal_until_original_closed_ack() {
        let tmp = tempfile::tempdir().unwrap();
        let name = OsStr::new("runtime.sqlite3");
        let database = rusqlite::Connection::open(tmp.path().join(name)).unwrap();
        database
            .execute_batch("CREATE TABLE sample(value INTEGER); INSERT INTO sample VALUES(19)")
            .unwrap();
        database.close().unwrap();
        let source = PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_lock, claim, work) = crate::execution_history_reset::fixture_inspection_claim(
            &source,
            &namespace,
            &tmp.path().join(name),
        );
        let (copy, lease) = crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            name,
            &namespace,
            work.clone(),
        )
        .unwrap();
        let sql = work.clone().issue_sql_work(&copy).unwrap();
        let root = Arc::new(copy);
        let native_lease: lillux_sqlite::SqliteCopyLease = lease;
        let owner = lillux_sqlite::ReadonlySqliteCopy::open(
            root.clone(),
            name,
            native_lease.clone(),
            sql.clone(),
        )
        .unwrap();
        assert!(claim.seal_production(&work).is_err());
        assert_eq!(
            owner
                .connection()
                .query_row("SELECT value FROM sample", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            19
        );
        let closed = owner.close().unwrap();
        assert!(
            claim.seal_production(&work).is_err(),
            "close alone does not acknowledge original producer handoff"
        );
        closed
            .require_original(&root, name, &native_lease, &sql)
            .unwrap();
        closed.acknowledge_producer_terminal().unwrap();
        let seal = claim.seal_production(&work).unwrap();
        assert!(!seal.slot(0).unwrap().never_started);
        assert!(seal.slot(1).unwrap().never_started);
        assert!(sql.begin_native_production(&root, name).is_err());
        assert!(
            claim.begin_inspection_settlement(seal, Vec::new()).is_err(),
            "attempted store cannot be replaced by an empty disposal aggregate"
        );
        assert!(root.entry_no_follow(name).unwrap().is_some());
    }

    #[test]
    fn genuine_copy_interval_drains_without_renewal_and_missing_aggregate_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let name = OsStr::new("runtime.sqlite3");
        std::fs::write(tmp.path().join(name), b"original bytes").unwrap();
        let source = PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_lock, claim, work) = crate::execution_history_reset::fixture_inspection_claim(
            &source,
            &namespace,
            &tmp.path().join(name),
        );
        let destination = work.clone().create_destination(&source, name).unwrap();
        let proxy = SealDuringPhysicalCopy {
            claim: claim.clone(),
            inner: work.clone(),
            checkpoints: std::sync::atomic::AtomicUsize::new(0),
            observed: AtomicBool::new(false),
        };
        let receipt = lillux_sqlite::copy_regular_without_sqlite_alias(
            &source,
            name,
            &destination.root,
            4096,
            &proxy,
        )
        .unwrap()
        .unwrap();
        assert!(proxy.observed.load(Ordering::SeqCst));
        assert_eq!(
            std::fs::read(destination.root.path().join(name)).unwrap(),
            b"original bytes"
        );
        let retained = work
            .retained_destination_role(&destination.root, name)
            .unwrap()
            .unwrap();
        retained.metadata_original().unwrap();
        let _actual_receipt = receipt;
        assert!(
            work.clone()
                .create_destination(&source, OsStr::new("scheduler.sqlite3"))
                .is_err()
        );
        let seal = claim.seal_production(&work).unwrap();
        assert!(!seal.slot(0).unwrap().never_started);
        assert!(claim.begin_inspection_settlement(seal, Vec::new()).is_err());
        assert_eq!(
            std::fs::read(tmp.path().join(name)).unwrap(),
            b"original bytes"
        );
    }

    #[test]
    fn genuine_never_started_seal_rejects_foreign_issuer_and_retains_charge() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let a = PinnedDirectory::open(first.path()).unwrap().unwrap();
        let b = PinnedDirectory::open(second.path()).unwrap().unwrap();
        let al = a.lock_exclusive().unwrap();
        let bl = b.lock_exclusive().unwrap();
        let (_state_a, claim, work) = crate::execution_history_reset::fixture_inspection_claim(
            &a,
            &al,
            &first.path().join("runtime.sqlite3"),
        );
        let (_state_b, other, foreign) = crate::execution_history_reset::fixture_inspection_claim(
            &b,
            &bl,
            &second.path().join("runtime.sqlite3"),
        );
        assert!(claim.seal_production(&foreign).is_err());
        let seal = claim.seal_production(&work).unwrap();
        assert!(seal.slot(0).unwrap().never_started && seal.slot(1).unwrap().never_started);
        let mut config =
            ryeos_node_runtime::config::Config::load(&ryeos_node_runtime::config::ConfigSources {
                app_root: Some(first.path().to_path_buf()),
                ..Default::default()
            })
            .unwrap();
        config.db_path = first.path().join("runtime.sqlite3");
        let identity = NodeIdentity::load(&config.node_signing_key_path).unwrap();
        let policy = OfflineResetPolicyOwner::load(&config, &identity, &_state_a, &a, &al).unwrap();
        assert!(
            claim.issue_copy_work(&policy, &identity).is_err(),
            "sealed original issuer cannot be reissued"
        );
        drop(policy);
        assert!(
            other
                .begin_inspection_settlement(seal.clone(), Vec::new())
                .is_err()
        );
        let settlement = claim.begin_inspection_settlement(seal, Vec::new()).unwrap();
        settlement.disposition_record().unwrap();
        settlement.dispose_witnesses().unwrap();
        settlement.prepare_terminal().unwrap();
        assert!(settlement.require_completed().is_err());
        assert!(
            first
                .path()
                .join(".ai/node/offline-reset-quota")
                .join(format!("reserved.{}", claim.operation_id()))
                .exists()
        );
        assert!(
            work.clone()
                .create_destination(&a, OsStr::new("runtime.sqlite3"))
                .is_err()
        );
    }

    // Wire/clock controls only. These unsigned records are never issuer inputs
    // and never grant acquisition or stand in for the real publication fixture.
    fn record() -> ClaimRecord {
        let reserved = OfflineResetQuota {
            max_retained_operations: 4,
            max_source_roles: ryeos_scheduler_contract::OFFLINE_RESET_SOURCE_ROLES,
            max_copied_bytes: 268435456,
            max_retained_destination_bytes: 536870912,
            max_read_bytes: 2147483648,
            max_sql_vm_steps: 20000000,
            max_memory_bytes: 67108864,
            max_native_storage_bytes: 50331648,
            max_native_allocator_steps: 1048576,
            max_native_connections: 64,
            max_operation_millis: 300000,
            max_cleanup_millis: 60000,
            max_cleanup_steps: 1024,
            max_recovery_record_bytes: 2097152,
        };
        let occupancy = lillux::time::OccupancyLimit::new(
            lillux::time::occupancy_now().unwrap(),
            occupancy_nanoseconds(&reserved).unwrap(),
        )
        .unwrap();
        ClaimRecord {
            schema: 9,
            purpose: "offline_reset_local_quota_reservation_v9".into(),
            resident_process: 1, resident_generation: 1,
            resident_bytes: lillux_sqlite::original_host_owner::backing::HostStorage::quote_native_region().unwrap().bytes() as u64,
            operation_id: "a".repeat(32),
            copy_root_identity: "{}".into(),
            destinations: destination_plans(&"a".repeat(32)),
            inspection: OriginalInspectionPlan {
                mode: OriginalInspectionMode::DryRun,
                scheduler_selected: false,
                accounting: [
                    InspectionAccountingRequirement::Required,
                    InspectionAccountingRequirement::Absent,
                ],
            },
            claim_nonce: "b".repeat(32),
            signer: "wire-only".into(),
            policy_digest: "c".repeat(64),
            app_root_identity: "{}".into(),
            runtime_identity: "{}".into(),
            prior_operation: None,
            reserved,
            occupancy,
            clock_binding: "f".repeat(64),
            operational_source: serde_json::from_value(serde_json::json!({
                "directory_identity":"{}",
                "roles":[{"device":1,"inode":2,"byte_length":0},null,null,null],
                "marker":{"device":1,"inode":3,"byte_length":
                    ryeos_state::operational::OPERATIONAL_INITIALIZED_CONTENT.len()}
            }))
            .unwrap(),
            sources: [
                InspectionSourcePlan {
                    purpose: "runtime".into(),
                    directory_identity: "{}".into(),
                    database_name: "runtime.sqlite3".into(),
                    roles: std::array::from_fn(|_| None),
                },
                InspectionSourcePlan {
                    purpose: "scheduler".into(),
                    directory_identity: "{}".into(),
                    database_name: "scheduler.sqlite3".into(),
                    roles: std::array::from_fn(|_| None),
                },
            ],
        }
    }

    #[test]
    fn native_dimensions_are_required_and_previous_claim_refuses() {
        let original = record();
        original.validate().unwrap();
        for field in [
            "max_native_storage_bytes",
            "max_native_allocator_steps",
            "max_native_connections",
        ] {
            let mut missing = serde_json::to_value(&original).unwrap();
            missing["reserved"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<ClaimRecord>(missing).is_err());
        }
        for field in ["resident_process", "resident_generation", "resident_bytes"] {
            let mut missing = serde_json::to_value(&original).unwrap();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<ClaimRecord>(missing).is_err());
        }
        let mut wrong = original.clone();
        wrong.resident_generation = 0;
        assert!(wrong.validate().is_err());
        let mut wrong = original.clone();
        wrong.reserved.max_memory_bytes = wrong.reserved.max_native_storage_bytes;
        assert!(wrong.validate().is_err(), "native arena cannot consume the resident graph's memory debit");
        let mut missing = serde_json::to_value(&original).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("operational_source");
        assert!(serde_json::from_value::<ClaimRecord>(missing).is_err());
        let mut insufficient = original.clone();
        insufficient.reserved.max_source_roles = 8;
        assert!(insufficient.validate().is_err());
        let mut old = original;
        old.schema = 7;
        old.purpose = "offline_reset_local_quota_reservation_v7".into();
        assert!(old.validate().is_err());
    }

    #[test]
    fn genuine_native_subreservation_shares_original_counters_and_retains_pending_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let source = PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (state, claim, work) = crate::execution_history_reset::fixture_inspection_claim(
            &source,
            &namespace,
            &tmp.path().join("runtime.sqlite3"),
        );
        let mut config =
            ryeos_node_runtime::config::Config::load(&ryeos_node_runtime::config::ConfigSources {
                app_root: Some(tmp.path().to_path_buf()),
                ..Default::default()
            })
            .unwrap();
        config.db_path = tmp.path().join("runtime.sqlite3");
        let identity = NodeIdentity::load(&config.node_signing_key_path).unwrap();
        let policy =
            OfflineResetPolicyOwner::load(&config, &identity, &state, &source, &namespace).unwrap();
        let limits = claim.record.reserved.clone();
        assert!(
            claim
                .issue_native_quota_admission(&policy, &identity, 0, 8)
                .is_err()
        );
        let admission = claim
            .issue_native_quota_admission(&policy, &identity, limits.max_native_storage_bytes, 8 + HOST_RELEASE_ATTEMPTS)
            .unwrap();
        assert!(
            claim
                .issue_native_quota_admission(
                    &policy,
                    &identity,
                    limits.max_native_storage_bytes,
                    8 + HOST_RELEASE_ATTEMPTS
                )
                .is_err()
        );
        {
            let issuer = admission._production.lock().unwrap();
            let issuer = issuer.as_ref().unwrap();
            assert_eq!(issuer.kind, OriginalProducerKind::NativeQuotaIssuer);
            assert!(!issuer.completed.load(Ordering::Acquire));
        }
        let cleanup_reserved = 8 + HOST_RELEASE_ATTEMPTS;
        let remaining_service = limits.max_native_allocator_steps - cleanup_reserved;
        let preparation_steps = admission.budget.snapshot().unwrap().allocator_steps;
        admission.debit_allocator_steps(remaining_service - preparation_steps).unwrap();
        assert!(admission.debit_allocator_steps(1).is_err());
        admission.debit_allocator_cleanup_steps(8).unwrap();
        assert!(admission.debit_allocator_cleanup_steps(1).is_err());
        for _ in 0..limits.max_native_connections {
            let ticket = admission.debit_connection_ticket().unwrap();
            drop(ticket);
        }
        assert!(admission.debit_connection_ticket().is_err());
        let evidence = claim.native_quota_evidence().unwrap();
        assert_eq!(evidence.storage_bytes, limits.max_native_storage_bytes);
        assert_eq!(evidence.connections_issued, limits.max_native_connections);
        assert_eq!(evidence.allocator_steps, remaining_service);
        assert_eq!(evidence.cleanup_steps, cleanup_reserved);
        assert!(
            claim.seal_production(&work).is_err(),
            "quota owner has no genuine native terminal yet"
        );
        let aliased = admission.clone();
        drop(admission);
        drop(aliased);
        assert_eq!(claim.native_quota_evidence().unwrap(), evidence);
        assert!(
            claim.seal_production(&work).is_err(),
            "Drop preserves unresolved producer obligation"
        );
    }

    #[test]
    fn required_inspection_mode_and_original_accounting_cannot_be_defaulted() {
        let original = record();
        let mut missing = serde_json::to_value(&original).unwrap();
        missing.as_object_mut().unwrap().remove("inspection");
        assert!(serde_json::from_value::<ClaimRecord>(missing).is_err());
        let mut old = original.clone();
        old.schema = 5;
        old.purpose = "offline_reset_local_quota_reservation_v5".into();
        assert!(old.validate().is_err());
        let mut wrong = original.clone();
        wrong.inspection.mode = OriginalInspectionMode::Mutating;
        assert!(wrong.validate().is_err());
        let mut wrong = original;
        wrong.inspection.accounting[0] = InspectionAccountingRequirement::Unused;
        assert!(wrong.validate().is_err());
    }

    #[test]
    fn signed_disposition_census_preserves_same_claim_and_full_charge() {
        // Wire/signature mechanism only, not an issued native/settlement grant.
        let tmp = tempfile::tempdir().unwrap();
        let signer =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.path().join("identity.pem"))
                .unwrap();
        let root = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let mut claim = record();
        claim.signer = signer.fingerprint().to_owned();
        claim.copy_root_identity = serde_json::to_string(&root.identity().unwrap()).unwrap();
        let claim_body = lillux::canonical_json(&serde_json::to_value(&claim).unwrap()).unwrap();
        let claim_bytes = lillux::signature::sign_content(&claim_body, signer.signing_key(), "#", None).into_bytes();
        let hash = lillux::sha256_hex(&claim_bytes);
        use super::super::creation_evidence::{
            InspectionDispositionRecord, InspectionStoreDispositionRecord,
            OriginalLocalSettlementObligation, OriginalRetainedLocalAuthority,
        };
        let disposition = InspectionDispositionRecord {
            schema: 2,
            purpose: "offline_reset_inspection_disposition_v2".into(),
            signer: claim.signer.clone(),
            operation_id: claim.operation_id.clone(),
            claim_nonce: claim.claim_nonce.clone(),
            claim_sha256: hash.clone(),
            policy_digest: claim.policy_digest.clone(),
            clock_binding: claim.clock_binding.clone(),
            parent_identity: root.identity().unwrap(),
            inspection: claim.inspection.clone(),
            native_quota: super::super::creation_evidence::OriginalNativeQuotaEvidence::unissued(),
            stores: std::array::from_fn(|index| InspectionStoreDispositionRecord::NeverStarted {
                purpose: claim.destinations[index].purpose.clone(),
                destination_name: claim.destinations[index].directory_name.clone(),
                original_present: [false; 4],
            }),
            remaining: vec![
                OriginalLocalSettlementObligation::ReservationPublicationDescriptor,
                OriginalLocalSettlementObligation::IssuedReservationPin,
            ],
            retained_authorities: [
                OriginalRetainedLocalAuthority::SourceDirectoryOwners,
                OriginalRetainedLocalAuthority::InspectionParentOwner,
                OriginalRetainedLocalAuthority::OriginalNamespaceAndStateLifelines,
                OriginalRetainedLocalAuthority::OriginalClockAndPolicyOwner,
            ],
        };
        claim.copy_root_identity = serde_json::to_string(&root.identity().unwrap()).unwrap();
        let signed = disposition.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&disposition, MAX_RECORD_BYTES).unwrap()).unwrap();
        let mut missing_native = serde_json::to_value(&disposition).unwrap();
        missing_native
            .as_object_mut()
            .unwrap()
            .remove("native_quota");
        assert!(serde_json::from_value::<InspectionDispositionRecord>(missing_native).is_err());
        let mut old_disposition = disposition.clone();
        old_disposition.schema = 1;
        old_disposition.purpose = "offline_reset_inspection_disposition_v1".into();
        assert!(old_disposition.validate().is_err());
        let mut malformed = disposition.clone();
        malformed.native_quota.connections_issued = 1;
        assert!(malformed.validate().is_err());
        let verified = InspectionDispositionRecord::verified(&signed, &signer).unwrap();
        require_disposition_claim(&verified, &claim, &hash).unwrap();
        let mut wrong = verified.clone();
        wrong.claim_nonce = "2".repeat(32);
        let signed_wrong = wrong.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&wrong, MAX_RECORD_BYTES).unwrap()).unwrap();
        let wrong = InspectionDispositionRecord::verified(&signed_wrong, &signer).unwrap();
        assert!(require_disposition_claim(&wrong, &claim, &hash).is_err());
        assert!(
            !verified.remaining.is_empty(),
            "signed subsidiary evidence cannot release its held original reservation"
        );
        assert!(require_operation_suffix("../foreign").is_err());
        let mut releasing = super::super::creation_evidence::InspectionTerminalRecord {
            schema: 2,
            purpose: "offline_reset_inspection_terminal_pending_v2".into(),
            disposition: verified,
            disposition_sha256: lillux::sha256_hex(&signed),
            witness_retired: [false; 2],
            reservation_retained: true,
        };
        releasing.validate().unwrap();
        // Recovery wire fixtures only: these signed data records do not issue
        // any native acquisition, removal, physical-close or refund authority.
        let mut pending_fixture = releasing.clone();
        pending_fixture.disposition.native_quota = super::super::creation_evidence::OriginalNativeQuotaEvidence {
            admitted: true, storage_bytes: 1, allocator_steps: 0,
            cleanup_reserved: 1, cleanup_steps: 0, connections_issued: 0,
        };
        let fixture_disposition = pending_fixture.disposition.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&pending_fixture.disposition, MAX_RECORD_BYTES).unwrap()).unwrap();
        pending_fixture.disposition_sha256 = lillux::sha256_hex(&fixture_disposition);
        let pending_bytes = pending_fixture.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&pending_fixture, MAX_RECORD_BYTES).unwrap()).unwrap();
        use super::super::creation_evidence::{OriginalAccountSettlementIntentRecord,
            OriginalPreparedSourceRetirementRecord, OriginalSourceRetirementPinFact};
        let prepared_fixture = OriginalPreparedSourceRetirementRecord {
            resident_generation: claim.resident_generation,
            resident_bytes: claim.resident_bytes,
            reservation_parent_identity: root.identity().unwrap(),
            schema: 1, purpose: "offline_reset_source_retirement_prepared_v1".into(),
            signer: claim.signer.clone(), operation_id: claim.operation_id.clone(), claim_sha256: hash.clone(),
            pending_terminal_sha256: lillux::sha256_hex(&pending_bytes),
            clock_binding: claim.clock_binding.clone(), policy_digest: claim.policy_digest.clone(),
            native_quota: pending_fixture.disposition.native_quota.clone(),
            source_pins: vec![OriginalSourceRetirementPinFact { slot: 0, device: 1, inode: 1, byte_length: 1 }],
            reservation_publication: OriginalSourceRetirementPinFact { slot: 13, device: 1, inode: 2, byte_length: 1 },
            issued_reservation: OriginalSourceRetirementPinFact { slot: 14, device: 1, inode: 2, byte_length: 1 },
            reservation_retained: true,
        };
        let prepared_bytes = prepared_fixture.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&prepared_fixture, MAX_RECORD_BYTES).unwrap()).unwrap();
        let intent = OriginalAccountSettlementIntentRecord::from_original_records(
            &pending_bytes, &prepared_bytes, &claim_bytes, &signer).unwrap();
        let intent_bytes = intent.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&intent, MAX_RECORD_BYTES).unwrap()).unwrap();
        let intent = OriginalAccountSettlementIntentRecord::verified(&intent_bytes, &signer).unwrap();
        intent.require_original_chain(&pending_bytes, &prepared_bytes, &signer).unwrap();
        assert!(intent.reservation_retained && !pending_fixture.disposition.remaining.is_empty());
        let mut foreign_prepared = prepared_fixture.clone();
        foreign_prepared.clock_binding = "e".repeat(64);
        let foreign_prepared = foreign_prepared.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&foreign_prepared, MAX_RECORD_BYTES).unwrap()).unwrap();
        assert!(intent.require_original_chain(&pending_bytes, &foreign_prepared, &signer).is_err());
        let mut releasing_intent = intent;
        releasing_intent.reservation_retained = false;
        assert!(releasing_intent.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&releasing_intent, MAX_RECORD_BYTES).unwrap()).is_err());
        releasing.reservation_retained = false;
        assert!(
            releasing.validate().is_err(),
            "no census refund schema is accepted"
        );
    }

    #[test]
    fn retained_clock_namespace_binding_required_and_old_schema_refused() {
        let original = record();
        let mut missing = serde_json::to_value(&original).unwrap();
        missing.as_object_mut().unwrap().remove("clock_binding");
        assert!(serde_json::from_value::<ClaimRecord>(missing).is_err());
        let mut old = original.clone();
        old.schema = 4;
        assert!(old.validate().is_err());
        let mut changed = original;
        changed.clock_binding.clear();
        assert!(changed.validate().is_err());
    }

    #[test]
    fn clock_binding_is_required_and_cannot_change_issued_duration() {
        let original = record();
        original.validate().unwrap();
        let mut old = original.clone();
        old.schema = 1;
        assert!(old.validate().is_err());
        let mut missing = serde_json::to_value(&original).unwrap();
        missing.as_object_mut().unwrap().remove("occupancy");
        assert!(serde_json::from_value::<ClaimRecord>(missing).is_err());
        let mut changed = original.clone();
        changed.occupancy.maximum_occupancy_ns += 1;
        assert!(changed.validate().is_err());
        changed = original;
        changed.reserved.max_operation_millis = i64::MAX as u64;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn destination_intent_is_required_and_bound_to_original_operation() {
        let original = record();
        original.validate().unwrap();
        let mut missing = serde_json::to_value(&original).unwrap();
        missing.as_object_mut().unwrap().remove("destinations");
        assert!(serde_json::from_value::<ClaimRecord>(missing).is_err());
        let mut old = original.clone();
        old.schema = 3;
        assert!(old.validate().is_err());
        let mut changed = original.clone();
        changed.destinations.swap(0, 1);
        assert!(changed.validate().is_err());
        changed = original;
        changed.operation_id = "d".repeat(32);
        assert!(changed.validate().is_err());
    }

    #[test]
    fn aggregate_reservations_include_original_cleanup_steps() {
        let quota = record().reserved;
        let mut one = quota.clone();
        one.max_source_roles /= 2;
        one.max_copied_bytes /= 2;
        one.max_retained_destination_bytes /= 2;
        one.max_read_bytes /= 2;
        one.max_sql_vm_steps /= 2;
        one.max_memory_bytes /= 2;
        one.max_native_storage_bytes /= 2;
        one.max_native_allocator_steps /= 2;
        one.max_native_connections /= 2;
        one.max_recovery_record_bytes /= 2;
        one.max_cleanup_steps = 512;
        assert!(validate_aggregate(&[one.clone(), one.clone()], &quota).is_ok());
        let mut excess = one.clone();
        excess.max_cleanup_steps = 513;
        assert!(validate_aggregate(&[one, excess], &quota).is_err());
    }

    #[test]
    fn original_clock_preserves_service_cleanup_expiry_and_incarnation() {
        let original = record();
        let cleanup = std::time::Duration::from_millis(original.reserved.max_cleanup_millis);
        let mut boundary = original.occupancy.start.clone();
        boundary.tick_ns += original.reserved.max_operation_millis * 1_000_000 - 1;
        assert!(
            matches!(original.occupancy.window_at(&boundary, cleanup).unwrap(),
            lillux::time::OccupancyWindowState::Service { remaining } if remaining.as_nanos() == 1)
        );
        let service_window = original.occupancy.window_at(&boundary, cleanup).unwrap();
        assert_eq!(
            original_cleanup_remaining(service_window, cleanup).unwrap(),
            cleanup + std::time::Duration::from_nanos(1)
        );
        boundary.tick_ns += 1;
        assert!(
            matches!(original.occupancy.window_at(&boundary, cleanup).unwrap(),
            lillux::time::OccupancyWindowState::Cleanup { remaining } if remaining == cleanup)
        );
        let cleanup_window = original.occupancy.window_at(&boundary, cleanup).unwrap();
        assert_eq!(
            original_cleanup_remaining(cleanup_window, cleanup).unwrap(),
            cleanup
        );
        boundary.tick_ns += original.reserved.max_cleanup_millis * 1_000_000;
        assert_eq!(
            original.occupancy.window_at(&boundary, cleanup).unwrap(),
            lillux::time::OccupancyWindowState::Expired
        );
        assert!(
            original_cleanup_remaining(
                original.occupancy.window_at(&boundary, cleanup).unwrap(),
                cleanup
            )
            .is_err()
        );
        assert!(
            original_cleanup_remaining(
                lillux::time::OccupancyWindowState::Service {
                    remaining: std::time::Duration::MAX
                },
                cleanup
            )
            .is_err()
        );
        boundary.incarnation_digest = "0".repeat(64);
        assert!(original.occupancy.window_at(&boundary, cleanup).is_err());
        // Expiry is no reason to discard the durable charge from the ledger.
        original.validate().unwrap();
    }
}

/// Actual source retirement plus native terminal. The charged reservation and
/// original finalizer lifelines remain owned; this is not full completion.
pub(super) struct OriginalReadyForFinalAck {
    seal: Arc<OriginalProductionSeal>,
    admission: Arc<OriginalSettlementAdmission>,
    factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    terminal: Arc<lillux_sqlite::OriginalNativeTerminal>,
    record: super::creation_evidence::OriginalPreparedSourceRetirementRecord,
    signed: Vec<u8>,
    publication: Mutex<OriginalRecordPublication>,
    pins: Vec<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>,
    receipts: Mutex<Vec<Option<Arc<lillux_sqlite::RetiredOriginalMetadataPin>>>>,
}
impl OriginalReadyForFinalAck {
    /// Actual State effect or cached receipt replay, irreversibly charged to
    /// the same original cleanup/read/memory owner BEFORE State effects.
    /// The guarded max512 reader can read the extra growth byte: charge513.
    pub(super) fn debit_state_ack_attempt(
        &self,
        read_bytes: u64,
        memory_bytes: u64,
        steps: u64,
    ) -> Result<()> {
        ensure!(
            matches!(
                (read_bytes, memory_bytes, steps),
                (513, 2048, 32) | (0, 256, 1)
            ),
            "State ACK attempt differs from fixed original effect/replay bound"
        );
        self.require_custody()?;
        self.admission.debit_original(steps)?;
        let mut used = self
            .admission
            .claim
            .copy_usage
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original State ACK debit busy or poisoned"))?;
        let read = used
            .read_bytes
            .checked_add(read_bytes)
            .context("State ACK original read overflow")?;
        let memory = used
            .buffer_bytes
            .checked_add(memory_bytes)
            .context("State ACK original memory overflow")?;
        ensure!(
            read <= self.admission.claim.record.reserved.max_read_bytes
                && memory.checked_add(used.native_storage_bytes)
                    .and_then(|bytes| bytes.checked_add(used.resident_storage_bytes)).is_some_and(
                    |total| total <= self.admission.claim.record.reserved.max_memory_bytes
                ),
            "State ACK attempt exceeds original read/memory envelope"
        );
        used.read_bytes = read;
        used.buffer_bytes = memory;
        drop(used);
        self.require_custody()
    }
    fn require_custody(&self) -> Result<()> {
        self.terminal.require_original(&self.factory)?;
        self.seal
            .require_original(&self.seal.claim, &self.seal.work)?;
        self.admission.checkpoint_original()
    }
    pub(super) fn require_original(
        &self,
        claim: &Arc<OriginalQuotaClaim>,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
    ) -> Result<()> {
        self.seal.require_original(claim, work)?;
        ensure!(
            Arc::ptr_eq(factory, &self.factory) && Arc::ptr_eq(terminal, &self.terminal),
            "foreign original ready-for-final-ACK owner"
        );
        self.require_custody()?;
        let publisher = {
            let publication = self
                .publication
                .try_lock()
                .map_err(|_| anyhow::anyhow!("prepared retirement publisher busy or poisoned"))?;
            ensure!(
                publication.acknowledged,
                "prepared retirement durability pending"
            );
            publication
                .owner
                .clone()
                .context("prepared retirement publisher custody absent")?
        };
        publisher.require_original(
            &claim.ledger,
            OsStr::new(
                &super::inspection_destination::prepared_source_retirement_name(
                    &claim.record.operation_id,
                ),
            ),
        )?;
        self.admission
            .debit_original_read((MAX_RECORD_BYTES + 1) as u64)?;
        ensure!(
            self.record.claim_sha256 == lillux::sha256_hex(&claim.signed)
                && self.record.reservation_retained,
            "ready-for-final-ACK prepared binding changed"
        );
        ensure!(
            publisher.read_original(MAX_RECORD_BYTES)? == self.signed,
            "prepared retirement authenticated bytes changed"
        );
        let receipts = self
            .receipts
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original source retirement receipts busy or poisoned"))?
            .clone();
        ensure!(
            receipts.len() == self.pins.len(),
            "source retirement census changed"
        );
        for (pin, receipt) in self.pins.iter().zip(receipts) {
            receipt
                .context("actual original source/marker close pending")?
                .require_original(pin)?;
        }
        self.require_custody()
    }
}
impl lillux_sqlite::OriginalMetadataPinRetirementAuthority for OriginalReadyForFinalAck {
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        pin: &Arc<lillux_sqlite::RetainedOriginalMetadataPin>,
    ) -> Result<()> {
        ensure!(
            Arc::ptr_eq(factory, &self.factory)
                && Arc::ptr_eq(terminal, &self.terminal)
                && self.pins.iter().any(|original| Arc::ptr_eq(original, pin)),
            "metadata retirement selects foreign original owner"
        );
        let acknowledged = self
            .publication
            .try_lock()
            .map_err(|_| anyhow::anyhow!("prepared retirement acknowledgement busy or poisoned"))?
            .acknowledged;
        ensure!(
            acknowledged,
            "source retirement requires durable prepared evidence"
        );
        self.require_custody()
    }
    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        self.admission.debit_original(steps)
    }
}
impl OriginalInspectionSettlement {
    pub(super) fn prepare_final_ack(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
    ) -> Result<Arc<OriginalReadyForFinalAck>> {
        let result = (|| -> Result<Arc<OriginalReadyForFinalAck>> {
            self.admission.checkpoint_original()?;
            self.seal
                .require_original(&self.seal.claim, &self.seal.work)?;
            terminal.require_original(factory)?;
            ensure!(
                terminal.arena_disposition()?
                    == lillux_sqlite::OriginalNativeArenaDisposition::Released,
                "ready-for-final-ACK requires actual original native release"
            );
            let cached = {
                let progress = self
                    .progress
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("source retirement custody busy or poisoned"))?;
                ensure!(
                    progress.disposition.acknowledged && progress.terminal.acknowledged,
                    "source retirement requires durable actual pending settlement"
                );
                progress.final_ack.clone()
            };
            let owner = if let Some(owner) = cached {
                ensure!(
                    Arc::ptr_eq(factory, &owner.factory) && Arc::ptr_eq(terminal, &owner.terminal),
                    "source retirement cannot replace actual backend owner"
                );
                owner
            } else {
                use super::creation_evidence::{
                    OriginalPreparedSourceRetirementRecord, OriginalSourceRetirementPinFact,
                };
                let claim = &self.seal.claim;
                ensure!(
                    claim._prior_operation.is_none(),
                    "original ancestor completion proof required before final-ACK readiness"
                );
                // Prepay bounded signed record plus fixed thirteen-pin owner
                // bookkeeping before creating buffers or publishing evidence.
                self.admission
                    .reserve_records((2 * MAX_RECORD_BYTES) as u64)?;
                self.admission.debit_original(2)?;
                let mut inventory = Vec::new();
                let mut actual = claim._source_pins.iter();
                for (source, plan) in claim.record.sources.iter().enumerate() {
                    for (role, fact) in plan.roles.iter().enumerate() {
                        if fact.is_some() {
                            inventory.push((
                                u8::try_from(source * 4 + role)?,
                                actual
                                    .next()
                                    .context("actual original source pin census incomplete")?
                                    .clone(),
                            ));
                        }
                    }
                }
                ensure!(actual.next().is_none(), "extra original copied-source pin");
                for (role, pin) in claim.operational_source_pins.retirement_pins() {
                    ensure!(role <= 4, "foreign Operational metadata role");
                    inventory.push((8 + role, pin));
                }
                let mut facts = Vec::new();
                let mut pins = Vec::new();
                for (slot, pin) in inventory {
                    self.admission.debit_original(2)?;
                    let identity = pin.observed_identity()?;
                    facts.push(OriginalSourceRetirementPinFact {
                        slot,
                        device: identity.device(),
                        inode: identity.inode(),
                        byte_length: identity.size(),
                    });
                    pins.push(pin);
                }
                self.admission.debit_original(4)?;
                let publication = self.admission.reservation_publication.identity_original()?;
                let issued = self.admission.original.original_reservation_identity()?;
                let budget = terminal.budget_snapshot()?;
                let record = OriginalPreparedSourceRetirementRecord {
                    resident_generation: claim.record.resident_generation,
                    resident_bytes: claim.record.resident_bytes,
                    reservation_parent_identity: claim.ledger.identity()?,
                    schema: 1,
                    purpose: "offline_reset_source_retirement_prepared_v1".into(),
                    signer: claim.record.signer.clone(),
                    operation_id: claim.record.operation_id.clone(),
                    claim_sha256: lillux::sha256_hex(&claim.signed),
                    pending_terminal_sha256: lillux::sha256_hex(&self.signed_terminal),
                    clock_binding: claim.record.clock_binding.clone(),
                    policy_digest: claim.record.policy_digest.clone(),
                    native_quota: super::creation_evidence::OriginalNativeQuotaEvidence {
                        admitted: true,
                        storage_bytes: budget.storage_bytes,
                        allocator_steps: budget.allocator_steps,
                        cleanup_reserved: budget.cleanup_reserved,
                        cleanup_steps: budget.cleanup_steps,
                        connections_issued: budget.connections_issued,
                    },
                    source_pins: facts,
                    reservation_publication: OriginalSourceRetirementPinFact {
                        slot: 13,
                        device: publication.device(),
                        inode: publication.inode(),
                        byte_length: publication.size(),
                    },
                    issued_reservation: OriginalSourceRetirementPinFact {
                        slot: 14,
                        device: issued.device(),
                        inode: issued.inode(),
                        byte_length: issued.size(),
                    },
                    reservation_retained: true,
                };
                self.admission.debit_original(1)?;
                let storage = lillux::cas::canonical_record_storage_quote(&record, MAX_RECORD_BYTES)?;
                claim.debit_original_record_working_storage(storage.working_bytes())?;
                let signed = record.signed_bytes(&claim.signer, &storage)?;
                let count = pins.len();
                let owner = Arc::new(OriginalReadyForFinalAck {
                    seal: self.seal.clone(),
                    admission: self.admission.clone(),
                    factory: factory.clone(),
                    terminal: terminal.clone(),
                    record,
                    signed,
                    publication: Mutex::new(Default::default()),
                    pins,
                    receipts: Mutex::new((0..count).map(|_| None).collect()),
                });
                let mut progress = self.progress.try_lock().map_err(|_| {
                    anyhow::anyhow!("source retirement registration busy or poisoned")
                })?;
                ensure!(
                    progress.final_ack.is_none(),
                    "source retirement registration conflict"
                );
                progress.final_ack = Some(owner.clone());
                owner
            };
            // Actual retirement owner is retained before publication/close effects.
            {
                let mut publication = owner.publication.try_lock().map_err(|_| {
                    anyhow::anyhow!("prepared retirement publication busy or poisoned")
                })?;
                self.publish_record(
                    &super::inspection_destination::prepared_source_retirement_name(
                        &self.seal.claim.record.operation_id,
                    ),
                    &owner.signed,
                    &mut publication,
                )?;
            }
            // Prepay opaque grant storage before cloning the fixed actual
            // owner inventory. Retry charges the same irreversible envelope.
            self.admission.reserve_records(1024)?;
            let authority: Arc<dyn lillux_sqlite::OriginalMetadataPinRetirementAuthority> =
                owner.clone();
            // Safety: actual cached signed prepared publication acknowledged,
            // exact original pins, terminal and charged cleanup custody above.
            let inputs = unsafe {
                lillux_sqlite::OriginalMetadataPinRetirementInputs::issued(
                    factory.clone(),
                    terminal.clone(),
                    owner.pins.clone(),
                    authority,
                )?
            };
            for (index, pin) in owner.pins.iter().enumerate() {
                let receipt =
                    owner.receipts.try_lock().map_err(|_| {
                        anyhow::anyhow!("source close receipt slot busy or poisoned")
                    })?[index]
                        .clone();
                if let Some(receipt) = receipt {
                    receipt.require_original(pin)?;
                    continue;
                }
                // No booking/receipt guard spans native descriptor close.
                let receipt = Arc::new(pin.retire_after_backend(factory, terminal, &inputs)?);
                owner.receipts.try_lock().map_err(|_| {
                    anyhow::anyhow!("actual source close receipt registration busy or poisoned")
                })?[index] = Some(receipt);
            }
            owner.require_original(&self.seal.claim, &self.seal.work, factory, terminal)?;
            Ok(owner)
        })();
        result.map_err(|error| self.retain_error(error))
    }
}

/// Real State ACK is retained separately from physical finalizer completion.
/// Every original reservation and lifeline obligation remains charged here.
pub(super) struct OriginalPostAckFinalization {
    removal_parent: Mutex<Option<OriginalRemovalParentProgress>>,
    account_intent: Mutex<Option<OriginalAccountIntentProgress>>,
    primary_state_lock: Mutex<Option<OriginalPrimaryStateLockProgress>>,
    offline_policy: Mutex<Option<super::offline_policy::OriginalOfflinePolicyOutside>>,
    account_state_lock: Mutex<Option<OriginalAccountStateLockProgress>>,
    inspection_group: Mutex<Option<OriginalControllerInspectionGroup>>,
    ready: Arc<OriginalReadyForFinalAck>,
    state: Mutex<OriginalPostAckStateCustody>,
    sql_custody: Mutex<Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
    destination_packets: Mutex<Option<std::mem::ManuallyDrop<Vec<OriginalDestinationPacketProgress>>>>,
    physical_registry: Mutex<OriginalOutsideRegistryCustody>,
    descriptor_retirements: Mutex<Vec<OriginalOutsideDescriptorRetirement>>,
    metadata_retirements: Mutex<Vec<OriginalOutsideMetadataRetirement>>,
    operation: ThreadHistoryDiscardOperationRef,
    native_lifelines: Mutex<Option<Arc<OriginalTransferredNativeAdmission>>>,
    master: Mutex<Option<Arc<OriginalFinalizerMaster>>>,
}
struct OriginalRemovalParentProgress {
    transferred: bool,
    owner: std::mem::ManuallyDrop<Option<Arc<lillux::PinnedDirectory>>>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}
struct OriginalAccountIntentProgress {
    records_reserved: bool,
    publisher_reserved: bool,
    record: std::mem::ManuallyDrop<Option<super::creation_evidence::OriginalAccountSettlementIntentRecord>>,
    signed: std::mem::ManuallyDrop<Option<Vec<u8>>>,
    name: std::mem::ManuallyDrop<Option<String>>,
    publication: std::mem::ManuallyDrop<OriginalRecordPublication>,
}
struct OriginalPrimaryStateLockProgress {
    source: std::mem::ManuallyDrop<Option<ryeos_node_runtime::state_lock::StateLock>>,
    owner: std::mem::ManuallyDrop<Option<Arc<lillux::ExactExclusiveFileLock>>>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}
pub(super) struct OriginalPrimaryStateLockFailure {
    policy: std::mem::ManuallyDrop<super::offline_policy::OriginalOfflinePolicyOutside>,
    original: std::mem::ManuallyDrop<ryeos_node_runtime::state_lock::StateLock>,
    parent: Arc<OriginalPostAckFinalization>,
    error: anyhow::Error,
}
impl OriginalPrimaryStateLockFailure {
    pub(super) fn into_original(self) -> std::result::Result<
        (ryeos_node_runtime::state_lock::StateLock, super::offline_policy::OriginalOfflinePolicyOutside,
            Arc<OriginalPostAckFinalization>, anyhow::Error), Self> {
        // Copied original process identities only, before extracting owners or
        // traversing lock/native custody. No fresh authority is stamped here.
        if self.parent.ready.seal.claim.record.resident_process != std::process::id()
            || self.policy.process != std::process::id() {
            return Err(self);
        }
        Ok((std::mem::ManuallyDrop::into_inner(self.original), std::mem::ManuallyDrop::into_inner(self.policy), self.parent, self.error))
    }
}
impl std::fmt::Debug for OriginalPrimaryStateLockFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_tuple("OriginalPrimaryStateLockFailure").field(&self.error).finish() }
}
impl std::fmt::Display for OriginalPrimaryStateLockFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "original primary State lock retained: {}", self.error) }
}
impl std::error::Error for OriginalPrimaryStateLockFailure {}
struct OriginalAccountStateLockProgress {
    owner: std::mem::ManuallyDrop<Option<Arc<lillux::ExactExclusiveFileLock>>>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
    aliases_joined: bool,
}
/// Complete concrete controller inventories, moved without allocating new
/// vectors. MD retains every alias/resource across partial transfer and unwind.
struct OriginalControllerInspectionGroup {
    groups: Option<std::mem::ManuallyDrop<Vec<crate::runtime_db::OriginalInspectionCopyGroup>>>,
    closed: Option<std::mem::ManuallyDrop<Vec<Arc<crate::runtime_db::SqliteInspectionClosed>>>>,
    acquisition: Option<std::mem::ManuallyDrop<Vec<Arc<crate::runtime_db::SqliteInspectionAcquisitionReleased>>>>,
    runtime: Option<std::mem::ManuallyDrop<Vec<Arc<crate::runtime_db::RuntimeInspectionRelease>>>>,
    cleanup: Option<std::mem::ManuallyDrop<Vec<Arc<crate::runtime_db::InspectionCopyCleanup>>>>,
    controller_disposals: Option<std::mem::ManuallyDrop<Vec<Arc<crate::runtime_db::InspectionCopyDisposal>>>>,
    settlement_disposals: Option<std::mem::ManuallyDrop<Vec<Arc<crate::runtime_db::InspectionCopyDisposal>>>>,
    failures: Option<std::mem::ManuallyDrop<Vec<anyhow::Error>>>,
}
struct OriginalOutsideMetadataRetirement {
    pin: Arc<lillux_sqlite::RetainedOriginalMetadataPin>,
    inputs: Option<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    receipt: Option<Arc<lillux_sqlite::RetiredOriginalMetadataPin>>,
}
struct OriginalOutsideStateMetadataAuthority {
    post_ack: std::sync::Weak<OriginalPostAckFinalization>,
    pin: Arc<lillux_sqlite::RetainedOriginalMetadataPin>,
}
impl lillux_sqlite::OriginalMetadataPinRetirementAuthority for OriginalOutsideStateMetadataAuthority {
    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>, pin: &Arc<lillux_sqlite::RetainedOriginalMetadataPin>) -> Result<()> {
        let post_ack = self.post_ack.upgrade().context("actual outside State metadata producer absent")?;
        ensure!(post_ack.ready.seal.claim.record.resident_process == std::process::id(),
            "outside State metadata belongs to original process");
        ensure!(Arc::ptr_eq(factory, &post_ack.ready.factory)
            && Arc::ptr_eq(terminal, &post_ack.ready.terminal) && Arc::ptr_eq(pin, &self.pin),
            "State metadata retirement selects foreign native/actual pin custody");
        terminal.require_original(factory)?;
        let master = post_ack.master.try_lock()
            .map_err(|_| anyhow::anyhow!("original metadata master busy or poisoned"))?
            .clone().context("actual original metadata master absent")?;
        master.checkpoint_original()?;
        let custody = post_ack.state.try_lock()
            .map_err(|_| anyhow::anyhow!("State physical metadata membership busy or poisoned"))?;
        let OriginalPostAckStateCustody::Physical(physical) = &*custody else {
            anyhow::bail!("actual State physical metadata token absent");
        };
        ensure!(physical.operation() == &post_ack.operation
            && physical.durability_pins().iter().any(|actual| Arc::ptr_eq(actual, pin)),
            "State metadata pin is not in its exact original physical custody");
        Ok(())
    }
    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        let post_ack = self.post_ack.upgrade().context("actual original State metadata producer absent")?;
        post_ack.ready.admission.debit_original(steps)
    }
}

fn retained_original_outside_callback(call: impl Fn() -> Result<()>) -> Result<()> {
    lillux_sqlite::original_physical_custody::retain_callback_refusal(&call)
}

enum OriginalOutsideRegistryCustody {
    Unissued,
    InFlight,
    Original(lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry),
}
struct OriginalOutsideDescriptorRetirement {
    owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    inputs: Arc<lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs>,
    receipt: Option<Arc<lillux_sqlite::RetiredOriginalFinalizerFile>>,
}
struct OriginalOutsideSharedDescriptorAuthority {
    post_ack: std::sync::Weak<OriginalPostAckFinalization>,
    registration: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration,
    owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
}
// SAFETY: private issuance binds the actual opaque original registry member
// and its exact cached File. The same registered original master retains and
// debits account/clock/ledger custody outside the native resident graph.
unsafe impl lillux_sqlite::OriginalOutsideFinalizerFileRetirementAuthority
    for OriginalOutsideSharedDescriptorAuthority {
    fn require_original(&self, owner: &Arc<lillux_sqlite::RetainedOriginalFinalizerFile>) -> Result<()> {
        let post_ack = self.post_ack.upgrade().context("actual outside physical producer absent")?;
        ensure!(post_ack.ready.seal.claim.record.resident_process == std::process::id(),
            "outside shared descriptor belongs to original process");
        ensure!(Arc::ptr_eq(owner, &self.owner), "outside retirement selects another actual descriptor");
        let master = post_ack.master.try_lock()
            .map_err(|_| anyhow::anyhow!("actual outside shared master busy or poisoned"))?
            .clone().context("actual original outside master absent")?;
        master.checkpoint_original()?;
        let custody = post_ack.physical_registry.try_lock()
            .map_err(|_| anyhow::anyhow!("actual original physical membership busy or poisoned"))?;
        let OriginalOutsideRegistryCustody::Original(registry) = &*custody else {
            anyhow::bail!("actual original shared registry absent or transferring");
        };
        registry.require_membership(&self.registration)?;
        ensure!(Arc::ptr_eq(&self.registration.owner()?, owner),
            "outside grant differs from its actual original registry File");
        Ok(())
    }
    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        let post_ack = self.post_ack.upgrade().context("actual original outside producer absent")?;
        post_ack.ready.admission.debit_original(steps)
    }
}

struct OriginalOutsideBoundResources {
    root: std::mem::ManuallyDrop<Option<Arc<lillux::PinnedDirectory>>>,
    name: lillux_sqlite::OriginalNativeOutsideBoundCoordinate,
    mode: lillux_sqlite::OriginalNativeBoundMode,
    domain: lillux_sqlite::OriginalNativeDomain,
    inspection: std::mem::ManuallyDrop<Option<(lillux_sqlite::SqliteCopyLease, Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>)>>,
    registration: Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>,
}
struct OriginalDestinationPacketProgress {
    raw: std::mem::ManuallyDrop<Option<lillux_sqlite::OriginalNativeOutsideConnectionCustody>>,
    parts: std::mem::ManuallyDrop<Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>>,
    bound: std::mem::ManuallyDrop<Option<OriginalOutsideBoundResources>>,
    authority: std::mem::ManuallyDrop<Option<Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>>>,
    destination: std::mem::ManuallyDrop<Option<lillux_sqlite::OriginalDestinationOutsideCustody>>,
    refusal: Option<lillux_sqlite::OriginalDestinationCustodyFailure>,
    normalized: bool,
    requires_destination: bool,
}
impl OriginalDestinationPacketProgress {
    fn from_original(raw: lillux_sqlite::OriginalNativeOutsideConnectionCustody) -> Self {
        Self { raw: std::mem::ManuallyDrop::new(Some(raw)), parts: std::mem::ManuallyDrop::new(None),
            bound: std::mem::ManuallyDrop::new(None), authority: std::mem::ManuallyDrop::new(None),
            destination: std::mem::ManuallyDrop::new(None), refusal: None, normalized: false, requires_destination: false }
    }
    fn normalize_original(&mut self) -> Result<()> {
        if self.normalized { return Ok(()); }
        if let Some(raw) = self.raw.take() {
            match raw.into_original_parts() {
                Ok(parts) => *self.parts = Some(parts),
                Err(raw) => { *self.raw = Some(raw); anyhow::bail!("actual native packet extraction refused"); }
            }
        }
        let parts = self.parts.as_mut().context("actual original native packet parts absent")?;
        ensure!(parts.bound_inputs.is_none() || parts.transient.is_none(),
            "actual packet retains conflicting original destination owners");
        if let Some(bound) = parts.bound_inputs.take() {
            let bound = match bound.into_original_parts() {
                Ok(bound) => bound,
                Err(bound) => { parts.bound_inputs = Some(bound); anyhow::bail!("actual original bound inputs extraction refused"); }
            };
            *self.authority = Some(bound.authority);
            *self.bound = Some(OriginalOutsideBoundResources {
                root: std::mem::ManuallyDrop::new(Some(bound.root)), name: bound.name,
                mode: bound.mode, domain: bound.domain,
                inspection: std::mem::ManuallyDrop::new(bound.inspection), registration: None,
            });
        } else { *self.authority = parts.transient.take(); }
        self.requires_destination = self.authority.is_some();
        self.normalized = true;
        Ok(())
    }
}

enum OriginalPostAckStateCustody {
    Original {
        state: Arc<ryeos_state::state_db::ClosedOriginalState>,
        acknowledgement: Arc<ryeos_state::state_db::OriginalStateAcknowledgement>,
    },
    // Exclusive temporary transfer custody, never disposal/settlement proof.
    Handoff,
    Outside(ryeos_state::state_db::OriginalOutsideStateFinalizer),
    Physical(ryeos_state::state_db::OriginalStatePhysicalCustody),
    Refused(ryeos_state::state_db::OriginalStateOutsideRetirementFailure),
}
impl OriginalPostAckFinalization {
    pub(super) fn register_original_account_state_lock(self: &Arc<Self>) -> Result<()> {
        self.retain_original_account_state_lock()?;
        let mut actual = self.account_state_lock.try_lock()
            .map_err(|_| anyhow::anyhow!("original account lock registry checkout busy or poisoned"))?
            .take().context("original account lock registry source already transferring")?;
        let result = self.with_original_physical_registry(&mut |registry, prepay| {
            ensure!(actual.aliases_joined, "original account lock aliases not joined");
            if let Some(binding) = &actual.registration {
                return registry.require_membership(binding);
            }
            #[cfg(target_os = "linux")]
            {
                let owner = actual.owner.take()
                    .context("original account lock absent without registry membership")?;
                // SAFETY: genuine account parent and actual same-Arc source
                // join above; all other owners remain unresolved independently.
                match unsafe { registry.register_operator_lock(owner, prepay) } {
                    Ok(binding) => actual.registration = Some(binding),
                    Err((owner, error)) => { *actual.owner = Some(owner); return Err(error); }
                }
                Ok(())
            }
            #[cfg(not(target_os = "linux"))]
            { let _ = prepay; anyhow::bail!("original registered operator lock requires Linux custody") }
        });
        // Cache the actual source or real membership before forwarding any
        // refusal/panic. The registry wrapper also retains its whole owner.
        *self.account_state_lock.lock().unwrap_or_else(|p| p.into_inner()) = Some(actual);
        result
    }
    fn original_state_inputs(&self) -> Result<(
        Arc<ryeos_state::state_db::ClosedOriginalState>,
        Arc<ryeos_state::state_db::OriginalStateAcknowledgement>,
    )> {
        self.ready.factory.require_original_process()?;
        let custody = self.state.try_lock()
            .map_err(|_| anyhow::anyhow!("original post-ACK State custody busy or poisoned"))?;
        match &*custody {
            OriginalPostAckStateCustody::Original { state, acknowledgement } =>
                Ok((state.clone(), acknowledgement.clone())),
            _ => anyhow::bail!("original State proof has entered consuming outside custody"),
        }
    }
    fn require_state_ack(&self) -> Result<()> {
        let (state, acknowledgement) = self.original_state_inputs()?;
        acknowledgement.require_original(
            &state, &self.ready.factory, &self.ready.terminal, &self.operation,
        )
    }
    /// The caller first removes only its redundant State/ACK aliases after
    /// retaining this actual producer. Refusal keeps the entire genuine tuple;
    /// retry takes that tuple, never manufactures a State or acknowledgement.
    pub(super) fn transfer_state_outside(
        &self,
        finalizer: Option<Arc<ryeos_state::state_db::OriginalStateFinalizer>>,
        guard: &ryeos_state::recovery::CasMutationGuard,
    ) -> Result<()> {
        self.ready.factory.require_original_process()?;
        self.ready.admission.checkpoint_original()?;
        let mut custody = self.state.try_lock()
            .map_err(|_| anyhow::anyhow!("original State handoff custody busy or poisoned"))?;
        if matches!(&*custody, OriginalPostAckStateCustody::Outside(_) | OriginalPostAckStateCustody::Physical(_)) {
            ensure!(finalizer.is_none(), "outside State retry supplied another finalizer");
            return Ok(());
        }
        match &*custody {
            OriginalPostAckStateCustody::Original { .. } =>
                ensure!(finalizer.is_some(), "actual original State finalizer absent"),
            OriginalPostAckStateCustody::Refused(_) =>
                ensure!(finalizer.is_none(), "State refusal already retains its actual finalizer"),
            _ => anyhow::bail!("original State handoff is already in progress"),
        }
        let original = std::mem::replace(&mut *custody, OriginalPostAckStateCustody::Handoff);
        drop(custody);
        let (state, acknowledgement, finalizer) = match original {
            OriginalPostAckStateCustody::Original { state, acknowledgement } =>
                (state, acknowledgement, finalizer.expect("checked actual State finalizer")),
            OriginalPostAckStateCustody::Refused(failure) => failure.into_original_inputs(),
            _ => unreachable!("checked exclusive State handoff"),
        };
        // The lower consuming entry authenticates the exact ACK/terminal/CAS
        // and all ten actual root receipts before calling this prepaid quote.
        // No State/controller gate spans that callback or native transfer.
        let prepay = |bytes, steps| retained_original_outside_callback(|| self.debit_finalizer_transfer(bytes, steps));
        // SAFETY: the actual offline synchronous consumers are quiescent,
        // the same continuously retained exclusive CAS frame and original
        // master/clock select this tuple; redundant aliases were removed by
        // the caller. Cache success or the whole refusal before late checks.
        let transferred = unsafe { state.into_outside_finalizer(
            acknowledgement, &self.ready.terminal, &self.operation,
            guard, finalizer, &prepay,
        ) };
        let (next, refusal) = match transferred {
            Ok(outside) => (OriginalPostAckStateCustody::Outside(outside), None),
            Err(failure) => {
                let message = format!("actual State outside handoff retains its original inputs: {failure}");
                (OriginalPostAckStateCustody::Refused(failure), Some(message))
            }
        };
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = next;
        if let Some(message) = refusal { anyhow::bail!(message); }
        self.ready.admission.checkpoint_original()
    }
    pub(super) fn prepay_outside_continuation(&self, memory_bytes: u64, steps: u64) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "outside continuation belongs to original process");
        let master = self.master.try_lock()
            .map_err(|_| anyhow::anyhow!("actual outside master custody busy or poisoned"))?
            .clone().context("actual original outside master absent")?;
        master.checkpoint_original()?;
        ensure!(steps > 0, "outside continuation requires positive original work; zero-byte scans allocate no storage");
        self.ready.admission.debit_original(steps)?;
        let mut used = self.ready.admission.claim.copy_usage.try_lock()
            .map_err(|_| anyhow::anyhow!("outside continuation memory debit busy or poisoned"))?;
        let memory = used.buffer_bytes.checked_add(memory_bytes)
            .context("outside continuation memory quote overflow")?;
        ensure!(memory.checked_add(used.native_storage_bytes)
            .and_then(|total| total.checked_add(used.resident_storage_bytes))
            .is_some_and(|total| total <= self.ready.admission.claim.record.reserved.max_memory_bytes),
            "outside continuation exceeds original signed memory envelope");
        used.buffer_bytes = memory;
        drop(used);
        master.checkpoint_original()
    }
    pub(super) fn retire_state_metadata_pins(self: &Arc<Self>) -> Result<()> {
        // Genuine native input grants are used BEFORE sealing/disposal of the
        // resident graph, then consumed once actual pin receipts are cached.
        self.ready.factory.require_original_process()?;
        self.ready.terminal.require_original(&self.ready.factory)?;
        let count = {
            let custody = self.state.try_lock()
                .map_err(|_| anyhow::anyhow!("State metadata quote custody busy or poisoned"))?;
            let OriginalPostAckStateCustody::Physical(physical) = &*custody else {
                anyhow::bail!("actual State physical metadata token absent");
            };
            u64::try_from(physical.durability_pins().len())?
        };
        self.prepay_outside_continuation(
            count.checked_mul(1024).and_then(|bytes| bytes.checked_add(512))
                .context("State metadata retirement memory overflow")?,
            count.checked_mul(4).and_then(|steps| steps.checked_add(4))
                .context("State metadata retirement work overflow")?,
        )?;
        let pins = {
            let custody = self.state.try_lock()
                .map_err(|_| anyhow::anyhow!("State metadata inventory busy or poisoned"))?;
            let OriginalPostAckStateCustody::Physical(physical) = &*custody else {
                anyhow::bail!("actual State physical token absent after original precharge");
            };
            physical.durability_pins().to_vec()
        };
        for (index, pin) in pins.into_iter().enumerate() {
            let retained = self.metadata_retirements.try_lock()
                .map_err(|_| anyhow::anyhow!("original State metadata registration busy or poisoned"))?
                .iter().find(|actual| Arc::ptr_eq(&actual.pin, &pin))
                .map(|actual| (actual.inputs.clone(), actual.receipt.clone()));
            let (inputs, receipt) = if let Some(retained) = retained { retained } else {
                let authority: Arc<dyn lillux_sqlite::OriginalMetadataPinRetirementAuthority> =
                    Arc::new(OriginalOutsideStateMetadataAuthority {
                        post_ack: Arc::downgrade(self), pin: pin.clone(),
                    });
                // SAFETY: genuine actual native terminal and State physical
                // pin inventory, original master/clock and precharged storage
                // above. These inputs remain cached until a real close receipt.
                let inputs = Arc::new(unsafe { lillux_sqlite::OriginalMetadataPinRetirementInputs::issued(
                    self.ready.factory.clone(), self.ready.terminal.clone(), vec![pin.clone()], authority,
                ) }?);
                self.metadata_retirements.lock().unwrap_or_else(|p| p.into_inner())
                    .push(OriginalOutsideMetadataRetirement { pin: pin.clone(), inputs: Some(inputs.clone()), receipt: None });
                (Some(inputs), None)
            };
            let receipt = if let Some(receipt) = receipt {
                receipt.require_original(&pin)?;
                receipt
            } else {
                let inputs = inputs.as_ref().context("original State metadata inputs absent without real receipt")?;
                let receipt = Arc::new(pin.retire_after_backend(&self.ready.factory, &self.ready.terminal, inputs)?);
                let mut retained = self.metadata_retirements.lock().unwrap_or_else(|p| p.into_inner());
                let actual = retained.iter_mut().find(|actual| Arc::ptr_eq(&actual.pin, &pin))
                    .expect("cached exact original State metadata inputs");
                actual.receipt = Some(receipt.clone());
                // The genuine receipt is retained before ending only these
                // completed native input references; never after unmapping.
                actual.inputs.take();
                receipt
            };
            let mut custody = self.state.try_lock()
                .map_err(|_| anyhow::anyhow!("actual State metadata receipt cache busy or poisoned"))?;
            let OriginalPostAckStateCustody::Physical(physical) = &mut *custody else {
                anyhow::bail!("actual State physical token absent while caching metadata receipt");
            };
            physical.retain_metadata_retirement(index, receipt)?;
        }
        Ok(())
    }
    pub(super) fn retire_preparation_terminal_alias(
        &self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        canonical: &lillux_sqlite::OriginalNativeConnectionTerminal,
    ) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(), "foreign preparation supervisor");
        let actual = self.native_lifelines.try_lock()
            .map_err(|_| anyhow::anyhow!("actual transferred native admission lookup busy or poisoned"))?
            .as_ref().context("actual transferred native admission absent")?.clone();
        actual.admission.retire_preparation_terminal_alias(factory, aggregate, canonical,
            &|bytes, steps| self.prepay_outside_continuation(bytes, steps))
    }
    /// Actual consuming authority transfers, not a count-derived closure.
    /// Caller must first deliver all State/Runtime/Operational original packets.
    pub(super) fn advance_state_destination_join(&self) -> Result<()> {
        self.ready.factory.require_original_process()?;
        self.ready.terminal.require_original(&self.ready.factory)?;
        {
            let packets = self.destination_packets.try_lock()
                .map_err(|_| anyhow::anyhow!("actual destination census gate busy or poisoned"))?;
            let packets = packets.as_ref().context("actual destination packet transfer not entered")?;
            ensure!(!packets.is_empty() && packets.iter().all(|packet| packet.normalized
                && packet.raw.is_none() && packet.authority.is_none() && packet.refusal.is_none()
                && (!packet.requires_destination || packet.destination.is_some())),
                "actual destination packet transfers remain unresolved");
        }
        ensure!(self.sql_custody.try_lock().map_err(|_| anyhow::anyhow!("actual SQL source census gate busy or poisoned"))?.is_empty(),
            "additional genuine SQL packet sources remain retained");
        let original = {
            let mut custody = self.state.try_lock()
                .map_err(|_| anyhow::anyhow!("actual State destination advance checkout busy or poisoned"))?;
            ensure!(matches!(&*custody, OriginalPostAckStateCustody::Physical(_)), "actual State physical receiver absent");
            std::mem::replace(&mut *custody, OriginalPostAckStateCustody::Handoff)
        };
        let OriginalPostAckStateCustody::Physical(mut physical) = original else { unreachable!() };
        let result = {
            let actual = std::cell::RefCell::new(&mut physical);
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                // SAFETY: actual source packets transferred from the original
                // closed State/Runtime/Operational receiver and same aggregate.
                // This gate establishes no count-derived physical proof.
                unsafe { actual.borrow_mut().advance_destination_join(&self.ready.factory,
                    &self.ready.terminal, &|bytes, steps| self.prepay_outside_continuation(bytes, steps)) }
            })
        };
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = OriginalPostAckStateCustody::Physical(physical);
        result
    }
    /// Move the actual sealed original controller/settlement inventories to
    /// the canonical copy-group receiver. No callback spans bookkeeping gates.
    pub(super) fn retain_original_inspection_group(&self,
        controller: &super::InspectionControllerCustody) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original inspection group transfer");
        self.ready.terminal.require_original(&self.ready.factory)?;
        lillux_sqlite::SqliteInspectionCopyWork::require_original_outside_master(
            self.ready.admission.original.as_ref(), &self.ready.factory, &self.ready.terminal)?;
        let settlement = controller.settlement.try_lock()
            .map_err(|_| anyhow::anyhow!("original copy group settlement lookup busy or poisoned"))?
            .clone().context("original copy group settlement absent")?;
        ensure!(Arc::ptr_eq(&settlement.admission, &self.ready.admission)
            && Arc::ptr_eq(&settlement.seal, &self.ready.seal),
            "copy group selects another actual original settlement");
        let initialized = self.inspection_group.try_lock()
            .map_err(|_| anyhow::anyhow!("inspection group quote gate busy or poisoned"))?.is_some();
        if !initialized {
            self.prepay_outside_continuation(
                u64::try_from(std::mem::size_of::<OriginalControllerInspectionGroup>())?, 16)?;
            let mut group = self.inspection_group.try_lock()
                .map_err(|_| anyhow::anyhow!("inspection group registration busy or poisoned"))?;
            if group.is_none() {
                *group = Some(OriginalControllerInspectionGroup { groups: None, closed: None, acquisition: None,
                    runtime: None, cleanup: None, controller_disposals: None,
                    settlement_disposals: None, failures: None });
            }
        }
        // Each prefix is stored in the supervisor before another gate can
        // refuse. Moving Vec storage does not grow or reallocate it.
        macro_rules! move_inventory {
            ($source:expr, $field:ident) => {{
                let mut original = $source.try_lock()
                    .map_err(|_| anyhow::anyhow!("actual inspection inventory busy or poisoned"))?;
                let mut receiver = self.inspection_group.try_lock()
                    .map_err(|_| anyhow::anyhow!("actual inspection receiver busy or poisoned"))?;
                let receiver = receiver.as_mut().expect("prepaid actual inspection receiver");
                if receiver.$field.is_none() {
                    receiver.$field = Some(std::mem::ManuallyDrop::new(std::mem::take(&mut *original)));
                } else {
                    ensure!(original.is_empty(), "original inspection inventory grew after sealed transfer");
                }
            }};
        }
        move_inventory!(controller.closed, closed);
        move_inventory!(controller.acquisition_released, acquisition);
        move_inventory!(controller.runtime_released, runtime);
        move_inventory!(controller.cleanup, cleanup);
        move_inventory!(controller.completed_disposals, controller_disposals);
        move_inventory!(controller.cleanup_failures, failures);
        let mut original = settlement._disposals.try_lock()
            .map_err(|_| anyhow::anyhow!("original settlement disposal transfer busy or poisoned"))?;
        let mut receiver = self.inspection_group.try_lock()
            .map_err(|_| anyhow::anyhow!("actual settlement disposal receiver busy or poisoned"))?;
        let receiver = receiver.as_mut().expect("actual inspection receiver");
        if receiver.settlement_disposals.is_none() {
            receiver.settlement_disposals = Some(original.take()
                .context("actual original settlement disposal inventory absent")?);
        } else {
            ensure!(original.is_none(), "original settlement disposal inventory replaced");
        }
        Ok(())
    }

    pub(super) fn coalesce_original_inspection_group(&self) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original inspection alias census");
        self.ready.terminal.require_original(&self.ready.factory)?;
        let quote = {
            let original = self.inspection_group.try_lock()
                .map_err(|_| anyhow::anyhow!("original inspection alias quote busy or poisoned"))?;
            let group = original.as_ref().context("actual original copy group not transferred")?;
            let canonical = group.settlement_disposals.as_ref().context("actual canonical disposals absent")?;
            let mut count = canonical.len();
            macro_rules! count { ($field:ident) => {
                count = count.checked_add(group.$field.as_ref()
                    .context("actual inspection inventory not transferred")?.len())
                    .context("original inspection alias census overflow")?;
            }; }
            count!(closed); count!(acquisition); count!(runtime); count!(cleanup);
            count!(controller_disposals); count!(failures);
            (count, canonical.len())
        };
        let steps = u64::try_from(quote.0)?.checked_mul(u64::try_from(quote.1)?.checked_add(1)
            .context("inspection canonical census overflow")?)
            .and_then(|n| n.checked_add(16)).context("inspection alias scan quote overflow")?;
        self.prepay_outside_continuation(0, steps)?;
        let mut original = self.inspection_group.try_lock()
            .map_err(|_| anyhow::anyhow!("original inspection alias receiver busy or poisoned"))?;
        let group = original.as_mut().context("actual inspection receiver absent")?;
        let canonical = group.settlement_disposals.as_ref().context("actual canonical disposals absent")?;
        ensure!(canonical.len() == quote.1, "inspection canonical inventory changed after precharge");
        let actual_count = canonical.len()
            .checked_add(group.closed.as_ref().context("closed inventory absent")?.len())
            .and_then(|n| n.checked_add(group.acquisition.as_ref().map_or(0, |v| v.len())))
            .and_then(|n| n.checked_add(group.runtime.as_ref().map_or(0, |v| v.len())))
            .and_then(|n| n.checked_add(group.cleanup.as_ref().map_or(0, |v| v.len())))
            .and_then(|n| n.checked_add(group.controller_disposals.as_ref().map_or(0, |v| v.len())))
            .and_then(|n| n.checked_add(group.failures.as_ref().map_or(0, |v| v.len())))
            .context("actual inspection alias inventory overflow")?;
        ensure!(actual_count == quote.0, "inspection alias inventory changed after original precharge");
        // Exact retained canonical parents prove these are redundant aliases.
        // No descriptor-bearing last reference is dropped by these filters.
        group.controller_disposals.as_mut().context("controller disposal inventory absent")?
            .retain(|alias| !canonical.iter().any(|owner| Arc::ptr_eq(owner, alias)));
        group.cleanup.as_mut().context("cleanup inventory absent")?
            .retain(|alias| !canonical.iter().any(|owner| owner.retains_cleanup_alias(alias)));
        group.closed.as_mut().context("closed inventory absent")?
            .retain(|alias| !canonical.iter().any(|owner| owner.retains_closed_alias(alias)));
        group.acquisition.as_mut().context("acquisition inventory absent")?
            .retain(|alias| !canonical.iter().any(|owner| owner.retains_acquisition_alias(alias)));
        group.runtime.as_mut().context("runtime inventory absent")?
            .retain(|alias| !canonical.iter().any(|owner| owner.retains_runtime_alias(alias)));
        // Unknown failure owners and unmatched originals stay retained. Neither
        // empty filtered inventories nor alias counts establish physical close.
        ensure!(group.controller_disposals.as_ref().unwrap().is_empty()
            && group.cleanup.as_ref().unwrap().is_empty() && group.closed.as_ref().unwrap().is_empty()
            && group.acquisition.as_ref().unwrap().is_empty() && group.runtime.as_ref().unwrap().is_empty(),
            "inspection controller retains unmatched original resource owners");
        Ok(())
    }

    pub(super) fn advance_original_inspection_group(&self) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original copy group consumer");
        self.ready.terminal.require_original(&self.ready.factory)?;
        let pending = {
            let original = self.inspection_group.try_lock()
                .map_err(|_| anyhow::anyhow!("copy group consumer quote busy or poisoned"))?;
            let group = original.as_ref().context("actual original copy group absent")?;
            if group.groups.is_none() {
                Some(group.settlement_disposals.as_ref().context("canonical disposal inventory absent")?.len())
            } else { None }
        };
        if let Some(count) = pending {
            let bytes = count.checked_mul(std::mem::size_of::<crate::runtime_db::OriginalInspectionCopyGroup>())
                .context("canonical copy group receiver quote overflow")?;
            self.prepay_outside_continuation(u64::try_from(bytes)?,
                u64::try_from(count)?.checked_add(8).context("copy group quote work overflow")?)?;
            let mut original = self.inspection_group.try_lock()
                .map_err(|_| anyhow::anyhow!("copy group receiver allocation busy or poisoned"))?;
            let group = original.as_mut().context("actual original copy group absent")?;
            if group.groups.is_none() {
                ensure!(group.settlement_disposals.as_ref().context("canonical disposal inventory absent")?.len() == count,
                    "original copy group grew after precharge");
                group.groups = Some(std::mem::ManuallyDrop::new(Vec::with_capacity(count)));
            }
        }
        let mut group = self.inspection_group.try_lock()
            .map_err(|_| anyhow::anyhow!("canonical copy group checkout busy or poisoned"))?
            .take().context("actual original copy group already in transfer")?;
        let result = {
            let actual = std::cell::RefCell::new(&mut group);
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                let mut group = actual.borrow_mut();
                loop {
                    let original = group.settlement_disposals.as_mut()
                        .context("actual canonical disposal inventory absent")?.pop();
                    let Some(original) = original else { break; };
                    let transferred = original.into_outside_group(&self.ready.factory, &self.ready.terminal,
                        &|bytes, steps| self.prepay_outside_continuation(bytes, steps));
                    match transferred {
                        Ok(transferred) => group.groups.as_mut().expect("prepaid canonical group vector").push(transferred),
                        Err((original, error)) => {
                            // Restore the same original into retained capacity
                            // before exposing alias/clock/callback refusal.
                            group.settlement_disposals.as_mut().unwrap().push(original);
                            return Err(error);
                        }
                    }
                }
                for original in group.groups.as_mut().expect("actual canonical group vector").iter_mut() {
                    original.consume_completed_cleanup()?;
                    original.consume_completed_body()?;
                    original.consume_terminal_wrapper()?;
                }
                Ok(())
            })
        };
        *self.inspection_group.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(group);
        result
    }

    pub(super) fn join_original_inspection_contexts(&self) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original inspection context join");
        self.ready.terminal.require_original(&self.ready.factory)?;
        let group_count = self.inspection_group.try_lock()
            .map_err(|_| anyhow::anyhow!("original inspection context quote busy or poisoned"))?
            .as_ref().context("actual original inspection group absent")?
            .groups.as_ref().context("actual canonical copy groups absent")?.len();
        let packet_count = self.destination_packets.try_lock()
            .map_err(|_| anyhow::anyhow!("original primitive context quote busy or poisoned"))?
            .as_ref().context("actual destination packets absent")?.len();
        let steps = u64::try_from(group_count)?.checked_mul(u64::try_from(packet_count)?)
            .and_then(|n| n.checked_mul(8)).and_then(|n| n.checked_add(16))
            .context("inspection context join quote overflow")?;
        self.prepay_outside_continuation(0, steps)?;
        let mut group = self.inspection_group.try_lock()
            .map_err(|_| anyhow::anyhow!("actual inspection context checkout busy or poisoned"))?
            .take().context("actual canonical inspection context already in transfer")?;
        let checkout = (|| {
            self.destination_packets.try_lock()
                .map_err(|_| anyhow::anyhow!("actual destination context checkout busy or poisoned"))?
                .take().context("actual destination contexts already in transfer")
        })();
        let mut packets = match checkout {
            Ok(packets) => packets,
            Err(error) => {
                *self.inspection_group.lock().unwrap_or_else(|p| p.into_inner()) = Some(group);
                return Err(error);
            }
        };
        let result = {
            let group = std::cell::RefCell::new(&mut group);
            let packets = std::cell::RefCell::new(&mut packets);
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                let mut group = group.borrow_mut();
                let mut packets = packets.borrow_mut();
                let groups = group.groups.as_mut().context("actual canonical inspection groups absent")?;
                ensure!(groups.len() == group_count && packets.len() == packet_count,
                    "inspection context inventory changed after precharge");
                for packet in packets.iter_mut() {
                    let parts = packet.parts.as_mut().context("actual destination primitive parts absent")?;
                    parts.leaf_evidence.require_original(&self.ready.factory, &self.ready.terminal)?;
                    for original in groups.iter_mut() {
                        let bound = packet.bound.as_mut().map(|bound| &mut *bound.inspection);
                        original.coalesce_native_packet_context(parts, bound)?;
                    }
                    if parts.domain == Some(lillux_sqlite::OriginalNativeDomain::RuntimeInspection) {
                        let destination = packet.destination.as_mut().context("actual inspection destination custody absent")?;
                        let mut joined = false;
                        for original in groups.iter() {
                            if original.join_destination_context(destination, &self.ready.factory,
                                &self.ready.terminal, &parts.leaf_evidence)? { joined = true; break; }
                        }
                        ensure!(joined, "actual inspection destination has no canonical original copy group");
                    }
                }
                for original in groups.iter_mut() {
                    original.consume_primitive_context()?;
                    original.consume_copy_context()?;
                    original.consume_copy_lease(&self.ready.factory, &self.ready.terminal,
                        &|bytes, steps| self.prepay_outside_continuation(bytes, steps))?;
                    original.retire_original_source_pins(&self.ready.factory, &self.ready.terminal,
                        &|bytes, steps| self.prepay_outside_continuation(bytes, steps))?;
                }
                Ok(())
            })
        };
        *self.destination_packets.lock().unwrap_or_else(|p| p.into_inner()) = Some(packets);
        *self.inspection_group.lock().unwrap_or_else(|p| p.into_inner()) = Some(group);
        result
    }

    pub(super) fn transfer_original_destination_packets(&self) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(), "foreign destination packet supervisor");
        self.ready.terminal.require_original(&self.ready.factory)?;
        let initialized = self.destination_packets.try_lock()
            .map_err(|_| anyhow::anyhow!("destination packet inventory busy or poisoned"))?.is_some();
        if !initialized {
            let count = self.sql_custody.try_lock()
                .map_err(|_| anyhow::anyhow!("actual SQL packet quote busy or poisoned"))?.len();
            ensure!(count > 0, "original destination packet inventory absent; no closure inferred");
            let bytes = u64::try_from(count)?.checked_mul(u64::try_from(std::mem::size_of::<OriginalDestinationPacketProgress>())?)
                .context("original destination packet storage overflow")?;
            let steps = u64::try_from(count)?.checked_mul(8).and_then(|steps| steps.checked_add(8))
                .context("original destination packet work overflow")?;
            self.prepay_outside_continuation(bytes, steps)?;
            let mut destination = self.destination_packets.try_lock()
                .map_err(|_| anyhow::anyhow!("actual destination packet initialization busy or poisoned"))?;
            let mut source = self.sql_custody.try_lock()
                .map_err(|_| anyhow::anyhow!("actual SQL packet initialization busy or poisoned"))?;
            ensure!(destination.is_none() && source.len() == count, "actual destination packet inventory changed after quote");
            let mut actual = Vec::with_capacity(count);
            actual.extend(source.drain(..).map(OriginalDestinationPacketProgress::from_original));
            *destination = Some(std::mem::ManuallyDrop::new(actual));
        }
        let mut actual = self.destination_packets.try_lock()
            .map_err(|_| anyhow::anyhow!("actual destination packet checkout busy or poisoned"))?
            .take().context("actual destination packet inventory absent")?;
        let result = {
            let packets = std::cell::RefCell::new(&mut *actual);
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                for packet in &mut **packets.borrow_mut() {
                    packet.normalize_original()?;
                    if packet.destination.is_some() { continue; }
                    if let Some(refusal) = packet.refusal.take() {
                        match refusal.into_original_inputs() {
                            Ok((factory, authority, error)) => {
                                if !Arc::ptr_eq(&factory, &self.ready.factory) {
                                    packet.refusal = Some(lillux_sqlite::OriginalDestinationCustodyFailure::retain(factory, authority, error));
                                    anyhow::bail!("retained packet refusal selected another original factory");
                                }
                                *packet.authority = Some(authority);
                                let parts = packet.parts.as_mut().expect("normalized original packet");
                                parts.original_error = Some(match parts.original_error.take() {
                                    Some(previous) => error.context(previous),
                                    None => error,
                                });
                            }
                            Err(refusal) => { packet.refusal = Some(refusal); anyhow::bail!("actual destination refusal extraction refused"); }
                        }
                    }
                    let Some(authority) = packet.authority.take() else { continue; };
                    let leaf = &packet.parts.as_ref().expect("normalized genuine packet").leaf_evidence;
                    match authority.into_outside_custody(&self.ready.factory, &self.ready.terminal, leaf,
                        &|bytes, steps| self.prepay_outside_continuation(bytes, steps)) {
                        Ok(destination) => *packet.destination = Some(destination),
                        Err(refusal) => {
                            packet.refusal = Some(refusal);
                            anyhow::bail!("actual destination authority transfer refused; whole packet retained");
                        }
                    }
                }
                Ok(())
            })
        };
        *self.destination_packets.lock().unwrap_or_else(|p| p.into_inner()) = Some(actual);
        result
    }
    pub(super) fn retain_original_sql_custody(
        &self,
        packets: Vec<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
    ) -> Result<()> {
        // PID refusal must return/retain the actual packets, not destroy their
        // owners. Packets themselves preserve all parts on Drop; this method
        // is private to the same prechecked original supervisor callers.
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "original SQL payload supervisor belongs to another process");
        self.sql_custody.lock().unwrap_or_else(|p| p.into_inner()).extend(packets);
        Ok(())
    }
    pub(super) fn with_original_physical_registry(
        &self,
        apply: &mut dyn FnMut(
            &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
            &dyn Fn(u64, u64) -> Result<()>,
        ) -> Result<()>,
    ) -> Result<()> {
        self.initialize_physical_registry()?;
        let mut registry = self.take_physical_registry()?;
        let prepay = |bytes, steps| retained_original_outside_callback(|| self.prepay_outside_continuation(bytes, steps));
        let result = {
            let registry_ref = std::cell::RefCell::new(&mut registry);
            let apply_ref = std::cell::RefCell::new(apply);
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                (apply_ref.borrow_mut())(&mut **registry_ref.borrow_mut(), &prepay)
            })
        };
        self.restore_physical_registry(registry);
        result
    }
    fn initialize_physical_registry(&self) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original physical inventory issuance");
        let mut custody = self.physical_registry.try_lock()
            .map_err(|_| anyhow::anyhow!("original physical inventory issuance busy or poisoned"))?;
        match &*custody {
            OriginalOutsideRegistryCustody::Original(_) => return Ok(()),
            OriginalOutsideRegistryCustody::InFlight => anyhow::bail!("original physical inventory is transferring"),
            OriginalOutsideRegistryCustody::Unissued => {}
        }
        *custody = OriginalOutsideRegistryCustody::InFlight;
        drop(custody);
        let result = (|| -> Result<_> {
            // Finite original inventory: fixed host/controller roots plus up
            // to eight root/namespace holders per admitted SQL/lease resource.
            // The original signed counters still bound the entire allocation.
            let maximum = usize::try_from(self.ready.seal.claim.record.reserved.max_native_connections)?
                .checked_mul(8).and_then(|count| count.checked_add(64))
                .context("original physical inventory capacity overflow")?;
            let prepay = |bytes, steps| retained_original_outside_callback(|| self.prepay_outside_continuation(bytes, steps));
            // SAFETY: actual original post-ACK master and charged finite
            // signed envelope, no renewed clock/budget or external grant.
            unsafe { lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry::issued(maximum, &prepay) }
        })();
        let (next, error) = match result {
            Ok(registry) => (OriginalOutsideRegistryCustody::Original(registry), None),
            Err(error) => (OriginalOutsideRegistryCustody::Unissued, Some(error)),
        };
        *self.physical_registry.lock().unwrap_or_else(|p| p.into_inner()) = next;
        if let Some(error) = error { return Err(error); }
        Ok(())
    }
    fn take_physical_registry(&self) -> Result<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original physical registry transfer");
        let mut custody = self.physical_registry.try_lock()
            .map_err(|_| anyhow::anyhow!("actual original physical registry busy or poisoned"))?;
        ensure!(matches!(&*custody, OriginalOutsideRegistryCustody::Original(_)),
            "actual original physical registry absent or transferring");
        let actual = std::mem::replace(&mut *custody, OriginalOutsideRegistryCustody::InFlight);
        let OriginalOutsideRegistryCustody::Original(registry) = actual else { unreachable!() };
        Ok(registry)
    }
    fn restore_physical_registry(&self, registry: lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry) {
        *self.physical_registry.lock().unwrap_or_else(|p| p.into_inner()) = OriginalOutsideRegistryCustody::Original(registry);
    }
    pub(super) fn register_original_destination_and_inspection_roots(&self) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original destination physical controller");
        self.ready.terminal.require_original(&self.ready.factory)?;
        let mut group = self.inspection_group.try_lock()
            .map_err(|_| anyhow::anyhow!("actual inspection physical checkout busy or poisoned"))?
            .take().context("actual inspection group already transferring")?;
        let checkout = (|| {
            self.destination_packets.try_lock()
                .map_err(|_| anyhow::anyhow!("actual destination physical checkout busy or poisoned"))?
                .take().context("actual destination physical sources already transferring")
        })();
        let mut packets = match checkout {
            Ok(packets) => packets,
            Err(error) => {
                *self.inspection_group.lock().unwrap_or_else(|p| p.into_inner()) = Some(group);
                return Err(error);
            }
        };
        let result = self.with_original_physical_registry(&mut |registry, prepay| {
            for packet in packets.iter_mut() {
                if let Some(bound) = packet.bound.as_mut() {
                    if let Some(binding) = &bound.registration {
                        registry.require_membership(binding)?;
                    } else {
                        let original = bound.root.take().context("actual bound root absent without registration")?;
                        // SAFETY: actual original normalized packet source;
                        // exact original shared registry and prepaid transfer.
                        match unsafe { registry.register_directory(original, prepay) } {
                            Ok(binding) => bound.registration = Some(binding),
                            Err((original, error)) => { *bound.root = Some(original); return Err(error); }
                        }
                    }
                }
                if let Some(destination) = packet.destination.as_mut() {
                    destination.retire_native_references(&self.ready.factory, &self.ready.terminal, prepay)?;
                    unsafe { destination.register_shared_roots(registry, prepay) }?;
                }
            }
            for original in group.groups.as_mut().context("actual original copy groups absent")?.iter_mut() {
                unsafe { original.register_original_shared_roots(&self.ready.factory,
                    &self.ready.terminal, registry, prepay) }?;
            }
            Ok(())
        });
        // Registry callback catches unwind while the complete original packet
        // and group owners remain borrowed here; restore BOTH before refusal.
        *self.destination_packets.lock().unwrap_or_else(|p| p.into_inner()) = Some(packets);
        *self.inspection_group.lock().unwrap_or_else(|p| p.into_inner()) = Some(group);
        result
    }

    pub(super) fn register_original_reservation_removal_parent(self: &Arc<Self>) -> Result<()> {
        let master = self.enter_finalizer_master()?;
        master.require_original(self)?;
        let mut custody = self.removal_parent.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation parent registration busy or poisoned"))?;
        if custody.is_none() {
            *custody = Some(OriginalRemovalParentProgress {
                transferred: false,
                owner: std::mem::ManuallyDrop::new(None), registration: None,
            });
        }
        let mut actual = custody.take().expect("cached actual removal parent progress");
        drop(custody);
        let result = self.with_original_physical_registry(&mut |registry, prepay| {
            if let Some(binding) = &actual.registration {
                return registry.require_membership(binding);
            }
            if actual.owner.is_none() {
                let work: Arc<dyn lillux_sqlite::OriginalWitnessCleanupWork> = master.admission.clone();
                *actual.owner = Some(master.admission.reservation_publication.prepare_original_removal_parent(
                    work, &master.admission.claim.ledger, master.admission.deadline)?);
            }
            let parent = actual.owner.take().expect("actual prepared removal parent cached");
            match unsafe { registry.register_directory(parent, prepay) } {
                Ok(binding) => { actual.registration = Some(binding); Ok(()) }
                Err((parent, error)) => { *actual.owner = Some(parent); Err(error) }
            }
        });
        *self.removal_parent.lock().unwrap_or_else(|p| p.into_inner()) = Some(actual);
        result
    }
    fn transfer_original_reservation_removal_parent(&self,
        removed: &lillux_sqlite::RemovedOriginalWitnessFile,
        retired: &lillux_sqlite::RetiredOriginalWitnessFile,
        work: &Arc<dyn lillux_sqlite::OriginalWitnessCleanupWork>) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign reservation parent transfer");
        let mut actual = self.removal_parent.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation parent transfer busy or poisoned"))?
            .take().context("actual reservation parent registration absent")?;
        let result = self.with_original_physical_registry(&mut |registry, prepay| {
            let binding = actual.registration.as_ref().context("actual reservation parent membership absent")?;
            registry.require_membership(binding)?;
            if actual.transferred { return Ok(()); }
            let count = u64::try_from(registry.registrations()?.count())?;
            prepay(0, count.checked_mul(8).and_then(|steps| steps.checked_add(4))
                .context("original parent membership scan overflow")?)?;
            self.ready.admission.reservation_publication.transfer_original_removal_parent(
                removed, retired, work, registry, binding)?;
            // Actual lower alias ended, while the registry retains SAME parent.
            actual.transferred = true;
            Ok(())
        });
        *self.removal_parent.lock().unwrap_or_else(|p| p.into_inner()) = Some(actual);
        result
    }
    pub(super) fn require_state_shared_roots_registered(&self,
        registry: &lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign State registration census");
        let state = self.state.try_lock()
            .map_err(|_| anyhow::anyhow!("actual State registration census busy or poisoned"))?;
        let OriginalPostAckStateCustody::Physical(physical) = &*state else {
            anyhow::bail!("actual State physical continuation absent");
        };
        physical.require_shared_roots_registered(registry, prepay)
    }
    pub(super) fn register_state_shared_roots(&self) -> Result<()> {
        self.initialize_physical_registry()?;
        let mut custody = self.state.try_lock()
            .map_err(|_| anyhow::anyhow!("actual State physical registration busy or poisoned"))?;
        ensure!(matches!(&*custody, OriginalPostAckStateCustody::Physical(_)),
            "actual State physical token absent");
        let mut registry = self.take_physical_registry()?;
        let actual = std::mem::replace(&mut *custody, OriginalPostAckStateCustody::Handoff);
        drop(custody);
        let OriginalPostAckStateCustody::Physical(mut physical) = actual else { unreachable!() };
        let prepay = |bytes, steps| retained_original_outside_callback(|| self.prepay_outside_continuation(bytes, steps));
        // SAFETY: actual State and registry owners moved outside their gates;
        // register exact retained roots/locks on the same original envelope.
        let result = unsafe { physical.register_shared_roots(&mut registry, &prepay) };
        // Cache BOTH partial original owners before a late refusal is exposed.
        self.restore_physical_registry(registry);
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = OriginalPostAckStateCustody::Physical(physical);
        result
    }
    /// # Safety
    /// The original supervisor has registered EVERY actual State, Runtime,
    /// Operational, lower-payload and account/authority owner, and permanently
    /// excludes new registrations. Descriptor receipts remain separate from
    /// metadata retirement, region exclusion and durable account settlement.
    pub(super) unsafe fn retire_non_account_physical_descriptors(self: &Arc<Self>) -> Result<()> {
        let mut registry = self.take_physical_registry()?;
        let prepay = |bytes, steps| retained_original_outside_callback(|| self.prepay_outside_continuation(bytes, steps));
        let prepared = (|| -> Result<()> {
            // SAFETY: complete actual inventory and original admission closure
            // are the caller's retained contract; no numeric census grants it.
            unsafe { registry.seal_inventory()?; registry.prepare_descriptor_owners(&prepay) }
        })();
        self.restore_physical_registry(registry);
        prepared?;
        let count = {
            let custody = self.physical_registry.try_lock()
                .map_err(|_| anyhow::anyhow!("original physical receipt census busy or poisoned"))?;
            let OriginalOutsideRegistryCustody::Original(registry) = &*custody else {
                anyhow::bail!("actual original physical registry absent");
            };
            registry.registrations()?.count()
        };
        self.prepay_outside_continuation(
            u64::try_from(count)?.checked_mul(1024).and_then(|bytes| bytes.checked_add(512))
                .context("shared descriptor input storage overflow")?,
            u64::try_from(count)?.checked_mul(4).and_then(|steps| steps.checked_add(4))
                .context("shared descriptor input work overflow")?,
        )?;
        for index in 0..count {
            let registration = {
                let custody = self.physical_registry.try_lock()
                    .map_err(|_| anyhow::anyhow!("original physical membership snapshot busy or poisoned"))?;
                let OriginalOutsideRegistryCustody::Original(registry) = &*custody else {
                    anyhow::bail!("actual original physical registry absent");
                };
                registry.registrations()?.nth(index).context("actual original descriptor membership absent")?
            };
            if registration.is_operator_lock()? { continue; }
            let owner = registration.owner()?;
            let retained = self.descriptor_retirements.try_lock()
                .map_err(|_| anyhow::anyhow!("original shared descriptor registration busy or poisoned"))?
                .iter().find(|actual| Arc::ptr_eq(&actual.owner, &owner))
                .map(|actual| (actual.inputs.clone(), actual.receipt.clone()));
            let (inputs, receipt) = if let Some(retained) = retained { retained } else {
                let authority: Arc<dyn lillux_sqlite::OriginalOutsideFinalizerFileRetirementAuthority> =
                    Arc::new(OriginalOutsideSharedDescriptorAuthority {
                        post_ack: Arc::downgrade(self), registration: registration.clone(), owner: owner.clone(),
                    });
                // SAFETY: actual original registry membership/master/clock,
                // exact real FD and input storage irreversibly prepaid above.
                let inputs = Arc::new(unsafe { lillux_sqlite::OriginalOutsideFinalizerFileRetirementInputs::issued(owner.clone(), authority) }?);
                self.descriptor_retirements.lock().unwrap_or_else(|p| p.into_inner())
                    .push(OriginalOutsideDescriptorRetirement { owner: owner.clone(), inputs: inputs.clone(), receipt: None });
                (inputs, None)
            };
            let receipt = if let Some(receipt) = receipt { receipt } else {
                let receipt = Arc::new(owner.retire_after_outside_handoff(&inputs)?);
                self.descriptor_retirements.lock().unwrap_or_else(|p| p.into_inner())
                    .iter_mut().find(|actual| Arc::ptr_eq(&actual.owner, &owner))
                    .expect("cached exact original shared descriptor").receipt = Some(receipt.clone());
                receipt
            };
            receipt.require_original(&owner)?;
            let mut custody = self.physical_registry.try_lock()
                .map_err(|_| anyhow::anyhow!("actual shared descriptor receipt registration busy or poisoned"))?;
            let OriginalOutsideRegistryCustody::Original(registry) = &mut *custody else {
                anyhow::bail!("actual original physical registry absent while caching receipt");
            };
            registry.retain_retirement(&registration, receipt)?;
        }
        let custody = self.physical_registry.try_lock()
            .map_err(|_| anyhow::anyhow!("original shared physical completion busy or poisoned"))?;
        let OriginalOutsideRegistryCustody::Original(registry) = &*custody else {
            anyhow::bail!("actual original physical registry absent");
        };
        ensure!(registry.non_account_descriptors_retired(), "actual non-account original descriptors remain pending; operator remains held for durable account work");
        Ok(())
    }
    pub(super) fn prepare_state_physical_custody(&self) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign outside State physical handoff");
        let mut custody = self.state.try_lock()
            .map_err(|_| anyhow::anyhow!("outside State physical handoff busy or poisoned"))?;
        if matches!(&*custody, OriginalPostAckStateCustody::Physical(_)) { return Ok(()); }
        ensure!(matches!(&*custody, OriginalPostAckStateCustody::Outside(_)),
            "actual outside State continuation absent");
        let original = std::mem::replace(&mut *custody, OriginalPostAckStateCustody::Handoff);
        drop(custody);
        let OriginalPostAckStateCustody::Outside(mut outside) = original else { unreachable!() };
        // SAFETY: cache these exact original SQL payloads in this retained
        // outside supervisor before the State physical transfer or any effect.
        let packets = match unsafe { outside.take_original_sql_custody() } {
            Ok(packets) => packets,
            Err(error) => {
                *self.state.lock().unwrap_or_else(|p| p.into_inner()) = OriginalPostAckStateCustody::Outside(outside);
                return Err(error);
            }
        };
        self.sql_custody.lock().unwrap_or_else(|p| p.into_inner()).extend(packets);
        let prepay = |bytes, steps| retained_original_outside_callback(|| self.prepay_outside_continuation(bytes, steps));
        // SAFETY: the actual native receipts and their resource payloads were
        // transferred and cached above; the same original outside clock and
        // envelope prepay the concrete physical custody before owner movement.
        let result = unsafe { outside.into_physical_custody(&prepay) };
        let (next, error) = match result {
            Ok(physical) => (OriginalPostAckStateCustody::Physical(physical), None),
            Err((outside, error)) => (OriginalPostAckStateCustody::Outside(outside), Some(error)),
        };
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = next;
        if let Some(error) = error { return Err(error); }
        // Register shared physical aliases with the outside supervisor before
        // attempting uniqueness or physical closure of any subsystem token.
        self.ready.admission.checkpoint_original()
    }
    pub(super) fn retire_outside_state_native_references(&self,
        retire_aliases: &dyn Fn(&lillux_sqlite::OriginalNativeConnectionTerminal) -> Result<()>,
    ) -> Result<()> {
        self.ready.factory.require_original_process()?;
        self.ready.admission.checkpoint_original()?;
        let mut custody = self.state.try_lock()
            .map_err(|_| anyhow::anyhow!("outside State custody busy or poisoned"))?;
        ensure!(matches!(&*custody, OriginalPostAckStateCustody::Outside(_)),
            "actual State outside continuation absent");
        let original = std::mem::replace(&mut *custody, OriginalPostAckStateCustody::Handoff);
        drop(custody);
        let OriginalPostAckStateCustody::Outside(mut outside) = original else { unreachable!() };
        let result = {
            let actual = std::cell::RefCell::new(&mut outside);
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                let mut actual = actual.borrow_mut();
                actual.prepare_destination_join(&self.ready.factory, &self.ready.terminal,
                    &|bytes, steps| self.prepay_outside_continuation(bytes, steps))?;
                actual.retire_native_references(&self.ready.factory, &self.ready.terminal, retire_aliases)
            })
        };
        // Restore the exact partially transferred continuation on every refusal.
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) =
            OriginalPostAckStateCustody::Outside(outside);
        result
    }
}

impl OriginalPostAckFinalization {
    /// Prepay one actual bounded consuming Runtime/finalizer handoff. This
    /// changes no completion phase and does not reread retired source pins.
    pub(super) fn prepay_finalizer_transfer(&self, memory_bytes: u64, steps: u64) -> Result<()> {
        self.require_state_ack()?;
        self.debit_finalizer_transfer(memory_bytes, steps)
    }
    fn debit_finalizer_transfer(&self, memory_bytes: u64, steps: u64) -> Result<()> {
        // The authentic State producer quotes its actual SQL aliases, custody
        // packets and generation holders. Charge that complete quote against
        // the same original signed envelope; a lease-only helper ceiling
        // cannot bound the SQL owner graph. These are irreversible cumulative
        // debits below, never a new allocation or cleanup grant.
        ensure!(
            memory_bytes > 0 && steps > 0,
            "original finalizer transfer requires a positive concrete quote"
        );
        self.ready.require_custody()?;
        self.ready.admission.debit_original(steps)?;
        let mut used = self
            .ready
            .admission
            .claim
            .copy_usage
            .try_lock()
            .map_err(|_| {
                anyhow::anyhow!("original finalizer transfer memory debit busy or poisoned")
            })?;
        let memory = used
            .buffer_bytes
            .checked_add(memory_bytes)
            .context("original finalizer transfer memory overflow")?;
        ensure!(
            memory.checked_add(used.native_storage_bytes)
                    .and_then(|bytes| bytes.checked_add(used.resident_storage_bytes)).is_some_and(
                |total| total <= self.ready.admission.claim.record.reserved.max_memory_bytes
            ),
            "original finalizer transfer exceeds shared memory envelope"
        );
        used.buffer_bytes = memory;
        drop(used);
        self.ready.require_custody()
    }
    pub(super) fn require_original(
        &self,
        ready: &Arc<OriginalReadyForFinalAck>,
        state: &Arc<ryeos_state::state_db::ClosedOriginalState>,
        acknowledgement: &Arc<ryeos_state::state_db::OriginalStateAcknowledgement>,
        operation: &ThreadHistoryDiscardOperationRef,
    ) -> Result<()> {
        let (actual_state, actual_acknowledgement) = self.original_state_inputs()?;
        ensure!(
            Arc::ptr_eq(ready, &self.ready)
                && Arc::ptr_eq(state, &actual_state)
                && Arc::ptr_eq(acknowledgement, &actual_acknowledgement)
                && operation == &self.operation,
            "post-ACK finalization selects foreign actual owner/effect"
        );
        self.require_state_ack()?;
        self.ready.require_original(
            &self.ready.seal.claim,
            &self.ready.seal.work,
            &self.ready.factory,
            &self.ready.terminal,
        )
    }
}
impl OriginalInspectionSettlement {
    pub(super) fn begin_post_ack_finalization(
        self: &Arc<Self>,
        ready: &Arc<OriginalReadyForFinalAck>,
        state: &Arc<ryeos_state::state_db::ClosedOriginalState>,
        acknowledgement: &Arc<ryeos_state::state_db::OriginalStateAcknowledgement>,
        operation: &ThreadHistoryDiscardOperationRef,
    ) -> Result<Arc<OriginalPostAckFinalization>> {
        let result = (|| -> Result<Arc<OriginalPostAckFinalization>> {
            // Pure actual ACK body/owner check before retaining the join;
            // there is no clock/FS/source reread or synthetic ACK promotion.
            acknowledgement.require_original(state, &ready.factory, &ready.terminal, operation)?;
            ensure!(
                Arc::ptr_eq(&ready.seal, &self.seal)
                    && Arc::ptr_eq(&ready.admission, &self.admission),
                "post-ACK ready belongs to foreign settlement"
            );
            let cached = {
                let progress = self
                    .progress
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("post-ACK custody busy or poisoned"))?;
                ensure!(
                    progress
                        .final_ack
                        .as_ref()
                        .is_some_and(|original| Arc::ptr_eq(original, ready)),
                    "post-ACK requires actual retained ready producer"
                );
                progress.post_ack.clone()
            };
            let owner = if let Some(owner) = cached {
                owner
            } else {
                // Charge the actual concrete parent, including its inline
                // account-lock progress, before allocating its Arc/control.
                self.admission.debit_original(4)?;
                let bytes = std::mem::size_of::<OriginalPostAckFinalization>()
                    .checked_add(2 * std::mem::size_of::<usize>())
                    .context("original post-ACK parent storage overflow")?;
                let mut used = self.admission.claim.copy_usage.try_lock()
                    .map_err(|_| anyhow::anyhow!("original post-ACK parent storage debit busy or poisoned"))?;
                let memory = used.buffer_bytes.checked_add(u64::try_from(bytes)?)
                    .context("original post-ACK parent memory overflow")?;
                ensure!(memory.checked_add(used.native_storage_bytes)
                    .and_then(|total| total.checked_add(used.resident_storage_bytes))
                    .is_some_and(|total| total <= self.admission.claim.record.reserved.max_memory_bytes),
                    "original post-ACK parent exceeds same signed memory envelope");
                used.buffer_bytes = memory;
                drop(used);
                self.admission.checkpoint_original()?;
                let owner = Arc::new(OriginalPostAckFinalization {
                    removal_parent: Mutex::new(None),
                    account_intent: Mutex::new(None),
                    primary_state_lock: Mutex::new(None),
                    offline_policy: Mutex::new(None),
                    account_state_lock: Mutex::new(None),
                    ready: ready.clone(),
                    state: Mutex::new(OriginalPostAckStateCustody::Original {
                        state: state.clone(),
                        acknowledgement: acknowledgement.clone(),
                    }),
                    sql_custody: Mutex::new(Vec::new()),
                    destination_packets: Mutex::new(None),
                    inspection_group: Mutex::new(None),
                    physical_registry: Mutex::new(OriginalOutsideRegistryCustody::Unissued),
                    descriptor_retirements: Mutex::new(Vec::new()),
                    metadata_retirements: Mutex::new(Vec::new()),
                    operation: operation.clone(),
                    native_lifelines: Mutex::new(None),
                    master: Mutex::new(None),
                });
                let mut progress = self
                    .progress
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("post-ACK registration busy or poisoned"))?;
                ensure!(
                    progress.post_ack.is_none(),
                    "post-ACK custody registration conflict"
                );
                // Store actual successful State ACK and all actual owners
                // before fallible late original clock/funding checks.
                progress.post_ack = Some(owner.clone());
                owner
            };
            owner.require_original(ready, state, acknowledgement, operation)?;
            Ok(owner)
        })();
        result.map_err(|error| self.retain_error(error))
    }
}

struct OriginalRuntimeFinalizerAdmission {
    post_ack: Arc<OriginalPostAckFinalization>,
    producer: Arc<crate::runtime_db::OriginalRuntimeFinalizer>,
    owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
}
impl lillux_sqlite::OriginalFinalizerFileRetirementAuthority for OriginalRuntimeFinalizerAdmission {
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        owner: &Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> Result<()> {
        ensure!(
            Arc::ptr_eq(factory, &self.post_ack.ready.factory)
                && Arc::ptr_eq(terminal, &self.post_ack.ready.terminal)
                && Arc::ptr_eq(owner, &self.owner),
            "Runtime retirement selects foreign post-ACK/native/descriptor owner"
        );
        // This producer checks its actual RuntimeMutation close and exact
        // private transferred directory/namespace slot; never a caller list.
        self.producer.require_original_file(factory, owner)?;
        self.post_ack.require_state_ack()?;
        self.post_ack.ready.require_custody()
    }
    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        self.post_ack.ready.admission.debit_original(steps)
    }
}
impl OriginalPostAckFinalization {
    /// Durable intent keeps the actual reservation charged. Its publisher
    /// descriptor remains an explicit account-tail owner, not a close receipt.
    pub(super) fn publish_original_account_settlement_intent(self: &Arc<Self>,
        settlement: &Arc<OriginalInspectionSettlement>) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original account settlement intent");
        ensure!(Arc::ptr_eq(&settlement.admission, &self.ready.admission)
            && Arc::ptr_eq(&settlement.seal, &self.ready.seal),
            "account intent selects another original settlement");
        let pending_publisher = {
            let progress = settlement.progress.try_lock()
                .map_err(|_| anyhow::anyhow!("original account intent parent lookup busy or poisoned"))?;
            ensure!(progress.post_ack.as_ref().is_some_and(|parent| Arc::ptr_eq(parent, self)),
                "account intent is not actual cached post-ACK parent");
            ensure!(progress.terminal.acknowledged,
                "account intent requires actual durable pending terminal acknowledgement");
            progress.terminal.owner.clone().context("actual pending terminal publisher absent")?
        };
        // Snapshot the SAME created publisher under its gate, then fund and
        // revalidate its exact coordinate/inode/bytes outside every gate.
        self.prepay_outside_continuation(128, 8)?;
        // This original helper irreversibly debits BOTH read_bytes and
        // buffer_bytes by MAX+1 before the owned read Vec is allocated. The
        // later two-record reserve funds construction/signing separately.
        self.ready.admission.debit_original_read((MAX_RECORD_BYTES + 1) as u64)?;
        let pending_name = super::inspection_destination::terminal_name(&self.ready.seal.claim.record.operation_id);
        pending_publisher.require_original(&self.ready.seal.claim.ledger, OsStr::new(&pending_name))?;
        let pending_identity = pending_publisher.identity_original()?;
        ensure!(pending_publisher.read_original(MAX_RECORD_BYTES)? == settlement.signed_terminal,
            "account intent actual pending publisher bytes changed");
        let pending_entry = self.ready.seal.claim.ledger.entry_no_follow(OsStr::new(&pending_name))?
            .context("account intent actual pending coordinate disappeared")?;
        ensure!(pending_entry.entry_type == lillux::PinnedEntryType::Regular
            && pending_entry.containing_device == pending_identity.device()
            && pending_entry.inode == pending_identity.inode(),
            "account intent actual pending publisher inode changed");
        self.require_state_ack()?;
        self.ready.require_original(&self.ready.seal.claim, &self.ready.seal.work,
            &self.ready.factory, &self.ready.terminal)?;
        self.prepay_outside_continuation(0, 4)?;
        let mut custody = self.account_intent.try_lock()
            .map_err(|_| anyhow::anyhow!("original account intent checkout busy or poisoned"))?;
        if custody.is_none() {
            *custody = Some(OriginalAccountIntentProgress {
                records_reserved: false, publisher_reserved: false, record: std::mem::ManuallyDrop::new(None),
                signed: std::mem::ManuallyDrop::new(None), name: std::mem::ManuallyDrop::new(None),
                publication: std::mem::ManuallyDrop::new(OriginalRecordPublication::default()),
            });
        }
        let mut actual = custody.take().expect("cached original account intent progress");
        drop(custody);
        let result = {
            let progress = std::cell::RefCell::new(&mut actual);
            lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
                let mut actual = progress.borrow_mut();
                if !actual.records_reserved {
                    self.ready.admission.reserve_records((2 * MAX_RECORD_BYTES) as u64)?;
                    actual.records_reserved = true;
                }
                if !actual.publisher_reserved {
                    // Publisher/control/name prepay only. Compound verification
                    // and serialization peak quote remains a source obligation.
                    self.prepay_outside_continuation(u64::try_from(
                        std::mem::size_of::<lillux_sqlite::RetainedOriginalWitnessFile>()
                            + 2 * std::mem::size_of::<usize>() + 128)?, 16)?;
                    actual.publisher_reserved = true;
                }
                if actual.record.is_none() {
                    let authentic = ClaimRecord::verified_original(&self.ready.seal.claim.signed, &self.ready.seal.claim.signer)?;
                    ensure!(serde_json::to_value(&authentic)? == serde_json::to_value(&self.ready.seal.claim.record)?,
                        "account intent changes the actual original claim");
                    let record = super::creation_evidence::OriginalAccountSettlementIntentRecord::from_original_records(
                        &settlement.signed_terminal, &self.ready.signed, &self.ready.seal.claim.signed,
                        &self.ready.seal.claim.signer)?;
                    *actual.record = Some(record);
                }
                if actual.signed.is_none() {
                    self.ready.admission.debit_original(2)?;
                    let record = actual.record.as_ref().expect("cached actual account intent");
                    self.ready.admission.debit_original(1)?;
                    let storage = lillux::cas::canonical_record_storage_quote(record, MAX_RECORD_BYTES)?;
                    self.ready.seal.claim.debit_original_record_working_storage(storage.working_bytes())?;
                    let signed = record.signed_bytes(&self.ready.seal.claim.signer, &storage)?;
                    ensure!(signed.len() <= MAX_RECORD_BYTES, "actual account intent exceeds original record capacity");
                    *actual.signed = Some(signed);
                }
                if actual.name.is_none() {
                    *actual.name = Some(super::inspection_destination::account_settlement_intent_name(
                        &self.ready.seal.claim.record.operation_id));
                }
                // Split immutable bytes/name from the actual mutable publisher
                // without a bookkeeping gate spanning any filesystem effect.
                let OriginalAccountIntentProgress { signed, name, publication, .. } = &mut **actual;
                self.ready.admission.publish_record(name.as_ref().expect("cached intent coordinate"),
                    signed.as_ref().expect("cached signed intent"), publication)
            })
        };
        // The complete original File, bytes, attempted/error/durable prefixes
        // are cached before any callback refusal or panic escapes this stage.
        *self.account_intent.lock().unwrap_or_else(|p| p.into_inner()) = Some(actual);
        result
    }
    pub(super) fn retain_original_offline_parent_inputs(self: &Arc<Self>,
        primary: ryeos_node_runtime::state_lock::StateLock,
        policy: super::offline_policy::OriginalOfflinePolicyOutside) -> std::result::Result<(), OriginalPrimaryStateLockFailure> {
        // Acquire both bookkeeping gates before moving either actual owner.
        // No callbacks, path effects or descriptor conversions span the gates.
        let checked = (|| -> Result<_> {
            let lock = self.primary_state_lock.try_lock()
                .map_err(|_| anyhow::anyhow!("actual primary State lock custody busy or poisoned"))?;
            let policy = self.offline_policy.try_lock()
                .map_err(|_| anyhow::anyhow!("actual offline policy custody busy or poisoned"))?;
            ensure!(lock.is_none() && policy.is_none(), "actual offline parent inputs already retained");
            Ok((lock, policy))
        })();
        let (mut lock_custody, mut policy_custody) = match checked {
            Ok(custody) => custody,
            Err(error) => return Err(OriginalPrimaryStateLockFailure {
                original: std::mem::ManuallyDrop::new(primary), policy: std::mem::ManuallyDrop::new(policy),
                parent: self.clone(), error }),
        };
        *lock_custody = Some(OriginalPrimaryStateLockProgress {
            source: std::mem::ManuallyDrop::new(Some(primary)), owner: std::mem::ManuallyDrop::new(None), registration: None,
        });
        *policy_custody = Some(policy);
        Ok(())
    }
    pub(super) fn register_original_primary_state_lock(self: &Arc<Self>) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(), "foreign original primary State lock handoff");
        self.retain_original_account_state_lock()?;
        let mut actual = self.primary_state_lock.try_lock()
            .map_err(|_| anyhow::anyhow!("actual primary State lock checkout busy or poisoned"))?
            .take().context("actual primary State lock absent or transferring")?;
        let result = self.with_original_physical_registry(&mut |registry, prepay| {
            if let Some(binding) = &actual.registration { return registry.require_membership(binding); }
            if let Some(primary) = actual.source.take() {
                match primary.into_original_owner() {
                    Ok(owner) => *actual.owner = Some(owner),
                    Err((primary, error)) => { *actual.source = Some(primary); return Err(error); }
                }
            }
            let canonical = self.ready.admission.original.state_lifetime.retain_original_owner()?;
            ensure!(actual.owner.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, &canonical)),
                "actual primary State lock differs from original account canonical allocation");
            #[cfg(target_os = "linux")]
            {
                let owner = actual.owner.take().context("actual primary State lock owner absent without membership")?;
                match unsafe { registry.register_operator_lock(owner, prepay) } {
                    Ok(binding) => actual.registration = Some(binding),
                    Err((owner, error)) => { *actual.owner = Some(owner); return Err(error); }
                }
                Ok(())
            }
            #[cfg(not(target_os = "linux"))]
            { let _ = prepay; anyhow::bail!("original primary operator lock requires Linux custody") }
        });
        *self.primary_state_lock.lock().unwrap_or_else(|p| p.into_inner()) = Some(actual);
        result
    }
    pub(super) fn register_original_offline_policy_root(&self) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(), "foreign original offline policy handoff");
        let mut actual = self.offline_policy.try_lock()
            .map_err(|_| anyhow::anyhow!("original offline policy root checkout busy or poisoned"))?
            .take().context("original offline policy absent or transferring")?;
        let result = self.with_original_physical_registry(&mut |registry, prepay| {
            ensure!(actual.process == std::process::id(), "foreign original retained offline policy");
            if let Some(binding) = &actual.registration { return registry.require_membership(binding); }
            if actual.shared_root.is_none() {
                registry.prepay_transfer(prepay,
                    u64::try_from(std::mem::size_of::<lillux::PinnedDirectory>() + 2 * std::mem::size_of::<usize>())?, 4)?;
                ensure!(actual.process == std::process::id(), "offline policy transfer process changed during prepay");
                let root = actual.app_root.take().context("original offline policy root absent")?;
                *actual.shared_root = Some(Arc::new(root));
            }
            let root = actual.shared_root.take().expect("cached original policy root");
            match unsafe { registry.register_directory(root, prepay) } {
                Ok(binding) => actual.registration = Some(binding),
                Err((root, error)) => { *actual.shared_root = Some(root); return Err(error); }
            }
            Ok(())
        });
        *self.offline_policy.lock().unwrap_or_else(|p| p.into_inner()) = Some(actual);
        result
    }
    pub(super) fn issue_runtime_retirement_inputs(
        self: &Arc<Self>,
        producer: &Arc<crate::runtime_db::OriginalRuntimeFinalizer>,
        owner: &Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> Result<lillux_sqlite::OriginalFinalizerFileRetirementInputs> {
        producer.require_original_file(&self.ready.factory, owner)?;
        // Original grant/authority storage is irreversibly paid before
        // allocation. No producer/booking guard spans any later native close.
        self.prepay_finalizer_transfer(1024, 4)?;
        let authority: Arc<dyn lillux_sqlite::OriginalFinalizerFileRetirementAuthority> =
            Arc::new(OriginalRuntimeFinalizerAdmission {
                post_ack: self.clone(),
                producer: producer.clone(),
                owner: owner.clone(),
            });
        // Safety: exact actual private Runtime producer membership above,
        // retained genuine State ACK/native terminal and original funded
        // clock/cleanup custody; future uses revalidate that same join.
        unsafe {
            lillux_sqlite::OriginalFinalizerFileRetirementInputs::issued(
                self.ready.factory.clone(),
                self.ready.terminal.clone(),
                owner.clone(),
                authority,
            )
        }
    }
}

struct OriginalStateFinalizerAdmission {
    post_ack: Arc<OriginalPostAckFinalization>,
    producer: Arc<ryeos_state::state_db::OriginalStateFinalizer>,
    owner: Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
}
impl lillux_sqlite::OriginalFinalizerFileRetirementAuthority for OriginalStateFinalizerAdmission {
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        owner: &Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> Result<()> {
        ensure!(
            Arc::ptr_eq(factory, &self.post_ack.ready.factory)
                && Arc::ptr_eq(terminal, &self.post_ack.ready.terminal)
                && Arc::ptr_eq(owner, &self.owner),
            "State retirement selects foreign post-ACK/native/descriptor owner"
        );
        // Pure pointer/body join: this exact ACKed State cached this producer.
        let (state, _) = self.post_ack.original_state_inputs()?;
        state.require_original_finalizer(&self.producer)?;
        drop(state);
        self.producer.require_original_file(factory, owner)?;
        self.post_ack.require_state_ack()?;
        self.post_ack.ready.require_custody()
    }
    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        self.post_ack.ready.admission.debit_original(steps)
    }
}
impl OriginalPostAckFinalization {
    pub(super) fn issue_state_retirement_inputs(
        self: &Arc<Self>,
        producer: &Arc<ryeos_state::state_db::OriginalStateFinalizer>,
        owner: &Arc<lillux_sqlite::RetainedOriginalFinalizerFile>,
    ) -> Result<lillux_sqlite::OriginalFinalizerFileRetirementInputs> {
        let (state, _) = self.original_state_inputs()?;
        state.require_original_finalizer(producer)?;
        drop(state);
        producer.require_original_file(&self.ready.factory, owner)?;
        self.prepay_finalizer_transfer(1024, 4)?;
        let authority: Arc<dyn lillux_sqlite::OriginalFinalizerFileRetirementAuthority> =
            Arc::new(OriginalStateFinalizerAdmission {
                post_ack: self.clone(),
                producer: producer.clone(),
                owner: owner.clone(),
            });
        // Safety: this actual ACKed State retains this exact producer, whose
        // private slot owns this transferred original File; native terminal,
        // State ACK and original charged clock/work/storage remain genuine.
        unsafe {
            lillux_sqlite::OriginalFinalizerFileRetirementInputs::issued(
                self.ready.factory.clone(),
                self.ready.terminal.clone(),
                owner.clone(),
                authority,
            )
        }
    }
}

/// Actual native admission lifelines moved into original post-ACK custody.
/// Ownership transfer is not namespace/State-lock disposal or completion.
pub(super) struct OriginalTransferredNativeAdmission {
    admission: Arc<OriginalNativeQuotaAdmission>,
    lifelines: Mutex<Option<OriginalNativeAdmissionLifelines>>,
}
impl OriginalTransferredNativeAdmission {
    fn join_original_state_lock(&self, post_ack: &Arc<OriginalPostAckFinalization>,
        canonical: &Arc<lillux::ExactExclusiveFileLock>) -> Result<()> {
        ensure!(self.admission.claim.record.resident_process == std::process::id(),
            "foreign original native account State lock join");
        self.require_original(post_ack)?;
        let custody = self.lifelines.try_lock()
            .map_err(|_| anyhow::anyhow!("actual native account lifelines join busy or poisoned"))?;
        custody.as_ref().context("actual native account lifelines absent")?
            .state_lifetime.join_original_owner(canonical)
    }
    fn retain_original_state_lock(&self,
        post_ack: &Arc<OriginalPostAckFinalization>) -> Result<Arc<lillux::ExactExclusiveFileLock>> {
        ensure!(self.admission.claim.record.resident_process == std::process::id(),
            "foreign original native account State lock census");
        self.require_original(post_ack)?;
        let custody = self.lifelines.try_lock()
            .map_err(|_| anyhow::anyhow!("actual native account lifelines busy or poisoned"))?;
        custody.as_ref().context("actual native account lifelines absent")?
            .state_lifetime.retain_original_owner()
    }
    fn transfer_actual(&self) -> Result<()> {
        // Both descriptor-owner guards acquired BEFORE moving any owner;
        // no FS/clock/native effect or fallible join spans the actual move.
        let mut destination = self
            .lifelines
            .try_lock()
            .map_err(|_| anyhow::anyhow!("transferred native lifeline owner busy or poisoned"))?;
        if destination.is_some() {
            return Ok(());
        }
        let mut source =
            self.admission.lifelines.try_lock().map_err(|_| {
                anyhow::anyhow!("original native lifeline handoff busy or poisoned")
            })?;
        let actual = source
            .take()
            .context("actual native lifelines absent without retained handoff")?;
        *destination = Some(actual);
        Ok(())
    }
    pub(super) fn require_original(
        &self,
        post_ack: &Arc<OriginalPostAckFinalization>,
    ) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.admission.claim, &post_ack.ready.seal.claim),
            "transferred native admission selects foreign claim"
        );
        let retained = post_ack
            .native_lifelines
            .try_lock()
            .map_err(|_| {
                anyhow::anyhow!("actual post-ACK native handoff receipt busy or poisoned")
            })?
            .as_ref()
            .is_some_and(|actual| std::ptr::eq(actual.as_ref(), self));
        ensure!(
            retained,
            "native lifeline receipt is not actual cached post-ACK owner"
        );
        post_ack.require_state_ack()?;
        ensure!(
            post_ack.ready.terminal.arena_disposition()?
                == lillux_sqlite::OriginalNativeArenaDisposition::Released,
            "native lifeline transfer requires actual original backend release"
        );
        let destination = self
            .lifelines
            .try_lock()
            .map_err(|_| anyhow::anyhow!("actual transferred lifelines busy or poisoned"))?;
        ensure!(
            destination.is_some(),
            "actual native lifeline transfer pending"
        );
        let source = self
            .admission
            .lifelines
            .try_lock()
            .map_err(|_| anyhow::anyhow!("native callback lifelines busy or poisoned"))?;
        ensure!(
            source.is_none(),
            "native callback still owns transferred lifelines"
        );
        drop(source);
        drop(destination);
        post_ack.ready.require_custody()
    }
}
impl OriginalPostAckFinalization {
    /// Join only the actual copy-account and transferred native lifeline lock
    /// aliases. This is a finite parent join, not a complete lock census or a
    /// registry seal. The actual registered file stays held through settlement.
    pub(super) fn retain_original_account_state_lock(self: &Arc<Self>) -> Result<()> {
        ensure!(self.ready.seal.claim.record.resident_process == std::process::id(),
            "foreign original account State lock join");
        self.require_state_ack()?;
        let master = self.master.try_lock()
            .map_err(|_| anyhow::anyhow!("original account lock master lookup busy or poisoned"))?
            .clone().context("actual original account master absent")?;
        master.checkpoint_original()?;
        ensure!(Arc::ptr_eq(&master.admission, &self.ready.admission)
            && Arc::ptr_eq(&master.admission.claim, &self.ready.seal.claim),
            "original lock join selects another account parent");
        if self.account_state_lock.try_lock()
            .map_err(|_| anyhow::anyhow!("original account lock joined-stage lookup busy or poisoned"))?
            .as_ref().is_some_and(|actual| actual.aliases_joined) {
            return Ok(());
        }
        let native = self.native_lifelines.try_lock()
            .map_err(|_| anyhow::anyhow!("original account lock native lookup busy or poisoned"))?
            .clone().context("actual transferred native lifelines absent")?;
        self.prepay_outside_continuation(0, 12)?;
        // Each holder retains its canonical SAME Arc before returning a
        // borrowed counted alias; every refusal leaves both originals intact.
        let copy_lock = master.admission.original.state_lifetime.retain_original_owner()?;
        let native_lock = native.retain_original_state_lock(self)?;
        copy_lock.require_original_process()?;
        native_lock.require_original_process()?;
        ensure!(Arc::ptr_eq(&copy_lock, &native_lock),
            "actual copy account and native lifelines retain different State locks");
        let mut retained = self.account_state_lock.try_lock()
            .map_err(|_| anyhow::anyhow!("original account State lock join busy or poisoned"))?;
        if let Some(actual) = retained.as_ref() {
            ensure!(actual.owner.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, &copy_lock)),
                "original account State lock canonical owner changed");
        } else {
            *retained = Some(OriginalAccountStateLockProgress {
                owner: std::mem::ManuallyDrop::new(Some(copy_lock)), registration: None, aliases_joined: false,
            });
        }
        let canonical = Arc::clone(retained.as_ref().expect("cached actual account State lock")
            .owner.as_ref().expect("actual canonical lock before registration"));
        drop(retained);
        // The whole canonical parent is cached before either duplicate
        // surrender. A partial join is memoized in its original source holder.
        master.admission.original.state_lifetime.join_original_owner(&canonical)?;
        native.join_original_state_lock(self, &canonical)?;
        self.account_state_lock.lock().unwrap_or_else(|p| p.into_inner())
            .as_mut().expect("cached actual account lock join").aliases_joined = true;
        Ok(())
    }
    pub(super) fn transfer_native_admission_lifelines(
        self: &Arc<Self>,
    ) -> Result<Arc<OriginalTransferredNativeAdmission>> {
        self.require_state_ack()?;
        ensure!(
            self.ready.terminal.arena_disposition()?
                == lillux_sqlite::OriginalNativeArenaDisposition::Released,
            "native admission transfer requires released original backend"
        );
        let cached = self
            .native_lifelines
            .try_lock()
            .map_err(|_| anyhow::anyhow!("post-ACK native custody busy or poisoned"))?
            .clone();
        let owner = if let Some(owner) = cached {
            owner
        } else {
            self.prepay_finalizer_transfer(4096, 32)?;
            let admission = {
                self.ready
                    .seal
                    .claim
                    .native_admission
                    .try_lock()
                    .map_err(|_| {
                        anyhow::anyhow!("original native admission lookup busy or poisoned")
                    })?
                    .as_ref()
                    .context("actual original native admission not registered")?
                    .upgrade()
                    .context("actual original native admission custody absent")?
            };
            ensure!(
                Arc::ptr_eq(&admission.claim, &self.ready.seal.claim),
                "native lifeline lookup selects foreign original claim"
            );
            let factory = {
                self.ready
                    .seal
                    .claim
                    .native_factory
                    .try_lock()
                    .map_err(|_| {
                        anyhow::anyhow!("original native factory lookup busy or poisoned")
                    })?
                    .as_ref()
                    .context("actual native factory not issued")?
                    .upgrade()
                    .context("actual native factory custody absent")?
            };
            ensure!(
                Arc::ptr_eq(&factory, &self.ready.factory),
                "native admission handoff selects foreign issued factory"
            );
            let owner = Arc::new(OriginalTransferredNativeAdmission {
                admission,
                lifelines: Mutex::new(None),
            });
            let mut retained = self
                .native_lifelines
                .try_lock()
                .map_err(|_| anyhow::anyhow!("post-ACK native registration busy or poisoned"))?;
            ensure!(
                retained.is_none(),
                "native lifeline transfer registration conflict"
            );
            *retained = Some(owner.clone());
            owner
        };
        // The actual destination is cached before transfer or late checks.
        owner.transfer_actual()?;
        owner.require_original(self)?;
        Ok(owner)
    }
}

impl Drop for OriginalNativeQuotaAdmission {
    fn drop(&mut self) {
        let custody = self
            .lifelines
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(actual) = custody.take() {
            // Backend release is not host lifeline retirement. Preserve the
            // actual charged claim/leases, never unlock/close them via Drop.
            std::mem::forget(self.claim.clone());
            std::mem::forget(actual);
        }
    }
}
impl Drop for OriginalTransferredNativeAdmission {
    fn drop(&mut self) {
        let custody = self
            .lifelines
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(actual) = custody.take() {
            std::mem::forget(self.admission.claim.clone());
            std::mem::forget(actual);
        }
    }
}

/// Remaining original live ledger/reservation/clock owner after actual State
/// ACK. This changes custody validation, never resource debt or completion.
pub(super) struct OriginalFinalizerMaster {
    release: Mutex<Option<OriginalAccountReservationReleaseProgress>>,
    post_ack: std::sync::Weak<OriginalPostAckFinalization>,
    admission: Arc<OriginalSettlementAdmission>,
    reservation_pin: Mutex<Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>>,
}
struct OriginalAccountReservationReleaseProgress {
    attempt_active: bool,
    issued: std::mem::ManuallyDrop<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>,
    progress: std::mem::ManuallyDrop<Option<lillux_sqlite::OriginalWitnessRemovalProgress>>,
    removed: std::mem::ManuallyDrop<Option<lillux_sqlite::RemovedOriginalWitnessFile>>,
    publication_retired: std::mem::ManuallyDrop<Option<lillux_sqlite::RetiredOriginalWitnessFile>>,
    issued_retired: std::mem::ManuallyDrop<Option<lillux_sqlite::RetiredOriginalMetadataPin>>,
    issued_inputs: std::mem::ManuallyDrop<Option<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>>,
    failure: std::mem::ManuallyDrop<Option<anyhow::Error>>,
}
struct OriginalReservationPinRetirementAuthority {
    master: std::sync::Weak<OriginalFinalizerMaster>,
    pin: std::sync::Weak<lillux_sqlite::RetainedOriginalMetadataPin>,
}
impl lillux_sqlite::OriginalMetadataPinRetirementAuthority for OriginalReservationPinRetirementAuthority {
    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        pin: &Arc<lillux_sqlite::RetainedOriginalMetadataPin>) -> Result<()> {
        factory.require_original_process()?;
        let master = self.master.upgrade().context("actual original reservation pin master absent")?;
        ensure!(master.admission.claim.record.resident_process == std::process::id(), "foreign original reservation pin master");
        let parent = master.post_ack.upgrade().context("actual original reservation pin parent absent")?;
        ensure!(Arc::ptr_eq(factory, &parent.ready.factory) && Arc::ptr_eq(terminal, &parent.ready.terminal)
            && std::sync::Weak::ptr_eq(&self.pin, &Arc::downgrade(pin)),
            "reservation pin retirement selects another original native/pin context");
        terminal.require_original(factory)?;
        master.checkpoint_original()?;
        let custody = master.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation pin membership busy or poisoned"))?;
        let actual = custody.as_ref().context("actual reservation release not entered")?;
        ensure!(Arc::ptr_eq(&actual.issued, pin) && actual.removed.is_some()
            && actual.progress.as_ref().context("actual reservation origin phase absent")?.durable()?,
            "reservation pin lacks exact original durable removal");
        Ok(())
    }
    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        self.master.upgrade().context("actual original reservation pin debit master absent")?
            .admission.debit_original(steps)
    }
}
impl OriginalFinalizerMaster {
    pub(super) fn advance_original_reservation_release(self: &Arc<Self>,
        parent: &Arc<OriginalPostAckFinalization>) -> Result<()> {
        self.require_original(parent)?;
        let entered = self.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation release entry lookup busy or poisoned"))?.is_some();
        if !entered {
            parent.ready.require_original(&parent.ready.seal.claim, &parent.ready.seal.work,
                &parent.ready.factory, &parent.ready.terminal)?;
            {
            let custody = parent.account_intent.try_lock()
                .map_err(|_| anyhow::anyhow!("actual reservation intent acknowledgement busy or poisoned"))?;
            let intent = custody.as_ref().context("actual reservation settlement intent absent")?;
            ensure!(intent.publication.acknowledged && intent.publication.owner.is_some()
                && intent.record.as_ref().is_some_and(|record| record.reservation_retained),
                "reservation release requires actual durable original intent");
            }
            parent.prepay_outside_continuation(0, 8)?;
            let issued = self.reservation_pin.try_lock()
                .map_err(|_| anyhow::anyhow!("actual reservation issued pin entry busy or poisoned"))?
                .clone().context("actual original reservation issued pin absent")?;
            ensure!(issued.observed_identity()? == self.admission.original.reservation_identity,
                "actual issued reservation identity changed before release");
            let mut custody = self.release.try_lock()
                .map_err(|_| anyhow::anyhow!("actual reservation release entry busy or poisoned"))?;
            ensure!(custody.is_none(), "actual reservation release entry conflict");
            *custody = Some(OriginalAccountReservationReleaseProgress {
                attempt_active: false, issued: std::mem::ManuallyDrop::new(issued),
                progress: std::mem::ManuallyDrop::new(None), removed: std::mem::ManuallyDrop::new(None),
                publication_retired: std::mem::ManuallyDrop::new(None), issued_retired: std::mem::ManuallyDrop::new(None),
                issued_inputs: std::mem::ManuallyDrop::new(None), failure: std::mem::ManuallyDrop::new(None),
            });
        }
        {
            let mut custody = self.release.try_lock()
                .map_err(|_| anyhow::anyhow!("actual reservation release attempt busy or poisoned"))?;
            let actual = custody.as_mut().context("actual reservation release entry absent")?;
            ensure!(!actual.attempt_active, "actual reservation release already advancing");
            actual.attempt_active = true;
        }
        let result = lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            self.advance_original_reservation_release_steps(parent)
        });
        let mut custody = self.release.lock().unwrap_or_else(|p| p.into_inner());
        let actual = custody.as_mut().expect("cached actual reservation release owner");
        actual.attempt_active = false;
        if let Err(error) = result {
            // Keep the first actual failure body. Later refusals remain primary
            // returned errors while their whole producer prefixes stay cached.
            if actual.failure.is_none() {
                *actual.failure = Some(error);
                return Err(anyhow::anyhow!("original reservation release refused; actual failure and all custody retained"));
            }
            return Err(error);
        }
        Ok(())
    }
    fn advance_original_reservation_release_steps(self: &Arc<Self>, parent: &Arc<OriginalPostAckFinalization>) -> Result<()> {
        let needs_removal = self.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation removal lookup busy or poisoned"))?
            .as_ref().context("actual reservation release absent")?.removed.is_none();
        let work: Arc<dyn lillux_sqlite::OriginalWitnessCleanupWork> = self.admission.clone();
        if needs_removal {
            let removed = self.admission.reservation_publication.remove_original_planned(
                work.clone(), &self.admission.claim.ledger, self.admission.deadline)
                .map_err(anyhow::Error::new)?;
            self.retain_original_removal(&removed)?;
        }
        let removed = self.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation durable receipt lookup busy or poisoned"))?
            .as_ref().and_then(|actual| actual.removed.as_ref().cloned())
            .context("actual reservation durable removal receipt absent")?;
        removed.require_original(&self.admission.reservation_publication, &work)?;
        self.cache_actual_reservation_close_prefix()?;
        let needs_publication = self.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation publication close lookup busy or poisoned"))?
            .as_ref().context("actual reservation release absent")?.publication_retired.is_none();
        if needs_publication {
            let remaining = self.admission.remaining_steps()?;
            let maximum = u32::try_from((remaining / 4).checked_sub(1).filter(|bound| *bound > 0)
                .context("original reservation witness close lacks finite funded scan")?.min(i32::MAX as u64))?;
            let receipt = self.admission.reservation_publication.retire_removed_original(&removed, maximum)?;
            let mut custody = self.release.lock().unwrap_or_else(|p| p.into_inner());
            *custody.as_mut().expect("cached actual reservation release").publication_retired = Some(receipt);
        }
        let (issued, inputs, complete) = {
            let custody = self.release.try_lock()
                .map_err(|_| anyhow::anyhow!("actual issued reservation close lookup busy or poisoned"))?;
            let actual = custody.as_ref().context("actual reservation release absent")?;
            (Arc::clone(&actual.issued), actual.issued_inputs.as_ref().cloned(), actual.issued_retired.is_some())
        };
        if !complete {
            let inputs = if let Some(inputs) = inputs { inputs } else {
                parent.prepay_outside_continuation(u64::try_from(
                    std::mem::size_of::<OriginalReservationPinRetirementAuthority>()
                        + std::mem::size_of::<lillux_sqlite::OriginalMetadataPinRetirementInputs>()
                        + std::mem::size_of::<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>()
                        + 4 * std::mem::size_of::<usize>())?, 8)?;
                let authority: Arc<dyn lillux_sqlite::OriginalMetadataPinRetirementAuthority> =
                    Arc::new(OriginalReservationPinRetirementAuthority { master: Arc::downgrade(self), pin: Arc::downgrade(&issued) });
                let inputs = Arc::new(unsafe { lillux_sqlite::OriginalMetadataPinRetirementInputs::issued(
                    parent.ready.factory.clone(), parent.ready.terminal.clone(), vec![issued.clone()], authority) }?);
                let mut custody = self.release.lock().unwrap_or_else(|p| p.into_inner());
                *custody.as_mut().expect("cached actual reservation release").issued_inputs = Some(inputs.clone());
                inputs
            };
            let receipt = issued.retire_after_backend(&parent.ready.factory, &parent.ready.terminal, &inputs)?;
            let mut custody = self.release.lock().unwrap_or_else(|p| p.into_inner());
            let actual = custody.as_mut().expect("cached actual reservation release");
            *actual.issued_retired = Some(receipt);
            // The actual successful close memo is cached before dropping only
            // completed native retirement inputs; no mapped loan crosses tail.
            actual.issued_inputs.take();
        }
        let retired = self.admission.reservation_publication.original_retirement_receipt()?
            .context("actual original reservation witness retirement absent")?;
        parent.transfer_original_reservation_removal_parent(&removed, &retired, &work)?;
        self.checkpoint_original()
    }
    fn cache_actual_reservation_close_prefix(&self) -> Result<()> {
        ensure!(self.admission.claim.record.resident_process == std::process::id(), "foreign original reservation close observer");
        let issued = self.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation close observer busy or poisoned"))?
            .as_ref().map(|actual| Arc::clone(&actual.issued));
        let Some(issued) = issued else { return Ok(()); };
        let publication = self.admission.reservation_publication.original_retirement_receipt()?;
        let pin = issued.original_retirement_receipt()?;
        let mut custody = self.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual reservation close prefix cache busy or poisoned"))?;
        let actual = custody.as_mut().context("actual reservation release prefix disappeared")?;
        if actual.publication_retired.is_none() { *actual.publication_retired = publication; }
        if actual.issued_retired.is_none() { *actual.issued_retired = pin; }
        let completed_inputs = if actual.issued_retired.is_some() { actual.issued_inputs.take() } else { None };
        drop(custody);
        drop(completed_inputs);
        Ok(())
    }
    fn retain_original_removal_progress(&self, progress: &lillux_sqlite::OriginalWitnessRemovalProgress) -> Result<()> {
        ensure!(self.admission.claim.record.resident_process == std::process::id(), "foreign original removal progress master");
        let work: Arc<dyn lillux_sqlite::OriginalWitnessCleanupWork> = self.admission.clone();
        progress.require_original(&self.admission.reservation_publication, &work)?;
        let mut custody = self.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual original account phase cache busy or poisoned"))?;
        let actual = custody.as_mut().context("original reservation release was not genuinely entered")?;
        *actual.progress = Some(progress.clone());
        Ok(())
    }
    fn retain_original_removal(&self, removed: &lillux_sqlite::RemovedOriginalWitnessFile) -> Result<()> {
        ensure!(self.admission.claim.record.resident_process == std::process::id(), "foreign original durable removal master");
        let work: Arc<dyn lillux_sqlite::OriginalWitnessCleanupWork> = self.admission.clone();
        removed.require_original(&self.admission.reservation_publication, &work)?;
        let mut custody = self.release.try_lock()
            .map_err(|_| anyhow::anyhow!("actual original durable removal cache busy or poisoned"))?;
        let actual = custody.as_mut().context("original durable account release was not entered")?;
        ensure!(actual.progress.as_ref().context("actual account origin phase absent")?.durable()?,
            "intermediate account cleanup cannot supply durable removal");
        *actual.removed = Some(removed.clone());
        Ok(())
    }
    fn transfer_actual_pin(&self) -> Result<()> {
        let mut destination = self
            .reservation_pin
            .try_lock()
            .map_err(|_| anyhow::anyhow!("finalizer master pin owner busy or poisoned"))?;
        if destination.is_some() {
            return Ok(());
        }
        let mut source = self
            .admission
            .original
            .reservation_pin
            .try_lock()
            .map_err(|_| anyhow::anyhow!("actual issued reservation handoff busy or poisoned"))?;
        let actual = source
            .take()
            .context("actual issued reservation absent without master custody")?;
        *destination = Some(actual);
        Ok(())
    }
    fn checkpoint_original(&self) -> Result<()> {
        ensure!(
            self.admission.claim.record.resident_process == std::process::id(),
            "original finalizer master belongs to another process"
        );
        let post_ack = self
            .post_ack
            .upgrade()
            .context("actual post-ACK master producer absent")?;
        let cached = post_ack
            .master
            .try_lock()
            .map_err(|_| anyhow::anyhow!("finalizer master producer lookup busy or poisoned"))?
            .as_ref()
            .is_some_and(|actual| std::ptr::eq(actual.as_ref(), self));
        let registered = self
            .admission
            .finalizer_master
            .try_lock()
            .map_err(|_| anyhow::anyhow!("registered original master lookup busy or poisoned"))?
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|actual| std::ptr::eq(actual.as_ref(), self));
        ensure!(
            cached
                && registered
                && self
                    .admission
                    .claim
                    .finalizer_master_entered
                    .load(Ordering::Acquire),
            "finalizer master is not exact retained original producer"
        );
        // Entry already joined the actual ACK and native terminal. This
        // same registered master retains its original ledger/clock authority
        // through the consuming State handoff; it does not need a mapped
        // native proof to verify those outside obligations on every retry.
        post_ack.ready.seal.require_original(
            &self.admission.claim, &post_ack.ready.seal.work,
        )?;
        let claim = &self.admission.claim;
        ensure!(
            !self.admission.deadline.has_elapsed()
                && claim.clock.binding_digest() == claim.record.clock_binding
                && self.admission.original.policy.generation_digest() == claim.record.policy_digest
                && self
                    .admission
                    .original
                    .policy
                    .require::<ryeos_scheduler_contract::NodeMaintenancePolicy>()?
                    .offline_reset
                    .as_ref()
                    == Some(&claim.record.reserved),
            "original finalizer clock/policy changed or expired"
        );
        let allowance = std::time::Duration::from_millis(claim.record.reserved.max_cleanup_millis);
        original_cleanup_remaining(
            claim
                .clock
                .window(&claim.record.occupancy, allowance)
                .map_err(anyhow::Error::msg)?,
            allowance,
        )?;
        // Charge bounded ledger path/identity/read verification BEFORE its
        // filesystem effects; never recurse through admission's dispatcher.
        let read = u64::try_from(claim.signed.len())?
            .checked_add(1)
            .context("original master signature read overflow")?;
        let path_bytes = u64::try_from(claim.ledger.path().as_os_str().len())?;
        let memory = path_bytes
            .checked_mul(4)
            .and_then(|n| n.checked_add(read))
            .and_then(|n| n.checked_add(4096))
            .context("original master scratch overflow")?;
        let steps = u64::try_from(claim.ledger.path().components().count())?
            .checked_mul(8)
            .and_then(|n| n.checked_add(64))
            .context("original master work overflow")?;
        {
            let mut used = claim.copy_usage.try_lock().map_err(|_| {
                anyhow::anyhow!("original master verification debit busy or poisoned")
            })?;
            let cleanup = used
                .cleanup_steps
                .checked_add(steps)
                .context("original master cleanup overflow")?;
            let reads = used
                .read_bytes
                .checked_add(read)
                .context("original master read overflow")?;
            let buffers = used
                .buffer_bytes
                .checked_add(memory)
                .context("original master memory overflow")?;
            ensure!(
                cleanup <= claim.record.reserved.max_cleanup_steps
                    && reads <= claim.record.reserved.max_read_bytes
                    && buffers
                        .checked_add(used.native_storage_bytes)
                    .and_then(|bytes| bytes.checked_add(used.resident_storage_bytes))
                        .is_some_and(|total| total <= claim.record.reserved.max_memory_bytes),
                "original finalizer master exhausted charged verification envelope"
            );
            used.cleanup_steps = cleanup;
            used.read_bytes = reads;
            used.buffer_bytes = buffers;
        }
        self.cache_actual_reservation_close_prefix()?;
        let (publication_retired, issued_retired, needs_original_coordinate) = {
            let custody = self.release.try_lock()
                .map_err(|_| anyhow::anyhow!("actual reservation phase verification busy or poisoned"))?;
            if let Some(actual) = custody.as_ref() {
                if let Some(receipt) = actual.publication_retired.as_ref() {
                    receipt.require_original(&self.admission.reservation_publication)?;
                }
                if let Some(receipt) = actual.issued_retired.as_ref() { receipt.require_original(&actual.issued)?; }
                (actual.publication_retired.is_some(), actual.issued_retired.is_some(),
                    match actual.progress.as_ref() {
                        Some(progress) => {
                            let work: Arc<dyn lillux_sqlite::OriginalWitnessCleanupWork> = self.admission.clone();
                            progress.require_original(&self.admission.reservation_publication, &work)?;
                            progress.requires_original_coordinate()?
                        }
                        None => true,
                    })
            } else { (false, false, true) }
        };
        let pin = self
            .reservation_pin
            .try_lock()
            .map_err(|_| anyhow::anyhow!("actual master reservation owner busy or poisoned"))?
            .clone()
            .context("actual original master reservation pin pending")?;
        self.admission.ledger_lock.ensure_protects(&claim.ledger)?;
        claim.ledger.ensure_path_binding()?;
        self.admission.reservation_publication.require_original(
            &claim.ledger,
            OsStr::new(&self.admission.original.reservation_name),
        )?;
        if !publication_retired {
            ensure!(self.admission.reservation_publication.identity_original()? == self.admission.reservation_identity
                && self.admission.reservation_publication.read_original(claim.signed.len())? == claim.signed,
                "actual original master reservation publication identity/bytes changed");
        }
        if !issued_retired {
            ensure!(pin.observed_identity()? == self.admission.original.reservation_identity,
                "actual original master issued reservation pin changed");
        }
        if needs_original_coordinate {
        let entry = claim
            .ledger
            .entry_no_follow(OsStr::new(&self.admission.original.reservation_name))?
            .context("charged original master reservation disappeared")?;
        ensure!(
            entry.entry_type == lillux::PinnedEntryType::Regular
                && entry.containing_device == self.admission.reservation_identity.device()
                && entry.inode == self.admission.reservation_identity.inode(),
            "original master reservation coordinate changed"
        );
        }
        ensure!(
            !self.admission.deadline.has_elapsed(),
            "original finalizer master clock expired during verification"
        );
        original_cleanup_remaining(
            claim
                .clock
                .window(&claim.record.occupancy, allowance)
                .map_err(anyhow::Error::msg)?,
            allowance,
        )?;
        Ok(())
    }
    pub(super) fn require_original(
        &self,
        post_ack: &Arc<OriginalPostAckFinalization>,
    ) -> Result<()> {
        let actual = self
            .post_ack
            .upgrade()
            .context("original master post-ACK owner absent")?;
        ensure!(
            Arc::ptr_eq(&actual, post_ack),
            "master selects foreign original post-ACK owner"
        );
        ensure!(
            *self
                .admission
                .phase
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original master phase busy or poisoned"))?
                == OriginalReservationPhase::PostAckFinalizerMasterRetained,
            "original finalizer master phase not entered"
        );
        self.checkpoint_original()
    }
}
impl OriginalPostAckFinalization {
    pub(super) fn enter_finalizer_master(self: &Arc<Self>) -> Result<Arc<OriginalFinalizerMaster>> {
        self.require_state_ack()?;
        ensure!(
            self.ready.terminal.arena_disposition()?
                == lillux_sqlite::OriginalNativeArenaDisposition::Released,
            "finalizer master requires actual original native release"
        );
        let cached = self
            .master
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original master entry custody busy or poisoned"))?
            .clone();
        let master = if let Some(master) = cached {
            master
        } else {
            // Genuine complete source retirement and original prepared ACK,
            // not decoded evidence, must precede master capability cessation.
            self.ready.require_original(
                &self.ready.seal.claim,
                &self.ready.seal.work,
                &self.ready.factory,
                &self.ready.terminal,
            )?;
            self.prepay_finalizer_transfer(4096, 32)?;
            let master = Arc::new(OriginalFinalizerMaster {
                release: Mutex::new(None),
                post_ack: Arc::downgrade(self),
                admission: self.ready.admission.clone(),
                reservation_pin: Mutex::new(None),
            });
            let mut cached = self
                .master
                .try_lock()
                .map_err(|_| anyhow::anyhow!("actual master registration busy or poisoned"))?;
            ensure!(
                cached.is_none(),
                "original finalizer master registration conflict"
            );
            *cached = Some(master.clone());
            master
        };
        {
            let mut registered = self
                .ready
                .admission
                .finalizer_master
                .try_lock()
                .map_err(|_| anyhow::anyhow!("admission master registration busy or poisoned"))?;
            if let Some(original) = registered.as_ref() {
                ensure!(
                    original
                        .upgrade()
                        .is_some_and(|actual| Arc::ptr_eq(&actual, &master)),
                    "original finalizer master cannot be replaced"
                );
            } else {
                *registered = Some(Arc::downgrade(&master));
            }
        }
        {
            let mut original = self.ready.admission.original.outside_master.try_lock()
                .map_err(|_| anyhow::anyhow!("original copy master registration busy or poisoned"))?;
            if let Some(previous) = original.as_ref() {
                ensure!(previous.upgrade().is_some_and(|actual| Arc::ptr_eq(&actual, &master)),
                    "original copy master cannot be replaced");
            } else {
                *original = Some(Arc::downgrade(&master));
            }
        }
        // Irreversible capability cessation before moving the actual issued
        // pin; every prior service/copy callback refuses rather than renews.
        self.ready
            .seal
            .claim
            .finalizer_master_entered
            .store(true, Ordering::Release);
        master.transfer_actual_pin()?;
        *self
            .ready
            .admission
            .phase
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original master phase publication busy or poisoned"))? =
            OriginalReservationPhase::PostAckFinalizerMasterRetained;
        master.require_original(self)?;
        Ok(master)
    }
}
