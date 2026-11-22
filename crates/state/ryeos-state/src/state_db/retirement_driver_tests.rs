//! The retirement driver: its happy path, every crash cut between its durable
//! steps, the refusal to cancel or write a fenced chain, and idempotent
//! re-drive.

use std::cell::Cell;
use std::collections::BTreeSet;

use super::retirement_driver::{chain_tip, read_chain_state};
use super::*;
use crate::objects::ThreadStatus;
use crate::objects::thread_event::NewEvent;
use crate::objects::thread_snapshot::{
    CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryPolicy, ThreadHistoryRetention, ThreadSnapshotBuilder,
};
use crate::recovery::{CasMutationGuard, EvidenceValidator, RetiredChainFence};
use crate::signer::TestSigner;
use crate::test_support::TestState;

const OWNER: &str = "T-owner";
const PEER: &str = "T-peer";

fn open() -> (tempfile::TempDir, TestState) {
    let dir = tempfile::tempdir().unwrap();
    let signer = TestSigner::default();
    let mut trust = TrustStore::new();
    trust.insert(signer.fingerprint().to_string(), signer.verifying_key());
    let db = TestState::open(dir.path(), Arc::new(trust)).unwrap();
    (dir, db)
}

fn event(id: &str, label: &str) -> ThreadEvent {
    NewEvent::new(id, id, "test_event")
        .payload(serde_json::json!({ "label": label }))
        .build()
}

fn create(db: &StateDb, id: &str) {
    let signer = TestSigner::default();
    let item_ref = "directive:system/test";
    let created = ThreadSnapshotBuilder::new(id, id, "directive", item_ref, "directive-runtime")
        .captured_history_policy(Some(CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::Durable,
            canonical_item_ref: item_ref.to_string(),
            item_content_hash: "11".repeat(32),
            item_signer_fingerprint: Some("22".repeat(32)),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: "33".repeat(32),
            resolved_from: CapturedPolicyProvenance::NodeDefault {
                node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
            },
        }))
        .build();
    db.create_chain(id, created.clone(), &signer).unwrap();
    let mut running = created;
    running.status = ThreadStatus::Running;
    running.started_at = Some(lillux::time::iso8601_now());
    db.append_events(
        id,
        id,
        vec![event(id, "start"), event(id, "second")],
        vec![SnapshotUpdate {
            thread_id: id.into(),
            new_snapshot: running,
        }],
        &signer,
    )
    .unwrap();
}

fn guard(db: &StateDb) -> CasMutationGuard {
    db.pinned_authority()
        .unwrap()
        .acquire_shared_guard()
        .unwrap()
}

/// Both chains, each with two events.
fn seeded() -> (tempfile::TempDir, TestState) {
    let (dir, db) = open();
    create(&db, OWNER);
    create(&db, PEER);
    (dir, db)
}

fn live_event_hashes(db: &StateDb, id: &str) -> (String, String) {
    let cas = db.pinned_cas().unwrap();
    let lock = db.acquire_chain_lock(id).unwrap();
    let (_, state) = read_chain_state(&cas, db.trust_store.as_ref(), &lock, id)
        .unwrap()
        .unwrap();
    let tip = chain_tip(&cas, &state).unwrap().unwrap();
    (tip.event_hash, state.last_event_hash.unwrap())
}

/// One event of the owner and one of the peer, both filed under the peer as
/// counterpart: an owner event retained for the peer's sake, and the peer's
/// own event retained for the owner's.
fn evidence(db: &StateDb) -> Vec<RetirementEvidenceRef> {
    vec![
        RetirementEvidenceRef {
            counterpart_root: PEER.to_string(),
            event_hash: live_event_hashes(db, OWNER).0,
        },
        RetirementEvidenceRef {
            counterpart_root: PEER.to_string(),
            event_hash: live_event_hashes(db, PEER).0,
        },
    ]
}

