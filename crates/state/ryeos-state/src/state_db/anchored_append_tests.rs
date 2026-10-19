//! Bounded thread-event reads and anchored conditional appends: exact fixed
//! anchors, stale-frontier refusal before any write, two-chain and mixed live /
//! retired cuts, and the append closure of the evidence-obligation publisher.

use std::collections::BTreeSet;

use super::*;
use crate::objects::ThreadStatus;
use crate::objects::thread_event::NewEvent;
use crate::objects::thread_snapshot::{
    CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryPolicy, ThreadHistoryRetention, ThreadSnapshotBuilder,
};
use crate::signer::TestSigner;
use crate::test_support::TestState;

const KIND: &str = "directive";

struct NoRecovery;

impl RuntimeLivenessInspector for NoRecovery {
    fn chain_has_live_recovery_state(&self, _: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
}

fn open() -> (tempfile::TempDir, TestState) {
    let dir = tempfile::tempdir().unwrap();
    let signer = TestSigner::default();
    let mut trust = TrustStore::new();
    trust.insert(signer.fingerprint().to_string(), signer.verifying_key());
    let db = TestState::open(dir.path(), Arc::new(trust)).unwrap();
    (dir, db)
}

fn created_snapshot(id: &str, kind: &str) -> ThreadSnapshot {
    let item_ref = "directive:system/test";
    ThreadSnapshotBuilder::new(id, id, kind, item_ref, "directive-runtime")
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
        .build()
}

/// Rows every created chain holds: the start event that makes it running.
const START_ROWS: i64 = 1;

fn create(db: &StateDb, id: &str, kind: &str) {
    let signer = TestSigner::default();
    let created = created_snapshot(id, kind);
    db.create_chain(id, created.clone(), &signer).unwrap();
    let mut running = created;
    running.status = ThreadStatus::Running;
    running.started_at = Some(lillux::time::iso8601_now());
    db.append_events(
        id,
        id,
        vec![event(id, "start")],
        vec![SnapshotUpdate {
            thread_id: id.into(),
            new_snapshot: running,
        }],
        &signer,
    )
    .unwrap();
}

fn event(id: &str, label: &str) -> ThreadEvent {
    NewEvent::new(id, id, "test_event")
        .payload(serde_json::json!({ "label": label }))
        .build()
}

fn append(db: &StateDb, id: &str, count: usize) {
    let events = (0..count).map(|n| event(id, &format!("e{n}"))).collect();
    db.append_events(id, id, events, vec![], &TestSigner::default())
        .unwrap();
}

fn event_rows(db: &StateDb, id: &str) -> i64 {
    db.projection()
        .with_sql(|connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM events WHERE thread_id = ?1",
                [id],
                |row| row.get(0),
            )?)
        })
        .unwrap()
}

fn guard(db: &StateDb) -> crate::recovery::CasMutationGuard {
    db.pinned_authority()
        .unwrap()
        .acquire_shared_guard()
        .unwrap()
}

fn capture(db: &StateDb, id: &str) -> AuthoritativeThreadEventReadAnchor {
    db.capture_thread_event_read_anchor(id, id, &guard(db))
        .unwrap()
        .expect("thread present")
}

fn anchored<'a>(
    id: &'a str,
    events: Vec<ThreadEvent>,
    signer: &'a TestSigner,
    expected: &'a AuthoritativeThreadEventReadAnchor,
) -> AnchoredThreadAppend<'a> {
    AnchoredThreadAppend {
        chain_root_id: id,
        thread_id: id,
        events,
        snapshot_updates: vec![],
        signer,
        runtime_liveness: &NoRecovery,
        expected,
    }
}

fn two_anchor<'a>(
    id: &'a str,
    signer: &'a TestSigner,
    expected: &'a AuthoritativeThreadEventReadAnchor,
    counterpart: &'a AuthoritativeThreadEventReadAnchor,
    kind: &'a str,
) -> TwoAnchorThreadAppend<'a> {
    TwoAnchorThreadAppend {
        chain_root_id: id,
        thread_id: id,
        events: vec![event(id, "two-anchor")],
        signer,
        runtime_liveness: &NoRecovery,
        expected,
        counterpart,
        required_target_kind: kind,
    }
}

fn committed(result: ConditionalThreadEventAppendResult) -> bool {
    matches!(result, ConditionalThreadEventAppendResult::Committed(_))
}

