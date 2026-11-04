//! Resource scratch, in-process birth, continuation and link tests.
use super::*;

#[test]
fn continuation_handoff_persists_created_successor_with_inherited_pin() {
    let store = test_store();
    let resume = continuation_resume_context(ProjectContext::LocalPath {
        path: PathBuf::from("/work/project"),
    });
    let launch_metadata = crate::launch_metadata::RuntimeLaunchMetadata::default()
        .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
        .with_resume_context(resume.clone());
    store
        .create_root_thread_with_events_and_launch_metadata(
            &thread_record("T-root", "T-root"),
            Vec::new(),
            Some(&launch_metadata),
        )
        .expect("root thread");

    let mut successor = thread_record("T-successor", "T-root");
    successor.project_root = Some(PathBuf::from("/work/project"));
    successor.base_project_snapshot_hash = Some("a".repeat(64));
    successor.project_authority = resume.project_authority.clone();
    let outcome = store
        .create_or_get_continuation_for_test(
            &successor,
            "T-root",
            "T-root",
            Some("follow"),
            "request-fingerprint",
            Some(&resume),
        )
        .expect("create continuation successor");
    assert!(matches!(outcome, ContinuationOutcome::Created { .. }));

    let projected = store
        .get_thread("T-successor")
        .expect("read successor")
        .expect("successor row");
    assert_eq!(projected.status, ThreadStatus::Created.as_str());
    assert_eq!(projected.upstream_thread_id.as_deref(), Some("T-root"));

    let inner = store.lock().expect("lock state store");
    let persisted = authoritative_snapshot_for_transition(&inner, "T-root", "T-successor")
        .expect("read authoritative successor snapshot");
    assert_eq!(persisted.base_project_snapshot_hash, Some("a".repeat(64)));
}

#[test]
fn exact_child_link_replay_after_parent_continues_does_not_cancel_child() {
    let store = test_store();
    store
        .create_thread_for_test(&thread_record("T-parent", "T-parent"))
        .expect("parent thread");
    store
        .create_thread_for_test(&thread_record("T-child", "T-child"))
        .expect("child thread");

    assert_eq!(
        store
            .record_child_link("T-parent", "T-child", "dispatch")
            .expect("initial child link"),
        None
    );
    store
        .finalize_thread(
            "T-parent",
            &FinalizeThreadRecord {
                status: ThreadStatus::Continued.as_str().to_string(),
                outcome_code: Some(ThreadStatus::Continued.as_str().to_string()),
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .expect("continue parent");

    assert_eq!(
        store
            .record_child_link("T-parent", "T-child", "dispatch")
            .expect("exact link replay"),
        None
    );
    let child = store
        .get_thread("T-child")
        .expect("read child")
        .expect("child row");
    assert_eq!(child.runtime.stop_intent, None);
}

#[test]
fn fresh_dispatch_link_after_parent_terminal_atomically_cancels_child() {
    let store = test_store();
    store
        .create_thread_for_test(&thread_record("T-parent", "T-parent"))
        .expect("parent thread");
    store
        .create_thread_for_test(&thread_record("T-child", "T-child"))
        .expect("child thread");
    store
        .finalize_thread(
            "T-parent",
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_string(),
                outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .expect("complete parent");

    assert_eq!(
        store
            .record_child_link("T-parent", "T-child", "dispatch")
            .expect("late child link"),
        Some(StopIntent::Cancel)
    );
    let child = store
        .get_thread("T-child")
        .expect("read child")
        .expect("child row");
    assert_eq!(child.runtime.stop_intent, Some(StopIntent::Cancel));
}

#[test]
fn exact_child_link_replay_propagates_durable_parent_kill() {
    let store = test_store();
    store
        .create_thread_for_test(&thread_record("T-parent", "T-parent"))
        .expect("parent thread");
    store
        .create_thread_for_test(&thread_record("T-child", "T-child"))
        .expect("child thread");
    store
        .record_child_link("T-parent", "T-child", "dispatch")
        .expect("initial child link");
    store
        .request_thread_stop("T-parent", StopIntent::Kill)
        .expect("kill parent");

    assert_eq!(
        store
            .record_child_link("T-parent", "T-child", "dispatch")
            .expect("exact link replay"),
        Some(StopIntent::Kill)
    );
    let child = store
        .get_thread("T-child")
        .expect("read child")
        .expect("child row");
    assert_eq!(child.runtime.stop_intent, Some(StopIntent::Kill));
}

#[test]
fn inherited_child_stop_rejects_active_in_process_owner_without_linking_or_tombstoning() {
    let store = test_store();
    let parent_id = "T-parent-stop-fenced";
    let child_id = "T-in-process-child-stop-fenced";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    let owner = store
        .register_in_process_handler(child_id)
        .expect("register in-process child owner");
    store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(child_id, child_id),
            vec![NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            }],
            &in_process_launch_metadata(),
            &owner,
        )
        .expect("publish in-process child");
    store
        .request_thread_stop(parent_id, StopIntent::Kill)
        .expect("tombstone parent");

    let error = store
        .record_child_link(parent_id, child_id, "dispatch")
        .expect_err("active in-process child cannot inherit undeclared stop authority");
    assert!(
        error
            .to_string()
            .contains("has no declared cancellation/drain contract")
    );
    let child = store
        .get_thread(child_id)
        .expect("read fenced child")
        .expect("fenced child row");
    assert_eq!(child.status, ThreadStatus::Running.as_str());
    assert_eq!(child.runtime.stop_intent, None);
    assert!(
        store
            .descendant_thread_ids(parent_id)
            .expect("read parent descendants")
            .is_empty()
    );
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("unregister in-process child owner")
    );
}