/// Run the driver for the owner with the peer as its evidence counterpart.
fn retire(
    db: &StateDb,
    validator: EvidenceValidator<'_>,
    cleanup: &dyn Fn() -> anyhow::Result<()>,
) -> anyhow::Result<ChainRetirementReport> {
    // A re-drive after the head is gone has no live chain to name evidence in.
    let evidence = if head_present(db, OWNER) {
        evidence(db)
    } else {
        Vec::new()
    };
    let guard = guard(db);
    let peer = db.acquire_chain_lock(PEER)?;
    let owner = db.acquire_chain_lock(OWNER)?;
    db.retire_chain(
        OWNER,
        &owner,
        &guard,
        ChainRetirement {
            counterpart_roots: BTreeSet::new(),
            evidence,
            counterpart_locks: &[&peer],
            validator,
            runtime_cleanup: cleanup,
        },
    )
}

fn accept(_: &ThreadEvent) -> anyhow::Result<()> {
    Ok(())
}

fn head_present(db: &StateDb, id: &str) -> bool {
    let lock = db.acquire_chain_lock(id).unwrap();
    lock.read_verified_head(db.trust_store.as_ref())
        .unwrap()
        .is_some()
}

fn fence(db: &StateDb, id: &str) -> RetiredChainFence {
    db.recovery.retired_chain_fence(id).unwrap()
}

/// Cut after the fence is published: manifest stored and fenced, nothing else.
fn cut_after_fence(db: &StateDb) -> String {
    let cas = db.pinned_cas().unwrap();
    let evidence = evidence(db);
    let guard = guard(db);
    let peer = db.acquire_chain_lock(PEER).unwrap();
    let owner = db.acquire_chain_lock(OWNER).unwrap();
    let request = ChainRetirement {
        counterpart_roots: BTreeSet::new(),
        evidence,
        counterpart_locks: &[&peer],
        validator: &accept,
        runtime_cleanup: &|| Ok(()),
    };
    let draft = db
        .derive_retirement_draft(&cas, OWNER, &owner, &request)
        .unwrap();
    let hash = db
        .recovery
        .store_retired_chain_manifest(&cas, &guard, &owner, draft, &accept)
        .unwrap();
    db.recovery
        .publish_retired_chain_fence(&guard, &owner, OWNER, &hash)
        .unwrap();
    hash
}

#[test]
fn drives_a_chain_to_a_fenced_unlinked_closure() {
    let (_dir, db) = seeded();
    let judged = Cell::new(0);
    let cleaned = Cell::new(0);
    let report = retire(
        &db,
        &|_| {
            judged.set(judged.get() + 1);
            Ok(())
        },
        &|| {
            cleaned.set(cleaned.get() + 1);
            Ok(())
        },
    )
    .unwrap();
    assert!(report.stored_manifest);
    assert_eq!(judged.get(), 2);
    assert_eq!(cleaned.get(), 1);
    assert_eq!(
        fence(&db, OWNER),
        RetiredChainFence::Retired {
            manifest_hash: report.manifest_hash.clone()
        }
    );
    assert!(!head_present(&db, OWNER));
    assert!(db.pending_chain_transition(OWNER).unwrap().is_none());
    assert!(db.get_thread(OWNER).unwrap().is_none());
    // The counterpart is neither fenced nor touched.
    assert_eq!(fence(&db, PEER), RetiredChainFence::Absent);
    assert!(head_present(&db, PEER));
    assert!(db.get_thread(PEER).unwrap().is_some());
    // The stored closure is the derived one and releases cleanly.
    let cas = db.pinned_cas().unwrap();
    let closure =
        crate::recovery::read_retired_chain_closure(&cas, OWNER, &report.manifest_hash).unwrap();
    assert_eq!(closure.retained_evidence.len(), 2);
    assert_eq!(
        closure.counterpart_roots,
        BTreeSet::from([PEER.to_string()])
    );
    assert_eq!(
        db.recovery
            .unreleased_retired_chains()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec![OWNER]
    );
}

