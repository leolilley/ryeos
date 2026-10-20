//! Conditional seat and mixed-cut store primitives: every action is admitted
//! only at the exact signed frontier it was read at, and a refusal writes
//! nothing.
use super::*;
use ryeos_state::{AuthoritativeThreadEventReadAnchor as Anchor, RetiredChainDependency};

const FENCED_KIND: &str = "fenced_test_kind";
const OWNER: &str = "fp:test";
const ITEM: &str = "directive:test";
const EXECUTOR: &str = "native:test";

fn new_store() -> super::test_native::TestStateStore {
    let root = tempfile::tempdir().expect("tempdir").keep();
    let identity = ryeos_node_runtime::identity::NodeIdentity::create(&root.join("node-key.pem"))
        .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let store = super::test_native::TestStateStore::open(&root, signer, Arc::new(trust))
        .expect("state store");
    store
        .install_fenced_chain_kinds(
            &ryeos_state::FencedChainKinds::new([FENCED_KIND.to_string()]).unwrap(),
        )
        .unwrap();
    crate::object_contracts::install_retained_evidence_validator(
        crate::object_contracts::accept_evidence,
    )
    .unwrap();
    store
}

fn record(id: &str, kind: &str) -> NewThreadRecord {
    let hash = "a".repeat(64);
    NewThreadRecord {
        thread_id: id.to_string(),
        chain_root_id: id.to_string(),
        kind: kind.to_string(),
        item_ref: ITEM.to_string(),
        executor_ref: EXECUTOR.to_string(),
        launch_mode: "wait".to_string(),
        current_site_id: "site:test".to_string(),
        origin_site_id: "site:test".to_string(),
        upstream_thread_id: None,
        requested_by: Some(OWNER.to_string()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 },
            canonical_item_ref: ITEM.to_string(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(
                ),
            },
        }),
    }
}

fn running(store: &StateStore, id: &str, kind: &str) {
    store.create_thread_for_test(&record(id, kind)).unwrap();
    store
        .claim_thread_launch(id, &format!("claim-{id}"), "daemon:test")
        .unwrap();
    store.mark_thread_running(id, None).unwrap();
}

fn running_seat(store: &StateStore, id: &str) {
    running(store, id, FENCED_KIND);
}

fn finish(store: &StateStore, id: &str) {
    store
        .finalize_thread(
            id,
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_owned(),
                outcome_code: Some("completed".to_owned()),
                result_json: Some(json!({"answer": 42})),
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
}

fn seat(id: &str) -> ExactSeat<'_> {
    ExactSeat {
        root_id: id,
        owner: OWNER,
        item_ref: ITEM,
        executor_ref: EXECUTOR,
    }
}

fn anchor(store: &StateStore, id: &str) -> Anchor {
    store
        .capture_thread_event_read_anchor(id, id)
        .unwrap()
        .expect("anchor")
}

fn event(tag: &str) -> NewEventRecord {
    NewEventRecord {
        event_type: "test.seat.event".into(),
        storage_class: "indexed".into(),
        payload: json!({"tag": tag}),
    }
}

fn push(store: &StateStore, id: &str, tag: &str) {
    store.append_events(id, id, &[event(tag)]).unwrap();
}

fn tip_seq(store: &StateStore, id: &str) -> u64 {
    anchor(store, id).tip().expect("tip").1
}

fn lease_active(store: &StateStore, id: &str) -> bool {
    store
        .lock()
        .unwrap()
        .runtime_db
        .active_seat_lease_snapshot(id)
        .unwrap()
        .is_some()
}

fn lease_claimed(store: &StateStore, id: &str) -> bool {
    store
        .lock()
        .unwrap()
        .runtime_db
        .seat_lease_is_reaping(id)
        .unwrap()
}

fn retire(store: &StateStore, id: &str) -> RetiredChainDependency {
    store
        .release_thread_launch_claim(id, &format!("claim-{id}"))
        .unwrap();
    store
        .retire_fenced_chain(
            id,
            "2099-01-01T00:00:00Z",
            &FencedChainRetirementPlan::default(),
            |_| Ok(0),
        )
        .unwrap();
    let g = store.lock().unwrap();
    match g.state_db.retired_chain_fence(id).unwrap() {
        ryeos_state::RetiredChainFence::Retired { manifest_hash } => RetiredChainDependency {
            chain_root_id: id.to_string(),
            manifest_hash,
        },
        other => panic!("chain {id} is not fenced after retirement: {other:?}"),
    }
}

