//! Fixed, versioned account transition evidence. These values describe one
//! transition; only the retained account owner may publish or act on them.
//! In particular an encoded settlement tag is not physical-release evidence.

use super::{CapacityRecord, RECORD_BYTES};
use anyhow::{Context, Result, ensure};
use lillux::process_control::ExactProcessIdentity;
use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;
use sha2::Digest as _;

// The encoded identity is bounded independently of serde's heap allocations.
// Lillux remains responsible for interpreting process coordinates and death.
const PROCESS_PAYLOAD_BYTES: usize = 512;
pub(super) const PROCESS_STORAGE_BYTES: usize = 2 + PROCESS_PAYLOAD_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StoredProcess {
    length: u16,
    bytes: [u8; PROCESS_PAYLOAD_BYTES],
}

impl StoredProcess {
    pub(super) fn capture(exact: &ExactProcessIdentity) -> Result<Self> {
        exact.incarnation_digest_bytes().map_err(anyhow::Error::msg)?;
        let mut bytes = [0; PROCESS_PAYLOAD_BYTES];
        let length = {
            let mut cursor = std::io::Cursor::new(bytes.as_mut_slice());
            serde_json::to_writer(&mut cursor, exact)
                .context("exact process identity exceeds capacity record bound")?;
            u16::try_from(cursor.position())?
        };
        Ok(Self { length, bytes })
    }

    pub(super) fn exact(&self) -> Result<ExactProcessIdentity> {
        let length = usize::from(self.length);
        ensure!(length > 0 && length <= PROCESS_PAYLOAD_BYTES,
            "capacity process identity length is invalid");
        ensure!(self.bytes[length..].iter().all(|byte| *byte == 0),
            "capacity process identity padding is nonzero");
        let exact: ExactProcessIdentity = serde_json::from_slice(&self.bytes[..length])?;
        // Deny alternate encodings, unknown/duplicate fields and incomplete
        // coordinates. This uses Lillux's full identity, not numeric PID alone.
        ensure!(Self::capture(&exact)? == *self,
            "capacity process identity is not canonically encoded");
        Ok(exact)
    }

    pub(super) fn digest(&self) -> Result<[u8; 32]> {
        self.exact()?.incarnation_digest_bytes().map_err(anyhow::Error::msg)
    }

    pub(super) fn encode_into(&self, destination: &mut [u8]) {
        assert_eq!(destination.len(), PROCESS_STORAGE_BYTES);
        destination[..2].copy_from_slice(&self.length.to_le_bytes());
        destination[2..].copy_from_slice(&self.bytes);
    }

    pub(super) fn decode(source: &[u8]) -> Result<Self> {
        ensure!(source.len() == PROCESS_STORAGE_BYTES, "capacity process layout changed");
        let value = Self {
            length: u16::from_le_bytes(source[..2].try_into()?),
            bytes: source[2..].try_into()?,
        };
        value.exact()?;
        Ok(value)
    }
}

pub(super) const ANCHOR_BYTES: usize = 12 * 1024;
const ANCHOR_PAYLOAD_BYTES: usize = ANCHOR_BYTES - 32;
const MAGIC: &[u8; 8] = b"RYERED02";
const SCHEMA: u32 = 2;
const BEFORE_START: usize = 1024;
const TARGET_START: usize = BEFORE_START + RECORD_BYTES;
const RECORDS_END: usize = TARGET_START + RECORD_BYTES;
const RECOVERY_PROCESS_START: usize = 54;
const RECOVERY_PROCESS_END: usize = RECOVERY_PROCESS_START + PROCESS_STORAGE_BYTES;
const AUTHORIZATION_START: usize = RECOVERY_PROCESS_END;
const AUTHORIZATION_END: usize = AUTHORIZATION_START + 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    Genesis,
    Debit,
    ReleaseSettlement,
    DeadIncarnationSettlement,
    /// Settles the served incarnation of an orderly rollover and opens the next epoch.
    RolloverRenewal,
    /// Settles any dead incarnation and opens the next epoch under an operator document.
    OperatorReissue,
}