#[test]
fn in_process_child_birth_links_the_child_to_its_live_parent() {
    let store = test_store();
    let parent_id = "T-in-process-parent-live";
    let child_id = "T-in-process-child-live";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    let owner = store
        .register_in_process_handler(child_id)
        .expect("register in-process child owner");
    let _birth = store
        .create_in_process_child_with_events_and_launch_metadata(
            &in_process_child_record(child_id, parent_id),
            in_process_child_birth_events(),
            &in_process_launch_metadata(),
            &owner,
        )
        .expect("publish in-process child");

    assert_eq!(
        store
            .descendant_thread_ids(parent_id)
            .expect("read parent descendants"),
        [child_id]
    );
    assert_eq!(
        in_process_reservation_phase(&store, child_id),
        Some(runtime_db::InProcessHandlerReservationPhase::Running)
    );
    assert_eq!(
        store
            .in_process_handler_chain_root_id(child_id)
            .expect("child runtime row names its chain"),
        parent_id
    );
    // The birth's own link is replayed exactly, with no stop to inherit.
    assert_eq!(
        store
            .record_child_link(parent_id, child_id, "dispatch")
            .expect("exact replay of the birth link"),
        None
    );
    let error = store
        .record_child_link("T-some-other-parent", child_id, "dispatch")
        .expect_err("an in-process member is linked only to its upstream");
    assert!(error.to_string().contains("was born under"));
    let child = store
        .get_thread(child_id)
        .expect("read child")
        .expect("child row");
    assert_eq!(child.status, ThreadStatus::Running.as_str());
    assert_eq!(child.runtime.stop_intent, None);
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("unregister in-process child owner")
    );
}

#[test]
fn recorded_thread_placement_proves_in_process_lineage() {
    let store = test_store();
    let parent_id = "T-placement-parent";
    let child_id = "T-placement-inline-child";
    let plain_member_id = "T-placement-plain-member";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    let owner = birth_in_process_child(&store, parent_id, child_id);
    store
        .create_thread_for_test(&in_process_child_record(plain_member_id, parent_id))
        .expect("plain chain member");

    assert_eq!(
        store
            .recorded_thread_placement(parent_id)
            .expect("place root"),
        RecordedThreadPlacement::Root
    );
    assert_eq!(
        store
            .recorded_thread_placement(child_id)
            .expect("place inline child"),
        RecordedThreadPlacement::InProcessMember {
            chain_root_id: parent_id.to_owned(),
            upstream_thread_id: parent_id.to_owned(),
        }
    );
    assert_eq!(
        store
            .recorded_thread_placement(plain_member_id)
            .expect("place plain member"),
        RecordedThreadPlacement::OtherMember {
            chain_root_id: parent_id.to_owned(),
        }
    );
    assert!(store.recorded_thread_placement("T-absent").is_err());
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("unregister in-process child owner")
    );
}

#[test]
fn in_process_child_link_replay_refuses_once_shutdown_closed_admission() {
    let store = test_store();
    let parent_id = "T-in-process-parent-shutdown";
    let child_id = "T-in-process-child-shutdown";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    let (owner, unspawned) = birth_unspawned_in_process_child(&store, parent_id, child_id);
    store
        .close_process_attachment_admission()
        .expect("close shutdown admission");

    let error = store
        .link_unspawned_in_process_child(unspawned)
        .expect_err("a closed gate must refuse the pre-spawn replay");
    assert!(
        error.to_string().contains("closed for daemon shutdown"),
        "unexpected refusal: {error:#}"
    );
    // The owner settles the unstarted child through the closed gate.
    store
        .finalize_in_process_handler_owned(child_id, &owner, &requested_failure())
        .expect("owner settles the unstarted child");
    assert_eq!(
        store
            .get_thread(child_id)
            .expect("read child")
            .expect("child row")
            .status,
        ThreadStatus::Failed.as_str()
    );
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("unregister in-process child owner")
    );
}