#[test]
fn read_cut_exclusion_runs_only_at_the_exact_frontier() {
    let store = new_store();
    running_seat(&store, "S-read");
    let before = anchor(&store, "S-read");
    assert_eq!(
        store
            .with_thread_event_read_anchors(std::slice::from_ref(&before), || Ok(7))
            .unwrap(),
        Some(7)
    );
    assert!(
        store
            .with_thread_event_read_anchors(&[], || Ok(()))
            .is_err()
    );
    push(&store, "S-read", "moved");
    assert_eq!(
        store
            .with_thread_event_read_anchors(std::slice::from_ref(&before), || Ok(7))
            .unwrap(),
        None
    );
    let current = anchor(&store, "S-read");
    assert!(
        store
            .with_thread_event_read_mixed_cut(std::slice::from_ref(&current), &[], || Ok(()))
            .is_err(),
        "a mixed cut with no retired dependency is not a mixed cut"
    );
}

#[test]
fn mixed_read_cut_follows_the_retired_fence() {
    let store = new_store();
    running_seat(&store, "S-live");
    running_seat(&store, "S-gone");
    finish(&store, "S-gone");
    let live = anchor(&store, "S-live");
    let retired = retire(&store, "S-gone");
    assert_eq!(
        store
            .with_thread_event_read_mixed_cut(
                std::slice::from_ref(&live),
                std::slice::from_ref(&retired),
                || Ok("ran"),
            )
            .unwrap(),
        Some("ran")
    );
    let wrong_closure = RetiredChainDependency {
        chain_root_id: "S-gone".into(),
        manifest_hash: "b".repeat(64),
    };
    assert!(
        store
            .with_thread_event_read_mixed_cut(std::slice::from_ref(&live), &[wrong_closure], || Ok(
                ()
            ))
            .is_err()
    );
    // A chain that still has a signed head is not retired.
    running_seat(&store, "S-headed");
    let headed = RetiredChainDependency {
        chain_root_id: "S-headed".into(),
        manifest_hash: retired.manifest_hash.clone(),
    };
    assert_eq!(
        store
            .with_thread_event_read_mixed_cut(std::slice::from_ref(&live), &[headed], || Ok(()))
            .unwrap(),
        None
    );
    push(&store, "S-live", "moved");
    assert_eq!(
        store
            .with_thread_event_read_mixed_cut(
                std::slice::from_ref(&live),
                std::slice::from_ref(&retired),
                || Ok(())
            )
            .unwrap(),
        None
    );
}

#[test]
fn exact_seat_action_is_admitted_only_for_the_anchored_running_seat() {
    let store = new_store();
    running_seat(&store, "S-act");
    let fresh = anchor(&store, "S-act");
    assert_eq!(
        store
            .with_exact_seat_action_if_anchor(&seat("S-act"), &fresh, || Ok("done"))
            .unwrap(),
        Some("done")
    );
    // Foreign owner, item and executor are not the signed seat.
    for foreign in [
        ExactSeat {
            owner: "fp:other",
            ..seat("S-act")
        },
        ExactSeat {
            item_ref: "directive:other",
            ..seat("S-act")
        },
        ExactSeat {
            executor_ref: "native:other",
            ..seat("S-act")
        },
    ] {
        assert_eq!(
            store
                .with_exact_seat_action_if_anchor(&foreign, &fresh, || Ok(()))
                .unwrap(),
            None
        );
    }
    // Another seat's anchor is not this seat's root anchor.
    running_seat(&store, "S-other");
    let other = anchor(&store, "S-other");
    assert!(
        store
            .with_exact_seat_action_if_anchor(&seat("S-act"), &other, || Ok(()))
            .is_err()
    );
    // A cut that omits the seat's own anchor is malformed.
    assert!(
        store
            .with_exact_seat_action_if_anchors(
                &seat("S-act"),
                &fresh,
                std::slice::from_ref(&other),
                || Ok(())
            )
            .is_err()
    );
    // A paired cut goes stale when either head moves.
    let paired = [fresh.clone(), other.clone()];
    assert!(
        store
            .with_exact_seat_action_if_anchors(&seat("S-act"), &fresh, &paired, || Ok(()))
            .unwrap()
            .is_some()
    );
    push(&store, "S-other", "moved");
    assert_eq!(
        store
            .with_exact_seat_action_if_anchors(&seat("S-act"), &fresh, &paired, || Ok(()))
            .unwrap(),
        None
    );
    push(&store, "S-act", "moved");
    assert_eq!(
        store
            .with_exact_seat_action_if_anchor(&seat("S-act"), &fresh, || Ok(()))
            .unwrap(),
        None
    );
}

#[test]
fn exact_seat_action_refuses_a_chain_that_is_not_of_a_fenced_kind() {
    let store = new_store();
    running(&store, "S-plain", "directive");
    let plain = anchor(&store, "S-plain");
    assert_eq!(
        store
            .with_exact_seat_action_if_anchor(&seat("S-plain"), &plain, || Ok(()))
            .unwrap(),
        None
    );
}

