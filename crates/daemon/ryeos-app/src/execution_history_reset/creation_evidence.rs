//! Signed facts about a created native directory. These are evidence only:
//! readers cannot reconstruct the original descriptor/removal owner from them.
use anyhow::{Context, Result, ensure};
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
        let record: Self = verify_inspection_record(bytes, identity)?;
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
    pub admitted: bool,
    pub storage_bytes: u64,
    pub allocator_steps: u64,
    pub cleanup_reserved: u64,
    pub cleanup_steps: u64,
    pub connections_issued: u32,
}
impl OriginalNativeQuotaEvidence {
    pub(super) fn unissued() -> Self {
        Self {
            admitted: false,
            storage_bytes: 0,
            allocator_steps: 0,
            cleanup_reserved: 0,
            cleanup_steps: 0,
            connections_issued: 0,
        }
    }
    pub(super) fn validate(&self) -> Result<()> {
        if !self.admitted {
            ensure!(
                *self == Self::unissued(),
                "unissued native admission has counters"
            );
        } else {
            ensure!(
                self.storage_bytes > 0
                    && self.cleanup_reserved > 0
                    && self.cleanup_steps <= self.cleanup_reserved,
                "invalid original native admission counter shape"
            );
        }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    AncestorOperation {
        reference: ryeos_state::ThreadHistoryDiscardOperationRef,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalRetainedLocalAuthority {
    SourceDirectoryOwners,
    InspectionParentOwner,
    OriginalNamespaceAndStateLifelines,
    OriginalClockAndPolicyOwner,
}

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
    pub remaining: Vec<OriginalLocalSettlementObligation>,
    pub retained_authorities: [OriginalRetainedLocalAuthority; 4],
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectionTerminalRecord {
    pub schema: u32,
    pub purpose: String,
    pub disposition: InspectionDispositionRecord,
    pub disposition_sha256: String,
    pub witness_retired: [bool; 2],
    // This local phase has no transferred process/account financial scope.
    // A pending record keeps the full original quota reservation charged.
    pub reservation_retained: bool,
}
impl InspectionDispositionRecord {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 2 && self.purpose == "offline_reset_inspection_disposition_v2",
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
        ensure!(
            self.retained_authorities
                == [
                    OriginalRetainedLocalAuthority::SourceDirectoryOwners,
                    OriginalRetainedLocalAuthority::InspectionParentOwner,
                    OriginalRetainedLocalAuthority::OriginalNamespaceAndStateLifelines,
                    OriginalRetainedLocalAuthority::OriginalClockAndPolicyOwner
                ],
            "inspection record changes original retained authority inventory"
        );
        ensure!(
            self.remaining.len() <= 16,
            "inspection disposition obligations exceed finite capacity"
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
        let record: Self = verify_inspection_record(bytes, identity)?;
        record.validate()?;
        ensure!(
            record.signer == identity.fingerprint(),
            "foreign disposition signer"
        );
        Ok(record)
    }
}
impl InspectionTerminalRecord {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 2
                && self.purpose == "offline_reset_inspection_terminal_pending_v2"
                && self.reservation_retained,
            "unsupported/releasing inspection terminal record"
        );
        self.disposition.validate()?;
        ensure!(
            self.disposition_sha256.len() == 64
                && self
                    .disposition_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid disposition receipt hash"
        );
        for (index, store) in self.disposition.stores.iter().enumerate() {
            ensure!(
                self.witness_retired[index]
                    == matches!(store, InspectionStoreDispositionRecord::Removed { .. }),
                "terminal witness disposition changes original store"
            );
        }
        Ok(())
    }
    pub(super) fn signed_bytes(&self, identity: &NodeIdentity,
        storage: &lillux::cas::CanonicalRecordStorageQuote) -> Result<Vec<u8>> {
        self.validate()?;
        sign_inspection_record(self, &self.disposition.signer, identity, storage)
    }
    pub(super) fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        let record: Self = verify_inspection_record(bytes, identity)?;
        record.validate()?;
        ensure!(
            record.disposition.signer == identity.fingerprint(),
            "foreign terminal signer"
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
fn verify_inspection_record<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
    identity: &NodeIdentity,
) -> Result<T> {
    ensure!(
        bytes.len() <= 8192,
        "inspection record exceeds original record capacity"
    );
    let raw = std::str::from_utf8(bytes)?;
    let body = lillux::signature::verified_leading_body(
        raw, "#", None, identity.verifying_key(), identity.fingerprint())?;
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

/// Exact actual original descriptor inventory, recorded before any source close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalSourceRetirementPinFact {
    pub slot: u8,
    pub device: u64,
    pub inode: u64,
    pub byte_length: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalPreparedSourceRetirementRecord {
    pub schema: u32,
    pub purpose: String,
    pub signer: String,
    pub operation_id: String,
    pub claim_sha256: String,
    pub pending_terminal_sha256: String,
    pub clock_binding: String,
    pub policy_digest: String,
    pub native_quota: OriginalNativeQuotaEvidence,
    pub source_pins: Vec<OriginalSourceRetirementPinFact>,
    pub resident_generation: u64,
    pub resident_bytes: u64,
    pub reservation_parent_identity: lillux::PinnedDirectoryIdentity,
    pub reservation_publication: OriginalSourceRetirementPinFact,
    pub issued_reservation: OriginalSourceRetirementPinFact,
    pub reservation_retained: bool,
}
impl OriginalPreparedSourceRetirementRecord {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1
                && self.purpose == "offline_reset_source_retirement_prepared_v1"
                && self.reservation_retained,
            "unsupported prepared source retirement"
        );
        self.native_quota.validate()?;
        ensure!(self.resident_generation > 0 && self.resident_bytes > 0,
            "prepared retirement lacks original resident provenance");
        ensure!(
            self.native_quota.admitted,
            "prepared retirement lacks actual native admission"
        );
        for (value, length) in [
            (&self.signer, 64),
            (&self.operation_id, 32),
            (&self.claim_sha256, 64),
            (&self.pending_terminal_sha256, 64),
            (&self.clock_binding, 64),
            (&self.policy_digest, 64),
        ] {
            ensure!(
                value.len() == length
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid prepared retirement binding"
            );
        }
        ensure!(
            !self.source_pins.is_empty() && self.source_pins.len() <= 13,
            "prepared source inventory exceeds original fixed roles"
        );
        let mut previous = None;
        for pin in &self.source_pins {
            ensure!(
                pin.slot < 13 && pin.inode > 0 && previous.is_none_or(|slot| pin.slot > slot),
                "prepared source inventory duplicate/foreign role"
            );
            previous = Some(pin.slot);
        }
        ensure!(
            self.reservation_publication.slot == 13
                && self.issued_reservation.slot == 14
                && self.reservation_publication.inode > 0
                && self.reservation_publication.device == self.issued_reservation.device
                && self.reservation_publication.inode == self.issued_reservation.inode
                && self.reservation_publication.byte_length == self.issued_reservation.byte_length,
            "prepared reservation descriptor identities differ"
        );
        Ok(())
    }
    pub(super) fn signed_bytes(&self, identity: &NodeIdentity,
        storage: &lillux::cas::CanonicalRecordStorageQuote) -> Result<Vec<u8>> {
        self.validate()?;
        sign_inspection_record(self, &self.signer, identity, storage)
    }
    pub(super) fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        let record: Self = verify_inspection_record(bytes, identity)?;
        record.validate()?;
        ensure!(
            record.signer == identity.fingerprint(),
            "foreign prepared retirement signer"
        );
        Ok(record)
    }
}