#[test]
fn in_process_child_link_replay_inherits_a_stop_landed_after_birth() {
    let store = test_store();
    let parent_id = "T-in-process-parent-late-stop";
    let child_id = "T-in-process-child-late-stop";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    let (owner, unspawned) = birth_unspawned_in_process_child(&store, parent_id, child_id);
    assert_eq!(
        store
            .get_thread(child_id)
            .expect("read child")
            .expect("child row")
            .runtime
            .stop_intent,
        None
    );
    store
        .request_thread_stop(parent_id, StopIntent::Kill)
        .expect("tombstone parent after the birth");

    // A plain link request is a replay only: without the birth's
    // unspawned witness it never inherits a stop landed after the birth.
    assert_eq!(
        store
            .record_child_link(parent_id, child_id, "dispatch")
            .expect("plain replay after the birth"),
        None
    );
    assert_eq!(
        store
            .link_unspawned_in_process_child(unspawned)
            .expect("witnessed replay before the handler spawns"),
        Some(StopIntent::Kill)
    );
    // Inherited once, the stop is the child's own row and every later
    // replay reports it.
    assert_eq!(
        store
            .record_child_link(parent_id, child_id, "dispatch")
            .expect("replay after inheritance"),
        Some(StopIntent::Kill)
    );
    store
        .finalize_in_process_handler_owned(child_id, &owner, &requested_failure())
        .expect("owner settles the unstarted child");
    assert_eq!(
        store
            .get_thread(child_id)
            .expect("read child")
            .expect("child row")
            .status,
        ThreadStatus::Killed.as_str()
    );
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("unregister in-process child owner")
    );
}

#[test]
fn in_process_child_born_under_a_stopped_parent_inherits_the_stop() {
    let store = test_store();
    let parent_id = "T-in-process-parent-stopped";
    let child_id = "T-in-process-child-stopped";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    store
        .request_thread_stop(parent_id, StopIntent::Cancel)
        .expect("tombstone parent");
    let owner = store
        .register_in_process_handler(child_id)
        .expect("register in-process child owner");
    let _birth = store
        .create_in_process_child_with_events_and_launch_metadata(
            &in_process_child_record(child_id, parent_id),
            in_process_child_birth_events(),
            &in_process_launch_metadata(),
            &owner,
        )
        .expect("a live parent with a stop still births its child");

    // Link and inherited stop landed together with the birth.
    assert_eq!(
        store
            .descendant_thread_ids(parent_id)
            .expect("read parent descendants"),
        [child_id]
    );
    assert_eq!(
        store
            .get_thread(child_id)
            .expect("read child")
            .expect("child row")
            .runtime
            .stop_intent,
        Some(StopIntent::Cancel)
    );
    // The birth caller's link request reports the inherited stop instead
    // of refusing the in-process child.
    assert_eq!(
        store
            .record_child_link(parent_id, child_id, "dispatch")
            .expect("replay reports the inherited stop"),
        Some(StopIntent::Cancel)
    );
    // Any terminal the owner writes settles to the inherited stop.
    let outcome = store
        .finalize_in_process_handler_owned(child_id, &owner, &requested_failure())
        .expect("owner settles the newborn child");
    assert!(matches!(
        outcome,
        FinalizeIfNonterminalOutcome::Finalized { .. }
    ));
    let child = store
        .get_thread(child_id)
        .expect("read child")
        .expect("child row");
    assert_eq!(child.status, ThreadStatus::Cancelled.as_str());
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("unregister in-process child owner")
    );
}

#[test]
fn in_process_child_birth_refuses_a_terminal_parent_before_any_write() {
    let store = test_store();
    let parent_id = "T-in-process-parent-terminal";
    let child_id = "T-in-process-child-refused";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    store
        .finalize_thread(
            parent_id,
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_string(),
                outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .expect("complete parent");
    let owner = store
        .register_in_process_handler(child_id)
        .expect("register in-process child owner");

    let error = store
        .create_in_process_child_with_events_and_launch_metadata(
            &in_process_child_record(child_id, parent_id),
            in_process_child_birth_events(),
            &in_process_launch_metadata(),
            &owner,
        )
        .expect_err("a terminal parent cannot acquire a live child");
    assert!(
        error.to_string().contains("under terminal parent"),
        "unexpected refusal: {error:#}"
    );
    assert!(!owner.has_committed_birth());
    assert_eq!(in_process_reservation_phase(&store, child_id), None);
    assert!(store.in_process_handler_chain_root_id(child_id).is_err());
    assert!(store.get_thread(child_id).expect("read child").is_none());
    assert!(
        store
            .descendant_thread_ids(parent_id)
            .expect("read parent descendants")
            .is_empty()
    );
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("unregister in-process child owner")
    );
}