#[test]
fn exact_seat_action_refuses_a_claimed_lease_and_a_finished_seat() {
    let store = new_store();
    running_seat(&store, "S-claimed");
    let cut = anchor(&store, "S-claimed");
    assert!(
        store
            .touch_seat_lease_if_anchors(&seat("S-claimed"), &cut, std::slice::from_ref(&cut))
            .unwrap()
    );
    assert!(
        store
            .claim_expired_seat_lease("S-claimed", i64::MAX)
            .unwrap()
    );
    assert_eq!(
        store
            .with_exact_seat_action_if_anchor(&seat("S-claimed"), &cut, || Ok(()))
            .unwrap(),
        None
    );
    finish(&store, "S-claimed");
    let done = anchor(&store, "S-claimed");
    assert_eq!(
        store
            .with_exact_seat_action_if_anchor(&seat("S-claimed"), &done, || Ok(()))
            .unwrap(),
        None
    );
}

#[test]
fn seat_lease_renewal_is_conditional_on_the_cut() {
    let store = new_store();
    running_seat(&store, "S-touch");
    let fresh = anchor(&store, "S-touch");
    let cut = [fresh.clone()];
    assert!(!lease_active(&store, "S-touch"));
    assert!(
        store
            .touch_seat_lease_if_anchors(&seat("S-touch"), &fresh, &cut)
            .unwrap()
    );
    assert!(lease_active(&store, "S-touch"));
    // A different seat identity renews nothing.
    store.remove_seat_lease("S-touch").unwrap();
    assert!(
        !store
            .touch_seat_lease_if_anchors(
                &ExactSeat {
                    owner: "fp:other",
                    ..seat("S-touch")
                },
                &fresh,
                &cut
            )
            .unwrap()
    );
    assert!(!lease_active(&store, "S-touch"));
    push(&store, "S-touch", "moved");
    assert!(
        !store
            .touch_seat_lease_if_anchors(&seat("S-touch"), &fresh, &cut)
            .unwrap()
    );
    assert!(!lease_active(&store, "S-touch"));
    // A mixed cut with a retired dependency gates the same way.
    running_seat(&store, "S-gone");
    finish(&store, "S-gone");
    let retired = retire(&store, "S-gone");
    let current = anchor(&store, "S-touch");
    let mixed = SeatCut {
        expected: &current,
        anchors: std::slice::from_ref(&current),
        retired: std::slice::from_ref(&retired),
    };
    assert!(
        store
            .touch_seat_lease_if_mixed_cut(&seat("S-touch"), &mixed)
            .unwrap()
    );
    assert!(lease_active(&store, "S-touch"));
}

#[test]
fn closed_lease_claim_needs_the_anchored_closing_tip() {
    let store = new_store();
    running_seat(&store, "S-close");
    push(&store, "S-close", "closing");
    let cut = anchor(&store, "S-close");
    let at_cut = SeatCut {
        expected: &cut,
        anchors: std::slice::from_ref(&cut),
        retired: &[],
    };
    assert!(
        store
            .touch_seat_lease_if_anchors(&seat("S-close"), &cut, std::slice::from_ref(&cut))
            .unwrap()
    );
    // The tip is not the closing event: nothing is claimed.
    assert!(
        !store
            .claim_closed_seat_lease_if_tip(&seat("S-close"), &at_cut, |_| Ok(false))
            .unwrap()
    );
    assert!(!lease_claimed(&store, "S-close"));
    // The judge sees the exact retained tip.
    assert!(
        store
            .claim_closed_seat_lease_if_tip(&seat("S-close"), &at_cut, |tip| {
                Ok(tip.thread_id == "S-close"
                    && tip.event_type == "test.seat.event"
                    && tip.payload == json!({"tag": "closing"})
                    && tip.thread_seq == tip_seq(&store, "S-close"))
            })
            .unwrap()
    );
    assert!(lease_claimed(&store, "S-close"));
    // Claiming again keeps the first marker and still reports a claim.
    assert!(
        store
            .claim_closed_seat_lease_if_tip(&seat("S-close"), &at_cut, |_| Ok(true))
            .unwrap()
    );

    // A later suffix means the old tip proves nothing.
    running_seat(&store, "S-late");
    push(&store, "S-late", "closing");
    let stale = anchor(&store, "S-late");
    store
        .touch_seat_lease_if_anchors(&seat("S-late"), &stale, std::slice::from_ref(&stale))
        .unwrap();
    push(&store, "S-late", "after");
    let stale_cut = SeatCut {
        expected: &stale,
        anchors: std::slice::from_ref(&stale),
        retired: &[],
    };
    assert!(
        !store
            .claim_closed_seat_lease_if_tip(&seat("S-late"), &stale_cut, |_| Ok(true))
            .unwrap()
    );
    assert!(!lease_claimed(&store, "S-late"));
    // A different lease identity is not claimed.
    running_seat(&store, "S-id");
    push(&store, "S-id", "closing");
    let id_cut = anchor(&store, "S-id");
    store
        .touch_seat_lease_if_anchors(&seat("S-id"), &id_cut, std::slice::from_ref(&id_cut))
        .unwrap();
    let foreign = ExactSeat {
        owner: "fp:other",
        ..seat("S-id")
    };
    assert!(
        !store
            .claim_closed_seat_lease_if_tip(
                &foreign,
                &SeatCut {
                    expected: &id_cut,
                    anchors: std::slice::from_ref(&id_cut),
                    retired: &[]
                },
                |_| Ok(true)
            )
            .unwrap()
    );
    assert!(!lease_claimed(&store, "S-id"));
}

