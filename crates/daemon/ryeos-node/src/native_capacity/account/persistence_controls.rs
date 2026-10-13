//! Actual filesystem controls for the bounded redo publisher. The records here
//! are synthetic transition subjects, not signed admission, death evidence or
//! native-release capabilities. Full reservation/cold-start controls must also
//! exercise actual authenticated inputs and retained physical owners.

use super::*;
use super::anchor::RetainedAnchor;
use super::redo::{AccountRedo, Operation};
use std::cell::Cell;

struct Work {
    remaining: Cell<u64>,
    calls: Cell<usize>,
    fail_at: Cell<Option<usize>>,
}
impl Work {
    fn new() -> Self {
        Self { remaining: Cell::new(1_000_000), calls: Cell::new(0), fail_at: Cell::new(None) }
    }
    fn pay(&self, steps: u64) -> Result<()> {
        self.calls.set(self.calls.get() + 1);
        self.remaining.set(self.remaining.get().checked_sub(steps).context("fixture work exhausted")?);
        ensure!(self.fail_at.get() != Some(self.calls.get()), "injected retained publication refusal");
        Ok(())
    }
}

struct Fixture {
    _temporary: tempfile::TempDir,
    directory: PinnedDirectory,
    store: RetainedStore,
    anchor: RetainedAnchor,
    policy: NodeNativeCapacityPolicy,
    genesis: AccountRedo,
    debit: AccountRedo,
    deadline: lillux::time::MonotonicDeadline,
    work: Work,
}
impl Fixture {
    fn new() -> Self {
        Self::with_policy(|_| {})
    }
    fn with_policy(adjust: impl FnOnce(&mut NodeNativeCapacityPolicy)) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        // Production creates this directory owner-private; the temp default
        // mode is not part of that contract.
        directory.tighten_owner_private_directory().unwrap();
        let (mut policy, genesis, target) = redo::controls::fixture();
        adjust(&mut policy);
        let debit = genesis.successor(Operation::Debit, target, &policy).unwrap();
        let account = directory.atomic_create_pinned_regular(OsStr::new("account"),
            &encode(genesis.target()).unwrap(), 0o600, crate::NODE_TEMP_PURPOSE).unwrap().unwrap();
        directory.atomic_create_pinned_regular(OsStr::new("anchor"),
            &genesis.encode(&policy).unwrap(), 0o600, crate::NODE_TEMP_PURPOSE).unwrap().unwrap();
        let store = RetainedStore::open(directory.try_clone().unwrap(), account.identity().unwrap()).unwrap();
        let mut anchor = RetainedAnchor::retain(directory.try_clone().unwrap(), directory.identity().unwrap()).unwrap();
        let deadline = lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(30));
        let work = Work::new();
        anchor.load(&policy, &target.issuance_digest, deadline, &|n| work.pay(n)).unwrap();
        work.calls.set(0);
        Self { _temporary: temporary, directory, store, anchor, policy, genesis, debit, deadline, work }
    }
    fn issuance(&self) -> [u8; 32] { self.genesis.target().issuance_digest }
    fn publish(&mut self, next: AccountRedo) -> Result<()> {
        self.anchor.publish(next, &self.policy, &self.issuance(), self.deadline, &|n| self.work.pay(n))
    }
    fn apply(&self, redo: &AccountRedo) -> Result<()> {
        self.store.apply_redo(redo, self.deadline, &|n| self.work.pay(n))
    }
    fn account_bytes(&self) -> [u8; RECORD_BYTES] {
        self.store.read_bytes(self.deadline, &|n| self.work.pay(n)).unwrap()
    }
    /// The orderly-rollover renewal of the fixture debit: one record that
    /// settles the served incarnation and opens epoch 2.
    fn renewal(&self) -> AccountRedo {
        let before = self.debit.target();
        self.debit.renew(Operation::RolloverRenewal, redo::controls::next_epoch(before),
            rollover_authorization(&self.issuance(), before).unwrap(), &self.policy).unwrap()
    }
    /// A fresh owner over the same files, as the next start would open them.
    fn reopen(&self) -> RetainedAnchor {
        let mut reopened = RetainedAnchor::retain(self.directory.try_clone().unwrap(),
            self.directory.identity().unwrap()).unwrap();
        reopened.load(&self.policy, &self.issuance(), self.deadline, &|n| self.work.pay(n)).unwrap();
        reopened
    }
    fn write_account_unchecked(&self, bytes: &[u8; RECORD_BYTES]) {
        // Injected account writes intentionally bypass the writer protocol to
        // represent crash prefixes. They never issue an authenticated grant.
        std::fs::write(self._temporary.path().join("account"), bytes).unwrap();
    }
}