/// Durable intent for the SAME original account transition. This preserves the
/// pending record and its reservation charge. It is signed data evidence, never
/// release authority, a physical-close receipt or completed settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountSettlementIntentRecord {
    pub schema: u32,
    pub purpose: String,
    pub prepared: OriginalPreparedSourceRetirementRecord,
    pub prepared_source_retirement_sha256: String,
    /// Exact signed original claim, preserved before its reservation name is
    /// unlinked. The account owner must verify its signature and domain shape;
    /// this embedded evidence never revives the predecessor's process owners.
    pub original_claim: String,
    pub reservation_retained: bool,
}
impl OriginalAccountSettlementIntentRecord {
    fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1
            && self.purpose == "offline_reset_account_settlement_intent_v1"
            && self.reservation_retained,
            "unsupported or releasing account settlement intent");
        self.prepared.validate()?;
        ensure!(!self.original_claim.is_empty() && self.original_claim.len() <= 8192
            && lillux::sha256_hex(self.original_claim.as_bytes()) == self.prepared.claim_sha256,
            "account intent lacks exact original signed claim evidence");
        let digest = &self.prepared_source_retirement_sha256;
        ensure!(digest.len() == 64 && digest.bytes().all(|b|
            b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid prepared-source receipt digest in account intent");
        Ok(())
    }

    /// The caller prepays verification, canonicalization and record storage
    /// from the SAME original cleanup envelope before invoking this function.
    /// Exact signed predecessor bytes are required; DTOs cannot substitute for
    /// the authenticated pending/prepared chain. No signing is performed here.
    pub(super) fn from_original_records(
        pending_bytes: &[u8], prepared_bytes: &[u8], original_claim_bytes: &[u8],
        identity: &NodeIdentity,
    ) -> Result<Self> {
        ensure!(!original_claim_bytes.is_empty() && original_claim_bytes.len() <= 8192,
            "original signed claim exceeds account evidence bound");
        let original_claim = std::str::from_utf8(original_claim_bytes)
            .context("original signed claim is not UTF-8")?;
        let pending = InspectionTerminalRecord::verified(pending_bytes, identity)?;
        let prepared = OriginalPreparedSourceRetirementRecord::verified(prepared_bytes, identity)?;
        ensure!(prepared.signer == pending.disposition.signer
            && prepared.operation_id == pending.disposition.operation_id
            && prepared.claim_sha256 == pending.disposition.claim_sha256
            && prepared.policy_digest == pending.disposition.policy_digest
            && prepared.clock_binding == pending.disposition.clock_binding
            && prepared.pending_terminal_sha256 == lillux::sha256_hex(pending_bytes),
            "account intent changes the exact original pending/prepared chain");
        let record = Self {
            schema: 1,
            purpose: "offline_reset_account_settlement_intent_v1".into(),
            prepared,
            prepared_source_retirement_sha256: lillux::sha256_hex(prepared_bytes),
            original_claim: original_claim.to_owned(),
            reservation_retained: true,
        };
        record.validate()?;
        Ok(record)
    }

    pub(super) fn require_original_chain(
        &self, pending_bytes: &[u8], prepared_bytes: &[u8], identity: &NodeIdentity,
    ) -> Result<()> {
        self.validate()?;
        ensure!(*self == Self::from_original_records(pending_bytes, prepared_bytes, self.original_claim.as_bytes(), identity)?,
            "account settlement intent does not select the authenticated original chain");
        Ok(())
    }

    pub(super) fn signed_bytes(&self, identity: &NodeIdentity,
        storage: &lillux::cas::CanonicalRecordStorageQuote) -> Result<Vec<u8>> {
        self.validate()?;
        sign_inspection_record(self, &self.prepared.signer, identity, storage)
    }

    pub(super) fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        let record: Self = verify_inspection_record(bytes, identity)?;
        record.validate()?;
        ensure!(record.prepared.signer == identity.fingerprint(),
            "foreign account settlement intent signer");
        Ok(record)
    }
}