impl Operation {
    /// The two operations that open an epoch; every other one keeps it.
    fn renews(self) -> bool {
        matches!(self, Operation::RolloverRenewal | Operation::OperatorReissue)
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Operation::Genesis => "genesis",
            Operation::Debit => "debit",
            Operation::ReleaseSettlement => "release_settlement",
            Operation::DeadIncarnationSettlement => "dead_incarnation_settlement",
            Operation::RolloverRenewal => "rollover_renewal",
            Operation::OperatorReissue => "operator_reissue",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AccountRedo {
    operation: Operation,
    before: CapacityRecord,
    target: CapacityRecord,
    recovery_attempts: u64,
    recovery_process: Option<StoredProcess>,
    /// Zero for every operation but the two renewals, which carry the digest
    /// of what authorised them: the rollover claim or the operator document.
    authorization: [u8; 32],
}

impl AccountRedo {
    pub(super) fn genesis(record: CapacityRecord, policy: &NodeNativeCapacityPolicy) -> Result<Self> {
        let redo = Self {
            operation: Operation::Genesis, before: record, target: record,
            recovery_attempts: 0, recovery_process: None, authorization: [0; 32],
        };
        redo.validate(policy)?;
        Ok(redo)
    }

    /// Prepare the successor only after the real account reached this target.
    /// This copies recovery consumption unchanged; no operation refunds it.
    /// Opening an epoch is never a successor transition: `renew` is the only
    /// constructor that resets the recovery counter.
    pub(super) fn successor(&self, operation: Operation, target: CapacityRecord,
        policy: &NodeNativeCapacityPolicy) -> Result<Self> {
        self.validate(policy)?;
        ensure!(operation != Operation::Genesis, "genesis cannot replace an existing transition");
        ensure!(!operation.renews(), "epoch renewal is not a successor transition");
        // A successor of a renewal keeps its epoch and recovery charge but not
        // the authorization: that digest names the renewal record alone.
        let next = Self { operation, before: self.target, target, authorization: [0; 32], ..*self };
        next.validate(policy)?;
        Ok(next)
    }

    /// Open the next epoch from the real current anchor. `target` must be the
    /// genesis image of `before.epoch + 1` with the same sequence and issuance;
    /// `authorization` names what authorised it and is never zero. The new
    /// epoch starts with zero recovery attempts: a repair charged after this
    /// record is published counts against the epoch it opened.
    pub(super) fn renew(&self, operation: Operation, target: CapacityRecord,
        authorization: [u8; 32], policy: &NodeNativeCapacityPolicy) -> Result<Self> {
        self.validate(policy)?;
        ensure!(operation.renews(), "capacity renewal requires a renewal operation");
        let next = Self {
            operation, before: self.target, target,
            recovery_attempts: 0, recovery_process: None, authorization,
        };
        next.validate(policy)?;
        Ok(next)
    }

    /// Shape-only preparation. The private publisher must first establish the
    /// previous recovery process's disposition and retain original bootstrap
    /// custody. Only durable publication can issue the finite attempt owner.
    pub(super) fn charge_recovery(&self, process: &ExactProcessIdentity,
        policy: &NodeNativeCapacityPolicy) -> Result<Self> {
        self.validate(policy)?;
        let next = Self {
            recovery_attempts: self.recovery_attempts.checked_add(1)
                .context("capacity recovery attempt counter exhausted")?,
            recovery_process: Some(StoredProcess::capture(process)?),
            ..*self
        };
        next.validate(policy)?;
        Ok(next)
    }

    pub(super) fn target(&self) -> &CapacityRecord { &self.target }
    pub(super) fn operation(&self) -> Operation { self.operation }
    pub(super) fn authorization(&self) -> [u8; 32] { self.authorization }
    pub(super) fn epoch(&self) -> u64 { self.target.epoch }
    /// Process digest of the incarnation this transition settled, when it is
    /// a settlement; recovery charges keep the settlement they follow. A
    /// renewal settles the incarnation its before image still carried.
    pub(super) fn settled_incarnation(&self) -> Option<[u8; 32]> {
        match self.operation {
            Operation::ReleaseSettlement | Operation::DeadIncarnationSettlement
            | Operation::RolloverRenewal | Operation::OperatorReissue => {
                self.before.outstanding.map(|debit| debit.process_digest)
            }
            Operation::Genesis | Operation::Debit => None,
        }
    }
    pub(super) fn recovery_process(&self) -> Option<&StoredProcess> { self.recovery_process.as_ref() }
    pub(super) fn recovery_attempts(&self) -> u64 { self.recovery_attempts }

    fn validate(&self, policy: &NodeNativeCapacityPolicy) -> Result<()> {
        policy.validate()?;
        self.before.validate()?;
        self.target.validate()?;
        ensure!(self.before.issuance_digest == self.target.issuance_digest,
            "capacity redo crosses issuance");
        ensure!(policy.allowance.covers(&self.before.consumed)
            && policy.allowance.covers(&self.target.consumed), "capacity redo exceeds allowance");
        ensure!(self.recovery_attempts <= policy.recovery.maximum_attempts,
            "capacity recovery allowance exhausted");
        ensure!((self.recovery_attempts == 0) == self.recovery_process.is_none(),
            "capacity recovery counter has no matching process");
        if let Some(process) = &self.recovery_process { process.exact()?; }
        if !self.operation.renews() {
            ensure!(self.before.epoch == self.target.epoch,
                "capacity redo changes epoch outside renewal");
            ensure!(self.authorization == [0; 32],
                "non-renewal capacity redo carries authorization");
        }
        match self.operation {
            Operation::Genesis => {
                ensure!(self.before == self.target && self.target.sequence == 0
                    && self.target.epoch == 1
                    && self.target.outstanding.is_none(), "invalid capacity genesis transition");
                ensure!(self.target.consumed == super::genesis_consumed(),
                    "capacity genesis omits or changes control storage");
            }
            Operation::Debit => {
                let debit = self.target.outstanding.context("capacity debit has no incarnation")?;
                ensure!(debit.units == policy.incarnation.units, "capacity debit changes signed envelope");
                let (sequence, consumed) = self.before.prepare_debit(policy.allowance, debit.units)?;
                let expected = CapacityRecord { sequence, consumed, outstanding: Some(debit), ..self.before };
                ensure!(self.target == expected, "capacity redo is not an exact debit transition");
            }
            Operation::ReleaseSettlement | Operation::DeadIncarnationSettlement => {
                ensure!(self.before.outstanding.is_some(), "capacity settlement lacks original debt");
                let expected = CapacityRecord { outstanding: None, ..self.before };
                ensure!(self.target == expected, "capacity settlement changes sequence or consumption");
                if self.operation == Operation::DeadIncarnationSettlement {
                    ensure!(self.recovery_process.is_some(), "dead-incarnation settlement lacks recovery attempt");
                }
            }
            Operation::RolloverRenewal | Operation::OperatorReissue => {
                // The before image may still carry the incarnation this record
                // settles; the owner proved its disposition before publishing.
                ensure!(self.before.epoch.checked_add(1) == Some(self.target.epoch),
                    "capacity renewal is not the next epoch");
                if let Some(maximum) = policy.renewal.maximum_epochs {
                    ensure!(self.target.epoch <= maximum, "capacity epoch bound reached");
                }
                ensure!(self.target.consumed == super::genesis_consumed(),
                    "capacity renewal does not open at genesis consumption");
                ensure!(self.target.outstanding.is_none(),
                    "capacity renewal opens with an outstanding incarnation");
                ensure!(self.target.sequence == self.before.sequence,
                    "capacity renewal changes sequence");
                ensure!(self.authorization != [0; 32], "capacity renewal lacks authorization");
                match self.operation {
                    Operation::RolloverRenewal => {
                        ensure!(policy.renewal.at_orderly_rollover,
                            "signed policy does not pre-authorise rollover renewal");
                        // A rollover settles the window it served, and its
                        // authorization is derived from that before image, so
                        // the anchor verifies itself rather than any digest.
                        ensure!(self.before.outstanding.is_some(),
                            "rollover renewal settles no served incarnation");
                        ensure!(self.authorization
                            == super::rollover_authorization(&self.target.issuance_digest, &self.before)?,
                            "rollover renewal authorization does not name its served incarnation");
                    }
                    _ => ensure!(policy.renewal.operator_reissue,
                        "signed policy does not pre-authorise operator re-issuance"),
                }
            }
        }
        Ok(())
    }

    /// Called only with this exact durably published redo. An arbitrary
    /// checksum-invalid account does not qualify; only an actual target prefix
    /// followed by the retained predecessor suffix is within the write model.
    pub(super) fn accepts_account_bytes(&self, bytes: &[u8; RECORD_BYTES]) -> Result<bool> {
        Ok(super::settlement_transition_bytes(bytes, &super::encode(&self.before)?,
            &super::encode(&self.target)?, true))
    }

    pub(super) fn encode(&self, policy: &NodeNativeCapacityPolicy) -> Result<[u8; ANCHOR_BYTES]> {
        self.validate(policy)?;
        let mut bytes = [0; ANCHOR_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&SCHEMA.to_le_bytes());
        bytes[12] = match self.operation {
            Operation::Genesis => 0, Operation::Debit => 1,
            Operation::ReleaseSettlement => 2, Operation::DeadIncarnationSettlement => 3,
            Operation::RolloverRenewal => 4, Operation::OperatorReissue => 5,
        };
        bytes[13..45].copy_from_slice(&self.target.issuance_digest);
        bytes[45..53].copy_from_slice(&self.recovery_attempts.to_le_bytes());
        if let Some(process) = &self.recovery_process {
            bytes[53] = 1;
            process.encode_into(&mut bytes[RECOVERY_PROCESS_START..RECOVERY_PROCESS_END]);
        }
        bytes[AUTHORIZATION_START..AUTHORIZATION_END].copy_from_slice(&self.authorization);
        bytes[BEFORE_START..TARGET_START].copy_from_slice(&super::encode(&self.before)?);
        bytes[TARGET_START..RECORDS_END].copy_from_slice(&super::encode(&self.target)?);
        let checksum: [u8; 32] = sha2::Sha256::digest(&bytes[..ANCHOR_PAYLOAD_BYTES]).into();
        bytes[ANCHOR_PAYLOAD_BYTES..].copy_from_slice(&checksum);
        Ok(bytes)
    }

    pub(super) fn decode(bytes: &[u8; ANCHOR_BYTES], policy: &NodeNativeCapacityPolicy,
        issuance: &[u8; 32]) -> Result<Self> {
        ensure!(&bytes[..8] == MAGIC && bytes[8..12] == SCHEMA.to_le_bytes(),
            "unsupported capacity redo format");
        let checksum: [u8; 32] = sha2::Sha256::digest(&bytes[..ANCHOR_PAYLOAD_BYTES]).into();
        ensure!(bytes[ANCHOR_PAYLOAD_BYTES..] == checksum, "capacity redo checksum mismatch");
        ensure!(&bytes[13..45] == issuance, "capacity redo belongs to another issuance");
        ensure!(bytes[AUTHORIZATION_END..BEFORE_START].iter().all(|byte| *byte == 0)
            && bytes[RECORDS_END..ANCHOR_PAYLOAD_BYTES].iter().all(|byte| *byte == 0),
            "capacity redo padding is nonzero");
        let operation = match bytes[12] {
            0 => Operation::Genesis, 1 => Operation::Debit,
            2 => Operation::ReleaseSettlement, 3 => Operation::DeadIncarnationSettlement,
            4 => Operation::RolloverRenewal, 5 => Operation::OperatorReissue,
            _ => anyhow::bail!("unknown capacity redo operation"),
        };
        let recovery_process = match bytes[53] {
            0 => {
                ensure!(bytes[RECOVERY_PROCESS_START..RECOVERY_PROCESS_END].iter().all(|byte| *byte == 0),
                    "absent capacity recovery process has data");
                None
            }
            1 => Some(StoredProcess::decode(&bytes[RECOVERY_PROCESS_START..RECOVERY_PROCESS_END])?),
            _ => anyhow::bail!("invalid capacity recovery process tag"),
        };
        let value = Self {
            operation,
            before: super::decode(bytes[BEFORE_START..TARGET_START].try_into()?)?,
            target: super::decode(bytes[TARGET_START..RECORDS_END].try_into()?)?,
            recovery_attempts: u64::from_le_bytes(bytes[45..53].try_into()?),
            recovery_process,
            authorization: bytes[AUTHORIZATION_START..AUTHORIZATION_END].try_into()?,
        };
        ensure!(&value.target.issuance_digest == issuance, "capacity redo images changed issuance");
        value.validate(policy)?;
        Ok(value)
    }
}

#[cfg(test)]
pub(super) mod controls {
    use super::*;
    use super::super::{IncarnationDebit, NativeCapacityUnits, RECORD_SCHEMA, genesis_consumed};
    use ryeos_node_runtime::node_policy::sections::native_capacity::{
        NativeIncarnationEnvelope, NativeRecoveryEnvelope, NativeRenewalEnvelope,
    };

    pub(in crate::native_capacity::account) fn process(pid: u32) -> ExactProcessIdentity {
        ExactProcessIdentity {
            boot_id: "2b0cd835-32e1-4a9a-8a48-bf8133127a61".into(),
            target_pid: pid, target_start_time_ticks: 71,
            group_leader_pid: pid, group_leader_start_time_ticks: 71,
        }
    }

    pub(in crate::native_capacity::account) fn fixture() -> (NodeNativeCapacityPolicy, AccountRedo, CapacityRecord) {
        let units = NativeCapacityUnits {
            storage_bytes: 1_048_576, host_control_storage_bytes: 262_144,
            allocator_steps: 100_000, connection_tickets: 2, cleanup_steps: 10_000,
        };
        let policy = NodeNativeCapacityPolicy {
            schema: 6, issuance_nonce: "a".repeat(32),
            allowance: NativeCapacityUnits { storage_bytes: 4_194_304,
                host_control_storage_bytes: 1_048_576, allocator_steps: 400_000,
                connection_tickets: 8, cleanup_steps: 40_000 },
            incarnation: NativeIncarnationEnvelope {
                units, maximum_occupancy_ms: 60_000, cleanup_occupancy_ms: 10_000,
                rollover_drain_margin_ms: 5_000,
                host_release_steps: 64, outside_cleanup_steps: 1024,
            },
            recovery: NativeRecoveryEnvelope {
                maximum_attempts: 2, work_per_attempt: 65_536,
                control_storage_bytes: 262_144, maximum_duration_ms: 10_000,
            },
            renewal: NativeRenewalEnvelope {
                at_orderly_rollover: true, operator_reissue: true, maximum_epochs: None,
            },
        };
        let genesis = CapacityRecord {
            schema: RECORD_SCHEMA, epoch: 1, issuance_digest: [0x31; 32], sequence: 0,
            consumed: genesis_consumed(), outstanding: None,
        };
        let redo = AccountRedo::genesis(genesis, &policy).unwrap();
        let (sequence, consumed) = genesis.prepare_debit(policy.allowance, units).unwrap();
        let exact = process(42);
        let target = CapacityRecord { sequence, consumed, outstanding: Some(IncarnationDebit {
            process_digest: exact.incarnation_digest_bytes().unwrap(), process: StoredProcess::capture(&exact).unwrap(),
            clock_digest: [2; 32], completion_digest: [3; 32], policy_generation_digest: [4; 32], units,
        }), ..genesis };
        (policy, redo, target)
    }

    /// The genesis image of the next epoch, as both renewal paths build it.
    pub(in crate::native_capacity::account) fn next_epoch(before: &CapacityRecord) -> CapacityRecord {
        CapacityRecord { epoch: before.epoch + 1, consumed: genesis_consumed(), outstanding: None, ..*before }
    }

    /// The authorization a rollover renewal of `before` must carry.
    fn rollover_auth(before: &CapacityRecord) -> [u8; 32] {
        super::super::rollover_authorization(&before.issuance_digest, before).unwrap()
    }

    fn assert_every_prefix_accepted(redo: &AccountRedo, policy: &NodeNativeCapacityPolicy) {
        let wire = redo.encode(policy).unwrap();
        let recovered = AccountRedo::decode(&wire, policy, &redo.target.issuance_digest).unwrap();
        assert_eq!(&recovered, redo);
        let before = super::super::encode(&redo.before).unwrap();
        let after = super::super::encode(&redo.target).unwrap();
        for cut in 0..=RECORD_BYTES {
            let mut torn = before;
            torn[..cut].copy_from_slice(&after[..cut]);
            assert!(recovered.accepts_account_bytes(&torn).unwrap(), "prefix {cut}");
        }
        let mut foreign = before;
        foreign[RECORD_BYTES / 2] ^= 0x99;
        assert!(!recovered.accepts_account_bytes(&foreign).unwrap());
    }

    #[test]
    fn exact_identity_is_bounded_canonical_and_preserves_birth_coordinates() {
        let exact = process(42);
        let stored = StoredProcess::capture(&exact).unwrap();
        assert_eq!(stored.exact().unwrap(), exact);
        assert_eq!(stored.digest().unwrap(), exact.incarnation_digest_bytes().unwrap());
        let mut bytes = [0; PROCESS_STORAGE_BYTES];
        stored.encode_into(&mut bytes);
        bytes[PROCESS_STORAGE_BYTES - 1] = 1;
        assert!(StoredProcess::decode(&bytes).is_err());
        let mut overlong = exact.clone();
        overlong.boot_id = "b".repeat(PROCESS_PAYLOAD_BYTES);
        assert!(StoredProcess::capture(&overlong).is_err());
        let mut reused = exact;
        reused.target_start_time_ticks += 1;
        assert_ne!(stored.digest().unwrap(), StoredProcess::capture(&reused).unwrap().digest().unwrap());
    }

    #[test]
    fn journaled_debit_accepts_every_write_prefix_and_refuses_foreign_bytes() {
        let (policy, genesis, target) = fixture();
        let redo = genesis.successor(Operation::Debit, target, &policy).unwrap();
        assert_every_prefix_accepted(&redo, &policy);
        let mut substituted = target;
        substituted.outstanding.as_mut().unwrap().process = StoredProcess::capture(&process(43)).unwrap();
        assert!(super::super::encode(&substituted).is_err());
    }

    #[test]
    fn journaled_renewals_accept_every_write_prefix_and_refuse_foreign_bytes() {
        let (policy, genesis, target) = fixture();
        let debit = genesis.successor(Operation::Debit, target, &policy).unwrap();
        // Path A settles the served incarnation and opens the epoch in one record.
        let rollover = debit.renew(Operation::RolloverRenewal, next_epoch(&target), rollover_auth(&target), &policy).unwrap();
        assert_every_prefix_accepted(&rollover, &policy);
        assert_eq!(rollover.epoch(), 2);
        assert_eq!(rollover.operation(), Operation::RolloverRenewal);
        assert_eq!(rollover.authorization(), rollover_auth(&target));
        // Path B opens the epoch from an already settled image.
        let settled = CapacityRecord { outstanding: None, ..target };
        let release = debit.successor(Operation::ReleaseSettlement, settled, &policy).unwrap();
        let reissue = release.renew(Operation::OperatorReissue, next_epoch(&settled), [0xc3; 32], &policy).unwrap();
        assert_every_prefix_accepted(&reissue, &policy);
        assert_eq!(reissue.epoch(), 2);
        assert_eq!(reissue.settled_incarnation(), None);
    }

    #[test]
    fn settlement_preserves_debit_sequence_consumption_and_recovery_spend() {
        let (policy, genesis, target) = fixture();
        let debit = genesis.successor(Operation::Debit, target, &policy).unwrap();
        let settled = CapacityRecord { outstanding: None, ..target };
        assert!(debit.successor(Operation::DeadIncarnationSettlement, settled, &policy).is_err());
        let recovery = debit.charge_recovery(&process(44), &policy).unwrap();
        let next = recovery.successor(Operation::DeadIncarnationSettlement, settled, &policy).unwrap();
        assert_eq!(next.recovery_attempts(), 1);
        assert_eq!(next.target().sequence, target.sequence);
        assert_eq!(next.target().consumed, target.consumed);
        assert_eq!(next.epoch(), 1);
        let wire = next.encode(&policy).unwrap();
        assert_eq!(AccountRedo::decode(&wire, &policy, &target.issuance_digest).unwrap(), next);
        let mut refunded = settled;
        refunded.consumed.storage_bytes -= 1;
        assert!(recovery.successor(Operation::ReleaseSettlement, refunded, &policy).is_err());
        let mut advanced = settled;
        advanced.sequence += 1;
        assert!(recovery.successor(Operation::ReleaseSettlement, advanced, &policy).is_err());
        let exhausted = next.charge_recovery(&process(45), &policy).unwrap();
        assert!(exhausted.charge_recovery(&process(46), &policy).is_err());
    }

    #[test]
    fn successor_cannot_renew_and_renew_cannot_settle_or_debit() {
        let (policy, genesis, target) = fixture();
        let debit = genesis.successor(Operation::Debit, target, &policy).unwrap();
        let next = next_epoch(&target);
        assert!(debit.successor(Operation::RolloverRenewal, next, &policy).is_err());
        assert!(debit.successor(Operation::OperatorReissue, next, &policy).is_err());
        let settled = CapacityRecord { outstanding: None, ..target };
        assert!(debit.renew(Operation::ReleaseSettlement, settled, [1; 32], &policy).is_err());
        assert!(debit.renew(Operation::DeadIncarnationSettlement, settled, [1; 32], &policy).is_err());
        assert!(debit.renew(Operation::Debit, target, [1; 32], &policy).is_err());
        assert!(debit.renew(Operation::Genesis, next, [1; 32], &policy).is_err());
        // Only `renew` resets the recovery counter, and only into the next epoch.
        let charged = debit.charge_recovery(&process(44), &policy).unwrap();
        assert_eq!(charged.recovery_attempts(), 1);
        let renewed = charged.renew(Operation::RolloverRenewal, next, rollover_auth(&target), &policy).unwrap();
        assert_eq!(renewed.recovery_attempts(), 0);
        assert!(renewed.recovery_process().is_none());
        assert_eq!(renewed.settled_incarnation(), Some(target.outstanding.unwrap().process_digest));
        // A repair charged after publication counts against the new epoch.
        let repaired = renewed.charge_recovery(&process(45), &policy).unwrap();
        assert_eq!(repaired.recovery_attempts(), 1);
        assert_eq!(repaired.epoch(), 2);
        assert_eq!(repaired.operation(), Operation::RolloverRenewal);
    }

    #[test]
    fn renewal_refuses_every_malformed_transition() {
        let (policy, genesis, target) = fixture();
        let debit = genesis.successor(Operation::Debit, target, &policy).unwrap();
        let next = next_epoch(&target);
        let auth = rollover_auth(&target);
        assert!(debit.renew(Operation::RolloverRenewal, next, auth, &policy).is_ok());
        // A non-zero digest that is not derived from the served incarnation.
        assert!(debit.renew(Operation::RolloverRenewal, next, [1; 32], &policy).is_err());
        let mut other = auth;
        other[0] ^= 1;
        assert!(debit.renew(Operation::RolloverRenewal, next, other, &policy).is_err());
        // A rollover from an image that settles nothing; operator re-issuance may.
        let settled = CapacityRecord { outstanding: None, ..target };
        let release = debit.successor(Operation::ReleaseSettlement, settled, &policy).unwrap();
        assert!(release.renew(Operation::RolloverRenewal, next_epoch(&settled), [1; 32], &policy).is_err());
        assert!(release.renew(Operation::OperatorReissue, next_epoch(&settled), [1; 32], &policy).is_ok());

        // A debit or settlement that changes the epoch.
        let (sequence, consumed) = next.prepare_debit(policy.allowance, policy.incarnation.units).unwrap();
        let cross_debit = CapacityRecord { epoch: 2, sequence, consumed, outstanding: target.outstanding, ..target };
        assert!(genesis.successor(Operation::Debit, cross_debit, &policy).is_err());
        let cross_settlement = CapacityRecord { epoch: 2, outstanding: None, ..target };
        assert!(debit.successor(Operation::ReleaseSettlement, cross_settlement, &policy).is_err());
        // A non-renewal record carrying an authorization digest.
        let authorized_debit = AccountRedo { authorization: [1; 32], ..debit };
        assert!(authorized_debit.validate(&policy).is_err());

        // Epoch +2 and epoch overflow.
        assert!(debit.renew(Operation::RolloverRenewal, CapacityRecord { epoch: 3, ..next }, [1; 32], &policy).is_err());
        assert!(debit.renew(Operation::RolloverRenewal, CapacityRecord { epoch: 1, ..next }, [1; 32], &policy).is_err());
        let last = CapacityRecord { epoch: u64::MAX, ..target };
        let last_debit = AccountRedo { before: CapacityRecord { epoch: u64::MAX, ..genesis.target }, target: last, ..debit };
        assert!(last_debit.validate(&policy).is_ok());
        for epoch in [u64::MAX, 0, 1] {
            let wrapped = CapacityRecord { epoch, ..next };
            assert!(AccountRedo { operation: Operation::RolloverRenewal, before: last, target: wrapped,
                recovery_attempts: 0, recovery_process: None, authorization: [1; 32] }.validate(&policy).is_err());
        }

        // Zero authorization.
        assert!(debit.renew(Operation::RolloverRenewal, next, [0; 32], &policy).is_err());
        // Consumption that is not the genesis image.
        let mut carried = next;
        carried.consumed = target.consumed;
        assert!(debit.renew(Operation::RolloverRenewal, carried, [1; 32], &policy).is_err());
        let mut short = next;
        short.consumed.storage_bytes -= 1;
        assert!(debit.renew(Operation::RolloverRenewal, short, [1; 32], &policy).is_err());
        // Sequence changed.
        assert!(debit.renew(Operation::RolloverRenewal, CapacityRecord { sequence: next.sequence + 1, ..next }, [1; 32], &policy).is_err());
        assert!(debit.renew(Operation::RolloverRenewal, CapacityRecord { sequence: 0, ..next }, [1; 32], &policy).is_err());
        // Outstanding incarnation in the target.
        let occupied = CapacityRecord { outstanding: target.outstanding, consumed: target.consumed, ..next };
        assert!(debit.renew(Operation::RolloverRenewal, occupied, [1; 32], &policy).is_err());
        // Issuance changed.
        assert!(debit.renew(Operation::RolloverRenewal, CapacityRecord { issuance_digest: [9; 32], ..next }, [1; 32], &policy).is_err());

        // Path bit disabled by the signed policy.
        let mut rollover_off = policy.clone();
        rollover_off.renewal.at_orderly_rollover = false;
        assert!(debit.renew(Operation::RolloverRenewal, next, auth, &rollover_off).is_err());
        assert!(debit.renew(Operation::OperatorReissue, next, [1; 32], &rollover_off).is_ok());
        let mut reissue_off = policy.clone();
        reissue_off.renewal.operator_reissue = false;
        assert!(debit.renew(Operation::OperatorReissue, next, [1; 32], &reissue_off).is_err());
        assert!(debit.renew(Operation::RolloverRenewal, next, auth, &reissue_off).is_ok());

        // Epoch bound reached; the bound is data and `None` is unbounded.
        let mut bounded = policy.clone();
        bounded.renewal.maximum_epochs = Some(1);
        assert!(debit.renew(Operation::RolloverRenewal, next, auth, &bounded).is_err());
        bounded.renewal.maximum_epochs = Some(2);
        let second = debit.renew(Operation::RolloverRenewal, next, auth, &bounded).unwrap();
        let (sequence, consumed) = next.prepare_debit(bounded.allowance, bounded.incarnation.units).unwrap();
        let second_debit = second.successor(Operation::Debit,
            CapacityRecord { sequence, consumed, outstanding: target.outstanding, ..next }, &bounded).unwrap();
        assert!(second_debit.renew(Operation::RolloverRenewal, next_epoch(second_debit.target()), rollover_auth(second_debit.target()), &bounded).is_err());
        assert!(second_debit.renew(Operation::RolloverRenewal, next_epoch(second_debit.target()), rollover_auth(second_debit.target()), &policy).is_ok());
        // A decoded record is held to the same bound as a constructed one.
        let wire = second.encode(&policy).unwrap();
        bounded.renewal.maximum_epochs = Some(1);
        assert!(AccountRedo::decode(&wire, &bounded, &target.issuance_digest).is_err());
        assert!(AccountRedo::decode(&wire, &policy, &target.issuance_digest).is_ok());
    }

    #[test]
    fn valid_checksum_does_not_admit_unknown_or_inconsistent_transitions() {
        let (policy, genesis, target) = fixture();
        let wire = genesis.successor(Operation::Debit, target, &policy).unwrap().encode(&policy).unwrap();
        for (offset, replacement) in [(12, 9), (12, 4), (12, 5), (53, 2), (AUTHORIZATION_START, 1),
            (AUTHORIZATION_END, 1), (RECORDS_END, 1)] {
            let mut corrupted = wire;
            corrupted[offset] = replacement;
            let digest: [u8; 32] = sha2::Sha256::digest(&corrupted[..ANCHOR_PAYLOAD_BYTES]).into();
            corrupted[ANCHOR_PAYLOAD_BYTES..].copy_from_slice(&digest);
            assert!(AccountRedo::decode(&corrupted, &policy, &target.issuance_digest).is_err(),
                "offset {offset}");
        }
        assert!(AccountRedo::decode(&wire, &policy, &[0; 32]).is_err());
        let mut foreign = wire;
        foreign[0] ^= 1;
        assert!(AccountRedo::decode(&foreign, &policy, &target.issuance_digest).is_err());
        // The previous redo format is foreign even with a valid checksum.
        let mut previous = wire;
        previous[..8].copy_from_slice(b"RYERED01");
        previous[8..12].copy_from_slice(&1u32.to_le_bytes());
        let digest: [u8; 32] = sha2::Sha256::digest(&previous[..ANCHOR_PAYLOAD_BYTES]).into();
        previous[ANCHOR_PAYLOAD_BYTES..].copy_from_slice(&digest);
        assert!(AccountRedo::decode(&previous, &policy, &target.issuance_digest).is_err());
    }
}
