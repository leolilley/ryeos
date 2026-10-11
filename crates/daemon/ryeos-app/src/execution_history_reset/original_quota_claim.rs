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

use lillux_sqlite::offline_host::{
    OfflineNativeAuthority, OfflineNativeFacts, OfflineNativeHost, OfflineNativeHostFailure,
    offline_native_required_cleanup_steps,
};

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
    occupancy: lillux::time::OccupancyLimit,
    clock_binding: String,
    sources: [InspectionSourcePlan; 2],
    operational_source: OperationalSourcePlan,
    copy_root_identity: String,
    destinations: [InspectionDestinationPlan; 2],
    inspection: OriginalInspectionPlan,
}

const CLAIM_SCHEMA: u32 = 10;
const CLAIM_PURPOSE: &str = "offline_reset_local_quota_reservation_v10";

impl ClaimRecord {
    /// Authenticate one signed reservation. A reservation under a retired
    /// schema is refused by name, before its exact shape is decoded.
    fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        ensure!(!bytes.is_empty() && bytes.len() <= MAX_RECORD_BYTES,
            "offline quota claim exceeds record bound");
        let raw = std::str::from_utf8(bytes)?;
        let (body, header) = lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)?;
        let header = header.context("offline quota claim is unsigned")?;
        ensure!(lillux::signature::is_valid_signature_for(
            &header.content_hash, &header.signature_b64, &header.signer_fingerprint,
            &body, identity.verifying_key(), identity.fingerprint()),
            "offline quota claim signature is invalid");
        super::creation_evidence::require_current_record(&body, "quota claim", CLAIM_SCHEMA, CLAIM_PURPOSE)?;
        let record: Self = serde_json::from_str(&body)?;
        record.validate()?;
        ensure!(record.signer == identity.fingerprint()
            && lillux::canonical_json(&serde_json::to_value(&record)?)? == body,
            "offline quota claim has foreign signer or noncanonical body");
        Ok(record)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == CLAIM_SCHEMA && self.purpose == CLAIM_PURPOSE,
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
        // The host draws its own region generation; the signed claim carries
        // only the memory the host's region and native arena may take.
        let host_bytes = u64::try_from(
            lillux_sqlite::original_host_owner::backing::HostStorage::quote_native_region()
                .map_err(anyhow::Error::msg)?
                .bytes(),
        )?;
        ensure!(
            host_bytes.checked_add(self.reserved.max_native_storage_bytes)
                .is_some_and(|bytes| bytes <= self.reserved.max_memory_bytes),
            "offline claim memory cannot hold the host region and native arena"
        );
        require_native_step_budget(&self.reserved)?;
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

/// A signed reservation left by a reset whose process is gone, read only so
/// recovery can settle it. It funds, admits and authorizes nothing.
pub(super) struct StaleReservation {
    record: ClaimRecord,
    sha256: String,
}

impl StaleReservation {
    /// Authenticate one stale reservation as this node's current-schema
    /// record.
    pub(super) fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        Ok(Self { record: ClaimRecord::verified(bytes, identity)?, sha256: lillux::sha256_hex(bytes) })
    }

    pub(super) fn operation_id(&self) -> &str {
        &self.record.operation_id
    }

    pub(super) fn claim_nonce(&self) -> &str {
        &self.record.claim_nonce
    }

    /// SHA-256 of the exact signed reservation bytes.
    pub(super) fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(super) fn mode(&self) -> OriginalInspectionMode {
        self.record.inspection.mode
    }

    /// The reservation must name these live roots. Its clock and policy are
    /// not compared: they belong to the process and boot that wrote it.
    pub(super) fn require_roots(
        &self,
        app_root: &lillux::PinnedDirectory,
        runtime: &lillux::PinnedDirectory,
        copy_root: &lillux::PinnedDirectory,
    ) -> Result<()> {
        ensure!(
            self.record.app_root_identity == serde_json::to_string(&app_root.identity()?)?
                && self.record.runtime_identity == serde_json::to_string(&runtime.identity()?)?
                && self.record.copy_root_identity == serde_json::to_string(&copy_root.identity()?)?,
            "stale offline reservation names another app root, runtime or inspections parent"
        );
        Ok(())
    }

    /// The disposition must be this reservation's own.
    pub(super) fn require_disposition(
        &self,
        disposition: &super::creation_evidence::InspectionDispositionRecord,
    ) -> Result<()> {
        require_disposition_claim(disposition, &self.record, &self.sha256)
    }

    /// The creation witness must be this reservation's own, for the
    /// destination `directory`.
    pub(super) fn require_witness(&self, evidence: &CreatedDirectoryRecord, directory: &str) -> Result<()> {
        let index = self
            .record
            .destinations
            .iter()
            .position(|plan| plan.directory_name == directory)
            .context("creation witness names no destination of its reservation")?;
        ensure!(
            evidence.operation_id == self.record.operation_id
                && evidence.claim_nonce == self.record.claim_nonce
                && evidence.claim_sha256 == self.sha256
                && evidence.policy_digest == self.record.policy_digest
                && evidence.clock_binding == self.record.clock_binding
                && evidence.source_purpose == self.record.sources[index].purpose
                && evidence.destination_name == directory
                && serde_json::to_string(&evidence.parent_identity)? == self.record.copy_root_identity,
            "creation witness differs from its stale reservation"
        );
        Ok(())
    }
}

/// A signed current-schema reservation of `operation_id` naming these live
/// roots, as a process that died after publishing leaves it, for recovery
/// tests. No source roles are selected.
#[cfg(test)]
pub(super) fn fixture_signed_reservation(
    identity: &NodeIdentity,
    roots: [&lillux::PinnedDirectory; 3],
    operation_id: &str,
    mode: OriginalInspectionMode,
    quota: &OfflineResetQuota,
) -> Vec<u8> {
    let [app_root, runtime, copy_root] = roots;
    let json = |directory: &lillux::PinnedDirectory| serde_json::to_string(&directory.identity().unwrap()).unwrap();
    let sources = ["runtime", "scheduler"].map(|purpose| InspectionSourcePlan {
        purpose: purpose.into(),
        directory_identity: json(runtime),
        database_name: format!("{purpose}.sqlite3"),
        roles: std::array::from_fn(|_| None),
    });
    let record = ClaimRecord {
        schema: CLAIM_SCHEMA,
        purpose: CLAIM_PURPOSE.into(),
        operation_id: operation_id.to_owned(),
        claim_nonce: "b".repeat(32),
        signer: identity.fingerprint().to_owned(),
        policy_digest: "c".repeat(64),
        app_root_identity: json(app_root),
        runtime_identity: json(runtime),
        prior_operation: None,
        reserved: quota.clone(),
        occupancy: lillux::time::OccupancyLimit::new(
            lillux::time::occupancy_now().unwrap(),
            occupancy_nanoseconds(quota).unwrap(),
        )
        .unwrap(),
        // A clock binding from another boot: recovery never compares it.
        clock_binding: "f".repeat(64),
        inspection: OriginalInspectionPlan::original(mode, false, None, &sources),
        operational_source: serde_json::from_value(serde_json::json!({
            "directory_identity": json(runtime),
            "roles": [{"device": 1, "inode": 2, "byte_length": 0}, null, null, null],
            "marker": {"device": 1, "inode": 3, "byte_length":
                ryeos_state::operational::OPERATIONAL_INITIALIZED_CONTENT.len()}
        }))
        .unwrap(),
        sources,
        copy_root_identity: json(copy_root),
        destinations: destination_plans(operation_id),
    };
    record.validate().unwrap();
    let body = lillux::canonical_json(&serde_json::to_value(&record).unwrap()).unwrap();
    lillux::signature::sign_content(&body, identity.signing_key(), "#", None).into_bytes()
}

/// Registers a claim on `ledger` as alive in this process until dropped,
/// for recovery tests.
#[cfg(test)]
pub(super) struct FixtureLiveClaim(lillux::PinnedDirectoryIdentity);
#[cfg(test)]
impl FixtureLiveClaim {
    pub(super) fn register(ledger: lillux::PinnedDirectoryIdentity) -> Self {
        register_live_claim(ledger);
        Self(ledger)
    }
}
#[cfg(test)]
impl Drop for FixtureLiveClaim {
    fn drop(&mut self) {
        deregister_live_claim(&self.0);
    }
}

/// Native SQL charges both the host's native step budget and the claim's
/// SQL ceiling, so the signed native step budget must cover the whole
/// ceiling, plus the host's minimum allocation funding margin (a heuristic
/// floor, not a bound) and its exact cleanup quote.
fn require_native_step_budget(quota: &OfflineResetQuota) -> Result<()> {
    ryeos_node_runtime::node_policy::sections::maintenance::validate_offline_reset_quota(quota)
}

fn occupancy_nanoseconds(quota: &OfflineResetQuota) -> Result<u64> {
    quota
        .max_operation_millis
        .checked_add(quota.max_cleanup_millis)
        .and_then(|millis| millis.checked_mul(1_000_000))
        .context("offline quota service/cleanup occupancy overflow")
}