#[test]
fn publication_refusals_retain_one_target_and_do_not_mutate_account() {
    // Includes refusal in the lower validation callback (old inode restored),
    // after lower commit, at readback/sync, and after cached completion. Lower
    // atomic_fs controls separately cover its marker/rename crash phases.
    for fail_at in 1..=8 {
        let mut f = Fixture::new();
        let original_anchor = f.directory.open_pinned_regular(OsStr::new("anchor"), false)
            .unwrap().unwrap().identity().unwrap();
        let account_identity = f.store.identity;
        f.work.fail_at.set(Some(fail_at));
        assert!(f.publish(f.debit).is_err(), "unreached publication boundary {fail_at}");
        let spent = f.work.remaining.get();
        f.work.fail_at.set(None);
        assert_eq!(f.account_bytes(), encode(f.genesis.target()).unwrap());
        f.publish(f.debit).unwrap();
        assert!(f.work.remaining.get() < spent, "retry replenished its original counter");
        assert_eq!(f.anchor.current().unwrap(), &f.debit);
        assert_ne!(f.directory.open_pinned_regular(OsStr::new("anchor"), false)
            .unwrap().unwrap().identity().unwrap(), original_anchor);
        f.apply(f.anchor.current().unwrap()).unwrap();
        assert_eq!(f.account_bytes(), encode(f.debit.target()).unwrap());
        assert_eq!(f.store.pinned.identity().unwrap(), account_identity);
        assert_eq!(f.directory.entries_no_follow_bounded(8).unwrap().len(), 2);
    }
}

#[test]
fn fresh_publisher_reconciles_exact_torn_images_in_the_issued_account_inode() {
    for cut in [0, 1, 8, 12, 20, 60, 100, 133, 269, PROCESS_END, RECORD_BYTES - 1, RECORD_BYTES] {
        let mut f = Fixture::new();
        f.publish(f.debit).unwrap();
        let mut torn = encode(f.genesis.target()).unwrap();
        let target = encode(f.debit.target()).unwrap();
        torn[..cut].copy_from_slice(&target[..cut]);
        f.write_account_unchecked(&torn);
        let reopened = f.reopen();
        f.apply(reopened.current().unwrap()).unwrap();
        assert_eq!(f.account_bytes(), target);
        assert_eq!(f.store.pinned.identity().unwrap(), f.store.identity);
    }
}