#[test]
fn in_process_child_interrupted_before_its_link_is_relinked_by_reconcile() {
    let store = test_store();
    let parent_id = "T-in-process-parent-crash";
    let child_id = "T-in-process-child-crash";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    let owner = store
        .register_in_process_handler(child_id)
        .expect("register in-process child owner");
    store
        .fixture_create_in_process_child_interrupted_before_link(
            &in_process_child_record(child_id, parent_id),
            in_process_child_birth_events(),
            &in_process_launch_metadata(),
            &owner,
        )
        .expect("publish in-process child without its link");
    assert!(
        store
            .descendant_thread_ids(parent_id)
            .expect("read parent descendants")
            .is_empty()
    );
    assert_eq!(
        in_process_reservation_phase(&store, child_id),
        Some(runtime_db::InProcessHandlerReservationPhase::Pending)
    );
    // The daemon dies: the owner is gone and the parent is stopped before
    // boot reconciliation reaches the published pending birth.
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("drop in-process child owner")
    );
    store
        .request_thread_stop(parent_id, StopIntent::Kill)
        .expect("tombstone parent");

    store
        .reconcile_in_process_handler_birth_running(child_id)
        .expect("reconcile the published pending birth");
    assert_eq!(
        store
            .descendant_thread_ids(parent_id)
            .expect("read parent descendants"),
        [child_id]
    );
    assert_eq!(
        in_process_reservation_phase(&store, child_id),
        Some(runtime_db::InProcessHandlerReservationPhase::Running)
    );
    // No handler ran for the ownerless member, so it inherits the stop.
    assert_eq!(
        store
            .get_thread(child_id)
            .expect("read child")
            .expect("child row")
            .runtime
            .stop_intent,
        Some(StopIntent::Kill)
    );
}

#[test]
fn terminal_in_process_child_without_its_link_is_relinked_at_settlement() {
    let store = test_store();
    let parent_id = "T-in-process-parent-settle";
    let child_id = "T-in-process-child-settle";
    store
        .create_thread_for_test(&thread_record(parent_id, parent_id))
        .expect("parent thread");
    let owner = store
        .register_in_process_handler(child_id)
        .expect("register in-process child owner");
    store
        .fixture_create_in_process_child_interrupted_before_link(
            &in_process_child_record(child_id, parent_id),
            in_process_child_birth_events(),
            &in_process_launch_metadata(),
            &owner,
        )
        .expect("publish in-process child without its link");
    store
        .finalize_in_process_handler_owned(child_id, &owner, &requested_failure())
        .expect("owner settles its child");
    assert!(
        store
            .descendant_thread_ids(parent_id)
            .expect("read parent descendants")
            .is_empty()
    );

    assert!(
        store
            .settle_in_process_handler_reservation_owned(child_id, &owner)
            .expect("settle terminal reservation")
    );
    assert_eq!(
        store
            .descendant_thread_ids(parent_id)
            .expect("read parent descendants"),
        [child_id]
    );
    let child = store
        .get_thread(child_id)
        .expect("read child")
        .expect("child row");
    assert_eq!(child.status, ThreadStatus::Failed.as_str());
    assert_eq!(child.runtime.stop_intent, None);
    assert!(
        store
            .unregister_in_process_handler(child_id, &owner)
            .expect("unregister in-process child owner")
    );
}

#[test]
fn trace_branch_does_not_project_ordinary_upstream_edge() {
    let store = test_store();
    store
        .create_thread_for_test(&thread_record("T-root", "T-root"))
        .expect("root thread");

    let branch = thread_record("T-branch", "T-root");
    let persisted = store
        .create_trace_branch(
            &branch,
            json!({
                "relation": "trace_branch",
                "child_thread_id": "T-branch",
                "parent_event_ref": {"chain_root_id": "T-root"},
                "state_anchor_ref": {"chain_root_id": "T-root"}
            }),
        )
        .expect("trace branch");

    let child = store
        .get_thread("T-branch")
        .expect("get branch")
        .expect("branch thread");
    assert_eq!(child.upstream_thread_id, None);
    assert!(store.list_chain_edges("T-root").expect("edges").is_empty());
    assert_eq!(persisted.len(), 2);
    assert_eq!(
        persisted[1].event_type,
        ryeos_state::event_types::EDGE_RECORDED
    );
    assert_eq!(persisted[1].payload["relation"], json!("trace_branch"));
}