/// Same original signed reservation and descriptor owners on publication
/// uncertainty. Dropping a claim releases nothing durable: its deposits stay
/// charged until the release tail or the next reset's recovery settles them.
/// The drop only removes the claim from this process's live-claim registry.
pub(super) struct OriginalQuotaClaim {
    record: ClaimRecord,
    /// The ledger this claim is registered under as live in this process.
    ledger_identity: lillux::PinnedDirectoryIdentity,
    /// The process that prepared this claim; no other process may act on it.
    process: u32,
    /// The claim's share of the exclusive StateLock, from `prepare` until the
    /// claim drops.
    state_lease: ResetStateLease,
    clock: lillux::time::RetainedOccupancyClock,
    native_service_deadline: lillux::time::MonotonicDeadline,
    native_cleanup_deadline: lillux::time::MonotonicDeadline,
    /// The one signed SQL VM ceiling, shared by copy and native SQL.
    sql_steps: SqlVmCeiling,
    signer: NodeIdentity,
    signed: Vec<u8>,
    app_root: lillux::PinnedDirectory,
    runtime: lillux::PinnedDirectory,
    ledger: lillux::PinnedDirectory,
    copy_root: lillux::PinnedDirectory,
    inspection_namespace: Mutex<Option<lillux::PinnedDirectoryLock>>,
    publication_entered: AtomicBool,
    publication_acknowledged: AtomicBool,
    reservation_publication: Mutex<Option<Arc<lillux_sqlite::RetainedOriginalWitnessFile>>>,
    copy_usage: Mutex<CopyUsage>,
    production: Mutex<OriginalProductionState>,
    copy_issuer: Mutex<Option<std::sync::Weak<OriginalCopyWork>>>,
    /// The authority of this claim's one offline native host; the host owns
    /// the factory, region, budget and admission.
    native_authority: std::sync::OnceLock<std::sync::Weak<ResetNativeAuthority>>,
    _prior_operation: Option<Arc<OriginalOperation>>,
    _source_roots: [lillux::PinnedDirectory; 2],
    _source_pins: Vec<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>,
    operational_source_pins: OperationalSourcePins,
}

/// Claims alive in this process, counted per ledger directory. Every claim
/// holds the node's exclusive StateLock, so under that lock a ledger entry
/// whose ledger has no live claim here belongs to a process that is gone.
static LIVE_CLAIMS: Mutex<Vec<(lillux::PinnedDirectoryIdentity, usize)>> = Mutex::new(Vec::new());

fn register_live_claim(ledger: lillux::PinnedDirectoryIdentity) {
    let mut live = LIVE_CLAIMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match live.iter_mut().find(|(identity, _)| *identity == ledger) {
        Some((_, count)) => *count += 1,
        None => live.push((ledger, 1)),
    }
}

fn deregister_live_claim(ledger: &lillux::PinnedDirectoryIdentity) {
    let mut live = LIVE_CLAIMS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(at) = live.iter().position(|(identity, _)| identity == ledger) {
        live[at].1 -= 1;
        if live[at].1 == 0 {
            live.swap_remove(at);
        }
    }
}

/// How many claims on `ledger` are alive in this process. Publication
/// requires its own claim to be the only one; recovery requires none.
pub(super) fn live_claims_on(ledger: &lillux::PinnedDirectoryIdentity) -> usize {
    LIVE_CLAIMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .find(|(identity, _)| identity == ledger)
        .map_or(0, |(_, count)| *count)
}

