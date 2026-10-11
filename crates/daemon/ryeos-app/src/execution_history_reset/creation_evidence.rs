//! Signed facts about a created native directory. These are evidence only:
//! readers cannot reconstruct the original descriptor/removal owner from them.
use anyhow::{Result, ensure};
use ryeos_node_runtime::identity::NodeIdentity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreatedDirectoryRecord {
    pub schema: u32,
    pub purpose: String,
    pub signer: String,
    pub operation_id: String,
    pub claim_nonce: String,
    pub claim_sha256: String,
    pub policy_digest: String,
    pub clock_binding: String,
    pub source_purpose: String,
    pub destination_name: String,
    pub parent_identity: lillux::PinnedDirectoryIdentity,
    pub created_identity: lillux::PinnedDirectoryIdentity,
}

pub(super) fn evidence_name(destination: &str) -> String {
    format!("created.{destination}")
}
// Compare the complete fixed coordinate without allocating a formatted name.
// Prefix matching alone is insufficient: reject both suffixes and slot changes.
fn matches_inspection_destination(name: &str, operation: &str, purpose: &str) -> bool {
    name.strip_prefix("inspection.")
        .and_then(|tail| tail.strip_prefix(operation))
        .and_then(|tail| tail.strip_prefix('.'))
        == Some(purpose)
}
impl CreatedDirectoryRecord {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.purpose == "offline_reset_inspection_created",
            "unsupported created-directory evidence"
        );
        for (text, size) in [
            (&self.signer, 64),
            (&self.operation_id, 32),
            (&self.claim_nonce, 32),
            (&self.claim_sha256, 64),
            (&self.policy_digest, 64),
            (&self.clock_binding, 64),
        ] {
            ensure!(
                text.len() == size
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "created-directory evidence has invalid binding"
            );
        }
        ensure!(
            matches!(self.source_purpose.as_str(), "runtime" | "scheduler")
                && matches_inspection_destination(
                    &self.destination_name, &self.operation_id, &self.source_purpose),
            "created-directory evidence changes fixed operation selection"
        );
        Ok(())
    }
    pub(super) fn signed_bytes(&self, identity: &NodeIdentity,
        storage: &lillux::cas::CanonicalRecordStorageQuote) -> Result<Vec<u8>> {
        self.validate()?;
        sign_inspection_record(self, &self.signer, identity, storage)
    }
    pub(super) fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        let record: Self = verify_inspection_record(
            bytes, identity, ("created-directory", 1, "offline_reset_inspection_created"))?;
        record.validate()?;
        ensure!(record.signer == identity.fingerprint(),
            "created-directory evidence is foreign");
        Ok(record)
    }

}

/// Required signed counter facts only; never acquisition/allocation terminals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalNativeQuotaEvidence {
    pub storage_bytes: u64,
    pub allocator_steps: u64,
    pub cleanup_reserved: u64,
    pub cleanup_steps: u64,
    pub connections_issued: u32,
}
impl OriginalNativeQuotaEvidence {
    /// Every disposed reset ran on an issued host, so its counters are
    /// always present.
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.storage_bytes > 0
                && self.cleanup_reserved > 0
                && self.cleanup_steps <= self.cleanup_reserved,
            "invalid original native admission counter shape"
        );
        Ok(())
    }
    pub(super) fn require_quota(
        &self,
        quota: &ryeos_scheduler_contract::OfflineResetQuota,
    ) -> Result<()> {
        self.validate()?;
        quota.validate()?;
        ensure!(
            self.storage_bytes <= quota.max_native_storage_bytes
                && self.connections_issued <= quota.max_native_connections
                && self
                    .allocator_steps
                    .checked_add(self.cleanup_reserved)
                    .is_some_and(|total| total <= quota.max_native_allocator_steps),
            "native admission evidence exceeds same signed original dimensions"
        );
        Ok(())
    }
}

/// Persistent facts about original inspection disposal. Never native authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum InspectionStoreDispositionRecord {
    NeverStarted {
        purpose: String,
        destination_name: String,
        original_present: [bool; 4],
    },
    Removed {
        purpose: String,
        destination_name: String,
        created_identity: lillux::PinnedDirectoryIdentity,
        witness_sha256: String,
        retired_outputs: [Option<(u64, u64)>; 4],
    },
}
/// One obligation a reset holds while its reservation is charged. The order
/// of the variants is the canonical order of a recorded list.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum OriginalLocalSettlementObligation {
    NativeAllocationAccounting {
        attempted: u32,
    },
    SourcePins {
        count: u32,
    },
    ReservationPublicationDescriptor,
    IssuedReservationPin,
}

/// Bound on any recorded obligation list.
const MAX_OBLIGATIONS: usize = 16;

/// Check that `obligations` is in canonical order with no repeated kind.
fn require_canonical_obligations(obligations: &[OriginalLocalSettlementObligation]) -> Result<()> {
    ensure!(
        obligations.len() <= MAX_OBLIGATIONS,
        "inspection obligations exceed finite capacity"
    );
    for pair in obligations.windows(2) {
        ensure!(
            pair[0] < pair[1]
                && std::mem::discriminant(&pair[0]) != std::mem::discriminant(&pair[1]),
            "inspection obligations are unsorted or repeat a kind"
        );
    }
    for obligation in obligations {
        match obligation {
            OriginalLocalSettlementObligation::NativeAllocationAccounting { attempted: _ }
            | OriginalLocalSettlementObligation::ReservationPublicationDescriptor
            | OriginalLocalSettlementObligation::IssuedReservationPin => {}
            OriginalLocalSettlementObligation::SourcePins { count } => ensure!(
                *count > 0 && *count <= ryeos_scheduler_contract::OFFLINE_RESET_SOURCE_ROLES,
                "source pin obligation exceeds the fixed source roles"
            ),
        }
    }
    Ok(())
}

