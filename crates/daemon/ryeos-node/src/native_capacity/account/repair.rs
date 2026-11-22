//! Stopped-node repair of a torn capacity account.
//!
//! The anchor is published before the account is written, so while the anchor
//! verifies, the image the account must hold is already admitted: the anchor's
//! target. A host crash or a bad device can still leave the account in bytes
//! the redo's write model does not cover (a non-prefix mixture of the two
//! images, a flipped sector). Start refuses such an account by design, and
//! this command is the one offline way to mend it: it overwrites exactly the
//! torn bytes with the anchor's target and records that it did.
//!
//! What it never does:
//! - rewrite an account that verifies, or one the redo's own write model
//!   covers (start repairs those and charges the recovery attempt it owes);
//! - rebuild or touch the anchor (a torn anchor is mended only by an operator
//!   re-issuance that renews from the account record);
//! - settle an outstanding incarnation debit (the next start, or a
//!   re-issuance, classifies it under the state lock).
//!
//! The evidence line is appended before the account is written, keyed by the
//! anchor and the torn bytes it mends, so an interrupted run converges on
//! rerun with exactly one line per torn state.

use std::path::Path;

use anyhow::{Result, ensure};
use lillux::PinnedDirectory;
use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;
use ryeos_node_runtime::state_lock::StateLock;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::reanchor::{append_log, read_log, require_owned_log};
use super::reissue::{Step, StoppedCapacityAccount};
use super::{NativeCapacityAccountBinding, RECORD_BYTES, encode, hex_digest};
use crate::apply_authority::DocumentSigner;

/// Evidence of every account repair an operator ran, oldest first.
pub const REPAIR_EVIDENCE_LOG: &str = "native-capacity-repairs.jsonl";
const REASON_BOUND_CHARS: usize = 1024;

/// One account repair on a stopped node.
pub struct RepairRequest<'a> {
    pub app_root: &'a Path,
    /// Held by the command for the whole operation: a running daemon holds
    /// the same lock, so acquiring it is the proof that the node is stopped.
    pub state_lock: &'a StateLock,
    /// Names the operator the command acts for. The repair signs no document:
    /// it grants nothing the published anchor had not already admitted.
    pub signer: DocumentSigner<'a>,
    pub reason: &'a str,
    /// The command name recorded in the evidence log.
    pub command: &'a str,
}

/// What the command changed, or found unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairReport {
    pub epoch: u64,
    pub sequence: u64,
    /// False when the account already held the anchor's target image.
    pub repaired: bool,
    /// Digest of the torn bytes that were overwritten.
    pub torn_account_digest: Option<String>,
    /// Digest of the account image now in place.
    pub account_digest: String,
}

/// One line of the bounded evidence log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityRepairEvidence {
    pub epoch: u64,
    pub sequence: u64,
    /// Digest of the sound anchor the account was re-derived from.
    pub anchor_digest: String,
    pub torn_account_digest: String,
    pub repaired_account_digest: String,
    pub reason: String,
    pub recorded_at: String,
    pub actor: lillux::process_control::ControllerAccount,
    pub command: String,
}

/// Re-derive a stopped node's torn capacity account from its anchor.
/// Rerunning after any interruption converges on one repair.
pub fn repair_capacity_account(request: RepairRequest<'_>) -> Result<RepairReport> {
    let RepairRequest {
        app_root,
        state_lock,
        signer,
        reason,
        command,
    } = request;
    state_lock.ensure_protects_app_root(app_root)?;
    let report = crate::init::verify_init_completion(app_root)?
        .ok_or_else(|| anyhow::anyhow!("account repair requires completed initialization"))?;
    let operator_fingerprint = signer.operator_fingerprint().ok_or_else(|| {
        anyhow::anyhow!(
            "account repair needs the operator's consent: run it under the operator key or the \
             operator's signed request"
        )
    })?;
    ensure!(
        operator_fingerprint == report.operator_fingerprint,
        "operator key differs from the one that initialized this node"
    );
    super::OperatorLineage::load(app_root)?.require_member(&operator_fingerprint)?;
    let binding: NativeCapacityAccountBinding = report.capacity_account;
    let policy = binding.policy.clone();
    binding.require_installed(app_root, &report.node_fingerprint, &policy)?;
    let directory = PinnedDirectory::open(&app_root.join(".ai/node/native-capacity"))?
        .ok_or_else(|| anyhow::anyhow!("capacity directory absent"))?;
    ensure!(
        directory.identity()? == binding.account_directory_identity,
        "capacity directory differs from original issuance"
    );
    let onboarding = PinnedDirectory::open(&app_root.join(".ai/config/onboarding"))?
        .ok_or_else(|| anyhow::anyhow!("operator onboarding absent"))?;
    let state = PinnedDirectory::open(&app_root.join(".ai/state"))?
        .ok_or_else(|| anyhow::anyhow!("capacity runtime absent"))?;
    let mut owner = StoppedCapacityAccount::open(
        directory,
        binding.account_identity,
        onboarding,
        state,
        binding.issuance_digest()?,
        &policy,
        state_lock.witness()?,
    )?;
    state_lock.ensure_protects_app_root(app_root)?;
    owner.repair_account(
        &policy,
        lillux::process_control::ControllerAccount::current(),
        reason,
        command,
    )
}

