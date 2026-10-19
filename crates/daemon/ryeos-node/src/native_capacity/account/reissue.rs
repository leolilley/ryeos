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
use base64::Engine as _;
use lillux::PinnedDirectory;
use lillux::crypto::{Signer, SigningKey};
use lillux::time::MonotonicDeadline;
use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;
use ryeos_node_runtime::state_lock::StateLock;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::anchor::RetainedAnchor;
use super::redo::Operation;
use super::{
    CapacityRecord, NativeCapacityAccountBinding, REISSUANCE_DOCUMENT_BOUND,
    REISSUANCE_DOCUMENT_NAME, REISSUANCE_SCHEMA, ReissuanceBody, ReissuanceDocument, RetainedStore,
    encode, genesis_consumed, hex_digest, verify_reissuance_document,
};

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
    /// The operator key that signed the issuance; any other key is foreign.
    pub operator_key: &'a SigningKey,
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
        operator_key,
        reason,
        command,
    } = request;
    state_lock.ensure_protects_app_root(app_root)?;
    let report = crate::init::verify_init_completion(app_root)?
        .context("re-issuance requires completed initialization")?;
    let operator_fingerprint =
        ryeos_engine::trust::compute_fingerprint(&operator_key.verifying_key());
    ensure!(
        operator_fingerprint == report.operator_fingerprint,
        "operator key differs from the one that initialized this node"
    );
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
        operator_key,
        operator_fingerprint,
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
    operator_key: &'a SigningKey,
    operator_fingerprint: String,
    node_fingerprint: String,
    policy: NodeNativeCapacityPolicy,
}

/// Crash boundaries the controls inject after; production never sets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Document,
    Anchor,
    Account,
}

/// The record owners of a stopped node, opened under the operator's lock.
/// Work and time are bounded by the policy's recovery envelope even though
/// nothing is charged durably: an operator command is still finite work.
struct StoppedCapacityAccount {
    account: RetainedStore,
    anchor: RetainedAnchor,
    onboarding: PinnedDirectory,
    state: PinnedDirectory,
    issuance: [u8; 32],
    budget: Budget,
    fail_after: Cell<Option<Step>>,
}

/// Finite work and time for one command run; nothing here is durable.
struct Budget {
    deadline: MonotonicDeadline,
    work: Cell<u64>,
}

impl Budget {
    fn pay(&self, steps: u64) -> Result<()> {
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

impl StoppedCapacityAccount {
    fn open(
        directory: PinnedDirectory,
        account_identity: lillux::PinnedRegularFileIdentity,
        onboarding: PinnedDirectory,
        state: PinnedDirectory,
        issuance: [u8; 32],
        policy: &NodeNativeCapacityPolicy,
    ) -> Result<Self> {
        policy.validate()?;
        let directory_identity = directory.identity()?;
        let account = RetainedStore::open(directory.try_clone()?, account_identity)?;
        let anchor = RetainedAnchor::retain(directory, directory_identity)?;
        Ok(Self {
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

    fn crash_after(&self, step: Step) -> Result<()> {
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
        self.anchor
            .load(policy, &self.issuance, self.budget.deadline, &|steps| {
                self.budget.pay(steps)
            })?;
        let current = *self.anchor.current()?;
        let bytes = self
            .account
            .read_bytes(self.budget.deadline, &|steps| self.budget.pay(steps))?;
        ensure!(
            current.accepts_account_bytes(&bytes)?,
            "capacity account corruption has no permitted redo"
        );
        let existing = self.existing_document()?;

        if current.operation() == Operation::OperatorReissue {
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
                &identity.operator_key.verifying_key(),
                &identity.operator_fingerprint,
                &identity.node_fingerprint,
                &self.issuance,
            )?;
            ensure!(
                body.to_epoch == current.epoch(),
                "operator re-issuance document names another epoch"
            );
            if bytes != encode(current.target())? {
                self.account
                    .apply_redo(&current, self.budget.deadline, &|steps| {
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
            });
        }

        ensure!(
            policy.renewal.operator_reissue,
            "signed policy does not pre-authorise operator re-issuance"
        );
        // Whatever the anchor still carries must be proven dead before its
        // settlement can be folded into the renewal. Liveness is never
        // inferred from a numeric pid; only the exact incarnation decides.
        if let Some(debit) = current.target().outstanding {
            ensure!(
                debit
                    .process
                    .exact()?
                    .has_ended()
                    .map_err(anyhow::Error::msg)?,
                "outstanding native incarnation remains live or unproved; stop it first"
            );
        }
        if bytes != encode(current.target())? {
            self.account
                .apply_redo(&current, self.budget.deadline, &|steps| {
                    self.budget.pay(steps)
                })?;
        }
        let before = *current.target();
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

        let anchor_bytes = current.encode(policy)?;
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
        };
        let canonical = lillux::canonical_json(&serde_json::to_value(&body)?)?;
        let signature = identity.operator_key.sign(canonical.as_bytes());
        let document = ReissuanceDocument {
            body,
            signature: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
            ),
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
        let next = current.renew(Operation::OperatorReissue, target, document_digest, policy)?;
        self.anchor.publish(
            next,
            policy,
            &self.issuance,
            self.budget.deadline,
            &|steps| self.budget.pay(steps),
        )?;
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
            command: command.to_owned(),
        };
        append_evidence(&self.state, &evidence)?;
        Ok(ReissueReport {
            from_epoch,
            to_epoch,
            document_digest: evidence.document_digest,
            settled_incarnation: evidence.settled_incarnation,
            already_applied: false,
        })
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

#[cfg(all(test, target_os = "linux"))]
mod controls {
    //! Filesystem controls for the stopped-node owner. The records are the
    //! synthetic transition subjects of the redo controls; the operator key
    //! is deterministic. Nothing here is signed admission.

    use super::super::redo::AccountRedo;
    use super::super::{IncarnationDebit, RECORD_BYTES, decode, redo};
    use super::*;

    struct Fixture {
        _temporary: tempfile::TempDir,
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
            Self {
                _temporary: temporary,
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
                operator_key: &self.operator,
                operator_fingerprint: ryeos_engine::trust::compute_fingerprint(
                    &self.operator.verifying_key(),
                ),
                node_fingerprint: "c".repeat(64),
                policy: self.policy.clone(),
            }
        }

        fn owner(&self) -> StoppedCapacityAccount {
            StoppedCapacityAccount::open(
                self.directory.try_clone().unwrap(),
                self.account_identity,
                self.onboarding.try_clone().unwrap(),
                self.state.try_clone().unwrap(),
                self.issuance,
                &self.policy,
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
            &f.operator.verifying_key(),
            &f.identity().operator_fingerprint,
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
            &f.operator.verifying_key(),
            &f.identity().operator_fingerprint,
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
        };
        let canonical = lillux::canonical_json(&serde_json::to_value(&body).unwrap()).unwrap();
        let signature = foreign.sign(canonical.as_bytes());
        let document = serde_json::to_vec(&ReissuanceDocument {
            body,
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
}