/// Signed snapshot of one reset's disposed inspection stores and the
/// obligations it holds at disposal. `held` is what the release must still
/// discharge; it is never a completion check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectionDispositionRecord {
    pub schema: u32,
    pub purpose: String,
    pub signer: String,
    pub operation_id: String,
    pub claim_nonce: String,
    pub claim_sha256: String,
    pub policy_digest: String,
    pub clock_binding: String,
    pub parent_identity: lillux::PinnedDirectoryIdentity,
    pub inspection: super::inspection_destination::OriginalInspectionPlan,
    pub native_quota: OriginalNativeQuotaEvidence,
    pub stores: [InspectionStoreDispositionRecord; 2],
    pub held: Vec<OriginalLocalSettlementObligation>,
}
pub(super) const DISPOSITION_SCHEMA: u32 = 4;
pub(super) const DISPOSITION_PURPOSE: &str = "offline_reset_inspection_disposition_v4";
impl InspectionDispositionRecord {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == DISPOSITION_SCHEMA && self.purpose == DISPOSITION_PURPOSE,
            "unsupported inspection disposition record"
        );
        self.native_quota.validate()?;
        for (text, length) in [
            (&self.signer, 64),
            (&self.operation_id, 32),
            (&self.claim_nonce, 32),
            (&self.claim_sha256, 64),
            (&self.policy_digest, 64),
            (&self.clock_binding, 64),
        ] {
            ensure!(
                text.len() == length
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid original disposition binding"
            );
        }
        require_canonical_obligations(&self.held)?;
        // A disposed reset still holds its published reservation, the
        // issued reservation pin and its host's allocation accounting.
        ensure!(
            self.held.contains(&OriginalLocalSettlementObligation::ReservationPublicationDescriptor)
                && self.held.contains(&OriginalLocalSettlementObligation::IssuedReservationPin),
            "disposition omits the held reservation"
        );
        ensure!(
            self.held.iter().any(|obligation| matches!(
                obligation,
                OriginalLocalSettlementObligation::NativeAllocationAccounting { attempted: _ }
            )),
            "disposition omits its host's native allocation accounting"
        );
        for (store, purpose) in self.stores.iter().zip(["runtime", "scheduler"]) {
            let (selected, name) = match store {
                InspectionStoreDispositionRecord::NeverStarted {
                    purpose,
                    destination_name,
                    ..
                } => (purpose, destination_name),
                InspectionStoreDispositionRecord::Removed {
                    purpose,
                    destination_name,
                    witness_sha256,
                    ..
                } => {
                    ensure!(
                        witness_sha256.len() == 64
                            && witness_sha256
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                        "invalid original creation witness hash"
                    );
                    (purpose, destination_name)
                }
            };
            ensure!(
                selected == purpose
                    && matches_inspection_destination(name, &self.operation_id, purpose),
                "disposition changes original fixed slot"
            );
        }
        Ok(())
    }
    pub(super) fn signed_bytes(&self, identity: &NodeIdentity,
        storage: &lillux::cas::CanonicalRecordStorageQuote) -> Result<Vec<u8>> {
        self.validate()?;
        sign_inspection_record(self, &self.signer, identity, storage)
    }
    pub(super) fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        let record: Self = verify_inspection_record(
            bytes, identity, ("disposition", DISPOSITION_SCHEMA, DISPOSITION_PURPOSE))?;
        record.validate()?;
        ensure!(
            record.signer == identity.fingerprint(),
            "foreign disposition signer"
        );
        Ok(record)
    }
}
fn sign_inspection_record<T: Serialize>(
    record: &T,
    signer: &str,
    identity: &NodeIdentity,
    storage: &lillux::cas::CanonicalRecordStorageQuote,
) -> Result<Vec<u8>> {
    ensure!(
        signer == identity.fingerprint(),
        "inspection record changes original signer"
    );
    // The original caller funds this exact immutable quote before entry.
    // Fixed raw/index/output slabs replace the unbounded generic JSON tree.
    ensure!(storage.output_bytes() <= 8192, "inspection canonical output exceeds original record capacity");
    let body = String::from_utf8(lillux::cas::canonical_record_bytes(record, storage)?)?;
    let bytes = lillux::signature::sign_content_bounded(
        &body, identity.signing_key(), "#", None, 8192,
    ).map_err(anyhow::Error::msg)?.into_bytes();
    ensure!(
        bytes.len() <= 8192,
        "inspection record exceeds original record capacity"
    );
    Ok(bytes)
}
/// Reset scratch state this build refuses: a record under a retired schema,
/// or a name no current record uses. Nothing is upgraded in place; the
/// explicit scratch repair clears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResetScratchRepairRequired {
    pub(super) what: String,
}
impl std::fmt::Display for ResetScratchRepairRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}; stop the node and run `{}` to clear it",
            self.what,
            ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR
        )
    }
}
impl std::error::Error for ResetScratchRepairRequired {}

/// The schema and purpose of a signed record body, read before the record's
/// own exact shape so a retired schema is refused by name rather than as a
/// decoding failure.
#[derive(Deserialize)]
struct RecordHeader {
    schema: u64,
    purpose: String,
}

/// Refuse `body` unless it carries exactly the current `schema` and
/// `purpose` of the named record.
pub(super) fn require_current_record(
    body: &str,
    record: &'static str,
    schema: u32,
    purpose: &str,
) -> Result<()> {
    let header: RecordHeader = serde_json::from_str(body).map_err(|_| ResetScratchRepairRequired {
        what: format!("offline reset {record} record has no recognizable schema"),
    })?;
    if header.schema != u64::from(schema) || header.purpose != purpose {
        return Err(ResetScratchRepairRequired {
            what: format!(
                "offline reset {record} record uses retired schema {} (`{}`); this build reads schema {schema}",
                header.schema, header.purpose
            ),
        }
        .into());
    }
    Ok(())
}

