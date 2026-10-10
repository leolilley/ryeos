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
        let temporary = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        // Production creates this directory owner-private; the temp default
        // mode is not part of that contract.
        directory.tighten_owner_private_directory().unwrap();
        let (policy, genesis, target) = redo::controls::fixture();
        let debit = genesis.successor(Operation::Debit, target, &policy).unwrap();
        let account = directory.atomic_create_pinned_regular(OsStr::new("account"),
            &encode(genesis.target()).unwrap(), 0o600).unwrap().unwrap();
        directory.atomic_create_pinned_regular(OsStr::new("anchor"),
            &genesis.encode(&policy).unwrap(), 0o600).unwrap().unwrap();
        let store = RetainedStore::open(directory.try_clone().unwrap(), account.identity().unwrap()).unwrap();
        let mut anchor = RetainedAnchor::retain(directory.try_clone().unwrap(), directory.identity().unwrap()).unwrap();
        let deadline = lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(30));
        let work = Work::new();
        anchor.load(&policy, &target.issuance_digest, deadline, &|n| work.pay(n)).unwrap();
        work.calls.set(0);
        Self { _temporary: temporary, directory, store, anchor, policy, genesis, debit, deadline, work }
    }
    fn publish(&mut self, next: AccountRedo) -> Result<()> {
        self.anchor.publish(next, &self.policy, &self.genesis.target().issuance_digest,
            self.deadline, &|n| self.work.pay(n))
    }
    fn account_bytes(&self) -> [u8; RECORD_BYTES] {
        self.store.read_bytes(self.deadline, &|n| self.work.pay(n)).unwrap()
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
        f.store.apply_redo(f.anchor.current().unwrap(), f.deadline, &|n| f.work.pay(n)).unwrap();
        assert_eq!(f.account_bytes(), encode(f.debit.target()).unwrap());
        assert_eq!(f.store.pinned.identity().unwrap(), account_identity);
        assert_eq!(f.directory.entries_no_follow_bounded(8).unwrap().len(), 2);
    }
}

#[test]
fn fresh_publisher_reconciles_exact_torn_images_in_the_issued_account_inode() {
    // Injected account writes intentionally bypass the writer protocol to
    // represent crash prefixes. They never issue an authenticated grant.
    for cut in [0, 1, 8, 52, 92, 125, 261, PROCESS_END, RECORD_BYTES - 1, RECORD_BYTES] {
        let mut f = Fixture::new();
        f.publish(f.debit).unwrap();
        let mut torn = encode(f.genesis.target()).unwrap();
        let target = encode(f.debit.target()).unwrap();
        torn[..cut].copy_from_slice(&target[..cut]);
        std::fs::write(f._temporary.path().join("account"), torn).unwrap();
        let mut reopened = RetainedAnchor::retain(f.directory.try_clone().unwrap(), f.directory.identity().unwrap()).unwrap();
        reopened.load(&f.policy, &f.debit.target().issuance_digest, f.deadline, &|n| f.work.pay(n)).unwrap();
        f.store.apply_redo(reopened.current().unwrap(), f.deadline, &|n| f.work.pay(n)).unwrap();
        assert_eq!(f.account_bytes(), target);
        assert_eq!(f.store.pinned.identity().unwrap(), f.store.identity);
    }
}

#[test]
fn pending_recovery_charge_cannot_retry_the_earlier_debit_or_refund_consumption() {
    let mut f = Fixture::new();
    f.publish(f.debit).unwrap();
    f.store.apply_redo(&f.debit, f.deadline, &|n| f.work.pay(n)).unwrap();
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
    let orphan = f.directory.atomic_create_pinned_regular(name, b"unbound stage", 0o600).unwrap().unwrap();
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
    std::fs::write(f._temporary.path().join("account"), corrupt).unwrap();
    assert!(f.store.apply_redo(&f.debit, f.deadline, &|n| f.work.pay(n)).is_err());
    assert_eq!(f.account_bytes(), corrupt);
    // Inject an inode replacement, even with exactly the valid target bytes.
    let replacement = f._temporary.path().join("foreign-account");
    let target = encode(f.debit.target()).unwrap();
    std::fs::write(&replacement, target).unwrap();
    std::fs::rename(&replacement, f._temporary.path().join("account")).unwrap();
    assert!(f.store.apply_redo(&f.debit, f.deadline, &|n| f.work.pay(n)).is_err());
    assert_eq!(std::fs::read(f._temporary.path().join("account")).unwrap(), target);
}