/// These are the still-owned outside responsibilities after durable account
/// settlement. The record explicitly does not claim their later disposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalAccountPhysicalTail {
    SettlementPublication,
    OperatorLockTargetAndParent,
    LedgerDirectoriesAndNamespace,
    OriginalClockPolicyAndFinalizerControls,
    OutsideCustodyInventories,
}

/// One actual still-held descriptor in the account's physical tail. Roles
/// distinguish responsibilities even when two retained descriptors name the
/// same inode. Identity equality never merges the original owners.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalAccountHeldDescriptorRole {
    PendingPublication,
    PreparedPublication,
    IntentPublication,
    OperatorTarget,
    OperatorParent,
    LedgerDirectory,
    LedgerNamespace,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountHeldDescriptorFact {
    pub role: OriginalAccountHeldDescriptorRole,
    pub device: u64,
    pub inode: u64,
}
/// The account record is signed BEFORE its own file exists. Record only the
/// exact intended coordinate here. Its actual created descriptor and eventual
/// retirement must be observed by the later original supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountPlannedPublicationFact {
    pub parent_identity: lillux::PinnedDirectoryIdentity,
    pub name: String,
    pub disposition: OriginalAccountPublicationDisposition,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalAccountPublicationDisposition { Planned }

/// Producer observations of the actual singleton controls still responsible
/// for the original operation. Copied bindings are evidence only: the producer
/// must check each cached owner before emitting its role, never infer custody
/// from these decoded facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalAccountControlRole {
    Clock, Policy, Finalizer, State, Runtime,
    OperationalInspection, OperationalMutation, InspectionController,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountControlFact {
    pub role: OriginalAccountControlRole,
    pub original_process: u32,
    pub resident_generation: u64,
}
/// Exact member position in the SAME original registry, not a globally usable
/// handle. Distinct allocations remain distinct entries even on the same inode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountPhysicalMemberFact {
    pub index: u16,
    pub source: OriginalAccountPhysicalMemberSource,
    pub disposition: OriginalAccountPhysicalMemberDisposition,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum OriginalAccountPhysicalMemberSource {
    Directory { device: u64, inode: u64 },
    Namespace { device: u64, inode: u64 },
    Operator { target_device: u64, target_inode: u64, parent_device: u64, parent_inode: u64 },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalAccountPhysicalMemberDisposition { Held, Retired }

/// Identity and disposition of one actual retired descriptor. The producer
/// obtains the identity from the genuine cached lower close receipt, then joins
/// its fixed original role/length to the signed prepared inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountDescriptorCloseFact {
    pub descriptor: OriginalSourceRetirementPinFact,
    pub disposition: OriginalAccountDescriptorDisposition,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalAccountDescriptorDisposition { Retired }

/// Emitted observation of the actual admitted region release. Never-mapped and
/// pre-initialization dispositions cannot stand in for this signed provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountRegionReleaseFact {
    pub generation: u64,
    pub bytes: u64,
}
/// Actual native arena disposition and original issuer counters, captured
/// before the budget handle/mapping becomes inaccessible. These are facts,
/// not authority to revisit native storage after its physical release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountNativeReleaseFact {
    pub disposition: OriginalAccountNativeDisposition,
    pub budget: OriginalNativeQuotaEvidence,
    pub resident_generation: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OriginalAccountNativeDisposition { Released }