fn verify_inspection_record<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
    identity: &NodeIdentity,
    (record, schema, purpose): (&'static str, u32, &str),
) -> Result<T> {
    ensure!(
        bytes.len() <= 8192,
        "inspection record exceeds original record capacity"
    );
    let raw = std::str::from_utf8(bytes)?;
    let body = lillux::signature::verified_leading_body(
        raw, "#", None, identity.verifying_key(), identity.fingerprint())?;
    require_current_record(body, record, schema, purpose)?;
    let record: T = serde_json::from_str(body)?;
    // Selected evidence schemas contain integer counters, strings and fixed
    // structured facts. Reuse the bounded canonical record mechanism instead
    // of constructing a second, generic Value/Map ownership graph. This quote
    // is dimensions only: the enclosing recovery/admission caller must still
    // fund decoding, signature verification, counting and these working slabs.
    let storage = lillux::cas::canonical_record_storage_quote(&record, 8192)?;
    let canonical = lillux::cas::canonical_record_bytes(&record, &storage)?;
    ensure!(canonical.as_slice() == body.as_bytes(), "noncanonical inspection record");
    Ok(record)
}


fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Identity of one retired descriptor: slots 0–12 are the fixed source roles
/// and slot 14 is the issued reservation pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetiredPinFact {
    pub slot: u8,
    pub device: u64,
    pub inode: u64,
    pub byte_length: u64,
}
pub(super) const ISSUED_RESERVATION_PIN_SLOT: u8 = 14;

/// The native arena's state when the host's backend finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum NativeArenaRecord {
    NotAcquired,
    Released,
}

/// Plain counters of a finished offline native host, copied from its facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OfflineNativeFactsRecord {
    pub generation: u64,
    pub region_bytes: u64,
    pub storage_bytes: u64,
    pub arena: NativeArenaRecord,
    pub allocator_steps: u64,
    pub cleanup_reserved: u64,
    pub cleanup_steps: u64,
    pub connections_issued: u32,
    pub tickets_issued: u32,
    pub terminals: u32,
    pub ticketless: u32,
    pub leases: u32,
}
impl OfflineNativeFactsRecord {
    /// Copy the plain facts of a host whose backend finished in this process.
    pub(super) fn captured(facts: &lillux_sqlite::offline_host::OfflineNativeFacts) -> Result<Self> {
        let lillux_sqlite::offline_host::OfflineNativeFacts {
            process,
            generation,
            region_bytes,
            storage_bytes,
            arena,
            budget,
            tickets_issued,
            terminals,
            ticketless,
            leases,
        } = facts;
        ensure!(*process == std::process::id(), "offline native facts belong to another process");
        let lillux_sqlite::OriginalNativeBudgetSnapshot {
            storage_bytes: _,
            allocator_steps,
            cleanup_reserved,
            cleanup_steps,
            connections_issued,
        } = budget;
        let record = Self {
            generation: *generation,
            region_bytes: *region_bytes,
            storage_bytes: *storage_bytes,
            arena: match arena {
                lillux_sqlite::OriginalNativeArenaDisposition::NotAcquired => NativeArenaRecord::NotAcquired,
                lillux_sqlite::OriginalNativeArenaDisposition::Released => NativeArenaRecord::Released,
            },
            allocator_steps: *allocator_steps,
            cleanup_reserved: *cleanup_reserved,
            cleanup_steps: *cleanup_steps,
            connections_issued: *connections_issued,
            tickets_issued: *tickets_issued,
            terminals: u32::try_from(*terminals)?,
            ticketless: u32::try_from(*ticketless)?,
            leases: u32::try_from(*leases)?,
        };
        record.validate()?;
        Ok(record)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.generation > 0
                && self.region_bytes > 0
                && self.storage_bytes > 0
                && self.cleanup_reserved > 0
                && self.cleanup_steps <= self.cleanup_reserved,
            "offline native facts record has an impossible counter shape"
        );
        ensure!(
            usize::try_from(self.tickets_issued)? <= lillux_sqlite::MAX_NATIVE_CONNECTIONS
                && usize::try_from(self.connections_issued)? <= lillux_sqlite::MAX_NATIVE_CONNECTIONS,
            "offline native facts record exceeds the native ticket capacity"
        );
        Ok(())
    }
}

/// The host region's release fact, copied when the host was released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RegionReleaseFactRecord {
    pub generation: u64,
    pub bytes: u64,
}
impl RegionReleaseFactRecord {
    pub(super) fn captured(
        fact: &lillux_sqlite::original_host_owner::backing::HostStorageRegionReleaseFact,
    ) -> Result<Self> {
        Ok(Self {
            generation: fact.generation().map_err(anyhow::Error::msg)?,
            bytes: u64::try_from(fact.bytes().map_err(anyhow::Error::msg)?)?,
        })
    }
}

/// Where an abandoned deposit lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DepositLocation {
    Ledger,
    InspectionParent,
    RuntimeNamespace,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AbandonedDepositKind {
    Directory,
    Witness,
    Quarantine,
    SecureTemp,
    InertOperation,
}
/// The observed identity of an abandoned deposit. Recovery removes an entry
/// only while it still has this identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum DepositIdentity {
    Directory { identity: lillux::PinnedDirectoryIdentity },
    File { device: u64, inode: u64 },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AbandonedDeposit {
    pub location: DepositLocation,
    pub name: String,
    pub kind: AbandonedDepositKind,
    pub identity: DepositIdentity,
    pub witness_bound: bool,
}
/// Whether an abandoned reset's history change was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AppliedEvidence {
    Applied,
    NotApplied,
    Resumed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ReleasedOutcome {
    /// Built only from facts captured during the release: the finished
    /// host's counters, the region release fact, the discharged obligations
    /// and the retired pins.
    Completed {
        mode: super::inspection_destination::OriginalInspectionMode,
        disposition_sha256: String,
        native: OfflineNativeFactsRecord,
        region: RegionReleaseFactRecord,
        discharged: Vec<OriginalLocalSettlementObligation>,
        pins: Vec<RetiredPinFact>,
        #[serde(deserialize_with = "required_nullable")]
        state_ack: Option<ryeos_state::ThreadHistoryDiscardOperationRef>,
    },
    /// Recorded by recovery before it removes exactly these deposits.
    Abandoned {
        #[serde(deserialize_with = "required_nullable")]
        disposition_sha256: Option<String>,
        deposits: Vec<AbandonedDeposit>,
        applied: AppliedEvidence,
        recovered_at_ms: u64,
    },
}