#[test]
fn terminal_lease_removal_needs_signed_terminality() {
    let store = new_store();
    running_seat(&store, "S-term");
    let cut = anchor(&store, "S-term");
    store
        .touch_seat_lease_if_anchors(&seat("S-term"), &cut, std::slice::from_ref(&cut))
        .unwrap();
    assert!(
        !store
            .remove_terminal_seat_lease_if_proved("S-term")
            .unwrap()
    );
    assert!(lease_active(&store, "S-term"));
    finish(&store, "S-term");
    assert!(
        store
            .remove_terminal_seat_lease_if_proved("S-term")
            .unwrap()
    );
    assert!(!lease_active(&store, "S-term"));
    assert!(!lease_claimed(&store, "S-term"));
    // A terminal chain of an unfenced kind never proves a seat.
    running(&store, "S-plain", "directive");
    finish(&store, "S-plain");
    assert!(
        !store
            .remove_terminal_seat_lease_if_proved("S-plain")
            .unwrap()
    );
    assert!(
        !store
            .remove_terminal_seat_lease_if_proved("S-absent")
            .unwrap()
    );
}

#[test]
fn seat_append_renews_the_lease_in_one_gated_step() {
    let store = new_store();
    running_seat(&store, "S-app");
    let fresh = anchor(&store, "S-app");
    let before = tip_seq(&store, "S-app");
    let CutAppendResult::Committed(records) = store
        .append_seat_events_and_renew_if_anchor(&seat("S-app"), &[event("one")], &fresh)
        .unwrap()
    else {
        panic!("append at the exact frontier commits");
    };
    assert_eq!(records.len(), 1);
    assert_eq!(tip_seq(&store, "S-app"), before + 1);
    assert!(lease_active(&store, "S-app"));

    // The same cut is now stale: a conflict that writes nothing.
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchor(&seat("S-app"), &[event("two")], &fresh)
            .unwrap(),
        CutAppendResult::Conflict
    ));
    assert_eq!(tip_seq(&store, "S-app"), before + 1);

    // A foreign owner is not the running seat.
    let current = anchor(&store, "S-app");
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchor(
                &ExactSeat {
                    owner: "fp:other",
                    ..seat("S-app")
                },
                &[event("three")],
                &current
            )
            .unwrap(),
        CutAppendResult::NotRunning
    ));
    assert_eq!(tip_seq(&store, "S-app"), before + 1);

    // A claimed lease fences the seat.
    assert!(store.claim_expired_seat_lease("S-app", i64::MAX).unwrap());
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchor(&seat("S-app"), &[event("four")], &current)
            .unwrap(),
        CutAppendResult::NotRunning
    ));
    assert_eq!(tip_seq(&store, "S-app"), before + 1);

    // Malformed batches are refused outright.
    assert!(
        store
            .append_seat_events_and_renew_if_anchor(&seat("S-app"), &[], &current)
            .is_err()
    );
    assert!(
        store
            .append_seat_events_and_renew_if_anchor(
                &seat("S-app"),
                &[NewEventRecord {
                    storage_class: "ephemeral".into(),
                    ..event("eph")
                }],
                &current
            )
            .is_err()
    );
}