#[test]
fn renewal_refused_at_every_publication_boundary_ends_in_exactly_one_epoch() {
    // Boundary 1 is the predecessor check before any mutation; 2-4 precede the
    // lower rename; 5 is after the rename and before fsync; 6-8 are readback,
    // fsync and cached completion. A crash at any of them leaves the next
    // start either the debit (settled dead, epoch 1) or the renewal (repaired
    // under the new epoch's first recovery attempt, epoch 2); never both.
    let process = lillux::process_control::capture_current_process_identity().unwrap();
    let mut reached = [false; 2];
    for fail_at in 1..=8 {
        let mut f = Fixture::new();
        f.publish(f.debit).unwrap();
        f.apply(&f.debit).unwrap();
        let renewal = f.renewal();
        assert_eq!(renewal.settled_incarnation(),
            Some(f.debit.target().outstanding.unwrap().process_digest));
        f.work.calls.set(0);
        f.work.fail_at.set(Some(fail_at));
        assert!(f.publish(renewal).is_err(), "unreached publication boundary {fail_at}");
        f.work.fail_at.set(None);
        assert_eq!(f.account_bytes(), encode(f.debit.target()).unwrap(),
            "account written before the anchor at boundary {fail_at}");

        let mut reopened = f.reopen();
        let current = *reopened.current().unwrap();
        let charged = current.charge_recovery(&process, &f.policy).unwrap();
        reopened.publish(charged, &f.policy, &f.issuance(), f.deadline, &|n| f.work.pay(n)).unwrap();
        f.apply(reopened.current().unwrap()).unwrap();
        let settled = if current == f.debit {
            reached[0] = true;
            // The renewal never became durable: the served incarnation is
            // settled dead in the same epoch, as any crashed incarnation is.
            let target = CapacityRecord { outstanding: None, ..*charged.target() };
            let dead = charged.successor(Operation::DeadIncarnationSettlement, target, &f.policy).unwrap();
            reopened.publish(dead, &f.policy, &f.issuance(), f.deadline, &|n| f.work.pay(n)).unwrap();
            f.apply(reopened.current().unwrap()).unwrap();
            assert_eq!(dead.epoch(), 1);
            dead
        } else {
            reached[1] = true;
            assert_eq!(current, renewal, "boundary {fail_at} left neither image");
            assert_eq!(charged.epoch(), 2);
            assert_eq!(charged.recovery_attempts(), 1, "repair is charged to the opened epoch");
            charged
        };
        assert_eq!(decode(&f.account_bytes()).unwrap(), *settled.target());
        assert!(settled.target().outstanding.is_none());
        // A further start finds nothing to repair and opens no second epoch.
        let again = f.reopen();
        assert_eq!(again.current().unwrap(), &settled);
        f.apply(again.current().unwrap()).unwrap();
        assert_eq!(decode(&f.account_bytes()).unwrap().epoch, settled.epoch());
        assert!(settled.epoch() <= 2);
    }
    assert_eq!(reached, [true, true], "boundaries must cover both outcomes");
}

#[test]
fn fresh_publisher_reconciles_torn_renewal_images_including_the_epoch_bytes() {
    // Cuts inside the epoch field (12..20) leave a record that is neither
    // image; the exact-prefix rule still repairs it to the renewal target.
    // The full-length cut is the crash after a complete write: a no-op.
    for cut in [0, 1, 8, 12, 13, 19, 20, 28, 60, 100, 101, 133, 269, PROCESS_END, RECORD_BYTES - 1, RECORD_BYTES] {
        let mut f = Fixture::new();
        f.publish(f.debit).unwrap();
        f.apply(&f.debit).unwrap();
        let renewal = f.renewal();
        f.publish(renewal).unwrap();
        let mut torn = encode(f.debit.target()).unwrap();
        let target = encode(renewal.target()).unwrap();
        torn[..cut].copy_from_slice(&target[..cut]);
        f.write_account_unchecked(&torn);
        let reopened = f.reopen();
        assert_eq!(reopened.current().unwrap(), &renewal);
        assert_eq!(reopened.current().unwrap().settled_incarnation(),
            Some(f.debit.target().outstanding.unwrap().process_digest));
        f.apply(reopened.current().unwrap()).unwrap();
        assert_eq!(f.account_bytes(), target);
        let repaired = decode(&f.account_bytes()).unwrap();
        assert_eq!(repaired.epoch, 2);
        assert_eq!(repaired.sequence, f.debit.target().sequence);
        assert_eq!(repaired.consumed, genesis_consumed());
        // Applying again changes nothing: one renewal, one epoch.
        f.apply(reopened.current().unwrap()).unwrap();
        assert_eq!(f.account_bytes(), target);
        assert_eq!(f.store.pinned.identity().unwrap(), f.store.identity);
    }
}

/// Sixteen incarnations and thirty-two recovery attempts per epoch, as the
/// shipped profiles grant.
fn sixteen_incarnations(policy: &mut NodeNativeCapacityPolicy) {
    let units = policy.incarnation.units;
    policy.allowance = NativeCapacityUnits {
        storage_bytes: ACCOUNT_CONTROL_STORAGE_BYTES + 16 * units.storage_bytes,
        host_control_storage_bytes: 16 * units.host_control_storage_bytes,
        allocator_steps: 16 * units.allocator_steps,
        connection_tickets: 16 * units.connection_tickets,
        cleanup_steps: 16 * units.cleanup_steps,
    };
    policy.recovery.maximum_attempts = 32;
    policy.validate().unwrap();
}