/// Signed observation data only. Actual removal/sync and same-owner descriptor
/// receipts must be checked by the original producer BEFORE constructing this
/// fact. A decoded fact cannot retire a file or reconstruct a vanished owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountReservationReleaseFact {
    pub parent_identity: lillux::PinnedDirectoryIdentity,
    pub publication: OriginalSourceRetirementPinFact,
    pub issued_pin: OriginalSourceRetirementPinFact,
    pub unlinked: bool,
    pub parent_synced: bool,
    pub publication_close: OriginalAccountDescriptorCloseFact,
    pub issued_pin_close: OriginalAccountDescriptorCloseFact,
}

/// Durable account settlement with the physical tail STILL PENDING. Its own
/// publication, final operator close and final observer happen later, so they
/// cannot appear here as already-observed receipts. Overall completion requires
/// the separate actual supervisor outcome; this DTO grants no such authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OriginalAccountSettlementRecord {
    pub schema: u32,
    pub purpose: String,
    pub signer: String,
    pub operation_id: String,
    pub claim_sha256: String,
    pub policy_digest: String,
    pub clock_binding: String,
    pub intent_sha256: String,
    pub pending_terminal_sha256: String,
    pub prepared_source_retirement_sha256: String,
    pub reservation_release: OriginalAccountReservationReleaseFact,
    pub retired_source_pins: Vec<OriginalSourceRetirementPinFact>,
    pub native_release: OriginalAccountNativeReleaseFact,
    pub resident_release: OriginalAccountRegionReleaseFact,
    pub reservation_retained: bool,
    pub physical_cleanup_pending: bool,
    pub physical_tail: [OriginalAccountPhysicalTail; 5],
    pub retained_descriptors: [OriginalAccountHeldDescriptorFact; 7],
    pub planned_publication: OriginalAccountPlannedPublicationFact,
    pub original_process: u32,
    pub retained_controls: [OriginalAccountControlFact; 8],
    pub physical_members: Vec<OriginalAccountPhysicalMemberFact>,
}
impl OriginalAccountSettlementRecord {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1 && self.purpose == "offline_reset_account_settlement_v1"
            && !self.reservation_retained && self.physical_cleanup_pending,
            "unsupported account settlement or circular physical completion claim");
        ensure!(self.reservation_release.unlinked && self.reservation_release.parent_synced
            && self.reservation_release.publication_close.descriptor == self.reservation_release.publication
            && self.reservation_release.issued_pin_close.descriptor == self.reservation_release.issued_pin
            && self.resident_release.generation > 0 && self.resident_release.bytes > 0
            && self.native_release.resident_generation == self.resident_release.generation,
            "account settlement lacks required release observations");
        for (fact, role) in self.retained_descriptors.iter().zip([
            OriginalAccountHeldDescriptorRole::PendingPublication,
            OriginalAccountHeldDescriptorRole::PreparedPublication,
            OriginalAccountHeldDescriptorRole::IntentPublication,
            OriginalAccountHeldDescriptorRole::OperatorTarget,
            OriginalAccountHeldDescriptorRole::OperatorParent,
            OriginalAccountHeldDescriptorRole::LedgerDirectory,
            OriginalAccountHeldDescriptorRole::LedgerNamespace,
        ]) {
            ensure!(fact.role == role && fact.inode > 0,
                "account physical tail lacks an identified original descriptor role");
        }
        ensure!(self.planned_publication.parent_identity == self.reservation_release.parent_identity
            && self.planned_publication.name.strip_prefix("account-settled.") == Some(self.operation_id.as_str()),
            "planned account publication changes original ledger parent or coordinate");
        ensure!(self.original_process != 0, "account tail lacks original process binding");
        for (fact, role) in self.retained_controls.iter().zip([
            OriginalAccountControlRole::Clock, OriginalAccountControlRole::Policy,
            OriginalAccountControlRole::Finalizer, OriginalAccountControlRole::State,
            OriginalAccountControlRole::Runtime, OriginalAccountControlRole::OperationalInspection,
            OriginalAccountControlRole::OperationalMutation, OriginalAccountControlRole::InspectionController,
        ]) {
            ensure!(fact.role == role && fact.original_process == self.original_process
                && fact.resident_generation == self.resident_release.generation,
                "account tail control changes original role or incarnation");
        }
        // Wire bounds do not establish the producer's complete census. Its
        // actual original registry must be sealed and enumerated exhaustively.
        ensure!(!self.physical_members.is_empty() && self.physical_members.len() <= 128,
            "account physical member census exceeds finite record bound");
        let target = &self.retained_descriptors[3];
        let parent = &self.retained_descriptors[4];
        ensure!(self.physical_members.iter().any(|member| matches!(&member.source,
            OriginalAccountPhysicalMemberSource::Operator { target_device, target_inode, parent_device, parent_inode }
            if *target_device == target.device && *target_inode == target.inode
                && *parent_device == parent.device && *parent_inode == parent.inode)),
            "account member census omits its identified operator exclusion");
        for (index, member) in self.physical_members.iter().enumerate() {
            ensure!(usize::from(member.index) == index, "account physical member roles duplicate or missing");
            match &member.source {
                OriginalAccountPhysicalMemberSource::Directory { inode, .. }
                | OriginalAccountPhysicalMemberSource::Namespace { inode, .. } =>
                    ensure!(*inode > 0, "account member lacks actual descriptor identity"),
                OriginalAccountPhysicalMemberSource::Operator { target_inode, parent_inode, .. } =>
                    ensure!(*target_inode > 0 && *parent_inode > 0
                        && member.disposition == OriginalAccountPhysicalMemberDisposition::Held,
                        "account record cannot precede its retained operator exclusion"),
            }
        }
        self.native_release.budget.validate()?;
        ensure!(self.native_release.budget.admitted, "native release lacks original admitted counters");
        ensure!(self.physical_tail == [
            OriginalAccountPhysicalTail::SettlementPublication,
            OriginalAccountPhysicalTail::OperatorLockTargetAndParent,
            OriginalAccountPhysicalTail::LedgerDirectoriesAndNamespace,
            OriginalAccountPhysicalTail::OriginalClockPolicyAndFinalizerControls,
            OriginalAccountPhysicalTail::OutsideCustodyInventories,
        ], "account settlement changes retained physical tail responsibilities");
        for (value, length) in [
            (&self.signer, 64), (&self.operation_id, 32), (&self.claim_sha256, 64),
            (&self.policy_digest, 64), (&self.clock_binding, 64), (&self.intent_sha256, 64),
            (&self.pending_terminal_sha256, 64), (&self.prepared_source_retirement_sha256, 64),
        ] {
            ensure!(value.len() == length && value.bytes().all(|b|
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid account settlement binding");
        }
        ensure!(!self.retired_source_pins.is_empty() && self.retired_source_pins.len() <= 13,
            "account settlement source receipt inventory exceeds fixed original roles");
        let mut previous = None;
        for pin in &self.retired_source_pins {
            ensure!(pin.slot < 13 && pin.inode > 0 && previous.is_none_or(|slot| pin.slot > slot),
                "account settlement source receipt roles duplicate or foreign");
            previous = Some(pin.slot);
        }
        let publication = &self.reservation_release.publication;
        let issued = &self.reservation_release.issued_pin;
        ensure!(publication.slot == 13 && issued.slot == 14 && publication.inode > 0
            && publication.device == issued.device && publication.inode == issued.inode
            && publication.byte_length == issued.byte_length,
            "account settlement reservation receipt roles or original identities differ");
        Ok(())
    }

    /// Reauthenticate the exact predecessor chain. Physical observations still
    /// require the original producer's real receipts; signatures are evidence,
    /// not a replacement for in-process native or descriptor authority.
    pub(super) fn require_original_chain(&self, intent_bytes: &[u8], pending_bytes: &[u8],
        prepared_bytes: &[u8], original_quota: &ryeos_scheduler_contract::OfflineResetQuota,
        identity: &NodeIdentity) -> Result<()> {
        self.validate()?;
        // The caller obtains this quota from the authenticated original claim,
        // whose exact digest is joined below. Monotonicity against preparation
        // alone cannot bound the final allocator counters.
        self.native_release.budget.require_quota(original_quota)?;
        let intent = OriginalAccountSettlementIntentRecord::verified(intent_bytes, identity)?;
        intent.require_original_chain(pending_bytes, prepared_bytes, identity)?;
        let prepared = &intent.prepared;
        ensure!(self.signer == prepared.signer && self.operation_id == prepared.operation_id
            && self.claim_sha256 == prepared.claim_sha256 && self.policy_digest == prepared.policy_digest
            && self.clock_binding == prepared.clock_binding
            && self.intent_sha256 == lillux::sha256_hex(intent_bytes)
            && self.pending_terminal_sha256 == prepared.pending_terminal_sha256
            && self.prepared_source_retirement_sha256 == intent.prepared_source_retirement_sha256
            && self.resident_release.generation == prepared.resident_generation
            && self.resident_release.bytes == prepared.resident_bytes
            && self.native_release.budget.storage_bytes == prepared.native_quota.storage_bytes
            && self.native_release.budget.cleanup_reserved == prepared.native_quota.cleanup_reserved
            && self.native_release.budget.connections_issued == prepared.native_quota.connections_issued
            && self.native_release.budget.allocator_steps >= prepared.native_quota.allocator_steps
            && self.native_release.budget.cleanup_steps >= prepared.native_quota.cleanup_steps
            && self.retired_source_pins == prepared.source_pins
            && self.reservation_release.parent_identity == prepared.reservation_parent_identity
            && self.reservation_release.publication == prepared.reservation_publication
            && self.reservation_release.issued_pin == prepared.issued_reservation,
            "account settlement changes the authenticated original intent or receipt inventory");
        Ok(())
    }

    pub(super) fn signed_bytes(&self, identity: &NodeIdentity,
        storage: &lillux::cas::CanonicalRecordStorageQuote) -> Result<Vec<u8>> {
        self.validate()?;
        sign_inspection_record(self, &self.signer, identity, storage)
    }
    pub(super) fn verified(bytes: &[u8], identity: &NodeIdentity) -> Result<Self> {
        let record: Self = verify_inspection_record(bytes, identity)?;
        record.validate()?;
        ensure!(record.signer == identity.fingerprint(), "foreign account settlement signer");
        Ok(record)
    }
}