#[test]
fn seat_append_with_a_paired_cut_checks_the_counterpart_and_retired_fences() {
    let store = new_store();
    running_seat(&store, "S-a");
    running_seat(&store, "S-b");
    let a = anchor(&store, "S-a");
    let b = anchor(&store, "S-b");
    let pair = [a.clone(), b.clone()];
    let base = tip_seq(&store, "S-a");

    // A moved counterpart refuses and writes nothing.
    push(&store, "S-b", "moved");
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchors(&seat("S-a"), &[event("x")], &a, &pair)
            .unwrap(),
        CutAppendResult::Conflict
    ));
    assert_eq!(tip_seq(&store, "S-a"), base);
    assert!(!lease_active(&store, "S-a"));

    let b = anchor(&store, "S-b");
    let pair = [a.clone(), b.clone()];
    // A foreign owner is refused inside the guarded append.
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchors(
                &ExactSeat {
                    owner: "fp:other",
                    ..seat("S-a")
                },
                &[event("x")],
                &a,
                &pair
            )
            .unwrap(),
        CutAppendResult::NotRunning
    ));
    assert_eq!(tip_seq(&store, "S-a"), base);
    assert!(!lease_active(&store, "S-a"));

    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchors(&seat("S-a"), &[event("y")], &a, &pair)
            .unwrap(),
        CutAppendResult::Committed(_)
    ));
    assert_eq!(tip_seq(&store, "S-a"), base + 1);
    assert!(lease_active(&store, "S-a"));
    // The counterpart is never modified.
    assert_eq!(anchor(&store, "S-b"), b);

    // Retired dependencies ride the same guarded append.
    running_seat(&store, "S-gone");
    finish(&store, "S-gone");
    let retired = retire(&store, "S-gone");
    let a = anchor(&store, "S-a");
    let pair = [a.clone(), b.clone()];
    let mixed = SeatCut {
        expected: &a,
        anchors: &pair,
        retired: std::slice::from_ref(&retired),
    };
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_mixed_cut(&seat("S-a"), &[event("z")], &mixed)
            .unwrap(),
        CutAppendResult::Committed(_)
    ));
    // Retired dependencies without a live counterpart are guarded by the
    // target's own frontier and the fence.
    let a = anchor(&store, "S-a");
    let base = tip_seq(&store, "S-a");
    let alone = SeatCut {
        expected: &a,
        anchors: std::slice::from_ref(&a),
        retired: std::slice::from_ref(&retired),
    };
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_mixed_cut(&seat("S-a"), &[event("w")], &alone)
            .unwrap(),
        CutAppendResult::Committed(_)
    ));
    assert_eq!(tip_seq(&store, "S-a"), base + 1);
    // The same cut is now stale: the target moved, nothing is written.
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_mixed_cut(&seat("S-a"), &[event("v")], &alone)
            .unwrap(),
        CutAppendResult::Conflict
    ));
    assert_eq!(tip_seq(&store, "S-a"), base + 1);
    // A dependency that still has a live head is not a retired chain.
    let a = anchor(&store, "S-a");
    let headed = RetiredChainDependency {
        chain_root_id: "S-b".into(),
        manifest_hash: retired.manifest_hash.clone(),
    };
    let headed_cut = SeatCut {
        expected: &a,
        anchors: std::slice::from_ref(&a),
        retired: std::slice::from_ref(&headed),
    };
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_mixed_cut(&seat("S-a"), &[event("u")], &headed_cut)
            .unwrap(),
        CutAppendResult::Conflict
    ));
    assert_eq!(tip_seq(&store, "S-a"), base + 1);
}

#[test]
fn mixed_cut_append_admits_under_the_locks_and_refuses_when_the_cut_moves() {
    let store = new_store();
    running_seat(&store, "T-main");
    running_seat(&store, "T-side");
    let main = anchor(&store, "T-main");
    let side = anchor(&store, "T-side");
    let (m, s, evs) = (main, side, [event("a"), event("b")]);
    let base = tip_seq(&store, "T-main");
    let call = |m: &Anchor, s: &Anchor, evs: &[NewEventRecord], admit: bool| {
        store.append_events_if_mixed_cut(
            &MixedCutAppend {
                chain_root_id: "T-main",
                thread_id: "T-main",
                events: evs,
                expected: m,
                counterpart: Some(s),
                required_target_kind: FENCED_KIND,
                current_anchors: &[],
                retired: &[],
            },
            |view| {
                assert_eq!(view.target.thread_id, "T-main");
                assert_eq!(view.counterpart.unwrap().thread_id, "T-side");
                Ok(admit)
            },
        )
    };

    // Domain admission refuses: nothing written.
    assert!(matches!(
        call(&m, &s, &evs, false).unwrap(),
        CutAppendResult::Conflict
    ));
    assert_eq!(tip_seq(&store, "T-main"), base);

    let CutAppendResult::Committed(records) = call(&m, &s, &evs, true).unwrap() else {
        panic!("admitted append commits");
    };
    assert_eq!(records.len(), 2);
    assert_eq!(tip_seq(&store, "T-main"), base + 2);

    // The target's own cut is now stale.
    assert!(matches!(
        call(&m, &s, &evs, true).unwrap(),
        CutAppendResult::Conflict
    ));
    // A moved counterpart is stale too.
    let m = anchor(&store, "T-main");
    push(&store, "T-side", "moved");
    assert!(matches!(
        call(&m, &s, &evs, true).unwrap(),
        CutAppendResult::Conflict
    ));
    assert_eq!(tip_seq(&store, "T-main"), base + 2);

    // A target of the wrong kind is refused by the signed snapshot.
    let s = anchor(&store, "T-side");
    assert!(matches!(
        store
            .append_events_if_mixed_cut(
                &MixedCutAppend {
                    chain_root_id: "T-main",
                    thread_id: "T-main",
                    events: &evs,
                    expected: &m,
                    counterpart: Some(&s),
                    required_target_kind: "directive",
                    current_anchors: &[],
                    retired: &[],
                },
                |_| Ok(true)
            )
            .unwrap(),
        CutAppendResult::Conflict
    ));

    // A finished target is not a running target.
    finish(&store, "T-main");
    let m = anchor(&store, "T-main");
    assert!(matches!(
        call(&m, &s, &evs, true).unwrap(),
        CutAppendResult::NotRunning
    ));

    // Malformed batches are refused outright.
    running_seat(&store, "T-fresh");
    let f = anchor(&store, "T-fresh");
    assert!(call(&f, &s, &[], true).is_err());
}

