//! Stopped-node epoch re-issuance. The operator signs a document naming the
//! exact anchor it authorises; the account owner publishes one redo and one
//! account write under the node's state lock, exactly as a live settlement.
//!
//! The command never clears a restart inhibition and never constructs the
//! live account owner: there is no incarnation here, only the record owners
//! opened directly under the operator's exclusion.

use std::cell::Cell;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use lillux::PinnedDirectory;
use lillux::time::MonotonicDeadline;
use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;
use ryeos_node_runtime::state_lock::StateLock;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::OperatorLineage;
use super::anchor::RetainedAnchor;
use super::redo::{AccountRedo, Operation};
use super::{
    CapacityRecord, NativeCapacityAccountBinding, RECORD_BYTES, REISSUANCE_DOCUMENT_BOUND,
    REISSUANCE_DOCUMENT_NAME, REISSUANCE_SCHEMA, ReissuanceBody, ReissuanceDocument, RetainedStore,
    encode, genesis_consumed, hex_digest, verify_reissuance_document,
};
use crate::apply_authority::DocumentSigner;

/// Evidence of every epoch an operator opened on this node, oldest first.
pub const REISSUE_EVIDENCE_LOG: &str = "native-capacity-reissuances.jsonl";
const REISSUE_EVIDENCE_LOG_BOUND: usize = 256 * 1024;
const REASON_BOUND_CHARS: usize = 1024;

/// One operator re-issuance on a stopped node.
pub struct ReissueRequest<'a> {
    pub app_root: &'a Path,
    /// Held by the command for the whole operation: a running daemon holds
    /// the same lock, so acquiring it is the proof that the node is stopped.
    pub state_lock: &'a StateLock,
    /// What signs the re-issuance document and vouches for the operator: the
    /// operator's own key in the operator's process, or the node key under
    /// the operator-signed request being applied. The operator must be the
    /// one that initialized this node, or a registered successor.
    pub signer: DocumentSigner<'a>,
    pub reason: &'a str,
    /// The command name recorded in the evidence log.
    pub command: &'a str,
}

/// What the command changed, or found already changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReissueReport {
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub document_digest: String,
    pub settled_incarnation: Option<String>,
    /// The anchor already carried this document's digest: nothing new was
    /// written beyond repairing a torn account or a missing evidence line.
    pub already_applied: bool,
    /// The redo anchor was torn and could not be read, so the epoch opened
    /// from the account record instead of from the anchor.
    pub from_account_record: bool,
}

/// One line of the bounded evidence log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityReissueEvidence {
    pub document_digest: String,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub settled_incarnation: Option<String>,
    pub recorded_at: String,
    pub actor: lillux::process_control::ControllerAccount,
    pub command: String,
}

/// Open the next epoch of a stopped node's capacity account under the
/// operator key. Rerunning after any interruption converges on the same
/// single epoch: a published anchor is recognised by its document digest and
/// only repaired, never renewed again.
pub fn reissue_capacity(request: ReissueRequest<'_>) -> Result<ReissueReport> {
    let ReissueRequest {
        app_root,
        state_lock,
        signer,
        reason,
        command,
    } = request;
    state_lock.ensure_protects_app_root(app_root)?;
    let report = crate::init::verify_init_completion(app_root)?
        .context("re-issuance requires completed initialization")?;
    let operator_fingerprint = signer.operator_fingerprint().context(
        "operator re-issuance needs the operator's consent: sign it with the operator key or \
         apply it under the operator's signed request",
    )?;
    ensure!(
        operator_fingerprint == report.operator_fingerprint,
        "operator key differs from the one that initialized this node"
    );
    let lineage = super::OperatorLineage::load(app_root)?;
    lineage.require_member(&operator_fingerprint)?;
    let binding: NativeCapacityAccountBinding = report.capacity_account;
    let policy = binding.policy.clone();
    ensure!(
        policy.renewal.operator_reissue,
        "signed policy does not pre-authorise operator re-issuance"
    );
    binding.require_installed(app_root, &report.node_fingerprint, &policy)?;
    let directory = PinnedDirectory::open(&app_root.join(".ai/node/native-capacity"))?
        .context("capacity directory absent")?;
    ensure!(
        directory.identity()? == binding.account_directory_identity,
        "capacity directory differs from original issuance"
    );
    let onboarding = PinnedDirectory::open(&app_root.join(".ai/config/onboarding"))?
        .context("operator onboarding absent")?;
    let state =
        PinnedDirectory::open(&app_root.join(".ai/state"))?.context("capacity runtime absent")?;
    let identity = ReissueIdentity {
        signer,
        operator_fingerprint,
        lineage,
        node_fingerprint: report.node_fingerprint,
        policy,
    };
    let mut owner = StoppedCapacityAccount::open(
        directory,
        binding.account_identity,
        onboarding,
        state,
        binding.issuance_digest()?,
        &identity.policy,
        state_lock.witness()?,
    )?;
    state_lock.ensure_protects_app_root(app_root)?;
    owner.reissue(
        &identity,
        lillux::process_control::ControllerAccount::current(),
        reason,
        command,
    )
}

/// Every recorded re-issuance, oldest first. An unreadable line is an error,
/// never silently skipped: evidence that cannot be read is not evidence.
pub fn read_capacity_reissuances(state_dir: &Path) -> Result<Vec<CapacityReissueEvidence>> {
    let Some(directory) = PinnedDirectory::open(state_dir)? else {
        return Ok(Vec::new());
    };
    read_evidence(&directory)
}

/// The authenticated operator and node this command acts for.
struct ReissueIdentity<'a> {
    signer: DocumentSigner<'a>,
    operator_fingerprint: String,
    /// The operators whose earlier documents the re-issuance may follow.
    lineage: OperatorLineage,
    node_fingerprint: String,
    policy: NodeNativeCapacityPolicy,
}

/// What an epoch is renewed from: the readable anchor with the account it
/// leads, or, when the anchor is torn, the verified account record alone.
enum Predecessor {
    Anchor {
        current: AccountRedo,
        bytes: [u8; RECORD_BYTES],
    },
    Account {
        record: CapacityRecord,
        bytes: [u8; RECORD_BYTES],
    },
}

impl Predecessor {
    fn record(&self) -> CapacityRecord {
        match self {
            Self::Anchor { current, .. } => *current.target(),
            Self::Account { record, .. } => *record,
        }
    }
}

/// Crash boundaries the controls inject after; production never sets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Document,
    Anchor,
    Account,
    /// A repair has recorded its evidence and not yet written the account.
    RepairEvidence,
    /// A repair has written the account.
    RepairAccount,
}

/// The record owners of a stopped node, opened under the operator's lock.
/// Work and time are bounded by the policy's recovery envelope even though
/// nothing is charged durably: an operator command is still finite work.
pub(super) struct StoppedCapacityAccount<'lock> {
    /// The proof that this stopped node's state lock is held; the only way
    /// an incarnation recorded in another pid namespace counts as ended.
    state_lock: lillux::ExclusiveLockWitness<'lock>,
    pub(super) account: RetainedStore,
    pub(super) anchor: RetainedAnchor,
    onboarding: PinnedDirectory,
    pub(super) state: PinnedDirectory,
    pub(super) issuance: [u8; 32],
    pub(super) budget: Budget,
    pub(super) fail_after: Cell<Option<Step>>,
}

/// Finite work and time for one command run; nothing here is durable.
pub(super) struct Budget {
    pub(super) deadline: MonotonicDeadline,
    work: Cell<u64>,
}

impl Budget {
    pub(super) fn pay(&self, steps: u64) -> Result<()> {
        ensure!(
            !self.deadline.has_elapsed(),
            "operator re-issuance interval exhausted"
        );
        self.work.set(
            self.work
                .get()
                .checked_sub(steps)
                .context("operator re-issuance work exhausted")?,
        );
        Ok(())
    }
}

impl<'lock> StoppedCapacityAccount<'lock> {
    pub(super) fn open(
        directory: PinnedDirectory,
        account_identity: lillux::PinnedRegularFileIdentity,
        onboarding: PinnedDirectory,
        state: PinnedDirectory,
        issuance: [u8; 32],
        policy: &NodeNativeCapacityPolicy,
        state_lock: lillux::ExclusiveLockWitness<'lock>,
    ) -> Result<Self> {
        policy.validate()?;
        let directory_identity = directory.identity()?;
        let account = RetainedStore::open(directory.try_clone()?, account_identity)?;
        let anchor = RetainedAnchor::retain(directory, directory_identity)?;
        Ok(Self {
            state_lock,
            account,
            anchor,
            onboarding,
            state,
            issuance,
            budget: Budget {
                deadline: MonotonicDeadline::after(Duration::from_millis(
                    policy.recovery.maximum_duration_ms,
                )),
                work: Cell::new(policy.recovery.work_per_attempt),
            },
            fail_after: Cell::new(None),
        })
    }

    /// Refuse, typed, an account the sound anchor's redo cannot account for.
    /// An account the cut has not reached yet names the cut, not a repair.
    pub(super) fn require_account_in_redo(
        &self,
        current: &super::redo::AccountRedo,
        bytes: &[u8; super::RECORD_BYTES],
    ) -> Result<()> {
        if current.accepts_account_bytes(bytes)? {
            return Ok(());
        }
        if super::cut_retirement::account_awaits_cut(bytes, &encode(current.target())?) {
            anyhow::bail!(super::cut_retirement::PRE_CUT_CURE);
        }
        Err(super::torn::account_outside_redo(bytes))
    }

    /// Load the anchor. A torn one is refused with the cure the account
    /// proves under `policy`; the account is read best effort for that.
    pub(super) fn load_anchor(&mut self, policy: &NodeNativeCapacityPolicy) -> Result<()> {
        match self
            .anchor
            .load(policy, &self.issuance, self.budget.deadline, &|steps| {
                self.budget.pay(steps)
            }) {
            Ok(()) => Ok(()),
            Err(error) => {
                let account = self
                    .account
                    .read_bytes(self.budget.deadline, &|steps| self.budget.pay(steps))
                    .ok();
                Err(super::torn::with_anchor_cure(
                    error,
                    account.as_ref(),
                    policy,
                    &self.issuance,
                ))
            }
        }
    }

    /// Require that a recorded incarnation has ended, classified under the
    /// state lock this stopped node's operator holds: a previous daemon
    /// recorded in another pid namespace has released the exclusive lock by
    /// exiting, so it counts as ended. Every other indeterminate still fails
    /// closed.
    fn require_ended(
        &self,
        process: &lillux::process_control::ExactProcessIdentity,
        live_message: &str,
    ) -> Result<()> {
        ensure!(
            process
                .has_ended_holding_exclusive_lock(&self.state_lock)
                .map_err(anyhow::Error::msg)?,
            "{live_message}"
        );
        Ok(())
    }