/// Every recorded repair, oldest first. An unreadable line is an error.
pub fn read_capacity_repairs(state_dir: &Path) -> Result<Vec<CapacityRepairEvidence>> {
    let Some(directory) = PinnedDirectory::open(state_dir)? else {
        return Ok(Vec::new());
    };
    read_log(&directory, REPAIR_EVIDENCE_LOG, "account repair")
}

fn digest_of(bytes: &[u8]) -> String {
    hex_digest(&sha2::Sha256::digest(bytes).into())
}

impl StoppedCapacityAccount<'_> {
    /// The repair itself, under the state lock `open` witnessed.
    pub(super) fn repair_account(
        &mut self,
        policy: &NodeNativeCapacityPolicy,
        actor: std::result::Result<lillux::process_control::ControllerAccount, String>,
        reason: &str,
        command: &str,
    ) -> Result<RepairReport> {
        let actor = actor.map_err(|error| {
            anyhow::anyhow!(
                "account repair refused before any write: the acting account cannot be recorded as its evidence actor ({error}); run it as the node account"
            )
        })?;
        ensure!(
            reason.chars().count() <= REASON_BOUND_CHARS,
            "account repair reason exceeds its bound"
        );
        // The log this run must extend is checked before the first write.
        if let Some(log) = self
            .state
            .open_pinned_regular(std::ffi::OsStr::new(REPAIR_EVIDENCE_LOG), false)?
        {
            require_owned_log(&log, REPAIR_EVIDENCE_LOG, "account repair")?;
        }
        // A torn anchor refuses here with its own cure; this verb never
        // touches it.
        self.load_anchor(policy)?;
        let current = *self.anchor.current()?;
        let target = encode(current.target())?;
        let bytes: [u8; RECORD_BYTES] = self
            .account
            .read_bytes(self.budget.deadline, &|steps| self.budget.pay(steps))?;
        let report = |repaired: bool, torn: Option<String>| RepairReport {
            epoch: current.epoch(),
            sequence: current.target().sequence,
            repaired,
            torn_account_digest: torn,
            account_digest: digest_of(&target),
        };
        if bytes == target {
            return Ok(report(false, None));
        }
        ensure!(
            !current.accepts_account_bytes(&bytes)?,
            "capacity account is within its redo's write model; the next start applies the redo \
             and charges the recovery attempt it owes, so this command does not"
        );
        if super::cut_retirement::account_awaits_cut(&bytes, &target) {
            anyhow::bail!(super::cut_retirement::PRE_CUT_CURE);
        }
        // A record that verifies is never overwritten here, whatever it says.
        if super::decode(&bytes).is_ok() {
            return Err(super::torn::account_outside_redo(&bytes));
        }

        let torn_digest = digest_of(&bytes);
        let evidence = CapacityRepairEvidence {
            epoch: current.epoch(),
            sequence: current.target().sequence,
            anchor_digest: digest_of(&current.encode(policy)?),
            torn_account_digest: torn_digest.clone(),
            repaired_account_digest: digest_of(&target),
            reason: reason.to_owned(),
            recorded_at: lillux::time::iso8601_now(),
            actor,
            command: command.to_owned(),
        };
        self.append_repair_evidence_once(&evidence)?;
        self.crash_after(Step::RepairEvidence)?;
        self.account
            .overwrite_torn(&bytes, &target, self.budget.deadline, &|steps| {
                self.budget.pay(steps)
            })?;
        self.crash_after(Step::RepairAccount)?;
        Ok(report(true, Some(torn_digest)))
    }

    /// A rerun after an interruption between the evidence and the account
    /// write meets the same torn bytes and owes no second line for them.
    fn append_repair_evidence_once(&self, evidence: &CapacityRepairEvidence) -> Result<()> {
        let recorded: Vec<CapacityRepairEvidence> =
            read_log(&self.state, REPAIR_EVIDENCE_LOG, "account repair")?;
        if recorded.iter().any(|entry| {
            entry.anchor_digest == evidence.anchor_digest
                && entry.torn_account_digest == evidence.torn_account_digest
        }) {
            return Ok(());
        }
        append_log(&self.state, REPAIR_EVIDENCE_LOG, "account repair", evidence)
    }
}