#[test]
fn retained_event_reads_are_hash_exact() {
    let store = new_store();
    running_seat(&store, "S-evt");
    push(&store, "S-evt", "payload");
    let cut = anchor(&store, "S-evt");
    let (tip_hash, tip_seq) = cut.tip().expect("tip");
    let tip = store
        .retained_thread_event_by_hash(tip_hash)
        .unwrap()
        .expect("the anchored tip is retained");
    assert_eq!(tip.thread_id, "S-evt");
    assert_eq!(tip.thread_seq, tip_seq);
    assert_eq!(tip.payload, json!({"tag": "payload"}));
    assert!(
        store
            .retained_thread_event_by_hash(&"f".repeat(64))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .retained_thread_event_by_hash(&tip_hash.to_uppercase())
            .is_err()
    );
    assert!(store.retained_thread_event_by_hash("not-a-hash").is_err());
}

#[test]
fn authoritative_thread_result_comes_from_the_signed_snapshot() {
    let store = new_store();
    running_seat(&store, "S-res");
    // No terminal facts yet: nothing to report.
    assert!(
        store
            .get_thread_result_authoritative("S-res", "S-res", "running")
            .unwrap()
            .is_none()
    );
    finish(&store, "S-res");
    let record = store
        .get_thread_result_authoritative("S-res", "S-res", "completed")
        .unwrap()
        .expect("terminal result");
    assert_eq!(record.outcome_code.as_deref(), Some("completed"));
    assert_eq!(record.result, Some(json!({"answer": 42})));
    // The caller's authorization saw another status: refused.
    assert!(
        store
            .get_thread_result_authoritative("S-res", "S-res", "running")
            .is_err()
    );
    // A thread of another chain is not this chain's thread.
    assert!(
        store
            .get_thread_result_authoritative("S-other", "S-res", "completed")
            .is_err()
    );
}

fn wrong_closure(retired: &RetiredChainDependency) -> RetiredChainDependency {
    RetiredChainDependency {
        chain_root_id: retired.chain_root_id.clone(),
        manifest_hash: "cd".repeat(32),
    }
}

fn headed(id: &str, retired: &RetiredChainDependency) -> RetiredChainDependency {
    RetiredChainDependency {
        chain_root_id: id.into(),
        manifest_hash: retired.manifest_hash.clone(),
    }
}