#[test]
fn crash_loop_spends_one_epoch_then_exhausts_without_renewal() {
    // Pure record arithmetic: each turn of the loop is a debit, a crash with
    // no settlement, and the next start's recovery charge and dead-incarnation
    // settlement. Neither path renews, so the allowance ends the loop. The
    // second start begins right after a renewal publication, the one place a
    // repair is charged to a freshly opened epoch.
    let (mut policy, genesis, debited) = redo::controls::fixture();
    sixteen_incarnations(&mut policy);
    let incarnation = debited.outstanding.unwrap();
    let renewal = genesis.successor(Operation::Debit, debited, &policy).unwrap()
        .renew(Operation::RolloverRenewal, redo::controls::next_epoch(&debited),
            rollover_authorization(&[0x31; 32], &debited).unwrap(), &policy).unwrap();
    for (start, epoch) in [(genesis, 1), (renewal, 2)] {
        let mut current = start;
        if current.operation() == Operation::RolloverRenewal {
            current = current.charge_recovery(&redo::controls::process(4242), &policy).unwrap();
            assert_eq!(current.recovery_attempts(), 1);
        }
        for turn in 0..16u32 {
            let before = *current.target();
            let (sequence, consumed) = before.prepare_debit(policy.allowance, policy.incarnation.units).unwrap();
            let debit = current.successor(Operation::Debit,
                CapacityRecord { sequence, consumed, outstanding: Some(incarnation), ..before }, &policy).unwrap();
            let charged = debit.charge_recovery(&redo::controls::process(4300 + turn), &policy).unwrap();
            current = charged.successor(Operation::DeadIncarnationSettlement,
                CapacityRecord { outstanding: None, ..*charged.target() }, &policy).unwrap();
            assert_eq!(current.epoch(), epoch, "a crash loop never opens an epoch");
            assert!(current.recovery_attempts() <= 32);
        }
        let exhausted = current.target().prepare_debit(policy.allowance, policy.incarnation.units);
        assert_eq!(exhausted.unwrap_err(), CapacityDebitRefusal::AllowanceExhausted);
        assert_eq!(current.target().sequence, start.target().sequence + 16);
        assert!(current.recovery_attempts() <= 32);
        assert_eq!(current.epoch(), epoch);
    }
}

#[test]
fn orderly_rollovers_renew_epochs_without_exhausting_the_issuance() {
    let mut f = Fixture::with_policy(sixteen_incarnations);
    let incarnation = f.debit.target().outstanding.unwrap();
    for turn in 1..=24u64 {
        let before = *f.anchor.current().unwrap().target();
        assert_eq!(before.epoch, turn);
        let (sequence, consumed) = before.prepare_debit(f.policy.allowance, f.policy.incarnation.units).unwrap();
        let debit = f.anchor.current().unwrap().successor(Operation::Debit,
            CapacityRecord { sequence, consumed, outstanding: Some(incarnation), ..before }, &f.policy).unwrap();
        f.publish(debit).unwrap();
        f.apply(&debit).unwrap();
        let served = debit.target();
        let renewal = debit.renew(Operation::RolloverRenewal, redo::controls::next_epoch(served),
            rollover_authorization(&f.issuance(), served).unwrap(), &f.policy).unwrap();
        f.publish(renewal).unwrap();
        f.apply(&renewal).unwrap();
        let record = decode(&f.account_bytes()).unwrap();
        assert_eq!(record.epoch, turn + 1);
        assert_eq!(record.sequence, turn);
        assert_eq!(record.consumed, genesis_consumed());
        assert_eq!(f.anchor.current().unwrap().recovery_attempts(), 0);
        assert_eq!(summarize(f.anchor.current().unwrap(), &f.policy).unwrap().incarnations_remaining, 16);
    }
    assert_eq!(decode(&f.account_bytes()).unwrap().epoch, 25);
}