impl Drop for OriginalQuotaClaim {
    fn drop(&mut self) {
        deregister_live_claim(&self.ledger_identity);
    }
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

/// Release phase of one reservation. Each phase is entered only once the
/// previous one is durable, and never left backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReservationReleasePhase {
    /// The reservation is charged and its coordinate is live.
    Held,
    /// `disposed.<op>` is durable and the creation witnesses are retired.
    Disposed,
    /// `released.<op>` is durable.
    Committed,
    /// The reservation, the disposition and the released record are unlinked.
    Unlinked,
}
impl ReservationReleasePhase {
    fn next(self) -> Option<Self> {
        match self {
            Self::Held => Some(Self::Disposed),
            Self::Disposed => Some(Self::Committed),
            Self::Committed => Some(Self::Unlinked),
            Self::Unlinked => None,
        }
    }
}
struct OriginalSettlementAdmission {
    claim: Arc<OriginalQuotaClaim>,
    original: Arc<OriginalCopyWork>,
    deadline: lillux::time::MonotonicDeadline,
    ledger_lock: lillux::PinnedDirectoryLock,
    reservation_publication: Arc<lillux_sqlite::RetainedOriginalWitnessFile>,
    reservation_identity: lillux::OpenFileIdentity,
    phase: Mutex<ReservationReleasePhase>,
    /// The publication of `released.<op>`, retained once it is created. The
    /// committed and unlinked phases are checked against it, because the
    /// reservation coordinate is then legitimately gone.
    released_publication: Mutex<Option<Arc<lillux_sqlite::RetainedOriginalWitnessFile>>>,
    /// Set when the unlink of `released.<op>` itself begins: its coordinate
    /// may then legitimately be gone while the phase is still `Committed`.
    released_unlinking: AtomicBool,
}
impl OriginalSettlementAdmission {
    /// Enter the phase after `from`. Refuses unless `from` is current.
    fn advance_phase(&self, from: ReservationReleasePhase) -> Result<()> {
        let mut phase = self
            .phase
            .try_lock()
            .map_err(|_| anyhow::anyhow!("reservation release phase busy or poisoned"))?;
        let next = from.next().context("reservation release is already unlinked")?;
        if *phase == next {
            // An earlier attempt of the same step already advanced it.
            return Ok(());
        }
        ensure!(*phase == from, "reservation release phase is {:?}, not {from:?}", *phase);
        *phase = next;
        Ok(())
    }
    fn released_publication(&self) -> Result<Arc<lillux_sqlite::RetainedOriginalWitnessFile>> {
        self.released_publication
            .try_lock()
            .map_err(|_| anyhow::anyhow!("released record publication busy or poisoned"))?
            .clone()
            .context("released record publication descriptor absent")
    }
    fn checkpoint_original(&self) -> Result<()> {
        let phase = *self
            .phase
            .try_lock()
            .map_err(|_| anyhow::anyhow!("reservation release phase busy or poisoned"))?;
        ensure!(
            !self.deadline.has_elapsed(),
            "original settlement deadline expired; custody retained"
        );
        self.claim
            .state_lease
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
        let released_name =
            super::inspection_destination::released_name(&self.claim.record.operation_id);
        match phase {
            ReservationReleasePhase::Held | ReservationReleasePhase::Disposed => {
                // The reservation is still charged: its publication, live
                // coordinate and issued pin must all be the original ones.
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
            ReservationReleasePhase::Committed => {
                let released = self.released_publication()?;
                released.require_original(&self.claim.ledger, OsStr::new(&released_name))?;
                if self.released_unlinking.load(Ordering::Acquire) {
                    // The released record's own unlink is in progress, so its
                    // name may already be gone; the reservation and the
                    // disposition it follows must be.
                    let disposed =
                        super::inspection_destination::disposition_name(&self.claim.record.operation_id);
                    for name in [self.original.reservation_name.as_str(), disposed.as_str()] {
                        ensure!(
                            self.claim.ledger.entry_no_follow(OsStr::new(name))?.is_none(),
                            "released record unlink began with ledger coordinate `{name}` present"
                        );
                    }
                } else {
                    let identity = released.identity_original()?;
                    let current = self
                        .claim
                        .ledger
                        .entry_no_follow(OsStr::new(&released_name))?
                        .context("committed released record disappeared")?;
                    ensure!(
                        current.entry_type == lillux::PinnedEntryType::Regular
                            && current.containing_device == identity.device()
                            && current.inode == identity.inode(),
                        "committed released record coordinate changed"
                    );
                }
            }
            ReservationReleasePhase::Unlinked => {
                // The released record's descriptor was retired by its unlink;
                // only that same-owner retirement receipt can stand for it.
                self.released_publication()?
                    .original_retirement_receipt()?
                    .context("unlinked released record descriptor was not retired")?;
                let disposed =
                    super::inspection_destination::disposition_name(&self.claim.record.operation_id);
                for name in [self.original.reservation_name.as_str(), disposed.as_str(), released_name.as_str()] {
                    ensure!(
                        self.claim.ledger.entry_no_follow(OsStr::new(name))?.is_none(),
                        "unlinked reservation release left ledger coordinate `{name}`"
                    );
                }
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
                    .and_then(|bytes| bytes.checked_add(used.host_region_bytes))
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
                .and_then(|total| total.checked_add(used.host_region_bytes))
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
impl OriginalSettlementAdmission {
    /// The reservation itself may be unlinked only once `released.<op>` is
    /// durable, which is the `Committed` phase.
    fn reservation_unlink_admitted(&self) -> Result<bool> {
        Ok(*self
            .phase
            .try_lock()
            .map_err(|_| anyhow::anyhow!("reservation release phase busy or poisoned"))?
            == ReservationReleasePhase::Committed)
    }
}
impl lillux_sqlite::OriginalWitnessCleanupWork for OriginalSettlementAdmission {
    fn retain_original_witness_progress(&self, progress: &lillux_sqlite::OriginalWitnessRemovalProgress) -> Result<()> {
        ensure!(self.claim.process == std::process::id(), "foreign original account removal observer");
        // Inspection witnesses and the ledger records share this cleanup
        // work. The reservation's own removal is admitted only once the
        // release is committed.
        ensure!(
            !progress.is_original_owner(&self.reservation_publication)?
                || self.reservation_unlink_admitted()?,
            "reservation removal progress outside an admitted release"
        );
        Ok(())
    }
    fn retain_original_removed_witness(&self, removed: &lillux_sqlite::RemovedOriginalWitnessFile) -> Result<()> {
        ensure!(self.claim.process == std::process::id(), "foreign original durable removal observer");
        ensure!(
            !removed.is_original_owner(&self.reservation_publication)?
                || self.reservation_unlink_admitted()?,
            "reservation removal outside an admitted release"
        );
        Ok(())
    }
    fn prepay_original_removal_parent(&self, bytes: u64, steps: u64) -> Result<()> {
        ensure!(bytes > 0 && steps == 2, "invalid original witness parent quote");
        self.debit_original(steps)?;
        let mut used = self.claim.copy_usage.try_lock()
            .map_err(|_| anyhow::anyhow!("original witness parent memory debit busy or poisoned"))?;
        let memory = used.buffer_bytes.checked_add(bytes)
            .context("original witness parent memory overflow")?;
        ensure!(memory.checked_add(used.native_storage_bytes)
            .and_then(|total| total.checked_add(used.host_region_bytes))
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
    /// The release's pin retirement owner, from the pin retirement until
    /// the native alias quiesce.
    release_pins: Option<Arc<OriginalReleasePins>>,
    post_ack: Option<Arc<OriginalPostAckFinalization>>,
    /// The State acknowledgement this release records, captured as plain
    /// data when the post-acknowledgement owner is joined.
    state_ack: Option<ThreadHistoryDiscardOperationRef>,
    /// Whether this release prepared a State final acknowledgement, and so
    /// must record one.
    acknowledgement_expected: bool,
    disposition: OriginalRecordPublication,
    witnesses: [OriginalWitnessDisposition; 2],
    /// The signed `released.<op>` and its publication.
    released: Option<Vec<u8>>,
    released_publication: OriginalRecordPublication,
    /// Unlink receipts of `reserved.`, `disposed.` and `released.<op>`, in
    /// that order.
    unlinked: [OriginalRecordUnlink; 3],
    /// Identities of the retired source and issued reservation pins,
    /// captured as each pin is retired.
    pins: Vec<super::creation_evidence::RetiredPinFact>,
    /// Plain facts of the offline native host, captured once its backend
    /// finished; no factory, terminal or region owner is kept here.
    native: Option<OfflineNativeFacts>,
    /// The host's region release fact, captured when the host is released.
    native_region: Option<lillux_sqlite::original_host_owner::backing::HostStorageRegionReleaseFact>,
}
/// One ledger record's unlink: its durable removal, then its descriptor's
/// retirement.
#[derive(Default)]
struct OriginalRecordUnlink {
    removed: Option<Arc<lillux_sqlite::RemovedOriginalWitnessFile>>,
    retired: Option<Arc<lillux_sqlite::RetiredOriginalWitnessFile>>,
}
/// Complete two-slot ORIGINAL resource custody. This is not successful accounting.
pub(super) struct OriginalInspectionSettlement {
    /// The issued reservation pin once moved out of the copy work for its
    /// retirement, kept here until the pin owner holds it.
    moved_reservation_pin: Mutex<Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>>,
    seal: Arc<OriginalProductionSeal>,
    /// Canonical copy disposals, moved whole into the post-ACK inspection
    /// group; dropped with the settlement when no post-ACK path takes them.
    _disposals: Mutex<Option<Vec<Arc<crate::runtime_db::InspectionCopyDisposal>>>>,
    admission: Arc<OriginalSettlementAdmission>,
    disposition: super::creation_evidence::InspectionDispositionRecord,
    signed_disposition: Vec<u8>,
    progress: Mutex<OriginalSettlementProgress>,
}
impl std::fmt::Debug for OriginalInspectionSettlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalInspectionSettlement")
            .field("operation", &self.seal.claim.record.operation_id)
            .field("held", &self.disposition.held)
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
        ensure!(self.process == std::process::id() && bytes > 0,
            "foreign or empty original record working quote");
        let mut used = self.copy_usage.try_lock()
            .map_err(|_| anyhow::anyhow!("original record working debit busy or poisoned"))?;
        let total = used.buffer_bytes.checked_add(bytes)
            .context("original record working memory overflow")?;
        ensure!(total.checked_add(used.native_storage_bytes)
            .and_then(|memory| memory.checked_add(used.host_region_bytes))
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
                phase: Mutex::new(ReservationReleasePhase::Held),
                released_publication: Mutex::new(None),
                released_unlinking: AtomicBool::new(false),
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
            // A snapshot of what this reset holds at disposal; the release
            // discharges it. The order is the record's canonical order.
            let mut held = Vec::new();
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
            held.push(Obligation::NativeAllocationAccounting {
                attempted: u32::try_from(native_count)?,
            });
            let retained_source_pins = self
                ._source_pins
                .len()
                .checked_add(self.operational_source_pins.retained_count())
                .context("original source pin obligation overflow")?;
            if retained_source_pins > 0 {
                held.push(Obligation::SourcePins {
                    count: u32::try_from(retained_source_pins)?,
                });
            }
            held.push(Obligation::ReservationPublicationDescriptor);
            held.push(Obligation::IssuedReservationPin);
            let disposition = super::creation_evidence::InspectionDispositionRecord {
                schema: super::creation_evidence::DISPOSITION_SCHEMA,
                purpose: super::creation_evidence::DISPOSITION_PURPOSE.into(),
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
                held,
            };
            // Reserve two bounded record buffers BEFORE signing or publication;
            // retain the full irreversible allowance even if exact bytes are less.
            admission.reserve_records((2 * MAX_RECORD_BYTES) as u64)?;
            admission.debit_original(2)?;
            admission.debit_original(1)?;
            let storage = lillux::cas::canonical_record_storage_quote(&disposition, MAX_RECORD_BYTES)?;
            self.debit_original_record_working_storage(storage.working_bytes())?;
            let signed_disposition = disposition.signed_bytes(&self.signer, &storage)?;
            Ok(Arc::new(OriginalInspectionSettlement {
                moved_reservation_pin: Mutex::new(None),
                seal: seal.clone(),
                _disposals: Mutex::new(Some(disposals.clone())),
                admission,
                disposition,
                signed_disposition,
                progress: Mutex::new(OriginalSettlementProgress {
                    release_pins: None,
                    post_ack: None,
                    state_ack: None,
                    acknowledgement_expected: false,
                    disposition: Default::default(),
                    released: None,
                    released_publication: Default::default(),
                    unlinked: Default::default(),
                    pins: Vec::new(),
                    witnesses: witnesses
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("original witness cardinality changed"))?,
                    native: None,
                    native_region: None,
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

    /// The end of disposal: the disposition is durable and every creation
    /// witness is retired and absent. The reservation enters `Disposed`.
    pub(super) fn seal_disposition(self: &Arc<Self>) -> Result<()> {
        let result = (|| -> Result<()> {
            let progress = self.progress.try_lock().map_err(|_| {
                anyhow::anyhow!("original settlement progress busy or poisoned")
            })?;
            ensure!(
                progress.disposition.acknowledged,
                "disposal lacks durable disposition acknowledgement"
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
            drop(progress);
            self.admission.advance_phase(ReservationReleasePhase::Held)?;
            self.admission.checkpoint_original()
        })();
        result.map_err(|error| self.retain_error(error))
    }
    /// Capture the offline native host's plain facts once its backend
    /// finished. A retry must present the same facts.
    pub(super) fn retain_native_facts(self: &Arc<Self>, facts: OfflineNativeFacts) -> Result<()> {
        let result = (|| -> Result<()> {
            ensure!(
                facts.process == std::process::id()
                    && facts.arena == lillux_sqlite::OriginalNativeArenaDisposition::Released,
                "offline native host facts lack this process's released arena"
            );
            let mut progress = self
                .progress
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
            match &progress.native {
                Some(captured) => ensure!(*captured == facts, "offline native host facts changed after capture"),
                None => progress.native = Some(facts),
            }
            Ok(())
        })();
        result.map_err(|error| self.retain_error(error))
    }
    /// The settlement's share of the native alias quiesce: once the backend facts, every retired
    /// pin and (for a performed reset) the State acknowledgement are
    /// captured as plain data, end the pin-retirement owner, the
    /// post-acknowledgement owner and the canonical copy disposals, which
    /// hold the host's factory and backend terminal. Nothing native is
    /// called; a retry after success finds them already gone. Returns the
    /// names of the owners this call ended.
    pub(super) fn quiesce_native_owners(self: &Arc<Self>) -> Result<Vec<&'static str>> {
        let result = (|| -> Result<Vec<&'static str>> {
            let mut progress = self
                .progress
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
            ensure!(progress.native.is_some(), "quiesce before the host backend facts were captured");
            if let Some(pins) = &progress.release_pins {
                pins.require_original(&self.seal.claim, &self.seal.work, &pins.factory, &pins.terminal)?;
                ensure!(!progress.pins.is_empty(), "quiesce before the retired pin facts were captured");
            }
            ensure!(
                progress.post_ack.is_none() || progress.state_ack.is_some(),
                "quiesce before the State acknowledgement was captured"
            );
            // Taken before any owner ends, so a busy slot refuses with
            // nothing ended.
            let mut disposals = self
                ._disposals
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement disposals busy or poisoned"))?;
            let OriginalSettlementProgress {
                release_pins,
                post_ack,
                state_ack: _,
                acknowledgement_expected: _,
                disposition: _,
                witnesses: _,
                released: _,
                released_publication: _,
                unlinked: _,
                pins: _,
                native: _,
                native_region: _,
            } = &mut *progress;
            // Post-acknowledgement owners joined the pin owner, so they end
            // first.
            let post_ack = post_ack.take();
            let release_pins = release_pins.take();
            let disposals_taken = disposals.take();
            drop(disposals);
            drop(progress);
            let disposals = disposals_taken;
            let mut ended = Vec::new();
            if post_ack.is_some() {
                ended.push("settlement post-acknowledgement owner");
            }
            if release_pins.is_some() {
                ended.push("settlement release pins");
            }
            drop(post_ack);
            drop(release_pins);
            if disposals.as_ref().is_some_and(|disposals| !disposals.is_empty()) {
                ended.push("settlement copy disposals");
            }
            drop(disposals);
            Ok(ended)
        })();
        result.map_err(|error| self.retain_error(error))
    }
    /// The settlement's share of the host release: keep the released host's region release
    /// fact, which must name the finished host's generation and size.
    pub(super) fn retain_region_release(
        self: &Arc<Self>,
        region: lillux_sqlite::original_host_owner::backing::HostStorageRegionReleaseFact,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            let mut progress = self
                .progress
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
            let native = progress.native.as_ref().context("region released before the host facts were captured")?;
            ensure!(
                region.generation().map_err(anyhow::Error::msg)? == native.generation
                    && u64::try_from(region.bytes().map_err(anyhow::Error::msg)?)? == native.region_bytes,
                "offline native host region release fact differs from the finished host"
            );
            if let Some(captured) = &progress.native_region {
                ensure!(
                    captured.generation().map_err(anyhow::Error::msg)? == native.generation,
                    "offline native host region release fact changed after capture"
                );
                return Ok(());
            }
            progress.native_region = Some(region);
            Ok(())
        })();
        result.map_err(|error| self.retain_error(error))
    }
    /// Sign `released.<op>` from captured facts only and publish it.
    /// The record's creation is sticky: once attempted, a failure is never
    /// retried in this process, because the record may exist; the next reset
    /// settles it either way.
    pub(super) fn commit_release(self: &Arc<Self>) -> Result<()> {
        let result = (|| -> Result<()> {
            let phase = *self
                .admission
                .phase
                .try_lock()
                .map_err(|_| anyhow::anyhow!("reservation release phase busy or poisoned"))?;
            if matches!(phase, ReservationReleasePhase::Committed | ReservationReleasePhase::Unlinked) {
                return Ok(());
            }
            ensure!(phase == ReservationReleasePhase::Disposed, "release commit requires the sealed disposal");
            let mut progress = self
                .progress
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
            let signed = match &progress.released {
                Some(signed) => signed.clone(),
                None => {
                    let record = self.completed_release_record(&progress)?;
                    let storage = lillux::cas::canonical_record_storage_quote(&record, MAX_RECORD_BYTES)?;
                    self.seal.claim.debit_original_record_working_storage(storage.working_bytes())?;
                    let signed = record.signed_bytes(&self.seal.claim.signer, &storage)?;
                    progress.released = Some(signed.clone());
                    signed
                }
            };
            self.publish_record(
                &super::inspection_destination::released_name(&self.seal.claim.record.operation_id),
                &signed,
                &mut progress.released_publication,
            )
            .context("the released record's creation is uncertain; settlement pending; the next reset settles it")?;
            let owner = progress
                .released_publication
                .owner
                .clone()
                .context("released record publication lacks its descriptor")?;
            drop(progress);
            {
                let mut slot = self
                    .admission
                    .released_publication
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("released record publication busy or poisoned"))?;
                match &*slot {
                    Some(existing) => ensure!(Arc::ptr_eq(existing, &owner), "released record publication changed"),
                    None => *slot = Some(owner),
                }
            }
            self.admission.advance_phase(ReservationReleasePhase::Disposed)?;
            self.admission.checkpoint_original()
        })();
        result.map_err(|error| self.retain_error(error))
    }
    /// Unlink `reserved.<op>`, then `disposed.<op>`, then
    /// `released.<op>`, each removed durably and its descriptor retired
    /// before the next. A retry resumes at the first unfinished record.
    pub(super) fn unlink_release(self: &Arc<Self>) -> Result<()> {
        let result = (|| -> Result<()> {
            let phase = *self
                .admission
                .phase
                .try_lock()
                .map_err(|_| anyhow::anyhow!("reservation release phase busy or poisoned"))?;
            if phase == ReservationReleasePhase::Unlinked {
                return Ok(());
            }
            ensure!(phase == ReservationReleasePhase::Committed, "release unlink requires the committed release");
            let owners = {
                let progress = self
                    .progress
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
                [
                    self.admission.reservation_publication.clone(),
                    progress.disposition.owner.clone().context("disposition descriptor absent")?,
                    progress.released_publication.owner.clone().context("released record descriptor absent")?,
                ]
            };
            let work: Arc<dyn lillux_sqlite::OriginalWitnessCleanupWork> = self.admission.clone();
            for (index, owner) in owners.iter().enumerate() {
                if index == 2 {
                    self.admission.released_unlinking.store(true, Ordering::Release);
                }
                let mut progress = self
                    .progress
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
                let unlink = &mut progress.unlinked[index];
                if unlink.retired.is_some() {
                    continue;
                }
                if unlink.removed.is_none() {
                    let removed = owner
                        .remove_original_planned(work.clone(), &self.seal.claim.ledger, self.admission.deadline)
                        .map_err(anyhow::Error::new)?;
                    unlink.removed = Some(Arc::new(removed));
                }
                let remaining = self.admission.remaining_steps()?;
                let maximum = u32::try_from(
                    (remaining / 4)
                        .checked_sub(1)
                        .filter(|bound| *bound > 0)
                        .context("ledger record retirement lacks finite admitted scan")?
                        .min(i32::MAX as u64),
                )?;
                let retired = owner.retire_removed_original(
                    unlink.removed.as_ref().expect("actual ledger record removal"),
                    maximum,
                )?;
                unlink.retired = Some(Arc::new(retired));
                drop(progress);
                self.admission.checkpoint_original()?;
            }
            self.admission.advance_phase(ReservationReleasePhase::Committed)?;
            self.admission.checkpoint_original()
        })();
        result.map_err(|error| self.retain_error(error))
    }
    /// SHA-256 of the signed `released.<op>`, once committed.
    pub(super) fn released_sha256(&self) -> Result<String> {
        let progress = self
            .progress
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?;
        Ok(lillux::sha256_hex(progress.released.as_ref().context("released record not signed")?))
    }
    /// The completion gate. It reads the release phase and the facts and
    /// receipts captured during the release only; no factory, terminal or
    /// region owner is consulted.
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
                progress.disposition.acknowledged,
                "original durable disposition acknowledgement incomplete"
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
            let native = progress
                .native
                .as_ref()
                .context("offline native host backend facts not captured")?;
            ensure!(
                native.arena == lillux_sqlite::OriginalNativeArenaDisposition::Released,
                "offline native host arena not released"
            );
            let region = progress
                .native_region
                .as_ref()
                .context("offline native host region release pending; the host has not been released")?;
            ensure!(
                region.generation().map_err(anyhow::Error::msg)? == native.generation
                    && u64::try_from(region.bytes().map_err(anyhow::Error::msg)?)? == native.region_bytes,
                "offline native host region release fact differs from the finished host"
            );
            let phase = *self
                .admission
                .phase
                .try_lock()
                .map_err(|_| anyhow::anyhow!("reservation release phase busy or poisoned"))?;
            ensure!(
                phase == ReservationReleasePhase::Unlinked,
                "reservation release incomplete at phase {phase:?}"
            );
            ensure!(
                progress.released_publication.acknowledged
                    && progress.unlinked.iter().all(|unlink| unlink.retired.is_some()),
                "released record or ledger unlink receipts incomplete"
            );
            let operation = &self.seal.claim.record.operation_id;
            let names = self.seal.claim.ledger.entry_names_bounded(MAX_CLAIMS * 5 + 1)?;
            ensure!(
                names.len() <= MAX_CLAIMS * 5,
                "offline quota ledger exceeds complete inventory capacity"
            );
            for name in names {
                ensure!(
                    !name.to_str().is_some_and(|text| text.ends_with(operation.as_str())),
                    "released reservation left a ledger name for its operation"
                );
            }
            // Every held obligation is discharged by captured facts: the
            // completed record built from them must validate.
            self.completed_release_record(&progress)?;
            Ok(())
        })();
        result.map_err(|error| self.retain_error(error))
    }

    /// The `Completed` released record built from captured facts only.
    fn completed_release_record(
        &self,
        progress: &OriginalSettlementProgress,
    ) -> Result<super::creation_evidence::OfflineResetReleasedRecord> {
        use super::creation_evidence::{CompletedReleaseFacts, OfflineResetReleasedRecord, ReleasedBinding};
        let claim = &self.seal.claim;
        // A mutating reset that applied its history change joined the State
        // acknowledgement; one that found the schema already current never
        // activated an operation and records none.
        let state_ack = match claim.record.inspection.mode {
            OriginalInspectionMode::DryRun => None,
            OriginalInspectionMode::Mutating => progress.state_ack.clone(),
        };
        ensure!(
            state_ack.is_some() == progress.acknowledgement_expected,
            "released State acknowledgement differs from the prepared final acknowledgement"
        );
        OfflineResetReleasedRecord::completed(
            ReleasedBinding {
                signer: claim.record.signer.clone(),
                operation_id: claim.record.operation_id.clone(),
                claim_sha256: lillux::sha256_hex(&claim.signed),
                claim_nonce: claim.record.claim_nonce.clone(),
                ledger_identity: claim.ledger.identity()?,
                reservation: (
                    self.admission.reservation_identity.device(),
                    self.admission.reservation_identity.inode(),
                ),
            },
            CompletedReleaseFacts {
                mode: claim.record.inspection.mode,
                disposition_sha256: lillux::sha256_hex(&self.signed_disposition),
                native: progress.native.as_ref().context("offline native host facts not captured")?,
                region: progress.native_region.as_ref().context("offline native host region release pending")?,
                held: &self.disposition.held,
                pins: progress.pins.clone(),
                state_ack,
                acknowledgement_expected: progress.acknowledgement_expected,
            },
        )
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
/// The claim's share of the exclusive StateLock. The claim owns it from
/// `prepare` until the claim drops, so a live claim implies the StateLock is
/// held; every checkpoint re-verifies that it still protects the app root.
struct ResetStateLease {
    lease: ryeos_node_runtime::state_lock::StateLockLease,
}
impl ResetStateLease {
    fn new(lease: ryeos_node_runtime::state_lock::StateLockLease) -> Self {
        Self { lease }
    }
    fn ensure_protects_app_root(&self, app_root: &std::path::Path) -> Result<()> {
        self.lease.ensure_protects_app_root(app_root)
    }
}
/// The native host's own lifelines beside the claim's StateLock lease: the
/// runtime namespace lock and the signed policy.
struct OriginalNativeAdmissionLifelines {
    namespace: lillux::PinnedDirectoryLock,
    policy: ryeos_node_runtime::node_policy::NodePolicySnapshot,
}
/// The reset's authority for its offline native host. With the claim's
/// StateLock lease it holds the runtime namespace lock and the signed policy
/// for the host's life, and on every host boundary revalidates the published
/// reservation, the bindings, the clock and the claim's service or cleanup
/// window.
pub(super) struct ResetNativeAuthority {
    claim: Arc<OriginalQuotaClaim>,
    lifelines: OriginalNativeAdmissionLifelines,
    /// The host's factory, bound once right after the host is issued.
    factory: std::sync::OnceLock<std::sync::Weak<lillux_sqlite::OriginalNativeFactory>>,
}
impl std::fmt::Debug for ResetNativeAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResetNativeAuthority")
            .field("operation", &self.claim.record.operation_id)
            .finish_non_exhaustive()
    }
}
impl ResetNativeAuthority {
    fn require_custody(&self, cleanup: bool) -> Result<()> {
        ensure!(self.claim.process == std::process::id(),
            "inherited offline native authority refused before custody traversal");
        let lifelines = &self.lifelines;
        self.claim
            .state_lease
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
            "native host authority changed signed original policy"
        );
        ensure!(
            self.claim.publication_acknowledged.load(Ordering::Acquire),
            "native host authority lacks actual original publication ACK"
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
                && entry.inode == original.inode(),
            "native original reservation incarnation or bytes changed"
        );
        // Once bound, the host's own factory scopes this read. Its generation
        // leases are then answered for by their owner instead of refusing
        // every unscoped read, while a lease on the reservation inode itself
        // still refuses. Before binding no lease of this host can exist.
        let bound = self.factory.get().and_then(std::sync::Weak::upgrade);
        let bytes = match bound {
            Some(factory) => factory.read_original_witness(&publication, MAX_RECORD_BYTES, cleanup)?,
            None => publication.read_original(MAX_RECORD_BYTES)?,
        };
        ensure!(
            bytes == self.claim.signed,
            "native original reservation incarnation or bytes changed"
        );
        Ok(())
    }
    /// The host's factory, once bound.
    fn issued_factory(&self) -> Result<Arc<lillux_sqlite::OriginalNativeFactory>> {
        self.factory
            .get()
            .and_then(std::sync::Weak::upgrade)
            .context("original offline native host factory absent")
    }
    fn require_factory(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.issued_factory()?, factory),
            "offline native authority selects a foreign factory"
        );
        Ok(())
    }
}
/// The claim's signed SQL VM ceiling. Copy SQL and native SQL charge the same
/// counter. A charge runs once per opcode, so it is one atomic update and an
/// in-memory check of the precomputed service-window edge: no lock, no IO and
/// no durable check. Steps are never refunded.
struct SqlVmCeiling {
    used: std::sync::atomic::AtomicU64,
    limit: u64,
    service: lillux::time::MonotonicDeadline,
}
impl SqlVmCeiling {
    fn new(limit: u64, service: lillux::time::MonotonicDeadline) -> Self {
        Self { used: std::sync::atomic::AtomicU64::new(0), limit, service }
    }
    fn charge(&self, steps: u64) -> Result<()> {
        ensure!(steps > 0, "empty original SQL VM debit");
        ensure!(!self.service.has_elapsed(), "original SQL service window elapsed; statement stopped");
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(steps).filter(|total| *total <= self.limit)
            })
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("original shared SQL VM ceiling exhausted"))
    }
    #[cfg(test)]
    fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