fn labels(page: &AuthoritativeThreadEventReadPage) -> Vec<String> {
    page.events
        .iter()
        .map(|(_, event)| event.payload["label"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn event_pages_walk_one_fixed_anchor_newest_first_and_ignore_later_appends() {
    let (_dir, db) = open();
    create(&db, "T-a", KIND);
    append(&db, "T-a", 5);
    let anchor = capture(&db, "T-a");
    assert_eq!(anchor.chain_root_id(), "T-a");
    assert_eq!(anchor.thread_id(), "T-a");
    assert_eq!(anchor.tip().map(|(_, seq)| seq), Some(6));
    // Events after the capture cannot enter the fixed prefix.
    append(&db, "T-a", 2);
    let guard = guard(&db);
    let mut seen = Vec::new();
    let mut progress = None;
    let mut pages = 0;
    loop {
        let page = db
            .read_thread_event_page(&anchor, progress.as_ref(), 2, 1 << 20, &guard)
            .unwrap();
        pages += 1;
        seen.extend(labels(&page));
        progress = page.progress;
        if progress.is_none() {
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, ["e4", "e3", "e2", "e1", "e0", "start"]);
}

#[test]
fn event_pages_refuse_zero_bounds_oversized_objects_and_foreign_progress() {
    let (_dir, db) = open();
    create(&db, "T-a", KIND);
    create(&db, "T-b", KIND);
    append(&db, "T-a", 3);
    append(&db, "T-b", 3);
    let guard = guard(&db);
    let anchor_a = capture(&db, "T-a");
    let anchor_b = capture(&db, "T-b");
    assert!(
        db.read_thread_event_page(&anchor_a, None, 0, 1 << 20, &guard)
            .is_err()
    );
    assert!(
        db.read_thread_event_page(&anchor_a, None, 1, 0, &guard)
            .is_err()
    );
    // A one-byte budget cannot admit even the first event; no partial page.
    assert!(
        db.read_thread_event_page(&anchor_a, None, 3, 1, &guard)
            .is_err()
    );
    let page = db
        .read_thread_event_page(&anchor_a, None, 1, 1 << 20, &guard)
        .unwrap();
    let progress = page.progress.expect("older events remain");
    assert!(
        db.read_thread_event_page(&anchor_b, Some(&progress), 1, 1 << 20, &guard)
            .is_err()
    );
    // Progress from an earlier cut of the same thread is not valid for a later one.
    append(&db, "T-a", 1);
    let later = capture(&db, "T-a");
    assert!(
        db.read_thread_event_page(&later, Some(&progress), 1, 1 << 20, &guard)
            .is_err()
    );
}

#[test]
fn capture_of_an_absent_thread_or_chain_is_none() {
    let (_dir, db) = open();
    create(&db, "T-a", KIND);
    let guard = guard(&db);
    assert!(
        db.capture_thread_event_read_anchor("T-a", "T-missing", &guard)
            .unwrap()
            .is_none()
    );
    let first = capture(&db, "T-a");
    assert_eq!(first.tip().map(|(_, seq)| seq), Some(1));
    assert_eq!(
        db.read_thread_event_page(&first, None, 4, 1 << 20, &guard)
            .unwrap()
            .events
            .len(),
        1
    );
}

#[test]
fn conditional_append_publishes_only_from_the_exact_frontier() {
    let (_dir, db) = open();
    let signer = TestSigner::default();
    create(&db, "T-a", KIND);
    append(&db, "T-a", 1);
    let stale = capture(&db, "T-a");
    append(&db, "T-a", 1);
    let before = event_rows(&db, "T-a");
    let refused = db
        .append_events_admitted_if_anchor(
            anchored("T-a", vec![event("T-a", "late")], &signer, &stale),
            &guard(&db),
        )
        .unwrap();
    assert!(matches!(
        refused,
        ConditionalThreadEventAppendResult::Conflict
    ));
    assert_eq!(event_rows(&db, "T-a"), before);

    let fresh = capture(&db, "T-a");
    assert!(committed(
        db.append_events_admitted_if_anchor(
            anchored("T-a", vec![event("T-a", "ok")], &signer, &fresh),
            &guard(&db),
        )
        .unwrap()
    ));
    assert_eq!(event_rows(&db, "T-a"), before + 1);
    // The consumed anchor cannot publish a second time.
    assert!(matches!(
        db.append_events_admitted_if_anchor(
            anchored("T-a", vec![event("T-a", "replay")], &signer, &fresh),
            &guard(&db),
        )
        .unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
}

#[test]
fn conditional_append_refuses_an_anchor_for_another_thread_and_ephemeral_events() {
    let (_dir, db) = open();
    let signer = TestSigner::default();
    create(&db, "T-a", KIND);
    create(&db, "T-b", KIND);
    let other = capture(&db, "T-b");
    assert!(
        db.append_events_admitted_if_anchor(
            anchored("T-a", vec![event("T-a", "x")], &signer, &other),
            &guard(&db),
        )
        .is_err()
    );
    let own = capture(&db, "T-a");
    let ephemeral = NewEvent::new("T-a", "T-a", "test_event")
        .durability(crate::objects::EventDurability::Ephemeral)
        .build();
    assert!(
        db.append_events_admitted_if_anchor(
            anchored("T-a", vec![ephemeral], &signer, &own),
            &guard(&db),
        )
        .is_err()
    );
    assert_eq!(event_rows(&db, "T-a"), START_ROWS);
}

#[test]
fn two_anchor_append_requires_both_frontiers_kind_and_admission() {
    let (_dir, db) = open();
    let signer = TestSigner::default();
    create(&db, "T-a", KIND);
    create(&db, "T-b", KIND);
    append(&db, "T-a", 1);
    append(&db, "T-b", 1);
    let target = capture(&db, "T-a");
    let counterpart = capture(&db, "T-b");

    // Wrong required kind: refused without a write.
    assert!(matches!(
        db.append_running_events_admitted_if_two_anchors(
            two_anchor("T-a", &signer, &target, &counterpart, "other_kind"),
            &guard(&db),
        )
        .unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    // Admission sees both signed snapshots and memberships, and may refuse.
    let mut seen = None;
    assert!(matches!(
        db.append_running_events_admitted_if_two_anchors_with_admission(
            two_anchor("T-a", &signer, &target, &counterpart, KIND),
            &guard(&db),
            |view| {
                seen = Some((
                    view.target.thread_id.clone(),
                    view.counterpart.thread_id.clone(),
                    view.target_thread_ids.to_vec(),
                    view.counterpart_thread_ids.to_vec(),
                ));
                Ok(false)
            },
        )
        .unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    assert_eq!(
        seen,
        Some((
            "T-a".into(),
            "T-b".into(),
            vec!["T-a".to_owned()],
            vec!["T-b".to_owned()]
        ))
    );
    assert_eq!(event_rows(&db, "T-a"), START_ROWS + 1);
    // An admission error publishes nothing.
    assert!(
        db.append_running_events_admitted_if_two_anchors_with_admission(
            two_anchor("T-a", &signer, &target, &counterpart, KIND),
            &guard(&db),
            |_| anyhow::bail!("domain refusal"),
        )
        .is_err()
    );
    assert_eq!(event_rows(&db, "T-a"), START_ROWS + 1);

    // A moved counterpart refuses before the target is written.
    append(&db, "T-b", 1);
    assert!(matches!(
        db.append_running_events_admitted_if_two_anchors(
            two_anchor("T-a", &signer, &target, &counterpart, KIND),
            &guard(&db),
        )
        .unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    assert_eq!(event_rows(&db, "T-a"), START_ROWS + 1);

    let counterpart = capture(&db, "T-b");
    assert!(committed(
        db.append_running_events_admitted_if_two_anchors(
            two_anchor("T-a", &signer, &target, &counterpart, KIND),
            &guard(&db),
        )
        .unwrap()
    ));
    assert_eq!(event_rows(&db, "T-a"), START_ROWS + 2);
    assert_eq!(
        event_rows(&db, "T-b"),
        START_ROWS + 2,
        "the counterpart is never written"
    );
    assert_eq!(
        capture(&db, "T-b"),
        counterpart,
        "the counterpart frontier is untouched"
    );
    // Same chain on both sides is a refused shape, not a conflict.
    let target = capture(&db, "T-a");
    assert!(
        db.append_running_events_admitted_if_two_anchors(
            two_anchor("T-a", &signer, &target, &target, KIND),
            &guard(&db),
        )
        .is_err()
    );
}

fn fence(db: &StateDb, id: &str, manifest: &str) {
    drop(db.acquire_chain_lock(id).unwrap());
    let lock = db.acquire_existing_chain_lock(id).unwrap();
    db.recovery
        .publish_retired_chain_fence(&guard(db), &lock, id, manifest)
        .unwrap();
}

#[test]
fn mixed_cut_excludes_live_and_retired_dependencies() {
    let (_dir, db) = open();
    let signer = TestSigner::default();
    create(&db, "T-a", KIND);
    create(&db, "T-b", KIND);
    create(&db, "T-c", KIND);
    let manifest = "ab".repeat(32);
    fence(&db, "T-retired", &manifest);
    let retired = [RetiredChainDependency {
        chain_root_id: "T-retired".into(),
        manifest_hash: manifest.clone(),
    }];
    let target = capture(&db, "T-a");
    let counterpart = capture(&db, "T-b");
    let extra = capture(&db, "T-c");
    let run = |current: &[AuthoritativeThreadEventReadAnchor],
               retired: &[RetiredChainDependency]| {
        db.append_running_events_admitted_if_mixed_cut_with_admission(
            two_anchor("T-a", &signer, &target, &counterpart, KIND),
            MixedCutDependencies {
                current_anchors: current,
                retired,
            },
            &guard(&db),
            |_| Ok(true),
        )
    };

    // A retired dependency naming a different closure than its fence refuses.
    let wrong = [RetiredChainDependency {
        chain_root_id: "T-retired".into(),
        manifest_hash: "cd".repeat(32),
    }];
    assert!(run(&[], &wrong).is_err());
    // An unfenced identity, and a chain that still has a head, are conflicts.
    drop(db.acquire_chain_lock("T-ghost").unwrap());
    let ghost = [RetiredChainDependency {
        chain_root_id: "T-ghost".into(),
        manifest_hash: manifest.clone(),
    }];
    assert!(matches!(
        run(&[], &ghost).unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    let live_as_retired = [RetiredChainDependency {
        chain_root_id: "T-c".into(),
        manifest_hash: manifest.clone(),
    }];
    assert!(matches!(
        run(&[], &live_as_retired).unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    // Mixing the same chain as live and retired is refused outright.
    let mixed = [RetiredChainDependency {
        chain_root_id: "T-c".into(),
        manifest_hash: manifest.clone(),
    }];
    assert!(run(std::slice::from_ref(&extra), &mixed).is_err());
    assert_eq!(event_rows(&db, "T-a"), START_ROWS);

    // A released closure can no longer prove the retired cut.
    fence(&db, "T-released", &manifest);
    let released_lock = db.acquire_existing_chain_lock("T-released").unwrap();
    db.recovery
        .publish_retired_chain_release(&guard(&db), &released_lock, "T-released", &BTreeSet::new())
        .unwrap();
    drop(released_lock);
    let released = [RetiredChainDependency {
        chain_root_id: "T-released".into(),
        manifest_hash: manifest.clone(),
    }];
    assert!(matches!(
        run(&[], &released).unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    // A moved extra live dependency refuses before any write.
    append(&db, "T-c", 1);
    assert!(matches!(
        run(std::slice::from_ref(&extra), &retired).unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    assert_eq!(event_rows(&db, "T-a"), START_ROWS);

    let extra = capture(&db, "T-c");
    assert!(committed(
        run(std::slice::from_ref(&extra), &retired).unwrap()
    ));
    assert_eq!(event_rows(&db, "T-a"), START_ROWS + 1);
}

fn obligation_fixture() -> (tempfile::TempDir, TestState) {
    let (dir, db) = open();
    create(&db, "T-owner", KIND);
    create(&db, "T-holder", KIND);
    append(&db, "T-holder", 1);
    (dir, db)
}

fn holders(db: &StateDb, owner_lock: &crate::chain::ChainLock) -> BTreeSet<String> {
    db.recovery
        .evidence_obligation_holders(owner_lock, "T-owner")
        .unwrap()
}

#[test]
fn obligation_is_durable_before_the_anchored_append_the_publisher_runs() {
    let (_dir, db) = obligation_fixture();
    let signer = TestSigner::default();
    let anchor = capture(&db, "T-holder");
    let guard = guard(&db);
    // Root-id order: holder before owner.
    let holder_lock = db.acquire_existing_chain_lock("T-holder").unwrap();
    let owner_lock = db.acquire_existing_chain_lock("T-owner").unwrap();
    let before = event_rows(&db, "T-holder");
    let committed_inside = db
        .recovery
        .publish_evidence_obligation_then(
            &guard,
            &owner_lock,
            &holder_lock,
            "T-owner",
            "T-holder",
            || {
                assert_eq!(
                    holders(&db, &owner_lock),
                    BTreeSet::from(["T-holder".to_owned()]),
                    "the obligation is visible before the evidence"
                );
                assert_eq!(event_rows(&db, "T-holder"), before);
                db.append_events_admitted_if_anchor_under_lock(
                    anchored(
                        "T-holder",
                        vec![event("T-holder", "evidence")],
                        &signer,
                        &anchor,
                    ),
                    &holder_lock,
                    &guard,
                )
            },
        )
        .unwrap();
    assert!(committed(committed_inside));
    assert_eq!(event_rows(&db, "T-holder"), before + 1);
    assert_eq!(holders(&db, &owner_lock).len(), 1);
}

#[test]
fn stale_anchor_inside_the_publisher_refuses_without_the_append() {
    let (_dir, db) = obligation_fixture();
    let signer = TestSigner::default();
    let stale = capture(&db, "T-holder");
    append(&db, "T-holder", 1);
    let guard = guard(&db);
    let holder_lock = db.acquire_existing_chain_lock("T-holder").unwrap();
    let owner_lock = db.acquire_existing_chain_lock("T-owner").unwrap();
    let before = event_rows(&db, "T-holder");
    let result = db
        .recovery
        .publish_evidence_obligation_then(
            &guard,
            &owner_lock,
            &holder_lock,
            "T-owner",
            "T-holder",
            || {
                db.append_events_admitted_if_anchor_under_lock(
                    anchored(
                        "T-holder",
                        vec![event("T-holder", "stale")],
                        &signer,
                        &stale,
                    ),
                    &holder_lock,
                    &guard,
                )
            },
        )
        .unwrap();
    assert!(matches!(
        result,
        ConditionalThreadEventAppendResult::Conflict
    ));
    assert_eq!(event_rows(&db, "T-holder"), before, "no event was appended");
    // The obligation was recorded first and stays: it only over-retains.
    assert_eq!(
        holders(&db, &owner_lock),
        BTreeSet::from(["T-holder".to_owned()])
    );
}

#[test]
fn stale_anchor_refuses_before_a_pending_retirement_is_touched() {
    let (_dir, db) = open();
    let signer = TestSigner::default();
    create(&db, "T-a", KIND);
    let stale = capture(&db, "T-a");
    append(&db, "T-a", 1);
    let prepared = {
        let lock = db.acquire_existing_chain_lock("T-a").unwrap();
        let head = lock
            .read_verified_head(db.trust_store.as_ref())
            .unwrap()
            .unwrap();
        db.recovery
            .prepare_remove(&lock, "T-a", &head.target_hash)
            .unwrap()
    };
    assert!(matches!(
        db.append_events_admitted_if_anchor(
            anchored("T-a", vec![event("T-a", "late")], &signer, &stale),
            &guard(&db),
        )
        .unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    let still = db
        .recovery
        .read_pending("T-a")
        .unwrap()
        .expect("pending survives");
    assert_eq!(still.transition_id, prepared.transition_id);
    assert_eq!(still.phase, prepared.phase);
}

#[test]
fn pending_transition_on_a_live_dependency_is_a_conflict_at_its_exact_frontier() {
    let (_dir, db) = open();
    let signer = TestSigner::default();
    for id in ["T-a", "T-b", "T-c"] {
        create(&db, id, KIND);
    }
    let target = capture(&db, "T-a");
    let counterpart = capture(&db, "T-b");
    let extra = capture(&db, "T-c");
    {
        let lock = db.acquire_existing_chain_lock("T-c").unwrap();
        let head = lock
            .read_verified_head(db.trust_store.as_ref())
            .unwrap()
            .unwrap();
        db.recovery
            .prepare_remove(&lock, "T-c", &head.target_hash)
            .unwrap();
    }
    assert!(matches!(
        db.append_running_events_admitted_if_mixed_cut_with_admission(
            two_anchor("T-a", &signer, &target, &counterpart, KIND),
            MixedCutDependencies {
                current_anchors: std::slice::from_ref(&extra),
                retired: &[],
            },
            &guard(&db),
            |_| Ok(true),
        )
        .unwrap(),
        ConditionalThreadEventAppendResult::Conflict
    ));
    assert_eq!(event_rows(&db, "T-a"), START_ROWS);
}