#[test]
fn refused_seat_append_never_creates_or_renews_a_lease() {
    let store = new_store();
    running_seat(&store, "S-x");
    running_seat(&store, "S-y");
    running_seat(&store, "S-z");
    running_seat(&store, "S-gone");
    finish(&store, "S-gone");
    let retired = retire(&store, "S-gone");
    let old = anchor(&store, "S-x");
    push(&store, "S-x", "moved");
    let a = anchor(&store, "S-x");
    let y = anchor(&store, "S-y");
    let base = tip_seq(&store, "S-x");
    let try_append = |cut: &SeatCut<'_>| {
        store.append_seat_events_and_renew_if_mixed_cut(&seat("S-x"), &[event("n")], cut)
    };
    let untouched = || {
        assert_eq!(tip_seq(&store, "S-x"), base);
        assert!(!lease_active(&store, "S-x"));
    };

    // A stale own frontier, on the single and the paired path.
    assert!(matches!(
        try_append(&SeatCut {
            expected: &old,
            anchors: std::slice::from_ref(&old),
            retired: &[],
        })
        .unwrap(),
        CutAppendResult::Conflict
    ));
    untouched();
    let pair = [a.clone(), y.clone()];
    assert!(matches!(
        try_append(&SeatCut {
            expected: &old,
            anchors: &[old.clone(), y.clone()],
            retired: &[],
        })
        .unwrap(),
        CutAppendResult::Conflict
    ));
    untouched();

    // A retired dependency that still has a head is a conflict; one whose
    // fence names another closure is an error. Neither renews the lease.
    let live = headed("S-z", &retired);
    for anchors in [std::slice::from_ref(&a), pair.as_slice()] {
        assert!(matches!(
            try_append(&SeatCut {
                expected: &a,
                anchors,
                retired: std::slice::from_ref(&live),
            })
            .unwrap(),
            CutAppendResult::Conflict
        ));
        untouched();
        let wrong = wrong_closure(&retired);
        assert!(
            try_append(&SeatCut {
                expected: &a,
                anchors,
                retired: std::slice::from_ref(&wrong),
            })
            .is_err()
        );
        untouched();
    }

    // A second, different anchor for the seat's own chain is malformed on
    // both paths.
    assert!(
        try_append(&SeatCut {
            expected: &a,
            anchors: &[a.clone(), old.clone()],
            retired: &[],
        })
        .is_err()
    );
    assert!(
        try_append(&SeatCut {
            expected: &a,
            anchors: &[a.clone(), old.clone(), y.clone()],
            retired: &[],
        })
        .is_err()
    );
    untouched();

    // A claimed lease fences the paired path too.
    store
        .touch_seat_lease_if_anchors(&seat("S-x"), &a, std::slice::from_ref(&a))
        .unwrap();
    assert!(store.claim_expired_seat_lease("S-x", i64::MAX).unwrap());
    assert!(matches!(
        try_append(&SeatCut {
            expected: &a,
            anchors: &pair,
            retired: &[],
        })
        .unwrap(),
        CutAppendResult::NotRunning
    ));
    assert_eq!(tip_seq(&store, "S-x"), base);

    // A finished seat and a running chain of an unfenced kind are not seats.
    running(&store, "S-plain", "directive");
    let plain = anchor(&store, "S-plain");
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchor(&seat("S-plain"), &[event("n")], &plain)
            .unwrap(),
        CutAppendResult::NotRunning
    ));
    assert!(!lease_active(&store, "S-plain"));
    finish(&store, "S-y");
    let y = anchor(&store, "S-y");
    let done = tip_seq(&store, "S-y");
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchor(&seat("S-y"), &[event("n")], &y)
            .unwrap(),
        CutAppendResult::NotRunning
    ));
    assert_eq!(tip_seq(&store, "S-y"), done);
    assert!(!lease_active(&store, "S-y"));
}

#[test]
fn a_stopping_seat_takes_no_append_touch_or_action() {
    let store = new_store();
    running_seat(&store, "S-stop");
    let a = anchor(&store, "S-stop");
    let base = tip_seq(&store, "S-stop");
    store
        .request_thread_stop("S-stop", crate::runtime_db::StopIntent::Cancel)
        .unwrap();
    assert!(matches!(
        store
            .append_seat_events_and_renew_if_anchor(&seat("S-stop"), &[event("n")], &a)
            .unwrap(),
        CutAppendResult::NotRunning
    ));
    assert!(
        !store
            .touch_seat_lease_if_anchors(&seat("S-stop"), &a, std::slice::from_ref(&a))
            .unwrap()
    );
    assert!(
        store
            .with_exact_seat_action_if_anchor(&seat("S-stop"), &a, || Ok(()))
            .unwrap()
            .is_none()
    );
    assert_eq!(tip_seq(&store, "S-stop"), base);
    assert!(!lease_active(&store, "S-stop"));
}