#[test]
fn trace_branch_duplicate_explicit_child_id_does_not_append_events() {
    let store = test_store();
    store
        .create_thread_for_test(&thread_record("T-root", "T-root"))
        .expect("root thread");

    let branch = thread_record("T-branch", "T-root");
    store
        .create_trace_branch(
            &branch,
            json!({
                "relation": "trace_branch",
                "child_thread_id": "T-branch",
                "parent_event_ref": {"chain_root_id": "T-root"},
                "state_anchor_ref": {"chain_root_id": "T-root"}
            }),
        )
        .expect("trace branch");

    let head_after_first = {
        let g = store.lock().expect("lock");
        g.state_db
            .read_generic_head_ref("chains", "T-root")
            .expect("read chain head")
            .expect("chain head")
            .target_hash
    };

    let err = store
        .create_trace_branch(
            &branch,
            json!({
                "relation": "trace_branch",
                "child_thread_id": "T-branch",
                "parent_event_ref": {"chain_root_id": "T-root"},
                "state_anchor_ref": {"chain_root_id": "T-root"}
            }),
        )
        .expect_err("duplicate child id should fail");

    assert!(
        err.to_string().contains("already exists"),
        "unexpected error: {err:#}"
    );

    let head_after_duplicate = {
        let g = store.lock().expect("lock");
        g.state_db
            .read_generic_head_ref("chains", "T-root")
            .expect("read chain head")
            .expect("chain head")
            .target_hash
    };
    assert_eq!(head_after_duplicate, head_after_first);

    let events = store
        .replay_events("T-root", Some("T-branch"), None, 10, 1024 * 1024)
        .expect("branch replay");
    assert_eq!(events.events.len(), 2);
    assert_eq!(
        events.events[0].event_type,
        ryeos_state::event_types::THREAD_CREATED
    );
    assert_eq!(
        events.events[1].event_type,
        ryeos_state::event_types::EDGE_RECORDED
    );
}
#[cfg(target_os = "linux")]
#[test]
fn j4_no_birth_funding_receipt_requires_same_actual_original_scratch_owner() {
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let (scratch, original) = scratch_reservation_fixture(&store, "j4-no-birth-funding");
    store
        .reserve_process_resource_launch_with_scratch(&original, &scratch)
        .unwrap();
    let receipt = store
        .remove_never_born_scratch_for_funding(&original, &scratch)
        .unwrap();
    assert!(
        store
            .begin_trusted_process_resource_spawn(&original)
            .is_err()
    );
    receipt
        .matches_original(&Arc::clone(&store), &original)
        .unwrap();
    assert!(
        store
            .process_resource_reservation(&original.owner_kind, &original.owner_coordinate)
            .unwrap()
            .is_some()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn j4_no_birth_funding_receipt_refuses_other_state_and_changed_original() {
    for foreign_state in [false, true] {
        let store_owner = test_store();
        let store = Arc::clone(store_owner.store());
        let (scratch, original) = scratch_reservation_fixture(&store, "j4-no-birth-foreign");
        store
            .reserve_process_resource_launch_with_scratch(&original, &scratch)
            .unwrap();
        let receipt = store
            .remove_never_born_scratch_for_funding(&original, &scratch)
            .unwrap();
        let mut changed = original.clone();
        if !foreign_state {
            changed.owner_coordinate.push_str("-changed");
        }
        // One Store is live per process, so the other Store is its
        // identity alone: an owner that is not the minting Store.
        let refused = if foreign_state {
            receipt.matches_original_of(&std::sync::Weak::new(), &changed)
        } else {
            receipt.matches_original(&store, &changed)
        };
        assert!(refused.is_err());
        drop(store);
        store_owner.close().unwrap();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn j4_spawn_intent_cannot_mint_no_birth_funding_receipt() {
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let (scratch, original) = scratch_reservation_fixture(&store, "j4-contacted-no-refund");
    store
        .reserve_process_resource_launch_with_scratch(&original, &scratch)
        .unwrap();
    let permit = store
        .begin_trusted_process_resource_spawn(&original)
        .unwrap();
    assert!(
        store
            .remove_never_born_scratch_for_funding(permit.reservation(), &scratch)
            .is_err()
    );
    assert!(scratch.path().unwrap().exists());
    assert_eq!(
        store
            .process_resource_reservation(&original.owner_kind, &original.owner_coordinate)
            .unwrap()
            .as_ref(),
        Some(permit.reservation())
    );
}