/// The commit point of one reset's release, for completion and recovery.
/// Its presence means the reservation it names is released; unlinking the
/// reservation, the disposition and then this record finishes the release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OfflineResetReleasedRecord {
    pub schema: u32,
    pub purpose: String,
    pub signer: String,
    pub operation_id: String,
    /// SHA-256 of the exact signed `reserved.<op>` bytes.
    pub claim_sha256: String,
    pub claim_nonce: String,
    pub ledger_identity: lillux::PinnedDirectoryIdentity,
    /// (device, inode) of `reserved.<op>`.
    pub reservation: (u64, u64),
    pub outcome: ReleasedOutcome,
}
pub(super) const RELEASED_SCHEMA: u32 = 1;
pub(super) const RELEASED_PURPOSE: &str = "offline_reset_reservation_released_v1";
const MAX_ABANDONED_DEPOSITS: usize = 64;
/// The operation id of a release recorded for debris that no reservation
/// anchors.
pub(super) const RECOVERY_ONLY_OPERATION: &str = "00000000000000000000000000000000";

/// The claim-side bindings every released record repeats.
pub(super) struct ReleasedBinding {
    pub signer: String,
    pub operation_id: String,
    pub claim_sha256: String,
    pub claim_nonce: String,
    pub ledger_identity: lillux::PinnedDirectoryIdentity,
    pub reservation: (u64, u64),
}

/// Captured plain-data facts of a completed release.
pub(super) struct CompletedReleaseFacts<'a> {
    pub mode: super::inspection_destination::OriginalInspectionMode,
    pub disposition_sha256: String,
    pub native: &'a lillux_sqlite::offline_host::OfflineNativeFacts,
    pub region: &'a lillux_sqlite::original_host_owner::backing::HostStorageRegionReleaseFact,
    pub held: &'a [OriginalLocalSettlementObligation],
    pub pins: Vec<RetiredPinFact>,
    pub state_ack: Option<ryeos_state::ThreadHistoryDiscardOperationRef>,
    /// Whether the release prepared a State final acknowledgement.
    pub acknowledgement_expected: bool,
}

impl OfflineResetReleasedRecord {
    /// Build a `Completed` record from captured facts only. The reservation
    /// publication obligation is discharged by this record's own unlink, so
    /// every other held obligation is discharged here.
    pub(super) fn completed(binding: ReleasedBinding, facts: CompletedReleaseFacts<'_>) -> Result<Self> {
        let ReleasedBinding { signer, operation_id, claim_sha256, claim_nonce, ledger_identity, reservation } =
            binding;
        let CompletedReleaseFacts {
            mode,
            disposition_sha256,
            native,
            region,
            held,
            pins,
            state_ack,
            acknowledgement_expected,
        } = facts;
        ensure!(
            state_ack.is_some() == acknowledgement_expected,
            "State acknowledgement presence differs from the prepared final acknowledgement"
        );
        require_canonical_obligations(held)?;
        let discharged = held
            .iter()
            .filter(|obligation| {
                **obligation != OriginalLocalSettlementObligation::ReservationPublicationDescriptor
            })
            .cloned()
            .collect();
        let record = Self {
            schema: RELEASED_SCHEMA,
            purpose: RELEASED_PURPOSE.into(),
            signer,
            operation_id,
            claim_sha256,
            claim_nonce,
            ledger_identity,
            reservation,
            outcome: ReleasedOutcome::Completed {
                mode,
                disposition_sha256,
                native: OfflineNativeFactsRecord::captured(native)?,
                region: RegionReleaseFactRecord::captured(region)?,
                discharged,
                pins,
                state_ack,
            },
        };
        record.validate()?;
        Ok(record)
    }