#[test]
fn owner_only_evidence_retires_a_chain_with_no_counterpart() {
    let (_dir, db) = seeded();
    let kept = live_event_hashes(&db, OWNER).0;
    let guard = guard(&db);
    let owner = db.acquire_chain_lock(OWNER).unwrap();
    let judged = Cell::new(0);
    let report = db
        .retire_chain(
            OWNER,
            &owner,
            &guard,
            ChainRetirement {
                counterpart_roots: BTreeSet::new(),
                evidence: vec![RetirementEvidenceRef {
                    counterpart_root: OWNER.to_string(),
                    event_hash: kept.clone(),
                }],
                counterpart_locks: &[],
                validator: &|event| {
                    judged.set(judged.get() + 1);
                    assert_eq!(event.chain_root_id, OWNER);
                    Ok(())
                },
                runtime_cleanup: &|| Ok(()),
            },
        )
        .unwrap();
    assert_eq!(judged.get(), 1);
    let cas = db.pinned_cas().unwrap();
    let closure =
        crate::recovery::read_retired_chain_closure(&cas, OWNER, &report.manifest_hash).unwrap();
    assert!(closure.counterpart_roots.is_empty());
    assert_eq!(closure.retained_evidence.len(), 1);
    let coordinate = &closure.retained_evidence[0];
    assert_eq!(coordinate.event_hash, kept);
    assert_eq!(coordinate.counterpart_tip, coordinate.owner_tip);
    assert!(
        db.recovery
            .evidence_obligation_holders(&owner, OWNER)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn owner_only_evidence_must_be_an_event_of_the_owner() {
    let (_dir, db) = seeded();
    let foreign = live_event_hashes(&db, PEER).0;
    let guard = guard(&db);
    let owner = db.acquire_chain_lock(OWNER).unwrap();
    let error = db
        .retire_chain(
            OWNER,
            &owner,
            &guard,
            ChainRetirement {
                counterpart_roots: BTreeSet::new(),
                evidence: vec![RetirementEvidenceRef {
                    counterpart_root: OWNER.to_string(),
                    event_hash: foreign,
                }],
                counterpart_locks: &[],
                validator: &accept,
                runtime_cleanup: &|| Ok(()),
            },
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("not an event of the retired chain"));
    drop(owner);
    assert_eq!(fence(&db, OWNER), RetiredChainFence::Absent);
    assert!(head_present(&db, OWNER));
}

#[test]
fn a_refused_validator_leaves_the_chain_live_and_unfenced() {
    let (_dir, db) = seeded();
    let cleaned = Cell::new(false);
    let error = retire(&db, &|_| anyhow::bail!("not evidence"), &|| {
        cleaned.set(true);
        Ok(())
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("not evidence"));
    assert_eq!(fence(&db, OWNER), RetiredChainFence::Absent);
    assert!(head_present(&db, OWNER));
    assert!(db.pending_chain_transition(OWNER).unwrap().is_none());
    assert!(!cleaned.get());
}

#[test]
fn a_coordinate_the_caller_names_must_be_locked_and_live() {
    let (_dir, db) = seeded();
    let evidence = evidence(&db);
    let guard = guard(&db);
    let owner = db.acquire_chain_lock(OWNER).unwrap();
    let error = db
        .retire_chain(
            OWNER,
            &owner,
            &guard,
            ChainRetirement {
                counterpart_roots: BTreeSet::new(),
                evidence,
                counterpart_locks: &[],
                validator: &accept,
                runtime_cleanup: &|| Ok(()),
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("was not locked"));
    assert_eq!(fence(&db, OWNER), RetiredChainFence::Absent);
}

#[test]
fn redriving_a_finished_retirement_changes_nothing() {
    let (_dir, db) = seeded();
    let first = retire(&db, &accept, &|| Ok(())).unwrap();
    let cleaned = Cell::new(0);
    let again = retire(&db, &accept, &|| {
        cleaned.set(cleaned.get() + 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(again.manifest_hash, first.manifest_hash);
    assert!(!again.stored_manifest);
    assert_eq!(cleaned.get(), 0);
    assert!(!head_present(&db, OWNER));
    // Startup replay of a finished retirement is a no-op too.
    let replay = db.replay_fenced_chain_retirements(&guard(&db)).unwrap();
    assert_eq!(replay.fences_checked, 1);
    assert_eq!(replay.heads_unlinked, 0);
}

#[test]
fn r3_fence_with_a_live_head_is_finished_by_redrive() {
    // The fence stands, so the retry keeps its manifest.
    let (_dir, db) = seeded();
    let hash = cut_after_fence(&db);
    assert!(head_present(&db, OWNER));
    let report = retire(&db, &accept, &|| Ok(())).unwrap();
    assert_eq!(report.manifest_hash, hash);
    assert!(!report.stored_manifest);
    assert!(!head_present(&db, OWNER));
    assert!(db.pending_chain_transition(OWNER).unwrap().is_none());
}

#[test]
fn r3_fence_with_a_live_head_is_finished_by_replay() {
    // The head is unlinked and the Remove left for runtime cleanup.
    let (_dir, db) = seeded();
    cut_after_fence(&db);
    let replay = db.replay_fenced_chain_retirements(&guard(&db)).unwrap();
    assert_eq!(replay.heads_unlinked, 1);
    assert!(!head_present(&db, OWNER));
    assert!(db.get_thread(OWNER).unwrap().is_none());
    let pending = db.pending_chain_transition(OWNER).unwrap().unwrap();
    assert_eq!(pending.operation, HeadOperation::Remove);
    assert_eq!(pending.phase, TransitionPhase::ApplyingProjection);
    let cleaned = Cell::new(0);
    retire(&db, &accept, &|| {
        cleaned.set(cleaned.get() + 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(cleaned.get(), 1);
    assert!(db.pending_chain_transition(OWNER).unwrap().is_none());
}

#[test]
fn r3_fence_with_a_prepared_remove_and_a_live_head_is_finished_by_replay() {
    let (_dir, db) = seeded();
    cut_after_fence(&db);
    let head = {
        let lock = db.acquire_chain_lock(OWNER).unwrap();
        lock.read_verified_head(db.trust_store.as_ref())
            .unwrap()
            .unwrap()
    };
    {
        let lock = db.acquire_chain_lock(OWNER).unwrap();
        db.recovery
            .prepare_remove(&lock, OWNER, &head.target_hash)
            .unwrap();
    }
    let replay = db.replay_fenced_chain_retirements(&guard(&db)).unwrap();
    assert_eq!(replay.heads_unlinked, 1);
    assert!(!head_present(&db, OWNER));
    let pending = db.pending_chain_transition(OWNER).unwrap().unwrap();
    assert_eq!(pending.phase, TransitionPhase::ApplyingProjection);
}

#[test]
fn r5_head_gone_with_runtime_cleanup_pending_is_finished_by_redrive() {
    let (_dir, db) = seeded();
    let error = retire(&db, &accept, &|| anyhow::bail!("runtime busy")).unwrap_err();
    assert!(format!("{error:#}").contains("runtime busy"));
    assert!(!head_present(&db, OWNER));
    let pending = db.pending_chain_transition(OWNER).unwrap().unwrap();
    assert_eq!(pending.operation, HeadOperation::Remove);
    assert!(matches!(
        fence(&db, OWNER),
        RetiredChainFence::Retired { .. }
    ));
    // The startup pending replay leaves cleanup to the runtime's owner.
    let replay = db.replay_pending_chain_transitions().unwrap();
    assert_eq!(replay.removals_awaiting_runtime_cleanup, 1);
    let cleaned = Cell::new(0);
    let report = retire(&db, &accept, &|| {
        cleaned.set(cleaned.get() + 1);
        Ok(())
    })
    .unwrap();
    assert!(!report.stored_manifest);
    assert_eq!(cleaned.get(), 1);
    assert!(db.pending_chain_transition(OWNER).unwrap().is_none());
}

#[test]
fn r6_acknowledged_retirement_is_complete_and_replay_ignores_it() {
    let (_dir, db) = seeded();
    retire(&db, &accept, &|| Ok(())).unwrap();
    assert!(db.pending_chain_transition(OWNER).unwrap().is_none());
    let replay = db.replay_pending_chain_transitions().unwrap();
    assert_eq!(replay.records_checked, 0);
    let replay = db.replay_fenced_chain_retirements(&guard(&db)).unwrap();
    assert_eq!(replay.heads_unlinked, 0);
}

#[test]
fn a_fenced_chains_removal_cannot_be_cancelled() {
    let (_dir, db) = seeded();
    cut_after_fence(&db);
    let owner = db.acquire_chain_lock(OWNER).unwrap();
    let head = owner
        .read_verified_head(db.trust_store.as_ref())
        .unwrap()
        .unwrap();
    db.recovery
        .prepare_remove(&owner, OWNER, &head.target_hash)
        .unwrap();
    let error = db.cancel_pending_chain_removal(OWNER, &owner).unwrap_err();
    assert!(error.to_string().contains("fenced chain"));
    assert!(db.pending_chain_transition(OWNER).unwrap().is_some());
    // An unfenced chain's Prepared Remove still cancels.
    let peer = db.acquire_chain_lock(PEER).unwrap();
    let peer_head = peer
        .read_verified_head(db.trust_store.as_ref())
        .unwrap()
        .unwrap();
    db.recovery
        .prepare_remove(&peer, PEER, &peer_head.target_hash)
        .unwrap();
    assert!(db.cancel_pending_chain_removal(PEER, &peer).unwrap());
}

#[test]
fn a_write_never_cancels_a_remove_on_a_fenced_chain() {
    let (_dir, db) = seeded();
    cut_after_fence(&db);
    {
        let owner = db.acquire_chain_lock(OWNER).unwrap();
        let head = owner
            .read_verified_head(db.trust_store.as_ref())
            .unwrap()
            .unwrap();
        db.recovery
            .prepare_remove(&owner, OWNER, &head.target_hash)
            .unwrap();
    }
    let error = db
        .append_events(
            OWNER,
            OWNER,
            vec![event(OWNER, "late")],
            vec![],
            &TestSigner::default(),
        )
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("fenced"),
        "unexpected refusal: {error:#}"
    );
    let pending = db.pending_chain_transition(OWNER).unwrap().unwrap();
    assert_eq!(pending.operation, HeadOperation::Remove);
    assert_eq!(pending.phase, TransitionPhase::Prepared);
}

#[test]
fn a_head_the_fenced_closure_does_not_describe_refuses_replay() {
    let (_dir, db) = seeded();
    let cas = db.pinned_cas().unwrap();
    let guard = guard(&db);
    let owner = db.acquire_chain_lock(OWNER).unwrap();
    let request = ChainRetirement {
        counterpart_roots: BTreeSet::new(),
        evidence: Vec::new(),
        counterpart_locks: &[],
        validator: &accept,
        runtime_cleanup: &|| Ok(()),
    };
    let mut draft = db
        .derive_retirement_draft(&cas, OWNER, &owner, &request)
        .unwrap();
    draft.retired_head_hash = "a".repeat(64);
    let hash = db
        .recovery
        .store_retired_chain_manifest(&cas, &guard, &owner, draft, &accept)
        .unwrap();
    db.recovery
        .publish_retired_chain_fence(&guard, &owner, OWNER, &hash)
        .unwrap();
    drop(owner);
    let error = db.replay_fenced_chain_retirements(&guard).unwrap_err();
    assert!(error.to_string().contains("does not describe"));
    assert!(head_present(&db, OWNER));
    assert!(db.pending_chain_transition(OWNER).unwrap().is_none());
}

#[test]
fn a_fenced_removal_naming_another_head_refuses_pending_replay() {
    let (_dir, db) = seeded();
    cut_after_fence(&db);
    {
        let owner = db.acquire_chain_lock(OWNER).unwrap();
        db.recovery
            .prepare_remove(&owner, OWNER, &"a".repeat(64))
            .unwrap();
    }
    let error = db.replay_pending_chain_transitions().unwrap_err();
    assert!(
        error.to_string().contains("fenced chain"),
        "unexpected refusal: {error:#}"
    );
    assert!(head_present(&db, OWNER));
    let pending = db.pending_chain_transition(OWNER).unwrap().unwrap();
    assert_eq!(pending.operation, HeadOperation::Remove);
}

#[test]
fn a_fenced_chain_with_an_unprojected_pending_set_refuses_to_retire() {
    let (_dir, db) = seeded();
    {
        let owner = db.acquire_chain_lock(OWNER).unwrap();
        let head = owner
            .read_verified_head(db.trust_store.as_ref())
            .unwrap()
            .unwrap();
        db.recovery
            .prepare_set(&owner, OWNER, Some(&head.target_hash), &"b".repeat(64))
            .unwrap();
    }
    cut_after_fence(&db);
    let error = retire(&db, &accept, &|| Ok(())).unwrap_err();
    assert!(
        format!("{error:#}").contains("not projected"),
        "unexpected refusal: {error:#}"
    );
    assert!(head_present(&db, OWNER));
    let pending = db.pending_chain_transition(OWNER).unwrap().unwrap();
    assert_eq!(pending.operation, HeadOperation::Set);
}

#[test]
fn a_released_fence_without_a_head_is_a_no_op() {
    let (_dir, db) = seeded();
    // No evidence, so the release has no counterpart to wait for.
    let guard = guard(&db);
    let owner = db.acquire_chain_lock(OWNER).unwrap();
    let run = |cleanup: &dyn Fn() -> anyhow::Result<()>| {
        db.retire_chain(
            OWNER,
            &owner,
            &guard,
            ChainRetirement {
                counterpart_roots: BTreeSet::new(),
                evidence: Vec::new(),
                counterpart_locks: &[],
                validator: &accept,
                runtime_cleanup: cleanup,
            },
        )
    };
    let first = run(&|| Ok(())).unwrap();
    let cas = db.pinned_cas().unwrap();
    db.recovery
        .release_retired_chain(&cas, &guard, &owner, OWNER)
        .unwrap();
    assert!(matches!(
        fence(&db, OWNER),
        RetiredChainFence::Released { .. }
    ));
    let again = run(&|| anyhow::bail!("cleanup must not run")).unwrap();
    assert_eq!(again.manifest_hash, first.manifest_hash);
    assert!(!again.stored_manifest);
}

#[test]
fn an_evidence_counterpart_without_a_live_head_refuses() {
    let (_dir, db) = seeded();
    let evidence = evidence(&db);
    {
        let peer = db.acquire_chain_lock(PEER).unwrap();
        db.remove_chain_head_ref(PEER, &peer).unwrap();
    }
    let guard = guard(&db);
    let peer = db.acquire_chain_lock(PEER).unwrap();
    let owner = db.acquire_chain_lock(OWNER).unwrap();
    let error = db
        .retire_chain(
            OWNER,
            &owner,
            &guard,
            ChainRetirement {
                counterpart_roots: BTreeSet::new(),
                evidence,
                counterpart_locks: &[&peer],
                validator: &accept,
                runtime_cleanup: &|| Ok(()),
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("has no live head"));
    assert_eq!(fence(&db, OWNER), RetiredChainFence::Absent);
    assert!(head_present_under(&owner, &db));
}

fn head_present_under(lock: &crate::chain::ChainLock, db: &StateDb) -> bool {
    lock.read_verified_head(db.trust_store.as_ref())
        .unwrap()
        .is_some()
}