    /// Load the anchor and refuse unless the stopped account carries no
    /// outstanding incarnation debit and holds its anchor's target image.
    /// A debit, live or dead, is never settled by moving the account's
    /// location: a restart or a re-issuance settles it, and a policy amendment
    /// settles a provably dead one first ([`Self::settle_cold`]).
    ///
    /// Returns where the account stands: its epoch, its debit sequence and
    /// the digest of the anchor's committed image, which a signed re-anchor
    /// carries so a later document cannot step back to an older position.
    pub(super) fn require_no_outstanding_debit(
        &mut self,
        policy: &NodeNativeCapacityPolicy,
    ) -> Result<super::reanchor::AccountPosition> {
        self.load_anchor(policy)?;
        let current = *self.anchor.current()?;
        let bytes = self
            .account
            .read_bytes(self.budget.deadline, &|steps| self.budget.pay(steps))?;
        self.require_account_in_redo(&current, &bytes)?;
        ensure!(
            bytes == encode(current.target())?,
            "capacity account has an unapplied redo; start the node or re-issue to settle it first"
        );
        ensure!(
            current.target().outstanding.is_none(),
            "an incarnation debit is outstanding; settle it before moving the account"
        );
        let target = *current.target();
        Ok(super::reanchor::AccountPosition {
            epoch: target.epoch,
            sequence: target.sequence,
            anchor_digest: hex_digest(&sha2::Sha256::digest(current.encode(policy)?).into()),
        })
    }

    /// Whether the stopped account owes the cold settlement a restart would
    /// perform first, checked under `policy` without writing: an outstanding
    /// debit whose incarnation has provably ended (dry-running the recovery
    /// charge and the settlement transition under `policy`), or an account
    /// image a published redo has not reached yet. A live or unproved
    /// incarnation is refused.
    pub(super) fn owes_cold_settlement(
        &mut self,
        policy: &NodeNativeCapacityPolicy,
    ) -> Result<bool> {
        self.load_anchor(policy)?;
        let current = *self.anchor.current()?;
        let bytes = self
            .account
            .read_bytes(self.budget.deadline, &|steps| self.budget.pay(steps))?;
        self.require_account_in_redo(&current, &bytes)?;
        let torn = bytes != encode(current.target())?;
        let Some(debit) = current.target().outstanding else {
            return Ok(torn);
        };
        self.require_ended(
            &debit.process.exact()?,
            "outstanding native incarnation remains live or unproved; stop it first",
        )?;
        if let Some(previous) = current.recovery_process() {
            self.require_ended(
                &previous.exact()?,
                "previous capacity recovery process remains live or unproved",
            )?;
        }
        let process = lillux::process_control::capture_current_process_identity()
            .map_err(anyhow::Error::msg)?;
        let charged = current.charge_recovery(&process, policy)?;
        charged.successor(
            Operation::DeadIncarnationSettlement,
            CapacityRecord {
                outstanding: None,
                ..*charged.target()
            },
            policy,
        )?;
        Ok(true)
    }

    /// Perform the cold settlement [`Self::owes_cold_settlement`] found, as
    /// the next start would: bring the account image to its published redo,
    /// then, while a dead incarnation's debit is outstanding, charge one
    /// recovery attempt to this process and publish and apply the
    /// dead-incarnation settlement. The debit's units stay consumed and the
    /// charge is never refunded; only the outstanding marker is cleared. A run
    /// interrupted anywhere converges on rerun. Returns the settled
    /// incarnation's digest when the anchor's last transition settled one.
    pub(super) fn settle_cold(
        &mut self,
        policy: &NodeNativeCapacityPolicy,
    ) -> Result<Option<String>> {
        if !self.owes_cold_settlement(policy)? {
            return Ok(None);
        }
        let current = *self.anchor.current()?;
        let bytes = self
            .account
            .read_bytes(self.budget.deadline, &|steps| self.budget.pay(steps))?;
        if bytes != encode(current.target())? {
            self.account
                .apply_redo(&current, self.budget.deadline, &|steps| {
                    self.budget.pay(steps)
                })?;
        }
        if current.target().outstanding.is_some() {
            let process = lillux::process_control::capture_current_process_identity()
                .map_err(anyhow::Error::msg)?;
            let admission = current.charge_recovery(&process, policy)?;
            self.anchor.publish(
                admission,
                policy,
                &self.issuance,
                self.budget.deadline,
                &|steps| self.budget.pay(steps),
            )?;
            self.account
                .apply_redo(self.anchor.current()?, self.budget.deadline, &|steps| {
                    self.budget.pay(steps)
                })?;
            let charged = *self.anchor.current()?;
            let settlement = charged.successor(
                Operation::DeadIncarnationSettlement,
                CapacityRecord {
                    outstanding: None,
                    ..*charged.target()
                },
                policy,
            )?;
            self.anchor.publish(
                settlement,
                policy,
                &self.issuance,
                self.budget.deadline,
                &|steps| self.budget.pay(steps),
            )?;
            self.account
                .apply_redo(self.anchor.current()?, self.budget.deadline, &|steps| {
                    self.budget.pay(steps)
                })?;
        }
        Ok(self
            .anchor
            .current()?
            .settled_incarnation()
            .as_ref()
            .map(hex_digest))
    }

    pub(super) fn crash_after(&self, step: Step) -> Result<()> {
        ensure!(
            self.fail_after.get() != Some(step),
            "injected re-issuance interruption after {step:?}"
        );
        Ok(())
    }

    /// The document currently published beside the issuance, if any.
    fn existing_document(&self) -> Result<Option<(lillux::PinnedRegularFile, Vec<u8>)>> {
        let Some(file) = self
            .onboarding
            .open_pinned_regular(OsStr::new(REISSUANCE_DOCUMENT_NAME), false)?
        else {
            return Ok(None);
        };
        file.require_effective_user_owned()?;
        let observation = file.observation()?;
        let bytes = file.read_stable_bounded(&observation, REISSUANCE_DOCUMENT_BOUND)?;
        Ok(Some((file, bytes)))
    }

    /// `actor` is the account this command runs as. It is settled before
    /// anything is read or written: every path that changes state ends in an
    /// evidence line naming it, so an account that cannot be recorded (root)
    /// must refuse here rather than open an epoch whose evidence never lands.
    fn reissue(
        &mut self,
        identity: &ReissueIdentity<'_>,
        actor: std::result::Result<lillux::process_control::ControllerAccount, String>,
        reason: &str,
        command: &str,
    ) -> Result<ReissueReport> {
        let actor = actor.map_err(|error| {
            anyhow::anyhow!(
                "operator re-issuance refused before any write: the acting account cannot be recorded as its evidence actor ({error}); run it as the node account"
            )
        })?;
        ensure!(
            reason.chars().count() <= REASON_BOUND_CHARS,
            "re-issuance reason exceeds its bound"
        );
        // The log this run must end by extending is checked before the first
        // write, so a refused log never leaves an epoch without its evidence.
        if let Some(log) = self
            .state
            .open_pinned_regular(OsStr::new(REISSUE_EVIDENCE_LOG), false)?
        {
            require_owned_evidence_log(&log)?;
        }
        let policy = &identity.policy;
        let predecessor = match self.load_anchor(policy) {
            Ok(()) => {
                let current = *self.anchor.current()?;
                let bytes = self
                    .account
                    .read_bytes(self.budget.deadline, &|steps| self.budget.pay(steps))?;
                self.require_account_in_redo(&current, &bytes)?;
                Predecessor::Anchor { current, bytes }
            }
            Err(error) => self.account_predecessor(error)?,
        };
        let existing = self.existing_document()?;

        if let Predecessor::Anchor { current, bytes } = &predecessor
            && current.operation() == Operation::OperatorReissue
        {
            // The anchor already opened an epoch under a document. Only the
            // document it names may complete it; a rerun repairs and reports.
            let (_, document_bytes) = existing.as_ref().context(
                "operator re-issuance anchor has no document; the stopped node cannot be repaired",
            )?;
            let document_digest: [u8; 32] = sha2::Sha256::digest(document_bytes).into();
            ensure!(
                document_digest == current.authorization(),
                "operator re-issuance document is stale: it is not the one the anchor names"
            );
            let body = verify_reissuance_document(
                document_bytes,
                &identity.lineage,
                &identity.node_fingerprint,
                &self.issuance,
            )?;
            ensure!(
                body.to_epoch == current.epoch(),
                "operator re-issuance document names another epoch"
            );
            if *bytes != encode(current.target())? {
                self.account
                    .apply_redo(current, self.budget.deadline, &|steps| {
                        self.budget.pay(steps)
                    })?;
            }
            let evidence = CapacityReissueEvidence {
                document_digest: hex_digest(&document_digest),
                from_epoch: body.from_epoch,
                to_epoch: body.to_epoch,
                settled_incarnation: current.settled_incarnation().as_ref().map(hex_digest),
                recorded_at: lillux::time::iso8601_now(),
                actor,
                command: command.to_owned(),
            };
            self.append_evidence_once(&evidence)?;
            return Ok(ReissueReport {
                from_epoch: evidence.from_epoch,
                to_epoch: evidence.to_epoch,
                document_digest: evidence.document_digest,
                settled_incarnation: evidence.settled_incarnation,
                already_applied: true,
                from_account_record: false,
            });
        }

        ensure!(
            policy.renewal.operator_reissue,
            "signed policy does not pre-authorise operator re-issuance"
        );
        // Whatever the anchor still carries must be proven dead before its
        // settlement can be folded into the renewal. Liveness is never
        // inferred from a numeric pid; only the exact incarnation decides.
        let before = predecessor.record();
        if let Some(debit) = before.outstanding {
            self.require_ended(
                &debit.process.exact()?,
                "outstanding native incarnation remains live or unproved; stop it first",
            )?;
        }
        if let Predecessor::Anchor { current, bytes } = &predecessor
            && *bytes != encode(current.target())?
        {
            self.account
                .apply_redo(current, self.budget.deadline, &|steps| {
                    self.budget.pay(steps)
                })?;
        }
        let from_epoch = before.epoch;
        let to_epoch = from_epoch
            .checked_add(1)
            .context("capacity epoch counter exhausted")?;
        if let Some(maximum) = policy.renewal.maximum_epochs {
            ensure!(
                to_epoch <= maximum,
                "capacity epoch bound reached: the signed policy allows {maximum} epochs"
            );
        }

        // The document names the image the epoch is renewed from: the anchor
        // when it is readable, else the account record, whose own length keeps
        // the two digests apart.
        let anchor_bytes: Vec<u8> = match &predecessor {
            Predecessor::Anchor { current, .. } => current.encode(policy)?.to_vec(),
            Predecessor::Account { bytes, .. } => bytes.to_vec(),
        };
        let body = ReissuanceBody {
            schema: REISSUANCE_SCHEMA.to_owned(),
            operator_fingerprint: identity.operator_fingerprint.clone(),
            node_fingerprint: identity.node_fingerprint.clone(),
            issuance_digest: hex_digest(&self.issuance),
            from_epoch,
            to_epoch,
            anchor_digest: hex_digest(&sha2::Sha256::digest(anchor_bytes).into()),
            previous_reissuance_digest: existing
                .as_ref()
                .map(|(_, previous)| hex_digest(&sha2::Sha256::digest(previous).into())),
            reason: reason.to_owned(),
            issued_at: lillux::time::iso8601_now(),
            authority: identity.signer.authority()?,
        };
        let canonical = lillux::canonical_json(&serde_json::to_value(&body)?)?;
        let document = ReissuanceDocument {
            body,
            request: identity.signer.request(),
            signature: identity.signer.sign(canonical.as_bytes()),
        };
        let document_bytes = serde_json::to_vec(&document)?;
        ensure!(
            u64::try_from(document_bytes.len())? <= REISSUANCE_DOCUMENT_BOUND,
            "operator re-issuance document exceeds its bound"
        );
        self.budget.pay(super::ACCOUNT_WRITE_WORK)?;
        self.onboarding.atomic_write_pinned_if_same(
            OsStr::new(REISSUANCE_DOCUMENT_NAME),
            existing.as_ref().map(|(file, _)| file),
            &document_bytes,
            0o600,
            crate::NODE_TEMP_PURPOSE,
        )?;
        self.onboarding.sync()?;
        self.crash_after(Step::Document)?;

        let document_digest: [u8; 32] = sha2::Sha256::digest(&document_bytes).into();
        let target = CapacityRecord {
            epoch: to_epoch,
            consumed: genesis_consumed(),
            outstanding: None,
            ..before
        };
        let from_account_record = matches!(predecessor, Predecessor::Account { .. });
        let next = match &predecessor {
            Predecessor::Anchor { current, .. } => {
                current.renew(Operation::OperatorReissue, target, document_digest, policy)?
            }
            Predecessor::Account { record, .. } => {
                AccountRedo::renew_from_account(*record, target, document_digest, policy)?
            }
        };
        if from_account_record {
            self.anchor.replace_torn(
                next,
                policy,
                &self.issuance,
                self.budget.deadline,
                &|steps| self.budget.pay(steps),
            )?;
        } else {
            self.anchor.publish(
                next,
                policy,
                &self.issuance,
                self.budget.deadline,
                &|steps| self.budget.pay(steps),
            )?;
        }
        self.crash_after(Step::Anchor)?;
        self.account
            .apply_redo(self.anchor.current()?, self.budget.deadline, &|steps| {
                self.budget.pay(steps)
            })?;
        self.crash_after(Step::Account)?;

        let evidence = CapacityReissueEvidence {
            document_digest: hex_digest(&document_digest),
            from_epoch,
            to_epoch,
            settled_incarnation: next.settled_incarnation().as_ref().map(hex_digest),
            recorded_at: lillux::time::iso8601_now(),
            actor,
            command: if from_account_record {
                format!(
                    "{command}; the redo anchor was torn and the epoch opened from the account record"
                )
            } else {
                command.to_owned()
            },
        };
        append_evidence(&self.state, &evidence)?;
        Ok(ReissueReport {
            from_epoch,
            to_epoch,
            document_digest: evidence.document_digest,
            settled_incarnation: evidence.settled_incarnation,
            already_applied: false,
            from_account_record,
        })
    }