    pub(super) fn validate(&self) -> Result<()> {
        let Self {
            schema,
            purpose,
            signer,
            operation_id,
            claim_sha256,
            claim_nonce,
            ledger_identity: _,
            reservation,
            outcome,
        } = self;
        ensure!(
            *schema == RELEASED_SCHEMA && purpose == RELEASED_PURPOSE,
            "unsupported offline reset released record"
        );
        ensure!(
            is_lower_hex(signer, 64)
                && is_lower_hex(operation_id, 32)
                && is_lower_hex(claim_sha256, 64)
                && is_lower_hex(claim_nonce, 32),
            "invalid offline reset released binding"
        );
        let recovery_only = operation_id == RECOVERY_ONLY_OPERATION;
        if recovery_only {
            ensure!(
                claim_sha256.bytes().all(|b| b == b'0')
                    && claim_nonce.bytes().all(|b| b == b'0')
                    && *reservation == (0, 0),
                "recovery-only release names a reservation"
            );
        } else {
            ensure!(reservation.1 > 0, "released record lacks its reservation identity");
        }
        match outcome {
            ReleasedOutcome::Completed { mode, disposition_sha256, native, region, discharged, pins, state_ack } => {
                ensure!(!recovery_only, "a recovery-only release cannot be completed");
                ensure!(is_lower_hex(disposition_sha256, 64), "invalid released disposition digest");
                native.validate()?;
                ensure!(
                    native.arena == NativeArenaRecord::Released,
                    "completed release requires the native arena released"
                );
                ensure!(
                    region.generation == native.generation && region.bytes == native.region_bytes,
                    "region release fact differs from the finished host"
                );
                require_canonical_obligations(discharged)?;
                ensure!(
                    !discharged.contains(&OriginalLocalSettlementObligation::ReservationPublicationDescriptor),
                    "the reservation publication is discharged by the release unlink, not recorded"
                );
                ensure!(
                    discharged.contains(&OriginalLocalSettlementObligation::IssuedReservationPin),
                    "completed release omits the issued reservation pin"
                );
                ensure!(
                    discharged.iter().any(|obligation| matches!(
                        obligation,
                        OriginalLocalSettlementObligation::NativeAllocationAccounting { attempted: _ }
                    )),
                    "completed release omits its host's native allocation accounting"
                );
                let source_count = discharged
                    .iter()
                    .find_map(|obligation| match obligation {
                        OriginalLocalSettlementObligation::SourcePins { count } => Some(*count),
                        OriginalLocalSettlementObligation::NativeAllocationAccounting { attempted: _ }
                        | OriginalLocalSettlementObligation::ReservationPublicationDescriptor
                        | OriginalLocalSettlementObligation::IssuedReservationPin => None,
                    })
                    .unwrap_or(0);
                let mut previous: Option<u8> = None;
                let mut sources = 0u32;
                let mut issued = 0u32;
                for pin in pins {
                    ensure!(
                        pin.inode > 0 && previous.is_none_or(|slot| pin.slot > slot),
                        "retired pins are unsorted, repeated or unidentified"
                    );
                    previous = Some(pin.slot);
                    if pin.slot < 13 {
                        sources += 1;
                    } else if pin.slot == ISSUED_RESERVATION_PIN_SLOT {
                        issued += 1;
                    } else {
                        anyhow::bail!("retired pin names a foreign slot");
                    }
                }
                ensure!(
                    sources == source_count && issued == 1,
                    "retired pins differ from the discharged pin obligations"
                );
                // Only a mutating reset acknowledges State. One that found
                // the schema already current applied nothing and records none.
                ensure!(
                    state_ack.is_none()
                        || *mode == super::inspection_destination::OriginalInspectionMode::Mutating,
                    "State acknowledgement presence differs from the reset mode"
                );
                if let Some(acknowledged) = state_ack {
                    acknowledged.validate()?;
                    ensure!(
                        acknowledged.operation_id == *operation_id,
                        "State acknowledgement names another operation"
                    );
                }
            }
            ReleasedOutcome::Abandoned { disposition_sha256, deposits, applied, recovered_at_ms } => {
                ensure!(*recovered_at_ms > 0, "abandoned release lacks its recovery time");
                if let Some(digest) = disposition_sha256 {
                    ensure!(is_lower_hex(digest, 64), "invalid abandoned disposition digest");
                }
                ensure!(
                    deposits.len() <= MAX_ABANDONED_DEPOSITS,
                    "abandoned deposits exceed finite capacity"
                );
                for pair in deposits.windows(2) {
                    ensure!(
                        (pair[0].location, pair[0].name.as_str()) < (pair[1].location, pair[1].name.as_str()),
                        "abandoned deposits are unsorted or repeated"
                    );
                }
                for deposit in deposits {
                    require_deposit(operation_id, deposit)?;
                }
                if recovery_only {
                    ensure!(
                        disposition_sha256.is_none()
                            && *applied == AppliedEvidence::NotApplied
                            && deposits.iter().all(|deposit| deposit.kind == AbandonedDepositKind::SecureTemp),
                        "a recovery-only release may record only reset temporary debris"
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn signed_bytes(&self, identity: &NodeIdentity,
        storage: &lillux::cas::CanonicalRecordStorageQuote) -> Result<Vec<u8>> {
        self.validate()?;
        sign_inspection_record(self, &self.signer, identity, storage)
    }

    pub(super) fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        let record: Self =
            verify_inspection_record(bytes, identity, ("released", RELEASED_SCHEMA, RELEASED_PURPOSE))?;
        record.validate()?;
        ensure!(record.signer == identity.fingerprint(), "foreign released record signer");
        Ok(record)
    }
}

/// Check one abandoned deposit's name against its kind's grammar for
/// `operation`, and its identity shape.
fn require_deposit(operation: &str, deposit: &AbandonedDeposit) -> Result<()> {
    let AbandonedDeposit { location, name, kind, identity, witness_bound } = deposit;
    let inspection = |name: &str| {
        ["runtime", "scheduler"]
            .iter()
            .any(|purpose| matches_inspection_destination(name, operation, purpose))
    };
    let witness = |name: &str| name.strip_prefix("created.").is_some_and(inspection);
    let directory = matches!(identity, DepositIdentity::Directory { identity: _ });
    let named = match kind {
        AbandonedDepositKind::Directory => {
            *location == DepositLocation::InspectionParent && inspection(name) && directory
        }
        AbandonedDepositKind::Witness => {
            *location == DepositLocation::InspectionParent && witness(name) && !directory
        }
        AbandonedDepositKind::Quarantine => {
            !directory
                && name.strip_prefix("cleanup.").is_some_and(|target| match location {
                    DepositLocation::Ledger => ["reserved.", "disposed.", "released."]
                        .iter()
                        .any(|prefix| target.strip_prefix(prefix) == Some(operation)),
                    DepositLocation::InspectionParent => witness(target),
                    DepositLocation::RuntimeNamespace => false,
                })
        }
        AbandonedDepositKind::SecureTemp => {
            !directory
                && matches!(
                    lillux::parse_secure_temp_name(std::ffi::OsStr::new(name)),
                    lillux::SecureTempClass::Temp { purpose, pid: _, seq: _ }
                        if purpose == crate::execution_history_reset::RESET_TEMP_PURPOSE.as_str()
                )
        }
        AbandonedDepositKind::InertOperation => {
            *location == DepositLocation::RuntimeNamespace
                && !directory
                && name.strip_prefix("execution-history-reset.operation.") == Some(operation)
        }
    };
    ensure!(named, "abandoned deposit `{name}` does not match its kind for this operation");
    ensure!(
        !*witness_bound || *kind == AbandonedDepositKind::Directory,
        "only an inspection directory can be witness-bound"
    );
    match identity {
        DepositIdentity::Directory { identity: _ } => {}
        DepositIdentity::File { device: _, inode } => {
            ensure!(*inode > 0, "abandoned file deposit lacks its identity")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::inspection_destination::OriginalInspectionMode;

    const QUOTE_BOUND: usize = 8192;

    fn signer() -> (tempfile::TempDir, NodeIdentity) {
        let tmp = tempfile::tempdir().unwrap();
        let identity = NodeIdentity::create(&tmp.path().join("identity.pem")).unwrap();
        (tmp, identity)
    }

    fn sign<T: Serialize>(record: &T, identity: &NodeIdentity) -> Vec<u8> {
        let storage = lillux::cas::canonical_record_storage_quote(record, QUOTE_BOUND).unwrap();
        sign_inspection_record(record, identity.fingerprint(), identity, &storage).unwrap()
    }

    fn sign_raw(body: serde_json::Value, identity: &NodeIdentity) -> Vec<u8> {
        let body = lillux::canonical_json(&body).unwrap();
        lillux::signature::sign_content(&body, identity.signing_key(), "#", None).into_bytes()
    }

    fn operation() -> String {
        "a".repeat(32)
    }

    fn binding(identity: &NodeIdentity) -> ReleasedBinding {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        ReleasedBinding {
            signer: identity.fingerprint().to_owned(),
            operation_id: operation(),
            claim_sha256: "b".repeat(64),
            claim_nonce: "c".repeat(32),
            ledger_identity: ledger.identity().unwrap(),
            reservation: (1, 7),
        }
    }

    fn held() -> Vec<OriginalLocalSettlementObligation> {
        vec![
            OriginalLocalSettlementObligation::NativeAllocationAccounting { attempted: 2 },
            OriginalLocalSettlementObligation::SourcePins { count: 2 },
            OriginalLocalSettlementObligation::ReservationPublicationDescriptor,
            OriginalLocalSettlementObligation::IssuedReservationPin,
        ]
    }

    fn pins() -> Vec<RetiredPinFact> {
        vec![
            RetiredPinFact { slot: 0, device: 1, inode: 10, byte_length: 4096 },
            RetiredPinFact { slot: 8, device: 1, inode: 11, byte_length: 0 },
            RetiredPinFact { slot: ISSUED_RESERVATION_PIN_SLOT, device: 1, inode: 7, byte_length: 900 },
        ]
    }

    /// Facts from a real host released in this process.
    fn released_host() -> lillux_sqlite::offline_host::OfflineNativeReleased {
        let host = lillux_sqlite::test_host::TestNativeHost::acquire();
        host.release()
    }

    fn completed(
        identity: &NodeIdentity,
        released: &lillux_sqlite::offline_host::OfflineNativeReleased,
        mode: OriginalInspectionMode,
    ) -> Result<OfflineResetReleasedRecord> {
        let state_ack = match mode {
            OriginalInspectionMode::DryRun => None,
            OriginalInspectionMode::Mutating => Some(ryeos_state::ThreadHistoryDiscardOperationRef {
                operation_id: operation(),
                sha256: "d".repeat(64),
            }),
        };
        OfflineResetReleasedRecord::completed(
            binding(identity),
            CompletedReleaseFacts {
                mode,
                disposition_sha256: "e".repeat(64),
                native: released.facts.as_ref().expect("finished host facts"),
                region: released.region.as_ref().expect("released region"),
                held: &held(),
                pins: pins(),
                acknowledgement_expected: state_ack.is_some(),
                state_ack,
            },
        )
    }

    fn deposit(
        location: DepositLocation,
        name: String,
        kind: AbandonedDepositKind,
        identity: DepositIdentity,
        witness_bound: bool,
    ) -> AbandonedDeposit {
        AbandonedDeposit { location, name, kind, identity, witness_bound }
    }

    fn file(inode: u64) -> DepositIdentity {
        DepositIdentity::File { device: 1, inode }
    }

    fn abandoned(identity: &NodeIdentity, deposits: Vec<AbandonedDeposit>) -> OfflineResetReleasedRecord {
        let ReleasedBinding { signer, operation_id, claim_sha256, claim_nonce, ledger_identity, reservation } =
            binding(identity);
        OfflineResetReleasedRecord {
            schema: RELEASED_SCHEMA,
            purpose: RELEASED_PURPOSE.into(),
            signer,
            operation_id,
            claim_sha256,
            claim_nonce,
            ledger_identity,
            reservation,
            outcome: ReleasedOutcome::Abandoned {
                disposition_sha256: Some("e".repeat(64)),
                deposits,
                applied: AppliedEvidence::NotApplied,
                recovered_at_ms: 1,
            },
        }
    }

    fn every_deposit_kind() -> Vec<AbandonedDeposit> {
        let op = operation();
        let tmp = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap().identity().unwrap();
        let mut deposits = vec![
            deposit(DepositLocation::Ledger, format!("cleanup.reserved.{op}"), AbandonedDepositKind::Quarantine, file(2), false),
            deposit(DepositLocation::Ledger, ".secure.tmp.reset.41.0".into(), AbandonedDepositKind::SecureTemp, file(3), false),
            deposit(
                DepositLocation::InspectionParent,
                format!("inspection.{op}.runtime"),
                AbandonedDepositKind::Directory,
                DepositIdentity::Directory { identity: directory },
                true,
            ),
            deposit(DepositLocation::InspectionParent, format!("created.inspection.{op}.runtime"), AbandonedDepositKind::Witness, file(4), false),
            deposit(
                DepositLocation::InspectionParent,
                format!("cleanup.created.inspection.{op}.scheduler"),
                AbandonedDepositKind::Quarantine,
                file(5),
                false,
            ),
            deposit(
                DepositLocation::RuntimeNamespace,
                format!("execution-history-reset.operation.{op}"),
                AbandonedDepositKind::InertOperation,
                file(6),
                false,
            ),
            deposit(DepositLocation::RuntimeNamespace, ".secure.tmp.reset.41.1".into(), AbandonedDepositKind::SecureTemp, file(8), false),
        ];
        deposits.sort_by(|left, right| (left.location, left.name.as_str()).cmp(&(right.location, right.name.as_str())));
        deposits
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completed_release_is_built_from_captured_facts_and_round_trips() {
        let (_tmp, identity) = signer();
        let released = released_host();
        for mode in [OriginalInspectionMode::DryRun, OriginalInspectionMode::Mutating] {
            let record = completed(&identity, &released, mode).unwrap();
            let ReleasedOutcome::Completed { discharged, native, region, .. } = &record.outcome else {
                panic!("completed outcome");
            };
            assert!(
                !discharged.contains(&OriginalLocalSettlementObligation::ReservationPublicationDescriptor),
                "the reservation publication is discharged by the unlink"
            );
            assert_eq!(discharged.len(), held().len() - 1);
            assert_eq!((region.generation, region.bytes), (native.generation, native.region_bytes));
            let signed = sign(&record, &identity);
            assert_eq!(OfflineResetReleasedRecord::verified(&signed, &identity).unwrap(), record);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completed_release_refuses_inconsistent_facts() {
        let (_tmp, identity) = signer();
        let released = released_host();
        let original = completed(&identity, &released, OriginalInspectionMode::DryRun).unwrap();
        let refuse = |change: &dyn Fn(&mut OfflineResetReleasedRecord), expected: &str| {
            let mut record = original.clone();
            change(&mut record);
            let error = record.validate().unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{expected}: {error:#}");
        };
        refuse(
            &|record| {
                if let ReleasedOutcome::Completed { discharged, .. } = &mut record.outcome {
                    // In canonical position, so only the discharge rule refuses it.
                    let at = discharged.len() - 1;
                    discharged.insert(at, OriginalLocalSettlementObligation::ReservationPublicationDescriptor);
                }
            },
            "discharged by the release unlink",
        );
        refuse(
            &|record| {
                if let ReleasedOutcome::Completed { pins, .. } = &mut record.outcome {
                    pins.remove(0);
                }
            },
            "differ from the discharged pin obligations",
        );
        refuse(
            &|record| {
                if let ReleasedOutcome::Completed { pins, .. } = &mut record.outcome {
                    pins.swap(0, 1);
                }
            },
            "unsorted",
        );
        refuse(
            &|record| {
                if let ReleasedOutcome::Completed { region, .. } = &mut record.outcome {
                    region.generation += 1;
                }
            },
            "differs from the finished host",
        );
        refuse(
            &|record| {
                if let ReleasedOutcome::Completed { state_ack, .. } = &mut record.outcome {
                    *state_ack = Some(ryeos_state::ThreadHistoryDiscardOperationRef {
                        operation_id: operation(),
                        sha256: "d".repeat(64),
                    });
                }
            },
            "differs from the reset mode",
        );
        refuse(
            &|record| {
                if let ReleasedOutcome::Completed { native, .. } = &mut record.outcome {
                    native.arena = NativeArenaRecord::NotAcquired;
                }
            },
            "arena released",
        );
        refuse(
            &|record| {
                if let ReleasedOutcome::Completed { discharged, .. } = &mut record.outcome {
                    discharged.remove(0);
                }
            },
            "omits its host's native allocation accounting",
        );
        refuse(&|record| record.operation_id = RECOVERY_ONLY_OPERATION.into(), "names a reservation");
        // A held list that is not canonical is refused before any record exists.
        let mut unsorted = held();
        unsorted.swap(0, 3);
        let error = OfflineResetReleasedRecord::completed(
            binding(&identity),
            CompletedReleaseFacts {
                mode: OriginalInspectionMode::DryRun,
                disposition_sha256: "e".repeat(64),
                native: released.facts.as_ref().unwrap(),
                region: released.region.as_ref().unwrap(),
                held: &unsorted,
                pins: pins(),
                state_ack: None,
                acknowledgement_expected: false,
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("unsorted"), "{error:#}");
        // A prepared final acknowledgement must be recorded, and an
        // unprepared one must not be.
        for (state_ack, acknowledgement_expected) in [
            (None, true),
            (
                Some(ryeos_state::ThreadHistoryDiscardOperationRef { operation_id: operation(), sha256: "d".repeat(64) }),
                false,
            ),
        ] {
            let error = OfflineResetReleasedRecord::completed(
                binding(&identity),
                CompletedReleaseFacts {
                    mode: OriginalInspectionMode::Mutating,
                    disposition_sha256: "e".repeat(64),
                    native: released.facts.as_ref().unwrap(),
                    region: released.region.as_ref().unwrap(),
                    held: &held(),
                    pins: pins(),
                    state_ack,
                    acknowledgement_expected,
                },
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("prepared final acknowledgement"), "{error:#}");
        }
    }

    #[test]
    fn abandoned_release_records_every_deposit_kind_and_round_trips() {
        let (_tmp, identity) = signer();
        let record = abandoned(&identity, every_deposit_kind());
        record.validate().unwrap();
        let signed = sign(&record, &identity);
        assert_eq!(OfflineResetReleasedRecord::verified(&signed, &identity).unwrap(), record);
        // The recovery-only record names no reservation and holds only debris.
        let mut recovery = abandoned(
            &identity,
            vec![deposit(DepositLocation::Ledger, ".secure.tmp.reset.9.0".into(), AbandonedDepositKind::SecureTemp, file(3), false)],
        );
        recovery.operation_id = RECOVERY_ONLY_OPERATION.into();
        recovery.claim_sha256 = "0".repeat(64);
        recovery.claim_nonce = "0".repeat(32);
        recovery.reservation = (0, 0);
        if let ReleasedOutcome::Abandoned { disposition_sha256, .. } = &mut recovery.outcome {
            *disposition_sha256 = None;
        }
        recovery.validate().unwrap();
        let signed = sign(&recovery, &identity);
        assert_eq!(OfflineResetReleasedRecord::verified(&signed, &identity).unwrap(), recovery);
    }

    #[test]
    fn abandoned_release_refuses_foreign_or_malformed_deposits() {
        let (_tmp, identity) = signer();
        let other = "f".repeat(32);
        let refused = |deposits: Vec<AbandonedDeposit>, expected: &str| {
            let error = abandoned(&identity, deposits).validate().unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{expected}: {error:#}");
        };
        refused(
            vec![deposit(DepositLocation::InspectionParent, format!("created.inspection.{other}.runtime"), AbandonedDepositKind::Witness, file(4), false)],
            "does not match its kind",
        );
        refused(
            vec![deposit(DepositLocation::Ledger, ".secure.tmp.state.41.0".into(), AbandonedDepositKind::SecureTemp, file(3), false)],
            "does not match its kind",
        );
        refused(
            vec![deposit(DepositLocation::Ledger, ".secure.tmp.41.0".into(), AbandonedDepositKind::SecureTemp, file(3), false)],
            "does not match its kind",
        );
        refused(
            vec![deposit(DepositLocation::Ledger, format!("execution-history-reset.operation.{}", operation()), AbandonedDepositKind::InertOperation, file(6), false)],
            "does not match its kind",
        );
        refused(
            vec![deposit(DepositLocation::RuntimeNamespace, format!("cleanup.reserved.{}", operation()), AbandonedDepositKind::Quarantine, file(2), false)],
            "does not match its kind",
        );
        refused(
            vec![deposit(DepositLocation::InspectionParent, format!("inspection.{}.runtime", operation()), AbandonedDepositKind::Directory, file(9), false)],
            "does not match its kind",
        );
        refused(
            vec![deposit(DepositLocation::InspectionParent, format!("created.inspection.{}.runtime", operation()), AbandonedDepositKind::Witness, file(4), true)],
            "witness-bound",
        );
        refused(
            vec![deposit(DepositLocation::InspectionParent, format!("created.inspection.{}.runtime", operation()), AbandonedDepositKind::Witness, file(0), false)],
            "lacks its identity",
        );
        let mut reversed = every_deposit_kind();
        reversed.reverse();
        refused(reversed, "unsorted");
        let mut recovery = abandoned(&identity, every_deposit_kind());
        recovery.operation_id = RECOVERY_ONLY_OPERATION.into();
        recovery.claim_sha256 = "0".repeat(64);
        recovery.claim_nonce = "0".repeat(32);
        recovery.reservation = (0, 0);
        let error = recovery.validate().unwrap_err();
        assert!(format!("{error:#}").contains("does not match its kind"), "{error:#}");
    }

    #[test]
    fn released_record_shape_is_exact() {
        let (_tmp, identity) = signer();
        let record = abandoned(&identity, every_deposit_kind());
        let value = serde_json::to_value(&record).unwrap();
        for field in ["ledger_identity", "reservation", "outcome", "claim_nonce"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<OfflineResetReleasedRecord>(missing).is_err(), "{field}");
        }
        let mut missing = value.clone();
        missing["outcome"].as_object_mut().unwrap().remove("disposition_sha256");
        assert!(
            serde_json::from_value::<OfflineResetReleasedRecord>(missing).is_err(),
            "a nullable digest is still required"
        );
        let mut unknown = value;
        unknown["outcome"]["refund"] = serde_json::json!(true);
        assert!(serde_json::from_value::<OfflineResetReleasedRecord>(unknown).is_err());
    }

    #[test]
    fn retired_record_schemas_are_refused_naming_the_repair() {
        let (_tmp, identity) = signer();
        let mut released = serde_json::to_value(abandoned(&identity, every_deposit_kind())).unwrap();
        released["schema"] = serde_json::json!(0);
        released["purpose"] = serde_json::json!("offline_reset_reservation_released_v0");
        let error = OfflineResetReleasedRecord::verified(&sign_raw(released, &identity), &identity).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("retired schema 0"), "{message}");
        assert!(message.contains(ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR), "{message}");
        assert!(error.downcast_ref::<ResetScratchRepairRequired>().is_some());

        // A schema-2 disposition still carried `remaining` and authorities.
        let disposition = serde_json::json!({
            "schema": 2,
            "purpose": "offline_reset_inspection_disposition_v2",
            "signer": identity.fingerprint(),
            "operation_id": operation(),
            "remaining": [],
            "retained_authorities": [],
        });
        let error = InspectionDispositionRecord::verified(&sign_raw(disposition, &identity), &identity).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("disposition record uses retired schema 2"), "{message}");
        assert!(message.contains(ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR), "{message}");

        let unrecognizable = serde_json::json!({ "kind": "terminal" });
        let error = InspectionDispositionRecord::verified(&sign_raw(unrecognizable, &identity), &identity).unwrap_err();
        assert!(format!("{error:#}").contains(ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR), "{error:#}");
    }
}