impl OfflineNativeAuthority for ResetNativeAuthority {
    fn checkpoint(&self, cleanup: bool) -> Result<()> {
        self.require_custody(cleanup)?;
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
            // Service work, including every new native ticket, ends when the
            // claim's producers drain or seal; only cleanup continues.
            let state = self
                .claim
                .production
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original native service gate busy or poisoned"))?;
            ensure!(
                state.phase == OriginalProductionPhase::Producing,
                "original producers draining or sealed; native service refused"
            );
        }
        Ok(())
    }
    fn require_runtime(&self, runtime: &lillux::PinnedDirectory) -> Result<()> {
        ensure!(
            self.claim.runtime.is_same_directory(runtime)?,
            "native factory selects foreign original runtime"
        );
        Ok(())
    }
    fn charge_sql_vm_steps(&self, steps: u64) -> Result<()> {
        self.claim.sql_steps.charge(steps)
    }
}
impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for ResetNativeAuthority {
    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>, cleanup: bool) -> Result<()> {
        self.checkpoint(cleanup)?;
        self.require_factory(factory)
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
    /// Fund and issue the reset's offline native host from the published
    /// claim's native sub-cap. Memory for the host region and the arena is
    /// charged to the claim first; the raw budget is minted once, here, and
    /// moves into the host. A refusal before minting carries no host; a
    /// refusal after it carries the partial host for its release.
    pub(super) fn issue_native_host(
        self: &Arc<Self>,
        owner: &OfflineResetPolicyOwner<'_>,
        identity: &NodeIdentity,
    ) -> std::result::Result<OfflineNativeHost, OfflineNativeHostFailure> {
        let funded = (|| -> Result<(Arc<ResetNativeAuthority>, lillux_sqlite::OriginalNativeBudget)> {
            // Authentic publication and the original Operational source and
            // marker are verified before any memory or native funding.
            self.require_published(owner, identity)?;
            self.record
                .operational_source
                .require_current(&self.runtime, &self.operational_source_pins)?;
            self.require_operational_marker()?;
            self.require_service_window()?;
            let quota = &self.record.reserved;
            let storage = quota.max_native_storage_bytes;
            let cleanup = offline_native_required_cleanup_steps(storage, quota.max_native_connections)?;
            require_native_step_budget(quota)?;
            ensure!(
                storage > 0 && cleanup < quota.max_native_allocator_steps,
                "offline native storage or cleanup quote outside signed dimensions"
            );
            let host_bytes = u64::try_from(
                lillux_sqlite::original_host_owner::backing::HostStorage::quote_native_region()
                    .map_err(anyhow::Error::msg)?
                    .bytes(),
            )?;
            {
                let mut used = self
                    .copy_usage
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("original native memory debit busy or poisoned"))?;
                ensure!(
                    used.native_storage_bytes == 0 && used.host_region_bytes == 0,
                    "original native host memory already charged"
                );
                ensure!(
                    used.buffer_bytes
                        .checked_add(storage)
                        .and_then(|bytes| bytes.checked_add(host_bytes))
                        .is_some_and(|bytes| bytes <= quota.max_memory_bytes),
                    "native arena, host region and buffers exceed signed offline memory"
                );
                used.native_storage_bytes = storage;
                used.host_region_bytes = host_bytes;
            }
            // The claim already holds the StateLock lease; the host's own
            // lifelines are the namespace lock and the signed policy.
            let (namespace, policy) = owner.retain_copy_inputs()?;
            let authority = Arc::new(ResetNativeAuthority {
                claim: self.clone(),
                lifelines: OriginalNativeAdmissionLifelines { namespace, policy },
                factory: std::sync::OnceLock::new(),
            });
            self.native_authority
                .set(Arc::downgrade(&authority))
                .map_err(|_| anyhow::anyhow!("original offline native host already issued"))?;
            OfflineNativeAuthority::checkpoint(authority.as_ref(), false)?;
            // SAFETY: this envelope is the signed claim's native sub-cap,
            // durably reserved once by its publication under the exclusive
            // StateLock and runtime namespace lock; it is minted once, here,
            // and moves into the one host issued below.
            let raw = unsafe {
                lillux_sqlite::OriginalNativeBudget::prepaid_by_original_issuer(
                    storage,
                    quota.max_native_allocator_steps,
                    quota.max_native_connections,
                    cleanup,
                )
            }
            .map_err(|refusal| anyhow::anyhow!("offline native funding refused: {refusal}"))?;
            Ok((authority, raw))
        })();
        let (authority, raw) = funded.map_err(|error| OfflineNativeHostFailure { host: None, error })?;
        let host_authority: Arc<dyn OfflineNativeAuthority> = authority.clone();
        // SAFETY: `issue`'s contract holds: the authority's lifelines keep the
        // StateLock and runtime namespace lock for the host's life, `raw` was
        // minted above once for this host, and every checkpoint revalidates
        // both locks, the published reservation and the claim's windows.
        let host = unsafe { OfflineNativeHost::issue(raw, host_authority) }?;
        let bound = host.factory().and_then(|factory| {
            authority
                .factory
                .set(Arc::downgrade(factory))
                .map_err(|_| anyhow::anyhow!("offline native host factory already bound"))
        });
        match bound {
            Ok(()) => Ok(host),
            Err(error) => Err(OfflineNativeHostFailure { host: Some(host), error }),
        }
    }
    fn native_authority(&self) -> Result<Arc<ResetNativeAuthority>> {
        let authority = self
            .native_authority
            .get()
            .and_then(std::sync::Weak::upgrade)
            .context("original offline native host not issued")?;
        ensure!(
            std::ptr::eq(Arc::as_ptr(&authority.claim), self),
            "offline native authority selects a foreign claim"
        );
        Ok(authority)
    }
    /// The factory of this claim's own issued host.
    fn issued_native_factory(&self) -> Result<Arc<lillux_sqlite::OriginalNativeFactory>> {
        self.native_authority()?.issued_factory()
    }
    pub(super) fn native_projection_lease_authority(self: &Arc<Self>, factory: &Arc<lillux_sqlite::OriginalNativeFactory>)
        -> Result<Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>> {
        ensure!(self.process == std::process::id(), "foreign offline lease issuance");
        let authority = self.native_authority()?;
        lillux_sqlite::OriginalNativeProjectionLeaseAuthority::require_original(authority.as_ref(), factory, false)?;
        Ok(authority)
    }
    fn native_quota_evidence(
        &self,
    ) -> Result<super::creation_evidence::OriginalNativeQuotaEvidence> {
        // Disposal runs only after the host was issued; a claim without one
        // has nothing to dispose and refuses here.
        let snapshot = self.issued_native_factory()?.budget_snapshot()?;
        let evidence = super::creation_evidence::OriginalNativeQuotaEvidence {
            storage_bytes: snapshot.storage_bytes,
            allocator_steps: snapshot.allocator_steps,
            cleanup_reserved: snapshot.cleanup_reserved,
            cleanup_steps: snapshot.cleanup_steps,
            connections_issued: snapshot.connections_issued,
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
impl OriginalQuotaClaim {
    /// The refusal of a destination's outside handoff. The offline native
    /// host has no outside receiver: every destination of this claim ends in
    /// process with its host census, so the handoff always refuses and keeps
    /// its owner; a foreign process, factory or terminal is named first.
    fn destination_outside_transfer_refusal(self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal) -> anyhow::Error {
        let foreign = (|| -> Result<()> {
            ensure!(self.process == std::process::id(), "foreign offline destination account");
            factory.require_original_process()?;
            aggregate.require_original(factory)
        })();
        match foreign {
            Err(error) => error,
            Ok(()) => anyhow::anyhow!(
                "offline reset destinations have no outside receiver; they end with the native host census"
            ),
        }
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
        _leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        _prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<lillux_sqlite::OriginalDestinationOutsideCustody, lillux_sqlite::OriginalDestinationCustodyFailure> {
        // The refusal keeps this authority, with its root and namespace.
        let error = self.claim.destination_outside_transfer_refusal(factory, aggregate);
        Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(factory.clone(), self, error))
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
        let issued = self.claim.issued_native_factory()?;
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
                    .and_then(|bytes| bytes.checked_add(used.host_region_bytes))
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
        _leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        _prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<lillux_sqlite::OriginalDestinationOutsideCustody, lillux_sqlite::OriginalDestinationCustodyFailure> {
        // The refusal keeps this authority, with its root and namespace.
        let error = self.claim.destination_outside_transfer_refusal(factory, aggregate);
        Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(factory.clone(), self, error))
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
        let issued = self.claim.issued_native_factory()?;
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
        let issued = self.seal.claim.issued_native_factory()?;
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
            OriginalProducerKind::Native(_) => lillux_sqlite::SqliteInspectionProductionKind::Native,
            OriginalProducerKind::Copy(_) => lillux_sqlite::SqliteInspectionProductionKind::Copy,
            OriginalProducerKind::Issuer
            | OriginalProducerKind::Namespace(_)
            | OriginalProducerKind::SqlIssue(_) => lillux_sqlite::SqliteInspectionProductionKind::Namespace,
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
        let state_lease = owner.retain_state_lease()?;
        let operational_namespace = owner.retain_namespace()?;
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
        let record = ClaimRecord {
            schema: CLAIM_SCHEMA,
            purpose: CLAIM_PURPOSE.into(),
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
        let sql_steps = SqlVmCeiling::new(record.reserved.max_sql_vm_steps, native_service_deadline);
        let ledger_identity = ledger.identity()?;
        let app_root = app_root.try_clone()?;
        let runtime = runtime.try_clone()?;
        // Registered as the claim comes into existence; its Drop deregisters.
        register_live_claim(ledger_identity);
        Ok(Arc::new(Self {
            record,
            ledger_identity,
            process: std::process::id(),
            state_lease: ResetStateLease::new(state_lease),
            clock,
            native_service_deadline, native_cleanup_deadline,
            sql_steps,
            signer: identity.clone(),
            signed,
            app_root,
            runtime,
            ledger,
            copy_root,
            inspection_namespace: Mutex::new(None),
            publication_entered: AtomicBool::new(false),
            publication_acknowledged: AtomicBool::new(false),
            reservation_publication: Mutex::new(None),
            copy_usage: Mutex::new(CopyUsage::default()),
            production: Mutex::new(OriginalProductionState::default()),
            copy_issuer: Mutex::new(None),
            native_authority: std::sync::OnceLock::new(),
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

    /// The active operation this claim was prepared to resume, if any.
    pub(super) fn prior_operation(&self) -> Option<&Arc<OriginalOperation>> {
        self._prior_operation.as_ref()
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

    pub(super) fn require_service_window(&self) -> Result<std::time::Duration> {
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
        let (namespace, policy) = owner.retain_copy_inputs()?;
        let inspection_namespace = self.retain_inspection_namespace()?;
        self.require_service_window()?;
        let work = Arc::new(OriginalCopyWork {
            claim: Arc::clone(self),
            namespace,
            inspection_namespace,
            policy,
            reservation_name,
            // Safety: the actual issued O_PATH reservation pin transfers once,
            // never a duplicate/new source or reconstructed funding owner.
            reservation_pin: Mutex::new(ReservationPinCustody::Held(unsafe {
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
        // Recovery has settled every reservation of a process that is gone,
        // and this process holds at most one live claim per ledger, so the
        // ledger may hold nothing but this claim's own reservation, and the
        // inspections parent nothing but this claim's own deposits.
        ensure!(
            live_claims_on(&self.ledger_identity) == 1,
            "another live reset claim in this process holds the offline quota ledger"
        );
        let mut original_present = false;
        for name in self.ledger.entry_names_bounded(2)? {
            let bytes = if name == OsStr::new(&selected_name) {
                lillux_sqlite::read_regular_without_sqlite_alias(&self.ledger, &name, MAX_RECORD_BYTES)?
            } else {
                None
            };
            match bytes {
                Some(bytes) if self.publication_entered.load(Ordering::Acquire) && bytes == self.signed => {
                    original_present = true;
                }
                Some(_) => {
                    return Err(super::creation_evidence::ResetScratchRepairRequired {
                        what: format!(
                            "another offline reservation already owns operation {}",
                            self.record.operation_id
                        ),
                        foreign: None,
                    }
                    .into());
                }
                None => {
                    return Err(super::creation_evidence::ResetScratchRepairRequired {
                        what: format!(
                            "offline quota ledger holds `{}` beside this reset's reservation",
                            name.to_string_lossy()
                        ),
                        foreign: super::scratch::foreign_ledger_path(&name),
                    }
                    .into());
                }
            }
        }
        let own_deposit = |name: &str| {
            let name = name.strip_prefix("cleanup.").unwrap_or(name);
            let name = name.strip_prefix("created.").unwrap_or(name);
            self.record.destinations.iter().any(|plan| plan.directory_name == name)
        };
        for name in self.copy_root.entry_names_bounded(8)? {
            let text = name.to_string_lossy();
            if !(original_present && own_deposit(&text)) {
                return Err(super::creation_evidence::ResetScratchRepairRequired {
                    what: format!("offline reset inspections hold `{text}` before this reset's reservation"),
                    foreign: super::scratch::foreign_inspections_path(&name),
                }
                .into());
            }
        }
        // The claim reserves its complete signed envelope.
        validate_aggregate(std::slice::from_ref(&self.record.reserved), owner.quota()?)?;
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
    host_region_bytes: u64,
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

/// Field order is drop order: the work's own locks, policy and reservation
/// pin end before the claim, whose StateLock lease outlives them.
struct OriginalCopyWork {
    namespace: lillux::PinnedDirectoryLock,
    inspection_namespace: lillux::PinnedDirectoryLock,
    policy: ryeos_node_runtime::node_policy::NodePolicySnapshot,
    reservation_name: String,
    /// The issued reservation pin until the release retires it; afterwards
    /// the identity it held, so checks still name the charged inode.
    reservation_pin: Mutex<ReservationPinCustody>,
    reservation_identity: lillux::OpenFileIdentity,
    claim: Arc<OriginalQuotaClaim>,
}
/// The issued reservation pin, or the identity it held once retired.
enum ReservationPinCustody {
    Held(Arc<lillux_sqlite::RetainedOriginalMetadataPin>),
    Retired(lillux::OpenFileIdentity),
}

enum OriginalSqlRootCustody {
    Owned(Arc<lillux::PinnedDirectory>),
    InFlight,
    Registered(lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration),
}
struct OriginalSqlWork {
    copy: Arc<OriginalCopyWork>,
    store: usize,
    root: Mutex<OriginalSqlRootCustody>,
}
impl OriginalSqlWork {
    fn service_root(&self) -> Result<Arc<lillux::PinnedDirectory>> {
        ensure!(self.copy.claim.process == std::process::id(), "foreign original SQL root");
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
        ensure!(self.copy.claim.process == std::process::id(), "foreign original SQL outside root");
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
        ensure!(self.copy.claim.process == std::process::id(), "foreign original SQL root receipt");
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
        self.copy.claim.sql_steps.charge(steps)
    }
}

impl OriginalCopyWork {
    fn original_reservation_identity(&self) -> Result<lillux::OpenFileIdentity> {
        let custody = self
            .reservation_pin
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original reservation pin owner busy or poisoned"))?;
        match &*custody {
            ReservationPinCustody::Held(pin) => {
                let pin = pin.clone();
                drop(custody);
                pin.observed_identity()
            }
            ReservationPinCustody::Retired(identity) => Ok(*identity),
        }
    }
    /// Move the issued reservation pin into the release's retirement,
    /// keeping its observed identity here. A retry returns the same pin.
    fn take_reservation_pin(
        &self,
        moved: &Mutex<Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>>,
    ) -> Result<Arc<lillux_sqlite::RetainedOriginalMetadataPin>> {
        let mut custody = self
            .reservation_pin
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original reservation pin owner busy or poisoned"))?;
        let mut moved = moved
            .try_lock()
            .map_err(|_| anyhow::anyhow!("moved reservation pin slot busy or poisoned"))?;
        if let ReservationPinCustody::Held(pin) = &*custody {
            let identity = pin.observed_identity()?;
            ensure!(identity == self.reservation_identity, "issued reservation pin changed before release");
            ensure!(moved.is_none(), "reservation pin moved twice");
            *moved = Some(pin.clone());
            *custody = ReservationPinCustody::Retired(identity);
        }
        moved.clone().context("issued reservation pin left without its release")
    }
    fn require_custody(&self) -> Result<()> {
        self.claim
            .state_lease
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
        let issued = self.claim.issued_native_factory()?;
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
            .and_then(|n| n.checked_add(used.host_region_bytes))
            .is_some_and(|n| n <= self.claim.record.reserved.max_memory_bytes),
            "original SQL owner exceeds same reserved host memory");
        used.buffer_bytes = buffers;
        used.sql_admitted[index] = true;
        drop(used);
        production.acknowledge_synchronous()?;
        Ok(Arc::new(OriginalSqlWork {
            store: index,
            copy: self,
            root: Mutex::new(OriginalSqlRootCustody::Owned(roots[index].as_ref().unwrap().clone())),
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
                    .and_then(|bytes| bytes.checked_add(used.host_region_bytes))
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

pub(super) fn require_operation_suffix(operation: &str) -> Result<()> {
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
        settlement.seal_disposition().unwrap();
        // A retry of the same sealed step is idempotent.
        settlement.seal_disposition().unwrap();
        let error = settlement.require_completed().unwrap_err();
        assert!(format!("{error:#}").contains("not captured"), "{error:#}");
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
            max_native_allocator_steps: 24000000,
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
            schema: CLAIM_SCHEMA,
            purpose: CLAIM_PURPOSE.into(),
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
        // The host draws its own generation: the v10 claim carries none.
        for field in ["resident_process", "resident_generation", "resident_bytes"] {
            let mut extra = serde_json::to_value(&original).unwrap();
            extra[field] = serde_json::json!(1);
            assert!(serde_json::from_value::<ClaimRecord>(extra).is_err(), "{field}");
        }
        let mut wrong = original.clone();
        wrong.reserved.max_memory_bytes = wrong.reserved.max_native_storage_bytes;
        assert!(wrong.validate().is_err(), "native arena cannot consume the host region's memory");
        let mut undersized = original.clone();
        undersized.reserved.max_native_allocator_steps = 1048576;
        let error = undersized.validate().unwrap_err();
        assert!(format!("{error:#}").contains("SQL VM step ceiling"), "{error:#}");
        let mut short = original.clone();
        short.reserved.max_native_allocator_steps = short.reserved.max_sql_vm_steps + 1;
        let error = short.validate().unwrap_err();
        assert!(format!("{error:#}").contains("allocation funding margin"), "{error:#}");
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
    fn native_sql_is_refused_at_the_shared_signed_ceiling() {
        let ceiling = SqlVmCeiling::new(1_000, lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(3600)));
        for _ in 0..1_000 {
            ceiling.charge(1).unwrap();
        }
        let error = ceiling.charge(1).unwrap_err();
        assert!(format!("{error:#}").contains("ceiling exhausted"), "{error:#}");
        assert_eq!(ceiling.used(), 1_000, "a refused charge is not recorded");
    }

    #[test]
    fn copy_and_native_sql_together_cannot_exceed_the_ceiling() {
        let ceiling = std::sync::Arc::new(SqlVmCeiling::new(
            40_000,
            lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(3600)),
        ));
        // Copy SQL and native SQL charge the same claim counter from
        // different threads; their total never passes the signed ceiling.
        let charge = |ceiling: std::sync::Arc<SqlVmCeiling>| {
            std::thread::spawn(move || (0..30_000).filter(|_| ceiling.charge(1).is_ok()).count())
        };
        let copy = charge(ceiling.clone());
        let native = charge(ceiling.clone());
        let admitted = copy.join().unwrap() + native.join().unwrap();
        assert_eq!(admitted, 40_000);
        assert_eq!(ceiling.used(), 40_000);
        assert!(ceiling.charge(1).is_err());
    }

    #[test]
    fn sql_is_stopped_once_the_service_window_edge_passes() {
        let ceiling = SqlVmCeiling::new(u64::MAX, lillux::time::MonotonicDeadline::after(std::time::Duration::from_millis(50)));
        ceiling.charge(1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80));
        let error = ceiling.charge(1).unwrap_err();
        assert!(format!("{error:#}").contains("service window elapsed"), "{error:#}");
        assert_eq!(ceiling.used(), 1);
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
        assert_eq!(ClaimRecord::verified(&claim_bytes, &signer).unwrap().operation_id, claim.operation_id);
        use super::super::creation_evidence::{
            DISPOSITION_PURPOSE, DISPOSITION_SCHEMA, InspectionDispositionRecord,
            InspectionStoreDispositionRecord, OriginalLocalSettlementObligation,
        };
        let disposition = InspectionDispositionRecord {
            schema: DISPOSITION_SCHEMA,
            purpose: DISPOSITION_PURPOSE.into(),
            signer: claim.signer.clone(),
            operation_id: claim.operation_id.clone(),
            claim_nonce: claim.claim_nonce.clone(),
            claim_sha256: hash.clone(),
            policy_digest: claim.policy_digest.clone(),
            clock_binding: claim.clock_binding.clone(),
            parent_identity: root.identity().unwrap(),
            inspection: claim.inspection.clone(),
            native_quota: super::super::creation_evidence::OriginalNativeQuotaEvidence {
                storage_bytes: 1, allocator_steps: 0,
                cleanup_reserved: 1, cleanup_steps: 0, connections_issued: 0,
            },
            stores: std::array::from_fn(|index| InspectionStoreDispositionRecord::NeverStarted {
                purpose: claim.destinations[index].purpose.clone(),
                destination_name: claim.destinations[index].directory_name.clone(),
                original_present: [false; 4],
            }),
            held: vec![
                OriginalLocalSettlementObligation::NativeAllocationAccounting { attempted: 0 },
                OriginalLocalSettlementObligation::ReservationPublicationDescriptor,
                OriginalLocalSettlementObligation::IssuedReservationPin,
            ],
        };
        let signed = disposition.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&disposition, MAX_RECORD_BYTES).unwrap()).unwrap();
        let mut missing_native = serde_json::to_value(&disposition).unwrap();
        missing_native
            .as_object_mut()
            .unwrap()
            .remove("native_quota");
        assert!(serde_json::from_value::<InspectionDispositionRecord>(missing_native).is_err());
        let mut old_disposition = disposition.clone();
        old_disposition.schema = 3;
        old_disposition.purpose = "offline_reset_inspection_disposition_v3".into();
        assert!(old_disposition.validate().is_err());
        let mut malformed = disposition.clone();
        malformed.native_quota.cleanup_steps = 2;
        assert!(malformed.validate().is_err());
        // The held snapshot must name the charged reservation and the host's
        // native accounting, in canonical order.
        let mut unheld = disposition.clone();
        unheld.held.pop();
        assert!(unheld.validate().is_err());
        let mut unsorted = disposition.clone();
        unsorted.held.reverse();
        assert!(unsorted.validate().is_err());
        let mut unaccounted = disposition.clone();
        unaccounted.held.remove(0);
        assert!(unaccounted.validate().is_err());
        let verified = InspectionDispositionRecord::verified(&signed, &signer).unwrap();
        require_disposition_claim(&verified, &claim, &hash).unwrap();
        let mut wrong = verified.clone();
        wrong.claim_nonce = "2".repeat(32);
        let signed_wrong = wrong.signed_bytes(&signer, &lillux::cas::canonical_record_storage_quote(&wrong, MAX_RECORD_BYTES).unwrap()).unwrap();
        let wrong = InspectionDispositionRecord::verified(&signed_wrong, &signer).unwrap();
        assert!(require_disposition_claim(&wrong, &claim, &hash).is_err());
        assert!(
            !verified.held.is_empty(),
            "the disposition snapshot never refunds its held reservation"
        );
        assert!(require_operation_suffix("../foreign").is_err());
    }

    #[test]
    fn retired_claim_schema_is_refused_naming_the_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let signer =
            ryeos_node_runtime::identity::NodeIdentity::create(&tmp.path().join("identity.pem"))
                .unwrap();
        let mut claim = serde_json::to_value(record()).unwrap();
        claim["signer"] = serde_json::json!(signer.fingerprint());
        claim["schema"] = serde_json::json!(9);
        claim["purpose"] = serde_json::json!("offline_reset_local_quota_reservation_v9");
        for (field, value) in [("resident_process", 1), ("resident_generation", 1), ("resident_bytes", 4096)] {
            claim[field] = serde_json::json!(value);
        }
        let body = lillux::canonical_json(&claim).unwrap();
        let bytes = lillux::signature::sign_content(&body, signer.signing_key(), "#", None).into_bytes();
        let error = ClaimRecord::verified(&bytes, &signer).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("quota claim record uses retired schema 9"), "{message}");
        assert!(message.contains(ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR), "{message}");
        assert!(error.downcast_ref::<super::super::creation_evidence::ResetScratchRepairRequired>().is_some());
    }

    #[test]
    fn release_phases_advance_once_in_order() {
        use ReservationReleasePhase::{Committed, Disposed, Held, Unlinked};
        assert_eq!(
            [Held, Disposed, Committed, Unlinked].map(ReservationReleasePhase::next),
            [Some(Disposed), Some(Committed), Some(Unlinked), None]
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
        // Each claim must still admit every source role, so the aggregate
        // quota holds twice that many.
        let mut quota = record().reserved;
        quota.max_source_roles *= 2;
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
        validate_aggregate(&[one.clone(), one.clone()], &quota).unwrap();
        let mut excess = one.clone();
        excess.max_cleanup_steps = 513;
        let error = validate_aggregate(&[one, excess], &quota).unwrap_err();
        assert!(format!("{error:#}").contains("aggregate resource quota exhausted"), "{error:#}");
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

#[cfg(test)]
mod live_claim_tests {
    use super::*;

    #[test]
    fn live_claims_are_counted_per_ledger_and_deregistered() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first = lillux::PinnedDirectory::open(first.path()).unwrap().unwrap().identity().unwrap();
        let second = lillux::PinnedDirectory::open(second.path()).unwrap().unwrap().identity().unwrap();
        assert_eq!(live_claims_on(&first), 0);
        register_live_claim(first);
        register_live_claim(first);
        assert_eq!(live_claims_on(&first), 2);
        // A claim on another ledger neither counts nor blocks this one.
        assert_eq!(live_claims_on(&second), 0);
        register_live_claim(second);
        deregister_live_claim(&first);
        assert_eq!((live_claims_on(&first), live_claims_on(&second)), (1, 1));
        deregister_live_claim(&first);
        deregister_live_claim(&second);
        assert_eq!((live_claims_on(&first), live_claims_on(&second)), (0, 0));
        // Deregistering an unregistered ledger changes nothing.
        deregister_live_claim(&first);
        assert_eq!(live_claims_on(&first), 0);
    }
}

/// Actual source retirement plus native terminal. The charged reservation and
/// original finalizer lifelines remain owned; this is not full completion.
pub(super) struct OriginalReleasePins {
    seal: Arc<OriginalProductionSeal>,
    admission: Arc<OriginalSettlementAdmission>,
    factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    terminal: Arc<lillux_sqlite::OriginalNativeTerminal>,
    /// The source pins with their identities, captured before any close.
    facts: Vec<super::creation_evidence::RetiredPinFact>,
    pins: Vec<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>,
    receipts: Mutex<Vec<Option<Arc<lillux_sqlite::RetiredOriginalMetadataPin>>>>,
}
impl OriginalReleasePins {
    /// Actual State effect or cached receipt replay, irreversibly charged to
    /// the same original cleanup/read/memory owner BEFORE State effects.
    /// Only State's own fixed effect and replay quotes are admitted.
    pub(super) fn debit_state_ack_attempt(
        &self,
        read_bytes: u64,
        memory_bytes: u64,
        steps: u64,
    ) -> Result<()> {
        let attempt = (read_bytes, memory_bytes, steps);
        ensure!(
            attempt == ryeos_state::state_db::STATE_ACK_EFFECT_PREPAY
                || attempt == ryeos_state::state_db::STATE_ACK_REPLAY_PREPAY,
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
                    .and_then(|bytes| bytes.checked_add(used.host_region_bytes)).is_some_and(
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
        ensure!(
            self.facts.len() == self.pins.len(),
            "source retirement facts differ from the pin census"
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
impl lillux_sqlite::OriginalMetadataPinRetirementAuthority for OriginalReleasePins {
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
        self.require_custody()
    }
    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        self.admission.debit_original(steps)
    }
}
impl OriginalInspectionSettlement {
    /// Readiness for the State final acknowledgement of a performed reset:
    /// the operation must be this claim's own (fresh, or the resumed one the
    /// claim was prepared for and adopted), and the release pins are retired
    /// first.
    pub(super) fn prepare_final_ack(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        operation: &Arc<OriginalOperation>,
    ) -> Result<Arc<OriginalReleasePins>> {
        let checked = (|| -> Result<()> {
            let claim = &self.seal.claim;
            ensure!(
                claim._prior_operation.as_ref().is_none_or(|prior| {
                    Arc::ptr_eq(prior, operation) && prior.reference() == operation.reference()
                }),
                "final acknowledgement selects an operation other than the claim's resumed one"
            );
            operation.require_quota_claim(claim)?;
            ensure!(
                claim.record.inspection.mode == OriginalInspectionMode::Mutating,
                "final acknowledgement prepared for a dry-run reset"
            );
            self.progress
                .try_lock()
                .map_err(|_| anyhow::anyhow!("original settlement progress busy or poisoned"))?
                .acknowledgement_expected = true;
            Ok(())
        })();
        checked.map_err(|error| self.retain_error(error))?;
        self.retire_release_pins(factory, terminal)
    }

    /// Retire every source pin and the issued reservation pin against
    /// the finished backend under one pin-retirement owner, and capture each
    /// pin's identity as plain data. Runs once the backend finished and the
    /// disposal is sealed; a retry returns the same owner and skips every
    /// retired pin.
    pub(super) fn retire_release_pins(
        self: &Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
    ) -> Result<Arc<OriginalReleasePins>> {
        let result = (|| -> Result<Arc<OriginalReleasePins>> {
            self.admission.checkpoint_original()?;
            self.seal
                .require_original(&self.seal.claim, &self.seal.work)?;
            terminal.require_original(factory)?;
            ensure!(
                terminal.arena_disposition()?
                    == lillux_sqlite::OriginalNativeArenaDisposition::Released,
                "release pin retirement requires actual original native release"
            );
            let cached = {
                let progress = self
                    .progress
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("source retirement custody busy or poisoned"))?;
                ensure!(
                    progress.disposition.acknowledged,
                    "source retirement requires the durable disposition"
                );
                progress.release_pins.clone()
            };
            ensure!(
                *self
                    .admission
                    .phase
                    .try_lock()
                    .map_err(|_| anyhow::anyhow!("reservation release phase busy or poisoned"))?
                    == ReservationReleasePhase::Disposed,
                "source retirement requires the sealed disposal"
            );
            let owner = if let Some(owner) = cached {
                ensure!(
                    Arc::ptr_eq(factory, &owner.factory) && Arc::ptr_eq(terminal, &owner.terminal),
                    "source retirement cannot replace actual backend owner"
                );
                owner
            } else {
                let claim = &self.seal.claim;
                // Prepay the fixed fourteen-pin owner bookkeeping before
                // capturing identities or retiring any pin.
                self.admission.reserve_records(MAX_RECORD_BYTES as u64)?;
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
                // The issued reservation pin moves out of the copy work last;
                // the copy work keeps the identity it held for later checks.
                let reservation = self
                    .admission
                    .original
                    .take_reservation_pin(&self.moved_reservation_pin)?;
                inventory.push((super::creation_evidence::ISSUED_RESERVATION_PIN_SLOT, reservation));
                let mut facts = Vec::new();
                let mut pins = Vec::new();
                for (slot, pin) in inventory {
                    self.admission.debit_original(2)?;
                    let identity = pin.observed_identity()?;
                    facts.push(super::creation_evidence::RetiredPinFact {
                        slot,
                        device: identity.device(),
                        inode: identity.inode(),
                        byte_length: identity.size(),
                    });
                    pins.push(pin);
                }
                let count = pins.len();
                let owner = Arc::new(OriginalReleasePins {
                    seal: self.seal.clone(),
                    admission: self.admission.clone(),
                    factory: factory.clone(),
                    terminal: terminal.clone(),
                    facts,
                    pins,
                    receipts: Mutex::new((0..count).map(|_| None).collect()),
                });
                let mut progress = self.progress.try_lock().map_err(|_| {
                    anyhow::anyhow!("source retirement registration busy or poisoned")
                })?;
                ensure!(
                    progress.release_pins.is_none(),
                    "source retirement registration conflict"
                );
                progress.release_pins = Some(owner.clone());
                drop(progress);
                // The owner now holds the moved reservation pin.
                drop(
                    self.moved_reservation_pin
                        .try_lock()
                        .map_err(|_| anyhow::anyhow!("moved reservation pin slot busy or poisoned"))?
                        .take(),
                );
                owner
            };
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
            // The retirement inputs hold the factory, terminal and pins; they
            // end here, before any later holder census.
            drop(inputs);
            owner.require_original(&self.seal.claim, &self.seal.work, factory, terminal)?;
            // Every pin is retired: keep its captured identity as the
            // release's plain fact, once.
            {
                let mut progress = self.progress.try_lock().map_err(|_| {
                    anyhow::anyhow!("retired pin facts busy or poisoned")
                })?;
                if progress.pins.is_empty() {
                    progress.pins.extend(owner.facts.iter().cloned());
                    progress.pins.sort_by_key(|fact| fact.slot);
                } else {
                    let mut captured = owner.facts.clone();
                    captured.sort_by_key(|fact| fact.slot);
                    ensure!(progress.pins == captured, "retired pin facts changed after capture");
                }
            }
            Ok(owner)
        })();
        result.map_err(|error| self.retain_error(error))
    }
}

/// Real State ACK is retained separately from physical finalizer completion.
/// Every original reservation and lifeline obligation remains charged here.
/// Field order is drop order: every post-acknowledgement owner ends before
/// `ready`, whose release pins, factory and backend terminal they joined.
pub(super) struct OriginalPostAckFinalization {
    state: Arc<ryeos_state::state_db::ClosedOriginalState>,
    acknowledgement: Arc<ryeos_state::state_db::OriginalStateAcknowledgement>,
    operation: ThreadHistoryDiscardOperationRef,
    ready: Arc<OriginalReleasePins>,
}

impl OriginalPostAckFinalization {
    fn original_state_inputs(&self) -> Result<(
        Arc<ryeos_state::state_db::ClosedOriginalState>,
        Arc<ryeos_state::state_db::OriginalStateAcknowledgement>,
    )> {
        self.ready.factory.require_original_process()?;
        Ok((self.state.clone(), self.acknowledgement.clone()))
    }
    fn require_state_ack(&self) -> Result<()> {
        let (state, acknowledgement) = self.original_state_inputs()?;
        acknowledgement.require_original(
            &state, &self.ready.factory, &self.ready.terminal, &self.operation,
        )
    }
}

impl OriginalPostAckFinalization {
    /// Check and charge one bounded in-process step after the State
    /// acknowledgement against the same settlement admission and its cleanup
    /// window. It renews no deadline and grants no new allowance.
    pub(super) fn debit_post_ack_cleanup(&self, steps: u64) -> Result<()> {
        self.require_state_ack()?;
        self.ready.require_custody()?;
        self.ready.admission.debit_original(steps)
    }
    pub(super) fn require_original(
        &self,
        ready: &Arc<OriginalReleasePins>,
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
        ready: &Arc<OriginalReleasePins>,
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
                        .release_pins
                        .as_ref()
                        .is_some_and(|original| Arc::ptr_eq(original, ready)),
                    "post-ACK requires actual retained ready producer"
                );
                ensure!(
                    progress.state_ack.as_ref().is_none_or(|captured| captured == operation),
                    "post-ACK State acknowledgement changed after capture"
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
                    .and_then(|total| total.checked_add(used.host_region_bytes))
                    .is_some_and(|total| total <= self.admission.claim.record.reserved.max_memory_bytes),
                    "original post-ACK parent exceeds same signed memory envelope");
                used.buffer_bytes = memory;
                drop(used);
                self.admission.checkpoint_original()?;
                let owner = Arc::new(OriginalPostAckFinalization {
                    ready: ready.clone(),
                    state: state.clone(),
                    acknowledgement: acknowledgement.clone(),
                    operation: operation.clone(),
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
                // before fallible late original clock/funding checks. The
                // acknowledged operation is kept as plain data too, so the
                // release record never needs this owner once it is retired.
                progress.post_ack = Some(owner.clone());
                progress.state_ack = Some(operation.clone());
                owner
            };
            owner.require_original(ready, state, acknowledgement, operation)?;
            Ok(owner)
        })();
        result.map_err(|error| self.retain_error(error))
    }
}