    /// The account record to renew from when the anchor is torn. Only a torn
    /// anchor whose cure is re-issuance qualifies: the account must verify
    /// and belong to this issuance and the signed policy must permit the
    /// re-issuance. Any other refusal is returned unchanged, so a file of
    /// another issuance or a node whose policy forbids re-issuance holds.
    fn account_predecessor(&self, error: anyhow::Error) -> Result<Predecessor> {
        let qualifies = error
            .downcast_ref::<super::torn::CapacityRecordTorn>()
            .is_some_and(|torn| {
                torn.file == super::torn::CapacityFile::Anchor
                    && torn.cure == super::torn::CapacityCure::ReissueFromAccount
            });
        if !qualifies {
            return Err(error);
        }
        let bytes = self
            .account
            .read_bytes(self.budget.deadline, &|steps| self.budget.pay(steps))?;
        let record = super::decode(&bytes).map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
        ensure!(
            record.issuance_digest == self.issuance,
            "capacity account belongs to another issuance"
        );
        Ok(Predecessor::Account { record, bytes })
    }

    /// A rerun after an interruption between the account write and the log
    /// append owes exactly one line; a rerun after a complete run owes none.
    fn append_evidence_once(&self, evidence: &CapacityReissueEvidence) -> Result<()> {
        let recorded = read_evidence(&self.state)?;
        if recorded
            .iter()
            .any(|entry| entry.document_digest == evidence.document_digest)
        {
            return Ok(());
        }
        append_evidence(&self.state, evidence)
    }
}

/// The evidence log is the node account's own record, held to the same rule as
/// the re-issuance document: owned by the account running this command and
/// writable by no group or other. A log another account owns or can write is
/// refused, never read as evidence and never extended.
fn require_owned_evidence_log(log: &lillux::PinnedRegularFile) -> Result<()> {
    log.require_effective_user_owned().with_context(|| {
        format!(
            "re-issuance evidence log {REISSUE_EVIDENCE_LOG} is refused: it must be owned by the account running this command and not group/other writable"
        )
    })
}

fn read_evidence(directory: &PinnedDirectory) -> Result<Vec<CapacityReissueEvidence>> {
    let Some(log) = directory.open_pinned_regular(OsStr::new(REISSUE_EVIDENCE_LOG), false)? else {
        return Ok(Vec::new());
    };
    require_owned_evidence_log(&log)?;
    let observation = log.observation()?;
    let raw = log.read_stable_bounded(&observation, REISSUE_EVIDENCE_LOG_BOUND as u64)?;
    raw.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line).map_err(|error| {
                anyhow::anyhow!("re-issuance evidence entry is unreadable: {error}")
            })
        })
        .collect()
}

/// Append one line by pinned compare-and-write, dropping the oldest whole
/// lines only when the log would exceed its bound.
fn append_evidence(directory: &PinnedDirectory, evidence: &CapacityReissueEvidence) -> Result<()> {
    let existing = directory.open_pinned_regular(OsStr::new(REISSUE_EVIDENCE_LOG), false)?;
    let mut body = match &existing {
        Some(log) => {
            require_owned_evidence_log(log)?;
            let observation = log.observation()?;
            log.read_stable_bounded(&observation, REISSUE_EVIDENCE_LOG_BOUND as u64)?
        }
        None => Vec::new(),
    };
    let mut line = serde_json::to_vec(evidence)?;
    line.push(b'\n');
    ensure!(
        line.len() <= REISSUE_EVIDENCE_LOG_BOUND,
        "re-issuance evidence entry exceeds the log bound"
    );
    while body.len() + line.len() > REISSUE_EVIDENCE_LOG_BOUND {
        match body.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                body.drain(..=end);
            }
            None => body.clear(),
        }
    }
    body.extend_from_slice(&line);
    directory.atomic_write_pinned_if_same(
        OsStr::new(REISSUE_EVIDENCE_LOG),
        existing.as_ref(),
        &body,
        0o600,
        crate::NODE_TEMP_PURPOSE,
    )?;
    directory.sync()?;
    Ok(())
}

/// Leave an outstanding incarnation debit in a stopped node's account, as a
/// start that crashed before release would have: one debit redo published
/// and applied under `binding`, the binding in force.
#[cfg(test)]
pub(crate) fn leave_outstanding_debit(
    app_root: &Path,
    binding: &NativeCapacityAccountBinding,
    process: &lillux::process_control::ExactProcessIdentity,
    state_lock: &StateLock,
) -> Result<()> {
    let open = |path: &str| -> Result<PinnedDirectory> {
        PinnedDirectory::open(&app_root.join(path))?.with_context(|| format!("{path} absent"))
    };
    let policy = &binding.policy;
    let issuance = binding.issuance_digest()?;
    let mut owner = StoppedCapacityAccount::open(
        open(".ai/node/native-capacity")?,
        binding.account_identity,
        open(".ai/config/onboarding")?,
        open(".ai/state")?,
        issuance,
        policy,
        state_lock.witness()?,
    )?;
    leave_debit(&mut owner, policy, issuance, process)
}

/// [`leave_outstanding_debit`] on an already open stopped-node owner.
#[cfg(test)]
pub(super) fn leave_debit(
    owner: &mut StoppedCapacityAccount<'_>,
    policy: &NodeNativeCapacityPolicy,
    issuance: [u8; 32],
    process: &lillux::process_control::ExactProcessIdentity,
) -> Result<()> {
    let deadline = owner.budget.deadline;
    owner.anchor.load(policy, &issuance, deadline, &|steps| {
        owner.budget.pay(steps)
    })?;
    let current = *owner.anchor.current()?;
    let before = *current.target();
    let (sequence, consumed) = before.prepare_debit(policy.allowance, policy.incarnation.units)?;
    let (_, _, debited) = super::redo::controls::fixture();
    let target = CapacityRecord {
        sequence,
        consumed,
        outstanding: Some(super::IncarnationDebit {
            units: policy.incarnation.units,
            process_digest: process
                .incarnation_digest_bytes()
                .map_err(anyhow::Error::msg)?,
            process: super::redo::StoredProcess::capture(process)?,
            ..debited.outstanding.context("fixture debit")?
        }),
        ..before
    };
    let next = current.successor(Operation::Debit, target, policy)?;
    owner
        .anchor
        .publish(next, policy, &issuance, deadline, &|steps| {
            owner.budget.pay(steps)
        })?;
    owner
        .account
        .apply_redo(owner.anchor.current()?, deadline, &|steps| {
            owner.budget.pay(steps)
        })
}

#[cfg(all(test, target_os = "linux"))]
mod controls {
    //! Filesystem controls for the stopped-node owner. The records are the
    //! synthetic transition subjects of the redo controls; the operator key
    //! is deterministic. Nothing here is signed admission.