#[test]
fn retired_fences_gate_touch_claim_action_and_plain_mixed_append() {
    let store = new_store();
    running_seat(&store, "S-r");
    running_seat(&store, "S-live");
    running_seat(&store, "S-z");
    running_seat(&store, "S-gone");
    finish(&store, "S-gone");
    let retired = retire(&store, "S-gone");
    let a = anchor(&store, "S-r");
    let live = anchor(&store, "S-live");
    let on_head = headed("S-z", &retired);
    let wrong = wrong_closure(&retired);
    let base = tip_seq(&store, "S-r");

    for anchors in [std::slice::from_ref(&a), &[a.clone(), live.clone()][..]] {
        let cut = |dep| cut_over(&a, anchors, dep);
        // Headed: refused without side effects. Wrong closure: an error.
        assert!(
            !store
                .touch_seat_lease_if_mixed_cut(&seat("S-r"), &cut(&on_head))
                .unwrap()
        );
        assert!(
            store
                .touch_seat_lease_if_mixed_cut(&seat("S-r"), &cut(&wrong))
                .is_err()
        );
        assert!(
            store
                .with_exact_seat_action_if_mixed_cut(&seat("S-r"), &cut(&on_head), || Ok(()))
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .claim_closed_seat_lease_if_tip(&seat("S-r"), &cut(&on_head), |_| Ok(true))
                .unwrap()
        );
        assert!(
            store
                .claim_closed_seat_lease_if_tip(&seat("S-r"), &cut(&wrong), |_| Ok(true))
                .is_err()
        );
        assert!(!lease_active(&store, "S-r"));
        assert!(!lease_claimed(&store, "S-r"));
    }

    // A tip the caller does not accept, or one that is not the anchor's, claims
    // nothing; a cut whose expected anchor is not the seat root is malformed.
    let ok = SeatCut {
        expected: &a,
        anchors: std::slice::from_ref(&a),
        retired: std::slice::from_ref(&retired),
    };
    assert!(
        !store
            .claim_closed_seat_lease_if_tip(&seat("S-r"), &ok, |_| Ok(false))
            .unwrap()
    );
    let foreign = SeatCut {
        expected: &live,
        anchors: std::slice::from_ref(&live),
        retired: &[],
    };
    assert!(
        store
            .claim_closed_seat_lease_if_tip(&seat("S-r"), &foreign, |_| Ok(true))
            .is_err()
    );
    assert!(!lease_active(&store, "S-r"));

    // Under the right fence the same cut is admitted.
    assert!(
        store
            .touch_seat_lease_if_mixed_cut(&seat("S-r"), &ok)
            .unwrap()
    );
    assert!(
        store
            .with_exact_seat_action_if_mixed_cut(&seat("S-r"), &ok, || Ok(()))
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .claim_closed_seat_lease_if_tip(&seat("S-r"), &ok, |tip| Ok(tip.thread_id == "S-r"))
            .unwrap()
    );
    assert!(lease_claimed(&store, "S-r"));

    // The plain mixed append honours the same fences.
    running_seat(&store, "T-main");
    let m = anchor(&store, "T-main");
    let call = |dep: &RetiredChainDependency| {
        store.append_events_if_mixed_cut(
            &MixedCutAppend {
                chain_root_id: "T-main",
                thread_id: "T-main",
                events: &[event("m")],
                expected: &m,
                counterpart: None,
                required_target_kind: FENCED_KIND,
                current_anchors: &[],
                retired: std::slice::from_ref(dep),
            },
            |view| {
                assert!(view.counterpart.is_none());
                Ok(true)
            },
        )
    };
    let main_base = tip_seq(&store, "T-main");
    assert!(matches!(call(&on_head).unwrap(), CutAppendResult::Conflict));
    assert!(call(&wrong).is_err());
    assert_eq!(tip_seq(&store, "T-main"), main_base);
    assert!(matches!(
        call(&retired).unwrap(),
        CutAppendResult::Committed(_)
    ));
    assert_eq!(tip_seq(&store, "T-main"), main_base + 1);
    assert!(matches!(call(&retired).unwrap(), CutAppendResult::Conflict));
    assert_eq!(tip_seq(&store, "T-main"), main_base + 1);
    assert_eq!(tip_seq(&store, "S-r"), base);
}

#[test]
fn an_append_error_resolves_from_signed_history() {
    let store = new_store();
    running_seat(&store, "E-1");
    let batch = [event("one")];
    let attempted = StateStore::durable_cut_events(&batch, "E-1", "E-1").unwrap();
    let predecessor = || {
        store
            .lock()
            .unwrap()
            .state_db
            .read_authoritative_thread_append_anchor("E-1", "E-1")
            .unwrap()
            .unwrap()
    };
    let resolve = |predecessor: &_| {
        let g = store.lock().unwrap();
        StateStore::cut_append_result(
            &g,
            "E-1",
            "E-1",
            predecessor,
            &attempted,
            &batch,
            Err(anyhow!("boom")),
        )
    };
    let base = tip_seq(&store, "E-1");

    // Nothing published: the original error.
    let before = predecessor();
    let error = resolve(&before).unwrap_err();
    assert_eq!(error.to_string(), "boom");
    assert!(error.downcast_ref::<EventAppendCommitUncertain>().is_none());

    // The exact batch published before the error: committed, exactly once.
    push(&store, "E-1", "one");
    let Some(CutAppendResult::Committed(records)) = resolve(&before).unwrap() else {
        panic!("a published batch resolves to committed");
    };
    assert_eq!(records.len(), 1);
    assert_eq!(tip_seq(&store, "E-1"), base + 1);

    // A different batch at that position cannot be told from ours.
    let before = predecessor();
    push(&store, "E-1", "other");
    let error = resolve(&before).unwrap_err();
    assert!(error.downcast_ref::<EventAppendCommitUncertain>().is_some());
    assert_eq!(tip_seq(&store, "E-1"), base + 2);
}

fn cut_over<'a>(
    expected: &'a Anchor,
    anchors: &'a [Anchor],
    dep: &'a RetiredChainDependency,
) -> SeatCut<'a> {
    SeatCut {
        expected,
        anchors,
        retired: std::slice::from_ref(dep),
    }
}