/// Sign a re-issuance body as the operator would, returning the file bytes.
fn signed_reissuance(body: &ReissuanceBody, key: &SigningKey) -> Vec<u8> {
    let canonical = lillux::canonical_json(&serde_json::to_value(body).unwrap()).unwrap();
    let signature = key.sign(canonical.as_bytes());
    serde_json::to_vec(&ReissuanceDocument {
        body: body.clone(),
        signature: format!("ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())),
    }).unwrap()
}

#[test]
fn operator_reissue_anchor_opens_only_with_its_present_verified_document() {
    // The anchor names a document by digest; the open re-reads that document
    // from the onboarding directory and refuses absent, stale, foreign or
    // tampered bytes before any account read or repair.
    let operator = SigningKey::from_bytes(&[5; 32]);
    let foreign_operator = SigningKey::from_bytes(&[6; 32]);
    let operator_fingerprint = ryeos_engine::trust::compute_fingerprint(&operator.verifying_key());
    let node_fingerprint = "e".repeat(64);
    let mut f = Fixture::new();
    f.publish(f.debit).unwrap();
    f.apply(&f.debit).unwrap();
    let settled = CapacityRecord { outstanding: None, ..*f.debit.target() };
    let release = f.debit.successor(Operation::ReleaseSettlement, settled, &f.policy).unwrap();
    f.publish(release).unwrap();
    f.apply(&release).unwrap();
    let body = ReissuanceBody {
        schema: REISSUANCE_SCHEMA.to_owned(),
        operator_fingerprint: operator_fingerprint.clone(),
        node_fingerprint: node_fingerprint.clone(),
        issuance_digest: hex_digest(&f.issuance()),
        from_epoch: 1,
        to_epoch: 2,
        anchor_digest: hex_digest(&sha2::Sha256::digest(f.anchor.current().unwrap().encode(&f.policy).unwrap()).into()),
        previous_reissuance_digest: None,
        reason: "exhausted".to_owned(),
        issued_at: "2026-10-14T00:00:00Z".to_owned(),
    };
    let document = signed_reissuance(&body, &operator);
    let reissue = release.renew(Operation::OperatorReissue, redo::controls::next_epoch(&settled),
        sha2::Sha256::digest(&document).into(), &f.policy).unwrap();
    f.publish(reissue).unwrap();
    f.apply(&reissue).unwrap();

    let onboarding_root = tempfile::tempdir().unwrap();
    let onboarding = || PinnedDirectory::open(onboarding_root.path()).unwrap().unwrap();
    let path = onboarding_root.path().join(REISSUANCE_DOCUMENT_NAME);
    let verify = |current: &AccountRedo, key: &SigningKey, fingerprint: &str| {
        verify_reissue_anchor_document(onboarding(), &key.verifying_key(), fingerprint,
            &node_fingerprint, &f.issuance(), current)
    };
    let current = *f.anchor.current().unwrap();
    assert_eq!(current.operation(), Operation::OperatorReissue);

    // Absent.
    let Err(absent) = verify(&current, &operator, &operator_fingerprint) else { panic!("absent document accepted") };
    assert!(absent.to_string().contains("has no document"));
    // Accept, and the retained input is the file that was read.
    std::fs::write(&path, &document).unwrap();
    let accepted = verify(&current, &operator, &operator_fingerprint).unwrap();
    assert_eq!(accepted.bytes, document);
    accepted.require_original().unwrap();
    // The check is only for a re-issuance anchor.
    assert!(verify(&release, &operator, &operator_fingerprint).is_err());
    // Foreign operator key or fingerprint for the same bytes.
    assert!(verify(&current, &foreign_operator, &operator_fingerprint).is_err());
    assert!(verify(&current, &operator, &"a".repeat(64)).is_err());
    // Tampered: any changed byte no longer hashes to the anchor's authorization.
    let mut tampered: ReissuanceDocument = serde_json::from_slice(&document).unwrap();
    tampered.body.reason = "tampered".to_owned();
    std::fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    let Err(stale) = verify(&current, &operator, &operator_fingerprint) else { panic!("tampered document accepted") };
    assert!(stale.to_string().contains("is stale"));
    // Stale and foreign bodies that the anchor itself names: publish an anchor
    // over each signed variant so the digest matches and the body is judged.
    for (label, edit) in [
        ("wrong epoch", Box::new(|b: &mut ReissuanceBody| { b.from_epoch = 2; b.to_epoch = 3; }) as Box<dyn Fn(&mut ReissuanceBody)>),
        ("wrong node", Box::new(|b: &mut ReissuanceBody| b.node_fingerprint = "f".repeat(64))),
        ("wrong issuance", Box::new(|b: &mut ReissuanceBody| b.issuance_digest = "0".repeat(64))),
        ("wrong operator", Box::new(|b: &mut ReissuanceBody| b.operator_fingerprint = "a".repeat(64))),
    ] {
        let mut variant = body.clone();
        edit(&mut variant);
        let bytes = signed_reissuance(&variant, &operator);
        let named = AccountRedo::decode(&f.anchor.current().unwrap().encode(&f.policy).unwrap(),
            &f.policy, &f.issuance()).unwrap();
        let named = release.renew(Operation::OperatorReissue, *named.target(),
            sha2::Sha256::digest(&bytes).into(), &f.policy).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(verify(&named, &operator, &operator_fingerprint).is_err(), "{label}");
        // Signed by another key over an otherwise valid body.
        let foreign = signed_reissuance(&body, &foreign_operator);
        let named = release.renew(Operation::OperatorReissue, *named.target(),
            sha2::Sha256::digest(&foreign).into(), &f.policy).unwrap();
        std::fs::write(&path, &foreign).unwrap();
        assert!(verify(&named, &operator, &operator_fingerprint).is_err(), "{label} foreign signature");
    }
    // The account was never touched by any refusal.
    assert_eq!(decode(&f.account_bytes()).unwrap(), *reissue.target());
}

#[test]
fn pending_recovery_charge_cannot_retry_the_earlier_debit_or_refund_consumption() {
    let mut f = Fixture::new();
    f.publish(f.debit).unwrap();
    f.apply(&f.debit).unwrap();
    let process = lillux::process_control::capture_current_process_identity().unwrap();
    let charged = f.debit.charge_recovery(&process, &f.policy).unwrap();
    f.work.calls.set(0);
    f.work.fail_at.set(Some(5)); // Lower publication completed; owner still pending.
    assert!(f.publish(charged).is_err());
    f.work.fail_at.set(None);
    assert!(f.publish(f.debit).is_err());
    f.publish(charged).unwrap();
    let recorded = f.anchor.current().unwrap();
    assert_eq!(recorded.recovery_attempts(), 1);
    assert_eq!(recorded.target(), f.debit.target());
    assert_eq!(f.account_bytes(), encode(f.debit.target()).unwrap());
}

#[test]
fn unbound_stage_refuses_before_more_staging_and_remains_available_to_operator() {
    let mut f = Fixture::new();
    let name = OsStr::new(".secure.authoring.unbound");
    let orphan = f.directory.atomic_create_pinned_regular(
        name,
        b"unbound stage",
        0o600,
        crate::NODE_TEMP_PURPOSE,
    ).unwrap().unwrap();
    let identity = orphan.identity().unwrap();
    for _ in 0..3 {
        assert!(f.publish(f.debit).is_err());
        assert_eq!(f.directory.entries_no_follow_bounded(8).unwrap().len(), 3);
        f.directory.require_pinned_regular_entry_fixed(&orphan).unwrap();
        assert_eq!(orphan.identity().unwrap(), identity);
        assert_eq!(f.account_bytes(), encode(f.genesis.target()).unwrap());
    }
}

#[test]
fn foreign_account_bytes_and_identical_replacement_inode_are_never_repaired() {
    let mut f = Fixture::new();
    f.publish(f.debit).unwrap();
    let mut corrupt = encode(f.genesis.target()).unwrap();
    corrupt[RECORD_BYTES / 2] = 0xf7;
    f.write_account_unchecked(&corrupt);
    assert!(f.apply(&f.debit).is_err());
    assert_eq!(f.account_bytes(), corrupt);
    // Inject an inode replacement, even with exactly the valid target bytes.
    let replacement = f._temporary.path().join("foreign-account");
    let target = encode(f.debit.target()).unwrap();
    std::fs::write(&replacement, target).unwrap();
    std::fs::rename(&replacement, f._temporary.path().join("account")).unwrap();
    assert!(f.apply(&f.debit).is_err());
    assert_eq!(std::fs::read(f._temporary.path().join("account")).unwrap(), target);
}