    use super::super::redo::AccountRedo;
    use super::super::{IncarnationDebit, RECORD_BYTES, decode, redo};
    use super::*;
    use crate::apply_authority::Authority;
    use base64::Engine as _;
    use lillux::crypto::{Signer as _, SigningKey};

    struct Fixture {
        _temporary: tempfile::TempDir,
        /// The stopped node's state lock, held for the fixture's lifetime.
        lock: StateLock,
        directory: PinnedDirectory,
        onboarding: PinnedDirectory,
        state: PinnedDirectory,
        policy: NodeNativeCapacityPolicy,
        issuance: [u8; 32],
        account_identity: lillux::PinnedRegularFileIdentity,
        operator: SigningKey,
        /// A debit of a process this test owned and has already reaped, so
        /// death is proven by the kernel, never assumed from a number.
        dead: IncarnationDebit,
    }

    /// Spawn a child, capture its exact incarnation while it waits on stdin,
    /// then let it end and reap it.
    fn ended_process() -> lillux::process_control::ExactProcessIdentity {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("read x")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let exact =
            lillux::process_control::capture_exact_process_identity(child.id(), None).unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(exact.has_ended().unwrap());
        exact
    }

    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(temporary.path().join("native-capacity")).unwrap();
            std::fs::create_dir_all(temporary.path().join("onboarding")).unwrap();
            std::fs::create_dir_all(temporary.path().join("state")).unwrap();
            let directory = PinnedDirectory::open(&temporary.path().join("native-capacity"))
                .unwrap()
                .unwrap();
            directory.tighten_owner_private_directory().unwrap();
            let onboarding = PinnedDirectory::open(&temporary.path().join("onboarding"))
                .unwrap()
                .unwrap();
            let state = PinnedDirectory::open(&temporary.path().join("state"))
                .unwrap()
                .unwrap();
            let (policy, genesis, debit_target) = redo::controls::fixture();
            let ended = ended_process();
            let dead = IncarnationDebit {
                process_digest: ended.incarnation_digest_bytes().unwrap(),
                process: redo::StoredProcess::capture(&ended).unwrap(),
                ..debit_target.outstanding.unwrap()
            };
            let account = directory
                .atomic_create_pinned_regular(
                    OsStr::new("account"),
                    &encode(genesis.target()).unwrap(),
                    0o600,
                    crate::NODE_TEMP_PURPOSE,
                )
                .unwrap()
                .unwrap();
            directory
                .atomic_create_pinned_regular(
                    OsStr::new("anchor"),
                    &genesis.encode(&policy).unwrap(),
                    0o600,
                    crate::NODE_TEMP_PURPOSE,
                )
                .unwrap()
                .unwrap();
            let lock = StateLock::acquire(&temporary.path().join("operator.lock")).unwrap();
            Self {
                _temporary: temporary,
                lock,
                directory,
                onboarding,
                state,
                policy,
                issuance: genesis.target().issuance_digest,
                account_identity: account.identity().unwrap(),
                operator: SigningKey::from_bytes(&[7; 32]),
                dead,
            }
        }

        fn identity(&self) -> ReissueIdentity<'_> {
            ReissueIdentity {
                signer: DocumentSigner::Operator(&self.operator),
                operator_fingerprint: ryeos_engine::trust::compute_fingerprint(
                    &self.operator.verifying_key(),
                ),
                lineage: OperatorLineage::single(
                    &ryeos_engine::trust::compute_fingerprint(&self.operator.verifying_key()),
                    &self.operator.verifying_key(),
                ),
                node_fingerprint: "c".repeat(64),
                policy: self.policy.clone(),
            }
        }

        fn owner(&self) -> StoppedCapacityAccount<'_> {
            StoppedCapacityAccount::open(
                self.directory.try_clone().unwrap(),
                self.account_identity,
                self.onboarding.try_clone().unwrap(),
                self.state.try_clone().unwrap(),
                self.issuance,
                &self.policy,
                self.lock.witness().unwrap(),
            )
            .unwrap()
        }

        fn run(&self, fail_after: Option<Step>) -> Result<ReissueReport> {
            let mut owner = self.owner();
            owner.fail_after.set(fail_after);
            owner.reissue(
                &self.identity(),
                lillux::process_control::ControllerAccount::current(),
                "controls",
                "ryeos node capacity reissue",
            )
        }

        /// The current anchor and the raw account bytes, read fresh.
        fn current(&self) -> (AccountRedo, [u8; RECORD_BYTES]) {
            let owner = self.owner();
            let mut owner = owner;
            owner
                .anchor
                .load(
                    &self.policy,
                    &self.issuance,
                    owner.budget.deadline,
                    &|steps| owner.budget.pay(steps),
                )
                .unwrap();
            let current = *owner.anchor.current().unwrap();
            let bytes = owner
                .account
                .read_bytes(owner.budget.deadline, &|steps| owner.budget.pay(steps))
                .unwrap();
            (current, bytes)
        }

        /// Publish a debit of a dead incarnation and write the account, as a
        /// start that crashed before release would have left it.
        fn debit_dead_incarnation(&self) {
            let (current, _) = self.current();
            let before = *current.target();
            let (sequence, consumed) = before
                .prepare_debit(self.policy.allowance, self.policy.incarnation.units)
                .unwrap();
            let target = CapacityRecord {
                sequence,
                consumed,
                outstanding: Some(self.dead),
                ..before
            };
            let next = current
                .successor(Operation::Debit, target, &self.policy)
                .unwrap();
            let mut owner = self.owner();
            owner
                .anchor
                .load(
                    &self.policy,
                    &self.issuance,
                    owner.budget.deadline,
                    &|steps| owner.budget.pay(steps),
                )
                .unwrap();
            owner
                .anchor
                .publish(
                    next,
                    &self.policy,
                    &self.issuance,
                    owner.budget.deadline,
                    &|steps| owner.budget.pay(steps),
                )
                .unwrap();
            owner
                .account
                .apply_redo(
                    owner.anchor.current().unwrap(),
                    owner.budget.deadline,
                    &|steps| owner.budget.pay(steps),
                )
                .unwrap();
        }

        fn document_path(&self) -> std::path::PathBuf {
            self._temporary
                .path()
                .join("onboarding")
                .join(REISSUANCE_DOCUMENT_NAME)
        }

        fn evidence(&self) -> Vec<CapacityReissueEvidence> {
            read_evidence(&self.state).unwrap()
        }
    }

    #[test]
    fn reissue_opens_exactly_one_epoch_and_settles_the_dead_incarnation() {
        let f = Fixture::new();
        f.debit_dead_incarnation();
        let (debit, _) = f.current();
        assert_eq!(debit.epoch(), 1);
        let report = f.run(None).unwrap();
        assert_eq!(
            (report.from_epoch, report.to_epoch, report.already_applied),
            (1, 2, false)
        );
        let expected_settled = hex_digest(&f.dead.process_digest);
        assert_eq!(
            report.settled_incarnation.as_deref(),
            Some(expected_settled.as_str())
        );
        let (current, bytes) = f.current();
        assert_eq!(current.operation(), Operation::OperatorReissue);
        assert_eq!(current.epoch(), 2);
        assert_eq!(current.recovery_attempts(), 0);
        assert_eq!(current.target().sequence, debit.target().sequence);
        assert_eq!(current.target().consumed, genesis_consumed());
        assert!(current.target().outstanding.is_none());
        assert_eq!(bytes, encode(current.target()).unwrap());
        let document = std::fs::read(f.document_path()).unwrap();
        let digest: [u8; 32] = sha2::Sha256::digest(&document).into();
        assert_eq!(digest, current.authorization());
        assert_eq!(report.document_digest, hex_digest(&digest));
        let body = verify_reissuance_document(
            &document,
            &f.identity().lineage,
            &f.identity().node_fingerprint,
            &f.issuance,
        )
        .unwrap();
        assert_eq!((body.from_epoch, body.to_epoch), (1, 2));
        assert_eq!(
            body.anchor_digest,
            hex_digest(&sha2::Sha256::digest(debit.encode(&f.policy).unwrap()).into())
        );
        assert_eq!(body.previous_reissuance_digest, None);
        let evidence = f.evidence();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].document_digest, report.document_digest);
        assert_eq!(evidence[0].command, "ryeos node capacity reissue");
        assert_eq!(evidence[0].settled_incarnation, report.settled_incarnation);
    }

    #[test]
    fn rerun_is_idempotent_and_the_next_epoch_needs_a_served_start() {
        let f = Fixture::new();
        let first = f.run(None).unwrap();
        assert_eq!((first.from_epoch, first.to_epoch), (1, 2));
        let document = std::fs::read(f.document_path()).unwrap();
        for _ in 0..3 {
            let again = f.run(None).unwrap();
            assert!(again.already_applied);
            assert_eq!((again.from_epoch, again.to_epoch), (1, 2));
            assert_eq!(again.document_digest, first.document_digest);
            assert_eq!(f.current().0.epoch(), 2);
            assert_eq!(std::fs::read(f.document_path()).unwrap(), document);
            assert_eq!(f.evidence().len(), 1);
        }
        // Only a debit in the new epoch lets the operator open another.
        f.debit_dead_incarnation();
        let second = f.run(None).unwrap();
        assert_eq!(
            (second.from_epoch, second.to_epoch, second.already_applied),
            (2, 3, false)
        );
        let body = verify_reissuance_document(
            &std::fs::read(f.document_path()).unwrap(),
            &f.identity().lineage,
            &f.identity().node_fingerprint,
            &f.issuance,
        )
        .unwrap();
        assert_eq!(
            body.previous_reissuance_digest.as_deref(),
            Some(first.document_digest.as_str())
        );
        assert_eq!(f.evidence().len(), 2);
    }

    #[test]
    fn interruption_after_each_step_converges_on_one_epoch() {
        for step in [Step::Document, Step::Anchor, Step::Account] {
            let f = Fixture::new();
            f.debit_dead_incarnation();
            let (debit, debit_bytes) = f.current();
            let error = f.run(Some(step)).unwrap_err();
            assert!(
                error.to_string().contains("injected"),
                "{step:?}: {error:#}"
            );
            let (interrupted, bytes) = f.current();
            match step {
                Step::Document => {
                    assert_eq!(interrupted, debit);
                    assert_eq!(bytes, debit_bytes);
                    assert!(f.document_path().exists());
                    assert!(f.evidence().is_empty());
                }
                Step::Anchor => {
                    assert_eq!(interrupted.operation(), Operation::OperatorReissue);
                    assert_eq!(bytes, debit_bytes);
                    assert!(f.evidence().is_empty());
                }
                Step::Account => {
                    assert_eq!(interrupted.operation(), Operation::OperatorReissue);
                    assert_eq!(bytes, encode(interrupted.target()).unwrap());
                    assert!(f.evidence().is_empty());
                }
                Step::RepairEvidence | Step::RepairAccount => {
                    unreachable!("repair steps are not injected into a re-issuance")
                }
            }
            let report = f.run(None).unwrap();
            assert_eq!((report.from_epoch, report.to_epoch), (1, 2), "{step:?}");
            assert_eq!(report.already_applied, step != Step::Document, "{step:?}");
            let (current, bytes) = f.current();
            assert_eq!(current.epoch(), 2, "{step:?}");
            assert_eq!(bytes, encode(current.target()).unwrap());
            let evidence = f.evidence();
            assert_eq!(evidence.len(), 1, "{step:?}");
            assert_eq!(evidence[0].document_digest, report.document_digest);
            let digest: [u8; 32] =
                sha2::Sha256::digest(std::fs::read(f.document_path()).unwrap()).into();
            assert_eq!(digest, current.authorization(), "{step:?}");
            // A further rerun changes nothing.
            let again = f.run(None).unwrap();
            assert!(again.already_applied);
            assert_eq!(f.current().0.epoch(), 2);
            assert_eq!(f.evidence().len(), 1);
        }
    }

    #[test]
    fn every_torn_account_prefix_after_publication_is_repaired_to_the_new_epoch() {
        // Injected account writes bypass the writer protocol to represent
        // crash prefixes, including cuts inside the epoch bytes 12..20.
        let f = Fixture::new();
        f.debit_dead_incarnation();
        let (_, before) = f.current();
        assert!(f.run(Some(Step::Anchor)).is_err());
        let (published, _) = f.current();
        let target = encode(published.target()).unwrap();
        let cuts = (0..=super::super::PROCESS_END)
            .chain((super::super::PROCESS_END..RECORD_BYTES).step_by(251))
            .chain([RECORD_BYTES - 1, RECORD_BYTES]);
        for cut in cuts {
            let mut torn = before;
            torn[..cut].copy_from_slice(&target[..cut]);
            std::fs::write(f._temporary.path().join("native-capacity/account"), torn).unwrap();
            let report = f.run(None).unwrap();
            assert!(report.already_applied, "cut {cut}");
            assert_eq!(f.current().1, target, "cut {cut}");
        }
        assert_eq!(f.evidence().len(), 1);
        assert_eq!(decode(&target).unwrap().epoch, 2);
    }

    #[test]
    fn stale_tampered_and_foreign_documents_are_refused_without_repair() {
        // Stale: a document that is not the one the anchor names.
        let f = Fixture::new();
        let first = f.run(None).unwrap();
        let first_document = std::fs::read(f.document_path()).unwrap();
        f.debit_dead_incarnation();
        let second = f.run(None).unwrap();
        assert_eq!((second.from_epoch, second.to_epoch), (2, 3));
        std::fs::write(f.document_path(), &first_document).unwrap();
        let error = f.run(None).unwrap_err();
        assert!(error.to_string().contains("stale"), "{error:#}");
        assert_eq!(f.current().0.epoch(), 3);
        assert_eq!(f.evidence().len(), 2);
        assert_eq!(f.evidence()[0].document_digest, first.document_digest);

        // Tampered: a body byte changed and re-encoded.
        let f = Fixture::new();
        f.run(None).unwrap();
        let mut document: ReissuanceDocument =
            serde_json::from_slice(&std::fs::read(f.document_path()).unwrap()).unwrap();
        document.body.reason.push('!');
        std::fs::write(f.document_path(), serde_json::to_vec(&document).unwrap()).unwrap();
        let error = f.run(None).unwrap_err();
        assert!(error.to_string().contains("stale"), "{error:#}");

        // Absent: the anchor names a document that no longer exists.
        std::fs::remove_file(f.document_path()).unwrap();
        let error = f.run(None).unwrap_err();
        assert!(error.to_string().contains("no document"), "{error:#}");
        assert_eq!(f.current().0.epoch(), 2);

        // Foreign: the anchor names a document another key signed.
        let f = Fixture::new();
        let (current, _) = f.current();
        let foreign = SigningKey::from_bytes(&[9; 32]);
        let body = ReissuanceBody {
            schema: REISSUANCE_SCHEMA.to_owned(),
            operator_fingerprint: f.identity().operator_fingerprint,
            node_fingerprint: f.identity().node_fingerprint,
            issuance_digest: hex_digest(&f.issuance),
            from_epoch: 1,
            to_epoch: 2,
            anchor_digest: hex_digest(
                &sha2::Sha256::digest(current.encode(&f.policy).unwrap()).into(),
            ),
            previous_reissuance_digest: None,
            reason: "foreign".to_owned(),
            issued_at: lillux::time::iso8601_now(),
            authority: Authority::Operator,
        };
        let canonical = lillux::canonical_json(&serde_json::to_value(&body).unwrap()).unwrap();
        let signature = foreign.sign(canonical.as_bytes());
        let document = serde_json::to_vec(&ReissuanceDocument {
            body,
            request: None,
            signature: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
            ),
        })
        .unwrap();
        std::fs::write(f.document_path(), &document).unwrap();
        let digest: [u8; 32] = sha2::Sha256::digest(&document).into();
        let target = CapacityRecord {
            epoch: 2,
            consumed: genesis_consumed(),
            outstanding: None,
            ..*current.target()
        };
        let next = current
            .renew(Operation::OperatorReissue, target, digest, &f.policy)
            .unwrap();
        let mut owner = f.owner();
        owner
            .anchor
            .load(&f.policy, &f.issuance, owner.budget.deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        owner
            .anchor
            .publish(
                next,
                &f.policy,
                &f.issuance,
                owner.budget.deadline,
                &|steps| owner.budget.pay(steps),
            )
            .unwrap();
        drop(owner);
        let error = f.run(None).unwrap_err();
        assert!(!error.to_string().contains("stale"), "{error:#}");
        let (_, bytes) = f.current();
        assert_eq!(
            decode(&bytes).unwrap().epoch,
            1,
            "a foreign document repairs nothing"
        );
        assert!(f.evidence().is_empty());
    }

    /// Re-issuance as the daemon applies it: no operator key, the node key
    /// signing under the operator's request.
    fn run_under(
        f: &Fixture,
        request: &crate::offline_operation::contract::SignedRequest,
    ) -> Result<ReissueReport> {
        let node = OperatorLineage::test_node_key();
        let mut identity = f.identity();
        identity.signer = DocumentSigner::Request {
            node: &node,
            request,
        };
        let mut owner = f.owner();
        owner.reissue(
            &identity,
            lillux::process_control::ControllerAccount::current(),
            "controls",
            "ryeos node capacity reissue",
        )
    }

    #[test]
    fn an_apply_time_document_is_node_signed_with_the_request_digest_and_verifies() {
        let f = Fixture::new();
        let node = OperatorLineage::test_node_key();
        let request =
            crate::apply_authority::test_request(&node, &f.operator, "node.capacity.reissue");
        run_under(&f, &request).expect("re-issuance under the request");
        let bytes = std::fs::read(f.document_path()).unwrap();
        let document: ReissuanceDocument = serde_json::from_slice(&bytes).unwrap();
        // Signed by the node, carrying the operator's request and its digest.
        assert_eq!(
            document.body.authority,
            Authority::NodeUnderRequest {
                request_digest: request.digest().unwrap()
            }
        );
        assert_eq!(document.request.as_ref(), Some(&request));
        let identity = f.identity();
        let body = verify_reissuance_document(
            &bytes,
            &identity.lineage,
            &identity.node_fingerprint,
            &f.issuance,
        )
        .expect("the node-signed document verifies through the operator's request");
        assert_eq!(body.operator_fingerprint, identity.operator_fingerprint);
        // The operator key signed nothing here: the signature is the node's.
        let canonical =
            lillux::canonical_json(&serde_json::to_value(&document.body).unwrap()).unwrap();
        let signature =
            crate::offline_operation::contract::decode_signature(&document.signature).unwrap();
        use lillux::crypto::Verifier as _;
        assert!(
            node.verifying_key()
                .verify(canonical.as_bytes(), &signature)
                .is_ok()
        );
        assert!(
            f.operator
                .verifying_key()
                .verify(canonical.as_bytes(), &signature)
                .is_err()
        );
    }

    #[test]
    fn a_document_whose_request_the_operator_did_not_sign_is_refused() {
        let f = Fixture::new();
        let node = OperatorLineage::test_node_key();
        let stranger = SigningKey::from_bytes(&[9; 32]);
        // The request names the real operator but carries a stranger's signature.
        let mut request =
            crate::apply_authority::test_request(&node, &f.operator, "node.capacity.reissue");
        request.signature =
            crate::apply_authority::test_request(&node, &stranger, "node.capacity.reissue")
                .signature;
        run_under(&f, &request).expect("the producer does not verify itself");
        let bytes = std::fs::read(f.document_path()).unwrap();
        let identity = f.identity();
        let error = verify_reissuance_document(
            &bytes,
            &identity.lineage,
            &identity.node_fingerprint,
            &f.issuance,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("apply_authority_request_not_operator"),
            "{error:#}"
        );
        // A request for another operation does not authorize a re-issuance.
        let f = Fixture::new();
        let wrong = crate::apply_authority::test_request(&node, &f.operator, "node.allow-restart");
        run_under(&f, &wrong).unwrap();
        let bytes = std::fs::read(f.document_path()).unwrap();
        let identity = f.identity();
        let error = verify_reissuance_document(
            &bytes,
            &identity.lineage,
            &identity.node_fingerprint,
            &f.issuance,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("apply_authority_request_operation"),
            "{error:#}"
        );
    }

    #[test]
    fn a_document_of_the_old_shape_is_refused_by_name_with_its_cure() {
        let f = Fixture::new();
        f.run(None).unwrap();
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(f.document_path()).unwrap()).unwrap();
        document["body"]["schema"] = "ryeos/native-capacity-reissuance/v1".into();
        let identity = f.identity();
        let error = verify_reissuance_document(
            &serde_json::to_vec(&document).unwrap(),
            &identity.lineage,
            &identity.node_fingerprint,
            &f.issuance,
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("capacity_document_old_shape"), "{text}");
        assert!(text.contains("ryeos init"), "{text}");
    }

    #[test]
    fn policy_without_operator_reissue_and_epoch_bound_refuse_before_any_write() {
        let mut f = Fixture::new();
        f.policy.renewal.operator_reissue = false;
        let error = f.run(None).unwrap_err();
        assert!(error.to_string().contains("pre-authorise"), "{error:#}");
        assert!(!f.document_path().exists());
        assert_eq!(f.current().0.epoch(), 1);

        f.policy.renewal.operator_reissue = true;
        f.policy.renewal.maximum_epochs = Some(1);
        let error = f.run(None).unwrap_err();
        assert!(error.to_string().contains("epoch bound"), "{error:#}");
        assert!(!f.document_path().exists());
        assert_eq!(f.current().0.epoch(), 1);

        f.policy.renewal.maximum_epochs = Some(2);
        assert_eq!(f.run(None).unwrap().to_epoch, 2);
        f.debit_dead_incarnation();
        assert!(f.run(None).unwrap_err().to_string().contains("epoch bound"));
        assert_eq!(f.current().0.epoch(), 2);
    }

    #[test]
    fn cold_settlement_settles_a_dead_debit_once_and_converges_after_a_torn_write() {
        let f = Fixture::new();
        assert!(!f.owner().owes_cold_settlement(&f.policy).unwrap());
        assert_eq!(f.owner().settle_cold(&f.policy).unwrap(), None);
        f.debit_dead_incarnation();
        let (debited, _) = f.current();

        // Interrupted after the settlement is published, before the account
        // image is written: the anchor no longer owes the debit, the file does.
        let mut owner = f.owner();
        let deadline = owner.budget.deadline;
        owner
            .anchor
            .load(&f.policy, &f.issuance, deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        let process = lillux::process_control::capture_current_process_identity().unwrap();
        let charged = debited.charge_recovery(&process, &f.policy).unwrap();
        owner
            .anchor
            .publish(charged, &f.policy, &f.issuance, deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        owner
            .account
            .apply_redo(owner.anchor.current().unwrap(), deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        let settlement = charged
            .successor(
                Operation::DeadIncarnationSettlement,
                CapacityRecord {
                    outstanding: None,
                    ..*charged.target()
                },
                &f.policy,
            )
            .unwrap();
        owner
            .anchor
            .publish(settlement, &f.policy, &f.issuance, deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        drop(owner);
        let (torn, bytes) = f.current();
        assert!(torn.target().outstanding.is_none());
        assert_ne!(bytes, encode(torn.target()).unwrap());
        f.owner()
            .require_no_outstanding_debit(&f.policy)
            .expect_err("a torn image still blocks a plain move");

        // The rerun completes the image without charging again.
        assert!(f.owner().owes_cold_settlement(&f.policy).unwrap());
        let settled = f.owner().settle_cold(&f.policy).unwrap();
        assert_eq!(settled, Some(hex_digest(&f.dead.process_digest)));
        let (after, bytes) = f.current();
        assert_eq!(after, torn);
        assert_eq!(bytes, encode(after.target()).unwrap());
        assert_eq!(after.recovery_attempts(), debited.recovery_attempts() + 1);
        assert!(!f.owner().owes_cold_settlement(&f.policy).unwrap());
        f.owner()
            .require_no_outstanding_debit(&f.policy)
            .expect("settled and whole");
    }

    #[test]
    fn cold_settlement_refuses_a_live_debit_without_writing() {
        let f = Fixture::new();
        let (current, _) = f.current();
        let before = *current.target();
        let (sequence, consumed) = before
            .prepare_debit(f.policy.allowance, f.policy.incarnation.units)
            .unwrap();
        let live = lillux::process_control::capture_current_process_identity().unwrap();
        let next = current
            .successor(
                Operation::Debit,
                CapacityRecord {
                    sequence,
                    consumed,
                    outstanding: Some(IncarnationDebit {
                        process_digest: live.incarnation_digest_bytes().unwrap(),
                        process: redo::StoredProcess::capture(&live).unwrap(),
                        ..f.dead
                    }),
                    ..before
                },
                &f.policy,
            )
            .unwrap();
        let mut owner = f.owner();
        let deadline = owner.budget.deadline;
        owner
            .anchor
            .load(&f.policy, &f.issuance, deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        owner
            .anchor
            .publish(next, &f.policy, &f.issuance, deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        owner
            .account
            .apply_redo(owner.anchor.current().unwrap(), deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        drop(owner);
        let before = f.current();
        let error = f.owner().settle_cold(&f.policy).expect_err("a live debit");
        assert!(
            format!("{error:#}").contains("remains live or unproved"),
            "{error:#}"
        );
        assert_eq!(f.current(), before);
    }

    /// A previous daemon recorded in another pid namespace: a real identity
    /// of this process with its namespace rewritten, so only the namespace
    /// says it is not this observer's.
    fn foreign_process() -> lillux::process_control::ExactProcessIdentity {
        let mut process = lillux::process_control::capture_current_process_identity().unwrap();
        process.pid_namespace = "pid:[1]".to_owned();
        process
    }

    #[test]
    fn a_foreign_namespace_debit_is_settled_through_the_lock_witness_as_a_dead_one() {
        let f = Fixture::new();
        // Without a proof the only classification fails closed.
        let error = foreign_process().has_ended().unwrap_err();
        assert!(error.contains("pid:[1]"), "{error}");
        leave_debit(&mut f.owner(), &f.policy, f.issuance, &foreign_process()).unwrap();
        assert!(f.owner().owes_cold_settlement(&f.policy).unwrap());
        let report = f.run(None).unwrap();
        assert_eq!(report.to_epoch, 2);
        assert!(
            report.settled_incarnation.is_some(),
            "reissue settles the foreign incarnation as it settles a dead one"
        );
        assert!(f.current().0.target().outstanding.is_none());
    }

    #[test]
    fn the_lock_witness_does_not_end_a_live_incarnation_of_this_namespace() {
        let f = Fixture::new();
        leave_debit(
            &mut f.owner(),
            &f.policy,
            f.issuance,
            &lillux::process_control::capture_current_process_identity().unwrap(),
        )
        .unwrap();
        let before = f.current();
        let error = f.owner().owes_cold_settlement(&f.policy).unwrap_err();
        assert!(
            format!("{error:#}").contains("remains live or unproved"),
            "{error:#}"
        );
        let error = f.run(None).unwrap_err();
        assert!(
            format!("{error:#}").contains("remains live or unproved"),
            "{error:#}"
        );
        assert_eq!(f.current(), before);
    }

    #[test]
    fn reanchor_precondition_refuses_an_outstanding_debit() {
        let f = Fixture::new();
        let position = f
            .owner()
            .require_no_outstanding_debit(&f.policy)
            .expect("a fresh account has no debit");
        assert_eq!((position.epoch, position.sequence), (1, 0));
        f.debit_dead_incarnation();
        let error = f
            .owner()
            .require_no_outstanding_debit(&f.policy)
            .expect_err("an outstanding debit, even a dead one, blocks a move");
        assert!(
            format!("{error:#}").contains("incarnation debit is outstanding"),
            "{error:#}"
        );
    }

    #[test]
    fn live_outstanding_incarnation_is_refused() {
        let f = Fixture::new();
        let (current, _) = f.current();
        let before = *current.target();
        let (sequence, consumed) = before
            .prepare_debit(f.policy.allowance, f.policy.incarnation.units)
            .unwrap();
        let live = lillux::process_control::capture_current_process_identity().unwrap();
        let outstanding = IncarnationDebit {
            process_digest: live.incarnation_digest_bytes().unwrap(),
            process: redo::StoredProcess::capture(&live).unwrap(),
            ..f.dead
        };
        let target = CapacityRecord {
            sequence,
            consumed,
            outstanding: Some(outstanding),
            ..before
        };
        let next = current
            .successor(Operation::Debit, target, &f.policy)
            .unwrap();
        let mut owner = f.owner();
        owner
            .anchor
            .load(&f.policy, &f.issuance, owner.budget.deadline, &|steps| {
                owner.budget.pay(steps)
            })
            .unwrap();
        owner
            .anchor
            .publish(
                next,
                &f.policy,
                &f.issuance,
                owner.budget.deadline,
                &|steps| owner.budget.pay(steps),
            )
            .unwrap();
        owner
            .account
            .apply_redo(
                owner.anchor.current().unwrap(),
                owner.budget.deadline,
                &|steps| owner.budget.pay(steps),
            )
            .unwrap();
        drop(owner);
        let error = f.run(None).unwrap_err();
        assert!(error.to_string().contains("remains live"), "{error:#}");
        assert!(!f.document_path().exists());
        assert_eq!(f.current().0.epoch(), 1);
    }

    #[test]
    fn more_than_twenty_reissuances_never_exhaust_and_keep_recovery_at_zero() {
        let f = Fixture::new();
        for expected in 2..=25 {
            f.debit_dead_incarnation();
            let report = f.run(None).unwrap();
            assert_eq!(
                (report.from_epoch, report.to_epoch, report.already_applied),
                (expected - 1, expected, false)
            );
            let (current, bytes) = f.current();
            assert_eq!(current.epoch(), expected);
            assert_eq!(current.recovery_attempts(), 0);
            assert_eq!(current.target().consumed, genesis_consumed());
            assert_eq!(current.target().sequence, expected - 1);
            assert_eq!(bytes, encode(current.target()).unwrap());
        }
        let evidence = f.evidence();
        assert_eq!(evidence.len(), 24);
        assert_eq!(evidence.last().unwrap().to_epoch, 25);
    }

    #[test]
    fn an_unrecordable_actor_refuses_before_any_write() {
        // The refusal ControllerAccount::current() gives for root, supplied
        // directly: the test process is not root.
        let refused = || {
            Err::<lillux::process_control::ControllerAccount, _>(
                "controller account must not be root".to_owned(),
            )
        };
        let f = Fixture::new();
        f.debit_dead_incarnation();
        let (debit, debit_bytes) = f.current();
        let mut owner = f.owner();
        let error = owner
            .reissue(
                &f.identity(),
                refused(),
                "controls",
                "ryeos node capacity reissue",
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("refused before any write"),
            "{error:#}"
        );
        assert!(error.to_string().contains("must not be root"), "{error:#}");
        assert!(!f.document_path().exists());
        assert_eq!(f.current(), (debit, debit_bytes));
        assert!(f.evidence().is_empty());

        // A rerun owing only its evidence line is refused the same way, and
        // still completes once run as a recordable account.
        assert!(f.run(Some(Step::Account)).is_err());
        let (opened, opened_bytes) = f.current();
        let mut owner = f.owner();
        let error = owner
            .reissue(
                &f.identity(),
                refused(),
                "controls",
                "ryeos node capacity reissue",
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("refused before any write"),
            "{error:#}"
        );
        assert_eq!(f.current(), (opened, opened_bytes));
        assert!(f.evidence().is_empty());
        assert!(f.run(None).unwrap().already_applied);
        assert_eq!(f.evidence().len(), 1);
    }

    #[test]
    fn an_evidence_log_writable_by_others_is_neither_read_nor_extended() {
        let f = Fixture::new();
        f.run(None).unwrap();
        let log = f._temporary.path().join("state").join(REISSUE_EVIDENCE_LOG);
        let recorded = std::fs::read(&log).unwrap();
        lillux::set_open_regular_file_mode(&std::fs::File::open(&log).unwrap(), 0o666).unwrap();
        let error = read_evidence(&f.state).unwrap_err();
        assert!(error.to_string().contains("evidence log"), "{error:#}");
        f.debit_dead_incarnation();
        let (debit, debit_bytes) = f.current();
        let document = std::fs::read(f.document_path()).unwrap();
        let error = f.run(None).unwrap_err();
        assert!(format!("{error:#}").contains("evidence log"), "{error:#}");
        assert_eq!(std::fs::read(&log).unwrap(), recorded);
        assert_eq!(
            f.current(),
            (debit, debit_bytes),
            "a refused log opens no epoch"
        );
        assert_eq!(std::fs::read(f.document_path()).unwrap(), document);
        let entry = CapacityReissueEvidence {
            document_digest: hex_digest(&[1; 32]),
            from_epoch: 9,
            to_epoch: 10,
            settled_incarnation: None,
            recorded_at: "2026-10-14T00:00:00Z".to_owned(),
            actor: lillux::process_control::ControllerAccount::current().unwrap(),
            command: "x".to_owned(),
        };
        assert!(
            format!("{:#}", append_evidence(&f.state, &entry).unwrap_err())
                .contains("evidence log")
        );
        assert_eq!(std::fs::read(&log).unwrap(), recorded);
    }

    #[test]
    fn evidence_log_drops_oldest_whole_lines_at_its_bound() {
        let f = Fixture::new();
        let entry = |n: u64| CapacityReissueEvidence {
            document_digest: hex_digest(&[n as u8; 32]),
            from_epoch: n,
            to_epoch: n + 1,
            settled_incarnation: None,
            recorded_at: "2026-10-14T00:00:00Z".to_owned(),
            actor: lillux::process_control::ControllerAccount::current().unwrap(),
            command: "x".repeat(4000),
        };
        let line_len = serde_json::to_vec(&entry(0)).unwrap().len() + 1;
        let fits = REISSUE_EVIDENCE_LOG_BOUND / line_len;
        for n in 0..(fits as u64 + 3) {
            append_evidence(&f.state, &entry(n)).unwrap();
        }
        let recorded = f.evidence();
        assert_eq!(recorded.len(), fits);
        assert_eq!(recorded[0].from_epoch, 3);
        assert_eq!(recorded.last().unwrap().from_epoch, fits as u64 + 2);
        assert!(
            std::fs::metadata(f._temporary.path().join("state").join(REISSUE_EVIDENCE_LOG))
                .unwrap()
                .len()
                <= REISSUE_EVIDENCE_LOG_BOUND as u64
        );
    }

    // ── Torn account and anchor ──────────────────────────────────────────

    use super::super::repair::{CapacityRepairEvidence, REPAIR_EVIDENCE_LOG};
    use super::super::torn::{CapacityCure, CapacityFile, CapacityRecordTorn, TornDefect};

    impl Fixture {
        fn account_path(&self) -> std::path::PathBuf {
            self._temporary.path().join("native-capacity/account")
        }

        fn anchor_path(&self) -> std::path::PathBuf {
            self._temporary.path().join("native-capacity/anchor")
        }

        fn mutate(path: &std::path::Path, change: impl FnOnce(&mut Vec<u8>)) {
            let mut bytes = std::fs::read(path).unwrap();
            change(&mut bytes);
            std::fs::write(path, bytes).unwrap();
        }

        fn repair(&self, fail_after: Option<Step>) -> Result<super::super::repair::RepairReport> {
            let mut owner = self.owner();
            owner.fail_after.set(fail_after);
            owner.repair_account(
                &self.policy,
                lillux::process_control::ControllerAccount::current(),
                "controls",
                "ryeos node capacity repair-account",
            )
        }

        fn repairs(&self) -> Vec<CapacityRepairEvidence> {
            super::super::reanchor::read_log(&self.state, REPAIR_EVIDENCE_LOG, "account repair")
                .unwrap()
        }

        /// The account image a debit leaves after its redo, and the image
        /// before it, for tears that mix the two.
        fn debited_images(&self) -> ([u8; RECORD_BYTES], [u8; RECORD_BYTES]) {
            let (_, genesis) = self.current();
            self.debit_dead_incarnation();
            let (_, debited) = self.current();
            (genesis, debited)
        }
    }

    fn torn_of(error: &anyhow::Error) -> CapacityRecordTorn {
        *error
            .downcast_ref::<CapacityRecordTorn>()
            .unwrap_or_else(|| panic!("not a typed torn refusal: {error:#}"))
    }

    fn load_error(f: &Fixture) -> anyhow::Error {
        let mut owner = f.owner();
        owner.load_anchor(&f.policy).unwrap_err()
    }

    #[test]
    fn every_torn_anchor_shape_is_typed_and_names_reissue_while_the_account_is_sound() {
        let shapes: [(&str, TornDefect, fn(&mut Vec<u8>)); 3] = [
            ("checksum", TornDefect::ChecksumMismatch, |bytes| {
                bytes[2000] ^= 0xff
            }),
            ("marker", TornDefect::ForeignFormat, |bytes| {
                bytes[..8].fill(0)
            }),
            ("size", TornDefect::WrongSize, |bytes| bytes.truncate(5000)),
        ];
        for (name, defect, tear) in shapes {
            let f = Fixture::new();
            Fixture::mutate(&f.anchor_path(), tear);
            let torn = torn_of(&load_error(&f));
            assert_eq!(
                (torn.file, torn.defect, torn.cure),
                (
                    CapacityFile::Anchor,
                    defect,
                    CapacityCure::ReissueFromAccount
                ),
                "{name}"
            );
            let text = load_error(&f).to_string();
            assert!(text.contains("capacity reissue"), "{name}: {text}");
            assert!(!text.contains("repair-account"), "{name}: {text}");
        }
    }

    #[test]
    fn a_torn_anchor_holds_when_the_account_is_torn_or_policy_forbids_reissue() {
        // Both files torn: nothing derives the record.
        let f = Fixture::new();
        Fixture::mutate(&f.anchor_path(), |bytes| bytes[2000] ^= 0xff);
        Fixture::mutate(&f.account_path(), |bytes| bytes[40] ^= 0xff);
        let torn = torn_of(&load_error(&f));
        assert_eq!(torn.cure, CapacityCure::Hold);
        assert!(torn.to_string().contains("restore"), "{torn}");
        let error = f.run(None).unwrap_err();
        assert_eq!(torn_of(&error).cure, CapacityCure::Hold);
        assert!(!f.document_path().exists());

        // Account sound but the signed policy forbids re-issuance.
        let mut f = Fixture::new();
        Fixture::mutate(&f.anchor_path(), |bytes| bytes[2000] ^= 0xff);
        f.policy.renewal.operator_reissue = false;
        assert_eq!(torn_of(&load_error(&f)).cure, CapacityCure::Hold);
        assert_eq!(torn_of(&f.run(None).unwrap_err()).cure, CapacityCure::Hold);
        assert!(!f.document_path().exists());
    }

    #[test]
    fn an_anchor_of_another_issuance_is_not_a_tear_and_offers_no_cure() {
        let f = Fixture::new();
        // The issuance digest sits at bytes 13..45; the checksum is remade so
        // the file is a whole, valid record of another node.
        Fixture::mutate(&f.anchor_path(), |bytes| {
            bytes[13] ^= 0xff;
            let checksum: [u8; 32] =
                sha2::Sha256::digest(&bytes[..redo::ANCHOR_PAYLOAD_BYTES]).into();
            bytes[redo::ANCHOR_PAYLOAD_BYTES..].copy_from_slice(&checksum);
        });
        let error = load_error(&f);
        assert!(
            error.downcast_ref::<CapacityRecordTorn>().is_none(),
            "{error:#}"
        );
        assert!(error.to_string().contains("another issuance"), "{error:#}");
        let error = f.run(None).unwrap_err();
        assert!(
            error.downcast_ref::<CapacityRecordTorn>().is_none(),
            "{error:#}"
        );
        assert!(!f.document_path().exists());
    }

    #[test]
    fn every_torn_account_shape_is_typed_and_names_repair_while_the_anchor_is_sound() {
        let f = Fixture::new();
        let (genesis, debited) = f.debited_images();
        let mut sectors = genesis;
        // A power-loss mixture: the head of the old image, the tail of the new.
        sectors[2048..].copy_from_slice(&debited[2048..]);
        let mut flipped = debited;
        flipped[40] ^= 0xff;
        let mut zeroed = debited;
        zeroed[..8].fill(0);
        for (name, bytes, defect) in [
            ("sectors", sectors, TornDefect::ChecksumMismatch),
            ("flip", flipped, TornDefect::ChecksumMismatch),
            ("marker", zeroed, TornDefect::ForeignFormat),
        ] {
            std::fs::write(f.account_path(), bytes).unwrap();
            let mut owner = f.owner();
            let error = owner.require_no_outstanding_debit(&f.policy).unwrap_err();
            let torn = torn_of(&error);
            assert_eq!(
                (torn.file, torn.defect, torn.cure),
                (CapacityFile::Account, defect, CapacityCure::RepairAccount),
                "{name}"
            );
            assert!(
                error.to_string().contains("repair-account"),
                "{name}: {error}"
            );
            // Re-issuance refuses it the same way, naming the same cure.
            assert_eq!(
                torn_of(&f.run(None).unwrap_err()).cure,
                CapacityCure::RepairAccount,
                "{name}"
            );
        }
    }

    #[test]
    fn a_verifiable_account_outside_the_redo_is_never_offered_repair() {
        let f = Fixture::new();
        let (genesis, _debited) = f.debited_images();
        // A whole, valid record that is neither image of the redo: replaced,
        // not torn by a write.
        let older = {
            let mut copy = genesis;
            copy[12..20].copy_from_slice(&2u64.to_le_bytes());
            let checksum: [u8; 32] =
                sha2::Sha256::digest(&copy[..super::super::PAYLOAD_BYTES]).into();
            copy[super::super::PAYLOAD_BYTES..].copy_from_slice(&checksum);
            decode(&copy).unwrap();
            copy
        };
        std::fs::write(f.account_path(), older).unwrap();
        let error = f.repair(None).unwrap_err();
        let torn = torn_of(&error);
        assert_eq!(
            (torn.defect, torn.cure),
            (TornDefect::ReplacedRecord, CapacityCure::Hold)
        );
        assert_eq!(std::fs::read(f.account_path()).unwrap(), older);
        assert!(f.repairs().is_empty());
    }

    #[test]
    fn an_account_of_the_wrong_size_holds_at_open() {
        let f = Fixture::new();
        Fixture::mutate(&f.account_path(), |bytes| bytes.truncate(100));
        let error = StoppedCapacityAccount::open(
            f.directory.try_clone().unwrap(),
            f.account_identity,
            f.onboarding.try_clone().unwrap(),
            f.state.try_clone().unwrap(),
            f.issuance,
            &f.policy,
            f.lock.witness().unwrap(),
        )
        .err()
        .expect("a resized account is refused");
        let torn = torn_of(&error);
        assert_eq!(
            (torn.file, torn.defect, torn.cure),
            (
                CapacityFile::Account,
                TornDefect::WrongSize,
                CapacityCure::Hold
            )
        );
    }

    #[test]
    fn repair_rederives_a_torn_account_from_the_anchor_and_records_one_line() {
        let f = Fixture::new();
        let (genesis, debited) = f.debited_images();
        let mut sectors = genesis;
        sectors[2048..].copy_from_slice(&debited[2048..]);
        std::fs::write(f.account_path(), sectors).unwrap();
        let report = f.repair(None).unwrap();
        assert!(report.repaired);
        assert_eq!((report.epoch, report.sequence), (1, 1));
        assert_eq!(std::fs::read(f.account_path()).unwrap(), debited);
        let torn_digest = hex_digest(&sha2::Sha256::digest(sectors).into());
        assert_eq!(
            report.torn_account_digest.as_deref(),
            Some(torn_digest.as_str())
        );
        let evidence = f.repairs();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].torn_account_digest, torn_digest);
        assert_eq!(evidence[0].repaired_account_digest, report.account_digest);
        assert_eq!(evidence[0].command, "ryeos node capacity repair-account");
        // The anchor is untouched and the debit is left for start to settle.
        let (current, bytes) = f.current();
        assert_eq!(bytes, debited);
        assert!(current.target().outstanding.is_some());
        // A rerun finds nothing to do and writes nothing.
        let again = f.repair(None).unwrap();
        assert!(!again.repaired);
        assert_eq!(f.repairs().len(), 1);
    }

    #[test]
    fn repair_refuses_what_start_repairs_and_leaves_it_unwritten() {
        let f = Fixture::new();
        let (genesis, debited) = f.debited_images();
        // The unapplied redo: the account still holds the before image.
        std::fs::write(f.account_path(), genesis).unwrap();
        let error = f.repair(None).unwrap_err();
        assert!(error.to_string().contains("write model"), "{error:#}");
        assert_eq!(std::fs::read(f.account_path()).unwrap(), genesis);
        // A prefix tear of the write itself.
        let mut prefix = genesis;
        prefix[..300].copy_from_slice(&debited[..300]);
        std::fs::write(f.account_path(), prefix).unwrap();
        assert!(
            f.repair(None)
                .unwrap_err()
                .to_string()
                .contains("write model")
        );
        assert_eq!(std::fs::read(f.account_path()).unwrap(), prefix);
        assert!(f.repairs().is_empty());
    }

    #[test]
    fn repair_never_touches_a_torn_anchor() {
        let f = Fixture::new();
        f.debit_dead_incarnation();
        Fixture::mutate(&f.anchor_path(), |bytes| bytes[2000] ^= 0xff);
        let anchor = std::fs::read(f.anchor_path()).unwrap();
        let account = std::fs::read(f.account_path()).unwrap();
        let error = f.repair(None).unwrap_err();
        assert_eq!(torn_of(&error).file, CapacityFile::Anchor);
        assert_eq!(std::fs::read(f.anchor_path()).unwrap(), anchor);
        assert_eq!(std::fs::read(f.account_path()).unwrap(), account);
        assert!(f.repairs().is_empty());
    }

    #[test]
    fn repair_interrupted_at_each_step_converges_on_one_line_per_torn_state() {
        for step in [Step::RepairEvidence, Step::RepairAccount] {
            let f = Fixture::new();
            let (genesis, debited) = f.debited_images();
            let mut sectors = genesis;
            sectors[2048..].copy_from_slice(&debited[2048..]);
            std::fs::write(f.account_path(), sectors).unwrap();
            let error = f.repair(Some(step)).unwrap_err();
            assert!(
                error.to_string().contains("injected"),
                "{step:?}: {error:#}"
            );
            assert_eq!(f.repairs().len(), 1, "{step:?}");
            let report = f.repair(None).unwrap();
            assert_eq!(report.repaired, step == Step::RepairEvidence, "{step:?}");
            assert_eq!(
                std::fs::read(f.account_path()).unwrap(),
                debited,
                "{step:?}"
            );
            assert_eq!(f.repairs().len(), 1, "{step:?}");
        }
    }

    #[test]
    fn a_repair_write_that_dies_midway_is_repaired_again() {
        let f = Fixture::new();
        let (genesis, debited) = f.debited_images();
        let mut sectors = genesis;
        sectors[2048..].copy_from_slice(&debited[2048..]);
        // The repair's own overwrite stopped after a prefix of the target,
        // over the torn bytes it was replacing.
        // Every cut lies inside the span where the two images differ.
        for cut in [1usize, 30, 150, 300] {
            let mut midway = sectors;
            midway[..cut].copy_from_slice(&debited[..cut]);
            std::fs::write(f.account_path(), midway).unwrap();
            let report = f.repair(None).unwrap();
            assert!(report.repaired, "cut {cut}");
            assert_eq!(
                std::fs::read(f.account_path()).unwrap(),
                debited,
                "cut {cut}"
            );
        }
    }

    #[test]
    fn repair_refuses_an_unrecordable_actor_and_an_oversized_reason_before_any_write() {
        let f = Fixture::new();
        let (_, debited) = f.debited_images();
        let mut torn = debited;
        torn[40] ^= 0xff;
        std::fs::write(f.account_path(), torn).unwrap();
        let mut owner = f.owner();
        let error = owner
            .repair_account(&f.policy, Err("root".to_owned()), "x", "x")
            .unwrap_err();
        assert!(error.to_string().contains("before any write"), "{error:#}");
        let mut owner = f.owner();
        let error = owner
            .repair_account(
                &f.policy,
                lillux::process_control::ControllerAccount::current(),
                &"r".repeat(1025),
                "x",
            )
            .unwrap_err();
        assert!(error.to_string().contains("bound"), "{error:#}");
        assert_eq!(std::fs::read(f.account_path()).unwrap(), torn);
        assert!(f.repairs().is_empty());
    }

    #[test]
    fn reissue_from_the_account_record_opens_one_epoch_when_the_anchor_is_torn() {
        for tear in [
            (|bytes: &mut Vec<u8>| bytes[2000] ^= 0xff) as fn(&mut Vec<u8>),
            |bytes| bytes[..8].fill(0),
            |bytes| bytes.truncate(5000),
        ] {
            let f = Fixture::new();
            f.debit_dead_incarnation();
            let (debit, debit_bytes) = f.current();
            Fixture::mutate(&f.anchor_path(), tear);
            let report = f.run(None).unwrap();
            assert!(report.from_account_record && !report.already_applied);
            assert_eq!((report.from_epoch, report.to_epoch), (1, 2));
            let expected = hex_digest(&f.dead.process_digest);
            assert_eq!(
                report.settled_incarnation.as_deref(),
                Some(expected.as_str())
            );
            let (current, bytes) = f.current();
            assert_eq!(current.operation(), Operation::OperatorReissue);
            assert_eq!(current.epoch(), 2);
            assert_eq!(current.recovery_attempts(), 0);
            assert_eq!(current.target().sequence, debit.target().sequence);
            assert_eq!(current.target().consumed, genesis_consumed());
            assert!(current.target().outstanding.is_none());
            assert_eq!(bytes, encode(current.target()).unwrap());
            // The document names the account image it renewed from.
            let document = std::fs::read(f.document_path()).unwrap();
            let body = verify_reissuance_document(
                &document,
                &f.identity().lineage,
                &f.identity().node_fingerprint,
                &f.issuance,
            )
            .unwrap();
            assert_eq!(
                body.anchor_digest,
                hex_digest(&sha2::Sha256::digest(debit_bytes).into())
            );
            let evidence = f.evidence();
            assert_eq!(evidence.len(), 1);
            assert!(evidence[0].command.contains("anchor was torn"));
            // Rerun is the ordinary already-applied path.
            let again = f.run(None).unwrap();
            assert!(again.already_applied && !again.from_account_record);
            assert_eq!(f.evidence().len(), 1);
        }
    }

    #[test]
    fn reissue_from_the_account_record_converges_after_an_interruption_at_each_step() {
        for step in [Step::Document, Step::Anchor, Step::Account] {
            let f = Fixture::new();
            f.debit_dead_incarnation();
            Fixture::mutate(&f.anchor_path(), |bytes| bytes[2000] ^= 0xff);
            let error = f.run(Some(step)).unwrap_err();
            assert!(
                error.to_string().contains("injected"),
                "{step:?}: {error:#}"
            );
            let report = f.run(None).unwrap();
            assert_eq!((report.from_epoch, report.to_epoch), (1, 2), "{step:?}");
            assert_eq!(report.already_applied, step != Step::Document, "{step:?}");
            let (current, bytes) = f.current();
            assert_eq!(current.epoch(), 2, "{step:?}");
            assert_eq!(bytes, encode(current.target()).unwrap(), "{step:?}");
            assert_eq!(f.evidence().len(), 1, "{step:?}");
            let digest: [u8; 32] =
                sha2::Sha256::digest(std::fs::read(f.document_path()).unwrap()).into();
            assert_eq!(digest, current.authorization(), "{step:?}");
            assert!(f.run(None).unwrap().already_applied);
            assert_eq!(f.evidence().len(), 1, "{step:?}");
        }
    }

    #[test]
    fn reissue_from_the_account_record_keeps_the_epoch_bound_and_the_liveness_proof() {
        let mut f = Fixture::new();
        Fixture::mutate(&f.anchor_path(), |bytes| bytes[2000] ^= 0xff);
        f.policy.renewal.maximum_epochs = Some(1);
        let error = f.run(None).unwrap_err();
        assert!(error.to_string().contains("epoch bound"), "{error:#}");
        assert!(!f.document_path().exists());
        // The torn anchor is exactly as it was.
        assert_eq!(torn_of(&load_error(&f)).file, CapacityFile::Anchor);
    }
}
