//! Launch planning, event readback, follow and terminal-evidence tests.
use super::*;

#[test]
fn launch_cancel_and_authoritative_continuation_bind_have_one_atomic_winner() {
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let source_thread_id = "T-continuation-bind-race-source";
    let successor_thread_id = "T-continuation-bind-race-successor";
    let mut source = thread_record(source_thread_id, source_thread_id);
    source.requested_by = Some("fp:owner".to_string());
    store
        .create_thread_for_test(&source)
        .expect("create continuation source");
    let launch_id = store
        .reserve_launch_planning(successor_thread_id, "fp:owner")
        .expect("reserve continuation planning");
    let barrier = Arc::new(std::sync::Barrier::new(3));

    let creator_store = store.clone();
    let creator_barrier = barrier.clone();
    let creator = std::thread::spawn(move || {
        creator_barrier.wait();
        let mut successor = thread_record(successor_thread_id, source_thread_id);
        successor.requested_by = Some("fp:owner".to_string());
        creator_store.create_continuation_for_test(
            &successor,
            source_thread_id,
            source_thread_id,
            Some("chained_resume"),
        )
    });

    let canceller_store = store.clone();
    let canceller_barrier = barrier.clone();
    let canceller = std::thread::spawn(move || {
        canceller_barrier.wait();
        canceller_store.cancel_launch_planning(&launch_id, "fp:owner")
    });

    barrier.wait();
    let created = creator.join().expect("creator thread");
    let cancelled = canceller
        .join()
        .expect("canceller thread")
        .expect("cancel resolution")
        .expect("owned planning record");
    let outcomes = (created, cancelled);
    match outcomes {
        (Ok(_), LaunchCancellationResolution::Bound { thread_id }) => {
            assert_eq!(thread_id, successor_thread_id);
            let source = store
                .get_thread(source_thread_id)
                .expect("read continued source")
                .expect("continuation source");
            assert_eq!(source.status, ThreadStatus::Continued.as_str());
            assert_eq!(
                source.successor_thread_id.as_deref(),
                Some(successor_thread_id)
            );
            assert!(
                store
                    .get_thread(successor_thread_id)
                    .expect("read published successor")
                    .is_some()
            );
        }
        (Err(error), LaunchCancellationResolution::Cancelled) => {
            assert!(
                error
                    .chain()
                    .any(|cause| cause.is::<LaunchPlanningInactive>()),
                "cancel-won continuation publication must retain typed planning inactivity: {error:#}"
            );
            let source = store
                .get_thread(source_thread_id)
                .expect("read unchanged source")
                .expect("continuation source");
            assert_eq!(source.status, ThreadStatus::Created.as_str());
            assert!(source.successor_thread_id.is_none());
            assert!(
                store
                    .get_thread(successor_thread_id)
                    .expect("read absent successor")
                    .is_none()
            );
        }
        (created, cancelled) => {
            panic!(
                "invalid continuation cancel-vs-bind race outcome: \
                 created={created:?}, cancelled={cancelled:?}"
            )
        }
    }
}

#[test]
fn direct_attachment_fork_proceeds_while_the_state_store_is_held() {
    // A held StateStore closes no fork window: the locks taken beneath
    // it are closed in every fork child, so a long hold must not stall
    // process creation on other threads.
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let guard = store.lock().expect("hold state store");
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let pending = lillux::spawn_awaiting_attachment(lillux::SubprocessRequest {
            cmd: "/bin/sh".to_string(),
            argv0: None,
            args: vec!["-c".to_string(), "exit 0".to_string()],
            cwd: None,
            envs: Vec::new(),
            stdin_data: None,
            timeout: 5.0,
            limits: None,
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        })
        .expect("spawn while the state store is held");
        sender.send(pending).expect("publish pending process");
    });

    let pending = receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("direct fork proceeds while the StateStore is held");
    drop(guard);
    pending.abort_and_reap().expect("abort held test target");
    worker.join().expect("spawn worker");
}

#[test]
fn direct_projection_access_fails_closed_while_repair_is_pending() {
    let store = test_store();
    ryeos_state::ProjectionRepairSink::request_repair(
        &*store.projection_health(),
        ryeos_state::ProjectionRepairRequest {
            chain_root_id: "T-root".into(),
            committed_head_hash: "head".into(),
            operation: "append_events",
            error: "projection failed".into(),
        },
    );
    let error = store
        .with_projection(|_| Ok(()))
        .expect_err("stale projection read must fail");
    assert!(error.to_string().contains("not current"));
}

#[test]
fn settlement_cost_boundary_rejects_invalid_values() {
    let valid = ryeos_engine::contracts::FinalCost {
        turns: 0,
        input_tokens: 1,
        output_tokens: 2,
        spend: ryeos_engine::launch_envelope_types::UsdNanos::parse_canonical("0.01").unwrap(),
        provider: None,
        basis: None,
        metadata: None,
    };
    assert!(validate_final_cost_for_settlement(&valid).is_ok());

    // Negative and non-finite spend are unrepresentable in `UsdNanos`;
    // the numeric-wire rejection is proven by the FinalCost contract test.
    for invalid in [
        ryeos_engine::contracts::FinalCost {
            input_tokens: i64::MAX as u64 + 1,
            ..valid.clone()
        },
        ryeos_engine::contracts::FinalCost {
            output_tokens: i64::MAX as u64 + 1,
            ..valid.clone()
        },
        ryeos_engine::contracts::FinalCost {
            basis: Some("estimated".to_string()),
            ..valid.clone()
        },
    ] {
        assert!(validate_final_cost_for_settlement(&invalid).is_err());
    }
}

#[test]
fn follow_terminal_admission_reserves_space_for_pending_children() {
    let waiter = follow_waiter_for_admission();
    let candidate = ryeos_runtime::envelope::encode_follow_terminal_envelope(
        "T-child",
        ryeos_runtime::envelope::RuntimeResultStatus::Completed,
        json!(1),
        None,
    )
    .unwrap();

    validate_prospective_follow_resume_payload(&waiter, "T-child", &candidate).unwrap();
}

#[test]
fn oversized_follow_terminal_is_replaced_before_persistence() {
    let waiter = follow_waiter_for_admission();
    let candidate = json!({
        "success": true,
        "child_thread_id": "T-terminal",
        "status": "completed",
        "result": "x".repeat(checkpoint_shape_limits().max_result_bytes),
        "outputs": null,
        "warnings": [],
        "cost": {
            "input_tokens": 11,
            "output_tokens": 7,
            "total_usd": "0.03",
        },
    });

    let (admitted, degraded) =
        admit_follow_terminal_envelope(&waiter, "T-child", "T-terminal", &candidate).unwrap();
    assert!(degraded);
    assert_eq!(
        admitted["projection"],
        ryeos_runtime::envelope::FOLLOW_ACTION_RESULT_PROJECTION
    );
    assert_eq!(admitted["success"], false);
    assert_eq!(admitted["child_thread_id"], "T-terminal");
    assert_eq!(admitted["result"]["code"], FOLLOW_ENVELOPE_LIMIT_CODE);
    assert_eq!(admitted["cost"]["input_tokens"], 11);
    assert_eq!(admitted["cost"]["output_tokens"], 7);
}

#[test]
fn graph_follow_cohort_retains_returns_without_private_child_state() {
    let mut waiter = follow_waiter_for_admission();
    waiter.expected_children = 12;
    waiter.children = (0..12)
        .map(|index| runtime_db::FollowWaiterChild {
            item_index: index,
            item_ref: "graph:farm/match".to_string(),
            spec_hash: format!("spec-{index}"),
            child_thread_id: format!("T-child-{index}"),
            child_chain_root_id: format!("T-child-{index}"),
            sealed_root_request:
                crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture(),
            terminal_thread_id: None,
            terminal_status: None,
            terminal_envelope: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        })
        .collect();

    for index in 0..12 {
        let child_thread_id = format!("T-child-{index}");
        let candidate = json!({
            "success": true,
            "child_thread_id": child_thread_id,
            "status": "completed",
            "result": {
                "success": true,
                "graph_id": "farm/match",
                "definition_ref": "graph:farm/match",
                "effective_definition_digest": "a".repeat(64),
                "graph_run_id": format!("gr-child-{index}"),
                "status": "completed",
                "steps": 31,
                "state": {"private_child_state": "x".repeat(512 * 1024)},
                "result": {"seed": index, "winner": "melon"},
                "node_costs": [],
                "hook_costs": [],
            },
            "outputs": null,
            "warnings": [],
            "cost": null,
        });
        validate_checkpoint_shape(&candidate, "large managed child terminal").unwrap();
        let (admitted, degraded) =
            admit_follow_terminal_envelope(&waiter, &child_thread_id, &child_thread_id, &candidate)
                .unwrap();
        assert!(!degraded, "child {index} was incorrectly degraded");
        assert_eq!(
            admitted["result"],
            json!({"seed": index, "winner": "melon"})
        );
        assert!(serde_json::to_vec(&admitted).unwrap().len() < 1024);
        waiter.children[index as usize].terminal_envelope = Some(admitted);
    }

    for child in &waiter.children {
        validate_prospective_follow_resume_payload(
            &waiter,
            &child.child_chain_root_id,
            child.terminal_envelope.as_ref().unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn follow_terminal_admission_rejects_mismatched_child_identity() {
    let waiter = follow_waiter_for_admission();
    let candidate = json!({
        "success": true,
        "child_thread_id": "T-other",
        "status": "completed",
        "result": 1,
        "outputs": null,
        "warnings": [],
        "cost": null,
    });
    let error = admit_follow_terminal_envelope(&waiter, "T-child", "T-terminal", &candidate)
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match terminal child"), "{error}");
}

#[test]
fn impossible_follow_cohort_is_rejected_before_reservation() {
    let seed = runtime_db::NewFollowWaiter {
        follow_key: "too-wide".to_string(),
        parent_thread_id: "T-parent".to_string(),
        parent_chain_root_id: "T-parent".to_string(),
        follow_node: "fanout".to_string(),
        graph_run_id: "run-1".to_string(),
        step_count: 1,
        frontier_id: None,
        fanout: true,
        expected_children: u32::MAX,
        child_project_authority: None,
    };

    let error = validate_follow_reservation_shape(&seed).unwrap_err();
    assert!(error.to_string().contains("maximum"));
}

#[test]
fn terminal_result_larger_than_event_is_retained_once_and_event_is_bounded() {
    let store = test_store();
    let thread_id = "T-large-terminal-result";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("create thread");
    let result = json!({
        "state": "x".repeat(ryeos_state::objects::MAX_THREAD_EVENT_SERIALIZED_BYTES)
    });
    let managed_envelope = crate::thread_lifecycle::managed_runtime_envelope(
        thread_id,
        ThreadStatus::Completed.as_str(),
        Some(&result),
        None,
        None,
        &json!({"answer": 42}),
        &["retained warning".to_string()],
    );

    store
        .finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_string(),
                outcome_code: Some("success".to_string()),
                result_json: Some(result.clone()),
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: Some(managed_envelope),
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .expect("large terminal result must not inflate the lifecycle event");

    let projected = store
        .get_thread_result(thread_id)
        .expect("read projected result")
        .expect("projected result exists");
    assert_eq!(projected.result.as_ref(), Some(&result));

    let events = store
        .replay_events(thread_id, Some(thread_id), None, 8, 1024 * 1024)
        .expect("replay terminal events")
        .events;
    let terminal = events
        .iter()
        .find(|event| event.event_type == ryeos_state::event_types::THREAD_COMPLETED)
        .expect("thread_completed event");
    assert!(terminal.payload.get("result").is_none());
    assert_eq!(terminal.payload["result_present"], true);
    assert!(terminal.payload["result_size_bytes"].as_u64().unwrap() > 512 * 1024);
    assert!(
        serde_json::to_vec(&terminal.payload).unwrap().len() < 1024,
        "terminal lifecycle payload must remain a summary"
    );

    let authority = store
        .get_thread_terminal_authority(thread_id)
        .expect("read terminal authority")
        .expect("terminal authority exists");
    let envelope = authority
        .managed_envelope
        .expect("managed envelope is reconstructed from its supplement");
    assert_eq!(envelope["result"], result);
    assert_eq!(envelope["outputs"], json!({"answer": 42}));
    assert_eq!(envelope["warnings"], json!(["retained warning"]));

    let inner = store.lock().expect("lock state store");
    let snapshot = authoritative_snapshot_for_transition(&inner, thread_id, thread_id)
        .expect("authoritative terminal snapshot");
    let supplement = snapshot
        .managed_runtime_terminal
        .expect("authoritative managed runtime supplement");
    assert_eq!(supplement.outputs, json!({"answer": 42}));
    assert_eq!(supplement.warnings, vec!["retained warning".to_string()]);
}

#[test]
fn large_terminal_error_is_retained_but_not_duplicated_into_the_event() {
    let store = test_store();
    let thread_id = "T-large-terminal-error";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("create thread");
    let error = json!({"error": "x".repeat(MAX_TERMINAL_EVENT_INLINE_ERROR_BYTES)});

    store
        .finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: ThreadStatus::Failed.as_str().to_string(),
                outcome_code: Some("large_failure".to_string()),
                result_json: None,
                error_json: Some(error.clone()),
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .expect("large terminal error remains durable outside the event");

    let projected = store
        .get_thread_result(thread_id)
        .expect("read projected error")
        .expect("projected error exists");
    assert_eq!(projected.error.as_ref(), Some(&error));

    let events = store
        .replay_events(thread_id, Some(thread_id), None, 8, 1024 * 1024)
        .expect("replay terminal events")
        .events;
    let terminal = events
        .iter()
        .find(|event| event.event_type == ryeos_state::event_types::THREAD_FAILED)
        .expect("thread_failed event");
    assert!(terminal.payload.get("error").is_none());
    assert_eq!(terminal.payload["error_omitted"], true);
    assert!(terminal.payload["error_digest"].as_str().is_some());
    assert!(serde_json::to_vec(&terminal.payload).unwrap().len() < 1024);
}

#[test]
fn oversized_terminal_content_refuses_before_mutating_the_chain() {
    let store = test_store();
    let thread_id = "T-oversized-terminal-result";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("create thread");
    let result = json!("x".repeat(ryeos_state::objects::MAX_THREAD_RESULT_CONTENT_BYTES));

    let error = store
        .finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_string(),
                outcome_code: Some("success".to_string()),
                result_json: Some(result),
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .expect_err("oversized terminal content must fail closed");
    assert!(error.to_string().contains("thread result"));

    let thread = store
        .get_thread(thread_id)
        .expect("read unchanged thread")
        .expect("thread remains present");
    assert_eq!(thread.status, ThreadStatus::Created.as_str());
    assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);
    assert!(
        store
            .get_thread_result(thread_id)
            .expect("read absent result")
            .is_none()
    );
}

#[test]
fn project_observation_exact_retry_returns_one_durable_event_and_divergence_refuses() {
    let store = test_store();
    let thread_id = "T-project-observation";
    running_in_process_test_root(&store, thread_id);
    let request = ryeos_runtime::ProjectObservationPublishParams {
        thread_id: thread_id.to_string(),
        graph_run_id: "G-campaign".to_string(),
        node: "classify".to_string(),
        step: 7,
        observation: ryeos_state::ProjectObservationRequest {
            namespace: "example.classification".to_string(),
            stable_id: "classification:game-1".to_string(),
            payload: json!({"status": "accepted", "score": 1}),
        },
    };
    let source_ref = "graph:example/campaign";
    let source_digest = "d".repeat(64);

    let first = store
        .publish_project_observation(&request, source_ref, &source_digest)
        .expect("publish project observation");
    assert!(first.inserted);
    let replay = store
        .publish_project_observation(&request, source_ref, &source_digest)
        .expect("fold exact project observation retry");
    assert!(!replay.inserted);
    assert_eq!(replay.event.event_hash, first.event.event_hash);
    assert_eq!(replay.event.chain_seq, first.event.chain_seq);

    let events = store
        .replay_events(thread_id, Some(thread_id), None, 16, 1024 * 1024)
        .expect("replay project observation evidence")
        .events
        .into_iter()
        .filter(|event| event.event_type == ryeos_state::event_types::PROJECT_OBSERVATION_RECORDED)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "exact retry must not append a duplicate");

    let mut changed_payload = request.clone();
    changed_payload.observation.payload = json!({"status": "rejected", "score": 0});
    assert!(
        store
            .publish_project_observation(&changed_payload, source_ref, &source_digest)
            .unwrap_err()
            .to_string()
            .contains("contradicts its durable identity")
    );

    let mut changed_occurrence = request;
    changed_occurrence.step = 8;
    assert!(
        store
            .publish_project_observation(&changed_occurrence, source_ref, &source_digest)
            .unwrap_err()
            .to_string()
            .contains("divergent durable event content")
    );
}

#[test]
fn provider_call_observation_exact_retry_folds_and_replay_stays_distinct() {
    let store = test_store();
    let thread_id = "T-provider-observation";
    running_in_process_test_root(&store, thread_id);
    let draft = ryeos_state::ProviderCallObservationDraft {
        turn: 2,
        attempt_number: 1,
        effect_coordinate_digest: "a".repeat(64),
        source: ryeos_state::ProviderCallObservationSource::Executed,
        answer_digest: "b".repeat(64),
        record_hash: "c".repeat(64),
        publication: ryeos_state::ProviderCallObservationPublication::Inserted,
        replayed_from: None,
    };

    let first = store
        .publish_provider_call_observation(thread_id, &draft)
        .expect("publish provider-call observation");
    assert!(first.inserted);
    let replay = store
        .publish_provider_call_observation(thread_id, &draft)
        .expect("fold provider-call observation retry");
    assert!(!replay.inserted);
    assert_eq!(replay.event.event_hash, first.event.event_hash);

    let events = store
        .replay_events(thread_id, Some(thread_id), None, 16, 1024 * 1024)
        .expect("replay provider-call observation")
        .events
        .into_iter()
        .filter(|event| {
            event.event_type == ryeos_state::event_types::PROVIDER_CALL_OBSERVATION_RECORDED
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);

    let replay = ryeos_state::ProviderCallObservationDraft {
        source: ryeos_state::ProviderCallObservationSource::Replay,
        publication: ryeos_state::ProviderCallObservationPublication::NotApplicable,
        replayed_from: Some(ryeos_state::ProviderCallReplaySource {
            produced_by_thread: thread_id.to_string(),
            attempt_id: "attempt-origin".to_string(),
        }),
        ..draft
    };
    let replay = store
        .publish_provider_call_observation(thread_id, &replay)
        .expect("publish same-coordinate replay observation");
    assert!(replay.inserted);
    assert_ne!(replay.event.event_hash, first.event.event_hash);

    let events = store
        .replay_events(thread_id, Some(thread_id), None, 16, 1024 * 1024)
        .expect("replay provider-call observations")
        .events
        .into_iter()
        .filter(|event| {
            event.event_type == ryeos_state::event_types::PROVIDER_CALL_OBSERVATION_RECORDED
        })
        .count();
    assert_eq!(events, 2);
}

#[test]
fn completed_observation_hook_appends_durable_daemon_evidence_on_replay() {
    let store = test_store();
    let thread_id = "T-hook-evidence";
    running_in_process_test_root(&store, thread_id);
    let identity = hook_observation_identity("hook:system/evidence");
    let seed = runtime_db::NewHookDispatch {
        seed_version: runtime_db::HOOK_DISPATCH_SEED_VERSION,
        dispatch_key: "a".repeat(64),
        chain_root_id: thread_id.to_string(),
        caller_thread_id: thread_id.to_string(),
        event: identity.occurrence.event().to_string(),
        hook_id: identity.hook_id.clone(),
        request_hash: "b".repeat(64),
    };
    assert!(matches!(
        store
            .reserve_hook_dispatch(&seed)
            .expect("reserve evidence hook"),
        runtime_db::HookDispatchReservation::Execute
    ));
    let response = json!({
        "thread": {"id": "T-hook-child", "status": "completed"},
        "result": {
            "kind": "build.step_completed",
            "payload": {"step": 3, "ok": true}
        },
        "dispatch": hook_test_dispatch_evidence()
    });
    store
        .complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
        .expect("complete evidence hook");
    let first = store
        .append_completed_hook_outcome(thread_id, &identity, &seed.dispatch_key, &seed.request_hash)
        .expect("append completed observation");
    assert_eq!(
        first.event_type,
        ryeos_state::event_types::HOOK_OBSERVATION_RECORDED
    );

    let runtime_db::HookDispatchReservation::Replay(replayed) = store
        .reserve_hook_dispatch(&seed)
        .expect("replay completed evidence hook")
    else {
        panic!("completed hook must replay");
    };
    let duplicate = store
        .append_completed_hook_outcome(
            thread_id,
            &identity,
            &replayed.dispatch_key,
            &replayed.request_hash,
        )
        .expect("append replayed observation");

    let events = store
        .replay_events(thread_id, Some(thread_id), None, 16, 1024 * 1024)
        .expect("replay durable hook evidence")
        .events
        .into_iter()
        .filter(|event| event.event_type == ryeos_state::event_types::HOOK_OBSERVATION_RECORDED)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2, "retry appends a foldable duplicate");
    let first_payload: ryeos_runtime::HookObservationRecordedPayload =
        serde_json::from_value(events[0].payload.clone()).expect("decode first observation");
    let duplicate_payload: ryeos_runtime::HookObservationRecordedPayload =
        serde_json::from_value(events[1].payload.clone()).expect("decode duplicate observation");
    assert_eq!(first_payload, duplicate_payload);
    assert_eq!(
        first_payload.observation_id,
        format!("hook-observation:{}", seed.dispatch_key)
    );
    assert_eq!(first_payload.occurrence_thread_id, thread_id);
    assert_eq!(first.payload, duplicate.payload);
}

#[test]
fn known_post_reservation_dispatch_failure_completes_and_replays_durable_integrity_evidence() {
    let store = test_store();
    let thread_id = "T-hook-known-failure";
    running_in_process_test_root(&store, thread_id);
    let identity = hook_observation_identity("hook:system/known-failure");
    let seed = runtime_db::NewHookDispatch {
        seed_version: runtime_db::HOOK_DISPATCH_SEED_VERSION,
        dispatch_key: "9".repeat(64),
        chain_root_id: thread_id.to_string(),
        caller_thread_id: thread_id.to_string(),
        event: identity.occurrence.event().to_string(),
        hook_id: identity.hook_id.clone(),
        request_hash: "8".repeat(64),
    };
    assert!(matches!(
        store.reserve_hook_dispatch(&seed).unwrap(),
        runtime_db::HookDispatchReservation::Execute
    ));
    let response =
        serde_json::to_value(ryeos_runtime::callback_contract::CallbackDispatchResponse {
            thread: Value::Null,
            result: ryeos_runtime::envelope::hook_dispatch_integrity_failure(
                "child dispatcher refused the admitted request",
            ),
            dispatch: ryeos_runtime::callback_contract::RuntimeDispatchEvidence {
                source: ryeos_runtime::callback_contract::RuntimeDispatchSource::Executed,
                effect_class: ryeos_runtime::callback_contract::RuntimeDispatchEffectClass::Live,
                action_digest: "7".repeat(64),
                effect_identity: None,
                publication:
                    ryeos_runtime::callback_contract::RuntimeDispatchPublication::NotApplicable,
                record_hash: None,
                replayed_from: None,
                result_projection:
                    ryeos_effect_contract::DispatchResultProjection::DispatchedSubject,
            },
        })
        .unwrap();
    store
        .complete_hook_dispatch(&seed.dispatch_key, &seed.request_hash, &response)
        .unwrap();
    let persisted = store
        .append_completed_hook_outcome(thread_id, &identity, &seed.dispatch_key, &seed.request_hash)
        .unwrap();
    assert_eq!(persisted.event_type, ryeos_state::event_types::HOOK_FAILED);
    let failure: ryeos_runtime::HookFailedPayload =
        serde_json::from_value(persisted.payload).unwrap();
    assert_eq!(
        failure.failure_class,
        ryeos_runtime::HookFailureClass::DispatchEnvelopeIntegrity
    );

    let runtime_db::HookDispatchReservation::Replay(replayed) =
        store.reserve_hook_dispatch(&seed).unwrap()
    else {
        panic!("known failure must complete rather than remain pending");
    };
    assert_eq!(replayed.response, response);
}

#[test]
fn terminal_occurrence_evidence_appends_to_the_live_successor() {
    let store = test_store();
    let predecessor = "T-hook-predecessor";
    let successor = "T-hook-successor";
    store
        .create_thread_for_test(&thread_record(predecessor, predecessor))
        .expect("create predecessor");
    store
        .mark_thread_running(predecessor, None)
        .expect("start predecessor");
    let mut successor_record = thread_record(successor, predecessor);
    successor_record.upstream_thread_id = Some(predecessor.to_string());
    store
        .create_thread_for_test(&successor_record)
        .expect("create successor");
    store
        .mark_thread_running(successor, None)
        .expect("start successor");
    store
        .finalize_thread(
            predecessor,
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
        .expect("settle predecessor as continued");

    let identity = ryeos_runtime::callback::HookDispatchIdentity {
        occurrence: ryeos_runtime::callback::HookDispatchOccurrence::new(
            "graph",
            "graph_completed",
            "graph:test/terminal-evidence",
            "r".repeat(64),
            "d".repeat(64),
        )
        .with_text_coordinate("graph_run_id", "G-terminal-evidence")
        .with_counter_coordinate("steps", 4),
        hook_id: "hook:system/terminal-evidence".to_string(),
        layer: ryeos_runtime::HookLayer::Infrastructure,
        result_mode: ryeos_runtime::HookResultMode::Observation,
        context_contract: ryeos_engine::hooks::HookContextContract {
            schema: ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA.to_string(),
            allowed_roots: std::collections::BTreeSet::from(["state".to_string()]),
        },
        context_hash: "c".repeat(64),
    };
    let seed = runtime_db::NewHookDispatch {
        seed_version: runtime_db::HOOK_DISPATCH_SEED_VERSION,
        dispatch_key: "e".repeat(64),
        chain_root_id: predecessor.to_string(),
        caller_thread_id: predecessor.to_string(),
        event: identity.occurrence.event().to_string(),
        hook_id: identity.hook_id.clone(),
        request_hash: "f".repeat(64),
    };
    assert!(matches!(
        store.reserve_hook_dispatch(&seed).unwrap(),
        runtime_db::HookDispatchReservation::Execute
    ));
    store
        .complete_hook_dispatch(
            &seed.dispatch_key,
            &seed.request_hash,
            &json!({
                "thread": {"id": "T-hook-child", "status": "completed"},
                "result": {"kind": "solve.completed", "payload": {"ok": true}},
                "dispatch": hook_test_dispatch_evidence()
            }),
        )
        .expect("complete terminal observation");

    let persisted = store
        .append_completed_hook_outcome(successor, &identity, &seed.dispatch_key, &seed.request_hash)
        .expect("append terminal evidence to successor");
    assert_eq!(persisted.thread_id, successor);
    let payload: ryeos_runtime::HookObservationRecordedPayload =
        serde_json::from_value(persisted.payload).expect("decode terminal evidence");
    assert_eq!(payload.occurrence_thread_id, predecessor);
    assert!(
        store
            .append_completed_hook_outcome(
                predecessor,
                &identity,
                &seed.dispatch_key,
                &seed.request_hash,
            )
            .unwrap_err()
            .to_string()
            .contains("requires a running caller")
    );
}

#[test]
fn in_process_root_birth_atomically_publishes_created_genesis_and_running_head() {
    let store = test_store();
    let thread_id = "T-in-process-root-birth";
    let metadata = in_process_launch_metadata();
    let owner = store
        .register_in_process_handler(thread_id)
        .expect("register in-process owner");

    let persisted = store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            vec![NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            }],
            &metadata,
            &owner,
        )
        .expect("atomically publish in-process root birth");

    assert_eq!(
        persisted
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            ryeos_state::event_types::THREAD_CREATED,
            ryeos_state::event_types::THREAD_STARTED,
        ]
    );
    let projected = store
        .get_thread(thread_id)
        .expect("read projected in-process root")
        .expect("projected in-process root");
    assert_eq!(projected.status, ThreadStatus::Running.as_str());
    assert!(projected.started_at.is_some());
    let projected_metadata = projected
        .runtime
        .launch_metadata
        .expect("in-process launch metadata");
    assert_eq!(
        projected_metadata.launch_driver,
        Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
    );
    assert_eq!(
        projected_metadata.in_process_lifecycle_authority,
        Some(ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE)
    );
    assert_eq!(
        replayed_event_types(&store, thread_id),
        ["thread_created", "thread_started"]
    );
    assert_eq!(
        store
            .in_process_handler_reservations_after(
                None,
                runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
            )
            .expect("list live reservations"),
        [runtime_db::InProcessHandlerReservation {
            thread_id: thread_id.to_string(),
            phase: runtime_db::InProcessHandlerReservationPhase::Running,
        }]
    );

    let inner = store.lock().expect("lock state store");
    let head_ref = inner
        .state_db
        .read_generic_head_ref("chains", thread_id)
        .expect("read signed chain head")
        .expect("signed chain head");
    assert!(
        !head_ref.target_hash.is_empty(),
        "atomic birth must publish a signed CAS head"
    );
    let cas = store.state_authority.cas_store().expect("open pinned CAS");
    let head_state: ryeos_state::ChainState = serde_json::from_value(
        cas.get_object(&head_ref.target_hash)
            .expect("load current ChainState")
            .expect("current ChainState object"),
    )
    .expect("decode current ChainState");
    head_state.validate().expect("validate current ChainState");
    assert_eq!(head_state.last_chain_seq, 2);
    let running_entry = head_state
        .threads
        .get(thread_id)
        .expect("running ChainState entry");
    assert_eq!(running_entry.status, ThreadStatus::Running);
    assert_eq!(running_entry.last_thread_seq, 2);
    let started_event_hash = persisted[1]
        .event_hash
        .as_deref()
        .expect("durable thread_started event hash");
    assert_eq!(
        head_state.last_event_hash.as_deref(),
        Some(started_event_hash)
    );
    assert_eq!(
        running_entry.last_event_hash.as_deref(),
        Some(started_event_hash)
    );

    let authoritative = authoritative_snapshot_for_transition(&inner, thread_id, thread_id)
        .expect("read authoritative running snapshot");
    assert_eq!(authoritative.status, ThreadStatus::Running);
    assert!(authoritative.started_at.is_some());
    assert_eq!(authoritative.last_chain_seq, 2);
    assert_eq!(authoritative.last_thread_seq, 2);
    assert_eq!(
        authoritative.last_event_hash.as_deref(),
        Some(started_event_hash)
    );

    let genesis_hash = head_state
        .prev_chain_state_hash
        .as_deref()
        .expect("event successor must retain the genesis ChainState");
    let genesis_state: ryeos_state::ChainState = serde_json::from_value(
        cas.get_object(genesis_hash)
            .expect("load genesis ChainState")
            .expect("genesis ChainState object"),
    )
    .expect("decode genesis ChainState");
    genesis_state
        .validate()
        .expect("validate genesis ChainState");
    assert_eq!(genesis_state.last_chain_seq, 0);
    assert!(genesis_state.last_event_hash.is_none());
    assert!(genesis_state.prev_chain_state_hash.is_none());
    let created_entry = genesis_state
        .threads
        .get(thread_id)
        .expect("created genesis entry");
    assert_eq!(created_entry.status, ThreadStatus::Created);
    assert_eq!(created_entry.last_thread_seq, 0);
    assert!(created_entry.last_event_hash.is_none());

    let genesis_snapshot = ThreadSnapshot::from_current_value(
        cas.get_object(&created_entry.snapshot_hash)
            .expect("load genesis snapshot")
            .expect("genesis snapshot object"),
    )
    .expect("decode current genesis snapshot");
    assert_eq!(genesis_snapshot.status, ThreadStatus::Created);
    assert!(genesis_snapshot.started_at.is_none());
    assert_eq!(genesis_snapshot.last_chain_seq, 0);
    assert_eq!(genesis_snapshot.last_thread_seq, 0);
    assert!(genesis_snapshot.last_event_hash.is_none());
}

#[test]
fn shutdown_gate_rejects_new_in_process_owner_registration() {
    let store = test_store();
    store
        .close_process_attachment_admission()
        .expect("close shutdown admission");
    let error = store
        .register_in_process_handler("T-after-shutdown-gate")
        .err()
        .expect("shutdown gate must reject a late in-process owner");
    assert!(error.to_string().contains("closed for daemon shutdown"));
}

#[cfg(target_os = "linux")]
#[test]
fn scratch_preservation_precedes_reservation_commit_and_survives_lost_ack() {
    let store = test_store();
    let (original, reservation) = scratch_reservation_fixture(&store, "scratch-precommit");
    let path = original.path().unwrap();
    assert!(store.reserve_process_resource_launch(&reservation).is_err());
    store.close_process_attachment_admission().unwrap();
    assert!(
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .is_err()
    );
    assert!(store.process_resource_reservations().unwrap().is_empty());
    drop(original);
    assert!(
        path.join("sentinel").exists(),
        "first possible commit must never be followed by automatic rollback"
    );

    // One Store is live per process: close the first before the second.
    store.close().unwrap();
    let store = test_store();
    let (original, reservation) = scratch_reservation_fixture(&store, "scratch-lost-reserve-ack");
    let path = original.path().unwrap();
    // Discard the successful acknowledgement and original process-local
    // guard. The authoritative stored birth and bytes must remain intact.
    store
        .reserve_process_resource_launch_with_scratch(&reservation, &original)
        .unwrap();
    assert!(
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .is_err()
    );
    drop(original);
    assert!(path.join("sentinel").exists());
    assert_eq!(
        store
            .process_resource_reservation(&reservation.owner_kind, &reservation.owner_coordinate)
            .unwrap(),
        Some(reservation.clone())
    );
    assert!(
        store
            .clear_process_resource_reservation(&reservation)
            .is_err()
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap(),
        reservation
            .runtime_custody
            .as_ref()
            .unwrap()
            .cas_roots()
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn scratch_physical_receipt_retains_roots_and_refuses_replacement_after_removal() {
    use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
    let store = test_store();
    let (original, reservation) = scratch_reservation_fixture(&store, "scratch-physical-receipt");
    let path = original.path().unwrap();
    store
        .reserve_process_resource_launch_with_scratch(&reservation, &original)
        .unwrap();
    let roots = store
        .lock()
        .unwrap()
        .runtime_db
        .process_resource_cas_roots()
        .unwrap();
    let pins = store
        .lock()
        .unwrap()
        .runtime_db
        .process_resource_materialization_pins()
        .unwrap();
    let removed = store
        .remove_never_issued_resource_scratch(&reservation, &original)
        .unwrap();
    assert!(!path.exists());
    assert!(original.path().is_none());
    assert_eq!(
        removed.runtime_custody.as_ref().unwrap().physical_state,
        ProcessCustodyPhysicalState::PhysicallyRemoved
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap(),
        roots
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_materialization_pins()
            .unwrap(),
        pins
    );
    assert!(
        store.clear_process_resource_reservation(&removed).is_err(),
        "physical removal cannot release resource/financial custody"
    );
    assert_eq!(
        store
            .recover_never_issued_resource_scratch(&removed)
            .unwrap(),
        removed
    );
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("replacement"), b"must remain").unwrap();
    assert!(
        store
            .recover_never_issued_resource_scratch(&removed)
            .is_err()
    );
    assert_eq!(
        std::fs::read(path.join("replacement")).unwrap(),
        b"must remain"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn scratch_disarmed_guard_with_live_root_cannot_mint_physical_receipt() {
    use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
    let store = test_store();
    let (original, reservation) = scratch_reservation_fixture(&store, "scratch-disarmed");
    let path = original.path().unwrap();
    store
        .reserve_process_resource_launch_with_scratch(&reservation, &original)
        .unwrap();
    original.disarm().unwrap();
    original.remove_now().unwrap(); // Existing Ok no-op is NOT proof.
    assert!(path.join("sentinel").exists());
    assert!(
        store
            .remove_never_issued_resource_scratch(&reservation, &original)
            .is_err()
    );
    assert_eq!(
        store.process_resource_reservations().unwrap()[0]
            .runtime_custody
            .as_ref()
            .unwrap()
            .physical_state,
        ProcessCustodyPhysicalState::Retained
    );
    // One Store is live per process: close the first before the second.
    drop(original);
    store.close().unwrap();

    // The SAME actual-scratch owner also refuses rollback once the real
    // one-shot stored spawn-intent transition has committed. No subprocess
    // is launched; this is the issued storage boundary, not GPU evidence.
    let store = test_store();
    let (original, reserved) = scratch_reservation_fixture(&store, "scratch-issued-no-rollback");
    let path = original.path().unwrap();
    store
        .reserve_process_resource_launch_with_scratch(&reserved, &original)
        .unwrap();
    let intent = store
        .begin_trusted_process_resource_spawn(&reserved)
        .unwrap()
        .into_reservation();
    let roots = store
        .lock()
        .unwrap()
        .runtime_db
        .process_resource_cas_roots()
        .unwrap();
    let pins = store
        .lock()
        .unwrap()
        .runtime_db
        .process_resource_materialization_pins()
        .unwrap();
    assert!(
        store
            .begin_trusted_process_resource_spawn(&reserved)
            .is_err()
    );
    assert!(
        store
            .remove_never_issued_resource_scratch(&intent, &original)
            .is_err()
    );
    assert!(
        store
            .recover_never_issued_resource_scratch(&intent)
            .is_err()
    );
    assert_eq!(
        std::fs::read(path.join("sentinel")).unwrap(),
        b"retained original scratch"
    );
    assert_eq!(original.path(), Some(path));
    assert_eq!(store.process_resource_reservations().unwrap(), vec![intent]);
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap(),
        roots
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_materialization_pins()
            .unwrap(),
        pins
    );
}

#[cfg(target_os = "linux")]
#[test]
fn scratch_replaced_parent_or_root_keeps_original_custody_and_replacement_bytes() {
    use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
    for replace_parent in [false, true] {
        let store = test_store();
        let (original, reservation) = scratch_reservation_fixture(&store, "scratch-replacement");
        let path = original.path().unwrap();
        store
            .reserve_process_resource_launch_with_scratch(&reservation, &original)
            .unwrap();
        let replaced = if replace_parent {
            path.parent().unwrap().to_owned()
        } else {
            path.clone()
        };
        let saved = replaced.with_extension("original");
        std::fs::rename(&replaced, &saved).unwrap();
        std::fs::create_dir(&replaced).unwrap();
        let replacement = if replace_parent {
            std::fs::create_dir(&path).unwrap();
            path.clone()
        } else {
            replaced
        };
        std::fs::write(replacement.join("replacement"), b"do not delete").unwrap();
        assert!(
            store
                .recover_never_issued_resource_scratch(&reservation)
                .is_err()
        );
        assert_eq!(
            std::fs::read(replacement.join("replacement")).unwrap(),
            b"do not delete"
        );
        let retained = store.process_resource_reservations().unwrap().remove(0);
        assert_eq!(
            retained.runtime_custody.as_ref().unwrap().physical_state,
            ProcessCustodyPhysicalState::Retained
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .runtime_db
                .process_resource_cas_roots()
                .unwrap(),
            reservation
                .runtime_custody
                .as_ref()
                .unwrap()
                .cas_roots()
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn scratch_failed_physical_removal_keeps_original_guard_intent_and_pins() {
    use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
    use std::os::unix::fs::PermissionsExt as _;
    let store = test_store();
    let (original, reservation) = scratch_reservation_fixture(&store, "scratch-removal-denied");
    let path = original.path().unwrap();
    store
        .reserve_process_resource_launch_with_scratch(&reservation, &original)
        .unwrap();
    let pins = store
        .lock()
        .unwrap()
        .runtime_db
        .process_resource_materialization_pins()
        .unwrap();
    // Actual unprivileged filesystem unlink refusal, not worker evidence.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500)).unwrap();
    let failed = store.remove_never_issued_resource_scratch(&reservation, &original);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        failed.is_err(),
        "control requires the unprivileged filesystem denial"
    );
    assert_eq!(original.path(), Some(path.clone()));
    assert!(path.join("sentinel").exists());
    let intent = store.process_resource_reservations().unwrap().remove(0);
    assert_eq!(
        intent.runtime_custody.as_ref().unwrap().physical_state,
        ProcessCustodyPhysicalState::CleanupIntent
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_materialization_pins()
            .unwrap(),
        pins
    );
    store
        .remove_never_issued_resource_scratch(&intent, &original)
        .unwrap();
    assert!(!path.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn scratch_missing_root_before_cleanup_intent_cannot_mint_receipt() {
    use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
    let store = test_store();
    let (original, reservation) = scratch_reservation_fixture(&store, "scratch-missing-no-intent");
    store
        .reserve_process_resource_launch_with_scratch(&reservation, &original)
        .unwrap();
    let roots = store
        .lock()
        .unwrap()
        .runtime_db
        .process_resource_cas_roots()
        .unwrap();
    original.remove_now().unwrap(); // Host fixture removal WITHOUT intent.
    assert!(
        store
            .recover_never_issued_resource_scratch(&reservation)
            .is_err()
    );
    assert_eq!(
        store.process_resource_reservations().unwrap()[0]
            .runtime_custody
            .as_ref()
            .unwrap()
            .physical_state,
        ProcessCustodyPhysicalState::Retained
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap(),
        roots
    );
}

#[cfg(target_os = "linux")]
#[test]
fn scratch_recovery_preserves_birth_authority_after_physical_removal_lost_ack() {
    use runtime_db::process_resource_custody::ProcessCustodyPhysicalState;
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("node-key.pem");
    let identity = ryeos_node_runtime::identity::NodeIdentity::create(&key).unwrap();
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    let trust = Arc::new(trust);
    let open = || TestStateStore::open(root.path(), signer.clone(), trust.clone()).unwrap();
    let store = open();
    let (original, reservation) = scratch_reservation_fixture(&store, "scratch-restart-lost-ack");
    let path = original.path().unwrap();
    store
        .reserve_process_resource_launch_with_scratch(&reservation, &original)
        .unwrap();
    let birth = store
        .lock()
        .unwrap()
        .runtime_db
        .fixture_previous_scratch_daemon_birth(&reservation, "fixture-original-daemon-incarnation")
        .unwrap();
    let intent = store
        .lock()
        .unwrap()
        .runtime_db
        .claim_never_issued_scratch_cleanup(&birth)
        .unwrap();
    original.remove_now().unwrap(); // Simulate crash BEFORE receipt ACK.
    assert!(!path.exists());
    let roots = store
        .lock()
        .unwrap()
        .runtime_db
        .process_resource_cas_roots()
        .unwrap();
    drop(original);
    drop(store);
    let reopened = open();
    assert_ne!(
        intent.daemon_generation_id,
        runtime_db::daemon_generation_id()
    );
    assert_eq!(
        reopened.process_resource_reservations().unwrap(),
        vec![intent.clone()]
    );
    assert_eq!(
        reopened
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap(),
        roots
    );
    let removed = reopened
        .recover_never_issued_resource_scratch(&intent)
        .unwrap();
    assert_eq!(removed.daemon_generation_id, birth.daemon_generation_id);
    assert_eq!(
        removed.runtime_custody.as_ref().unwrap().physical_state,
        ProcessCustodyPhysicalState::PhysicallyRemoved
    );
    assert_eq!(
        reopened
            .lock()
            .unwrap()
            .runtime_db
            .process_resource_cas_roots()
            .unwrap(),
        roots
    );
}

#[test]
fn shutdown_closure_serializes_resource_reservation_and_scope_binding() {
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let (reservation, _) = process_resource_reservation_fixture("reserve-race");
    let guard = store.lock().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let reserve_store = Arc::clone(&store);
    let reserve_record = reservation.clone();
    let reserve = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        reserve_store.reserve_process_resource_launch(&reserve_record)
    });
    ready_rx.recv().unwrap();
    // This is the shutdown linearization point while the reservation is
    // queued behind the same StateStore lock. It must observe closure
    // after acquiring the lock, not rely on a stale pre-lock read.
    store
        .process_attachment_admission_open
        .store(false, Ordering::Release);
    drop(guard);
    assert!(
        reserve
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("closed for daemon shutdown")
    );
    assert!(store.process_resource_reservations().unwrap().is_empty());

    // One Store is live per process: close the first before the second.
    drop(store);
    store_owner.close().unwrap();
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let (reservation, recovery) = process_resource_reservation_fixture("bind-race");
    store.reserve_process_resource_launch(&reservation).unwrap();
    let guard = store.lock().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let bind_store = Arc::clone(&store);
    let bind = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        bind_store.bind_process_resource_scope("thread", "bind-race", &recovery)
    });
    ready_rx.recv().unwrap();
    store
        .process_attachment_admission_open
        .store(false, Ordering::Release);
    drop(guard);
    assert!(
        bind.join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("closed for daemon shutdown")
    );
    assert_eq!(
        store
            .process_resource_reservation("thread", "bind-race")
            .unwrap()
            .unwrap()
            .launch_authority
            .local_scope_recovery()
            .cloned(),
        None
    );
}

#[test]
fn shutdown_gate_rejects_late_dedicated_worker_attachment() {
    let store = test_store();
    store
        .close_process_attachment_admission()
        .expect("close shutdown admission");
    let record = WorkerProcessRecord {
        worker_instance_id: "worker-after-shutdown".to_owned(),
        boot_identity_hash: "b".repeat(64),
        session_capsule_hash: "c".repeat(64),
        boot_epoch: 1,
        lifecycle_generation: 1,
        process_identity: crate::process::ExecutionProcessIdentity {
            schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            process_scope: None,
            boot_id: "test-boot".to_owned(),
            target_pid: 12345,
            target_start_time_ticks: 10,
            group_leader_pid: 12345,
            group_leader_start_time_ticks: 10,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        },
        control_channel_identity: "fd:test".to_owned(),
        state: runtime_db::WorkerProcessState::Attached,
        daemon_generation_id: "daemon:test".to_owned(),
        placement_thread_id: "T-after-shutdown".to_owned(),
        cleanup_state: "owned".to_owned(),
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let error = store
        .attach_worker_process(&record)
        .expect_err("shutdown gate must reject a late dedicated worker attach");
    assert!(
        error
            .to_string()
            .contains("attachment admission is closed for daemon shutdown")
    );
}

#[test]
fn in_process_root_birth_rejects_noncanonical_event_shape_without_partial_rows() {
    let store = test_store();
    let thread_id = "T-in-process-root-birth-rejected";
    let owner = store
        .register_in_process_handler(thread_id)
        .expect("register in-process owner");
    let error = store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            Vec::new(),
            &in_process_launch_metadata(),
            &owner,
        )
        .expect_err("missing thread_started event must fail closed");
    assert!(
        error
            .to_string()
            .contains("requires exactly one thread_started successor event")
    );
    assert!(
        store
            .get_thread(thread_id)
            .expect("inspect rejected root")
            .is_none()
    );
    let inner = store.lock().expect("lock state store");
    assert!(
        inner
            .runtime_db
            .get_runtime_info(thread_id)
            .expect("inspect rejected runtime row")
            .is_none()
    );
    assert!(
        inner
            .runtime_db
            .in_process_handler_reservations_after(
                None,
                runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
            )
            .expect("inspect rejected reservation")
            .is_empty()
    );
    assert!(
        inner
            .state_db
            .read_generic_head_ref("chains", thread_id)
            .expect("inspect rejected chain head")
            .is_none()
    );
}

#[test]
fn in_process_root_birth_requires_the_exact_registered_owner() {
    let store = test_store();
    let thread_id = "T-in-process-root-owner-qualified";
    let metadata = in_process_launch_metadata();
    let stale = InProcessHandlerControl::new();
    let events = || {
        vec![NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({}),
        }]
    };

    let error = store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            events(),
            &metadata,
            &stale,
        )
        .expect_err("unregistered owner must not publish an in-process root");
    assert!(
        error
            .to_string()
            .contains("recorded in-process handler has no active daemon owner")
    );

    let owner = store
        .register_in_process_handler(thread_id)
        .expect("register exact in-process owner");
    let error = store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            events(),
            &metadata,
            &stale,
        )
        .expect_err("stale owner must not publish an in-process root");
    assert!(error.to_string().contains("stale in-process handler owner"));
    assert!(
        store
            .get_thread(thread_id)
            .expect("inspect owner-rejected root")
            .is_none()
    );

    store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            events(),
            &metadata,
            &owner,
        )
        .expect("exact registered owner publishes the root");
}

#[test]
fn failed_duplicate_birth_owner_cannot_adopt_or_terminalize_existing_root() {
    let store = test_store();
    let thread_id = "T-in-process-duplicate-birth";
    let metadata = in_process_launch_metadata();
    let started = || {
        vec![NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({}),
        }]
    };
    let original_owner = store
        .register_in_process_handler(thread_id)
        .expect("register original owner");
    store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            started(),
            &metadata,
            &original_owner,
        )
        .expect("publish original root");
    assert!(original_owner.has_committed_birth());
    assert!(
        store
            .unregister_in_process_handler(thread_id, &original_owner)
            .expect("simulate loss of original volatile owner")
    );
    assert_eq!(
        original_owner.completion(),
        Some(InProcessHandlerCompletion::OwnerLostUnsettled)
    );

    let colliding_owner = store
        .register_in_process_handler(thread_id)
        .expect("register colliding owner");
    let error = store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            started(),
            &metadata,
            &colliding_owner,
        )
        .expect_err("duplicate birth must fail");
    assert!(!error.to_string().is_empty());
    assert!(!colliding_owner.has_committed_birth());

    let terminal = FinalizeThreadRecord {
        status: ThreadStatus::Failed.as_str().to_string(),
        outcome_code: Some("service_interrupted".to_string()),
        result_json: None,
        error_json: Some(json!({"error": "must not be adopted"})),
        artifacts: Vec::new(),
        final_cost: None,
        managed_envelope: None,
        result_project_snapshot_hash: None,
        result_workspace_output_capture_hash: None,
    };
    let error = store
        .finalize_in_process_handler_owned(thread_id, &colliding_owner, &terminal)
        .expect_err("pre-commit owner cannot terminalize an existing root");
    assert!(error.to_string().contains("before its birth commits"));
    let existing = store
        .get_thread(thread_id)
        .expect("read existing root")
        .expect("existing root remains");
    assert_eq!(existing.status, ThreadStatus::Running.as_str());
    let authoritative = store
        .get_authoritative_root_thread_snapshot(thread_id)
        .expect("read authoritative existing root")
        .expect("authoritative existing root remains");
    assert_eq!(authoritative.outcome_code, None);
    assert_eq!(
        replayed_event_types(&store, thread_id),
        ["thread_created", "thread_started"]
    );
    assert!(
        store
            .unregister_in_process_handler(thread_id, &colliding_owner)
            .expect("retire rejected colliding owner")
    );
    assert_eq!(
        colliding_owner.completion(),
        Some(InProcessHandlerCompletion::BirthAborted)
    );
}

#[test]
fn in_process_terminal_settlement_requires_the_exact_registered_owner() {
    let store = test_store();
    let thread_id = "T-in-process-exact-owner";
    let owner = store
        .register_in_process_handler(thread_id)
        .expect("register in-process owner");
    let duplicate = store
        .register_in_process_handler(thread_id)
        .err()
        .expect("duplicate in-process owner must fail closed");
    assert!(duplicate.to_string().contains("already registered"));
    store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            vec![NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            }],
            &in_process_launch_metadata(),
            &owner,
        )
        .expect("publish in-process root");

    let terminal = FinalizeThreadRecord {
        status: ThreadStatus::Completed.as_str().to_string(),
        outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
        result_json: Some(json!({"ok": true})),
        error_json: None,
        artifacts: Vec::new(),
        final_cost: None,
        managed_envelope: None,
        result_project_snapshot_hash: None,
        result_workspace_output_capture_hash: None,
    };
    let stale = InProcessHandlerControl::new();
    let error = store
        .finalize_in_process_handler_owned(thread_id, &stale, &terminal)
        .expect_err("stale in-process owner must not settle the root");
    assert!(error.to_string().contains("stale in-process handler owner"));
    assert_eq!(
        store
            .get_thread(thread_id)
            .expect("read root after rejected settlement")
            .expect("root after rejected settlement")
            .status,
        ThreadStatus::Running.as_str()
    );

    assert!(matches!(
        store
            .finalize_in_process_handler_owned(thread_id, &owner, &terminal)
            .expect("exact owner settles root"),
        FinalizeIfNonterminalOutcome::Finalized { .. }
    ));
    assert_eq!(
        store
            .get_thread(thread_id)
            .expect("read settled root")
            .expect("settled root")
            .status,
        ThreadStatus::Completed.as_str()
    );
    let error = store
        .settle_ownerless_in_process_handler_reservation(thread_id)
        .expect_err("shutdown settlement must not steal from an active owner");
    assert!(error.to_string().contains("while its owner is active"));
    assert!(
        store
            .settle_in_process_handler_reservation_owned(thread_id, &owner)
            .expect("exact owner settles terminal reservation")
    );
    let error = store
        .delete_ownerless_terminal_in_process_handler_reservation(thread_id)
        .expect_err("shutdown cleanup must not delete an active owner's reservation");
    assert!(error.to_string().contains("while its owner is active"));
    owner.mark_terminal_confirmed();
    assert!(
        store
            .unregister_in_process_handler(thread_id, &owner)
            .expect("unregister exact owner")
    );
    assert!(
        !store
            .is_in_process_handler_active(thread_id)
            .expect("inspect active owner")
    );
    assert_eq!(
        owner.completion(),
        Some(InProcessHandlerCompletion::TerminalConfirmed)
    );
    assert!(
        store
            .delete_ownerless_terminal_in_process_handler_reservation(thread_id)
            .expect("ownerless shutdown cleanup deletes terminal reservation residue")
    );
    assert!(
        store
            .in_process_handler_reservations_after(
                None,
                runtime_db::IN_PROCESS_HANDLER_RECONCILE_PAGE_SIZE,
            )
            .expect("list settled reservations")
            .is_empty()
    );
}

#[test]
fn primary_stop_boundary_rejects_cancel_and_kill_for_active_in_process_owner() {
    let store = test_store();
    let thread_id = "T-in-process-stop-fenced";
    let owner = store
        .register_in_process_handler(thread_id)
        .expect("register in-process owner");
    store
        .create_in_process_root_with_events_and_launch_metadata(
            &thread_record(thread_id, thread_id),
            vec![NewEventRecord {
                event_type: ryeos_state::event_types::THREAD_STARTED.to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            }],
            &in_process_launch_metadata(),
            &owner,
        )
        .expect("publish in-process root");

    assert_eq!(
        store
            .in_process_handler_stop_refusal(thread_id)
            .expect("read shared API/command stop fence"),
        Some(InProcessHandlerStopRefusal::MissingCancellationContract)
    );
    for intent in [StopIntent::Cancel, StopIntent::Kill] {
        let error = store
            .request_thread_stop(thread_id, intent)
            .expect_err("active in-process owner has no admitted stop contract");
        assert!(
            error
                .to_string()
                .contains("has no declared cancellation/drain contract")
        );
        let detail = store
            .get_thread(thread_id)
            .expect("read stop-fenced root")
            .expect("stop-fenced root");
        assert_eq!(detail.status, ThreadStatus::Running.as_str());
        assert_eq!(detail.runtime.stop_intent, None);
    }

    assert_eq!(
        replayed_event_types(&store, thread_id),
        ["thread_created", "thread_started"],
        "rejected stop requests must not append terminal or tombstone events"
    );
    assert!(
        store
            .unregister_in_process_handler(thread_id, &owner)
            .expect("unregister in-process owner")
    );
}

#[test]
fn fresh_launch_reservation_precedes_thread_publication() {
    let store = test_store();
    let thread_id = "T-fresh-launch-reservation";

    assert_eq!(
        store
            .reserve_fresh_thread_launch(thread_id, "claim-fresh", "daemon:test")
            .expect("reserve fresh launch"),
        runtime_db::LaunchClaimOutcome::Claimed
    );
    assert!(
        store
            .get_thread(thread_id)
            .expect("inspect unpublished thread")
            .is_none()
    );

    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("publish reserved thread");
    let claim = store
        .get_launch_claim(thread_id)
        .expect("read launch claim")
        .expect("claim remains attached after publication");
    assert_eq!(claim.claim_id, "claim-fresh");
    assert!(
        store
            .release_thread_launch_claim(thread_id, "claim-fresh")
            .expect("release launch reservation")
    );
}

#[test]
fn running_output_observations_are_fenced_by_exact_launch_owner() {
    let store = test_store();
    let thread_id = "T-output-owner";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .unwrap();
    store
        .claim_thread_launch(thread_id, "claim-1", "daemon:test")
        .unwrap();
    let first = store
        .get_launch_claim(thread_id)
        .unwrap()
        .unwrap()
        .claimed_by;
    store.mark_thread_running(thread_id, None).unwrap();
    let events = [NewEventRecord {
        event_type: ryeos_state::event_types::SUBPROCESS_OUTPUT_OBSERVED.into(),
        storage_class: "indexed".into(),
        payload: json!({"data": "first"}),
    }];
    assert!(
        store
            .append_events_if_thread_running_owned(thread_id, thread_id, &events, &first)
            .unwrap()
            .is_some()
    );
    store
        .release_thread_launch_claim(thread_id, "claim-1")
        .unwrap();
    store
        .claim_thread_launch(thread_id, "claim-2", "daemon:test")
        .unwrap();
    let second = store
        .get_launch_claim(thread_id)
        .unwrap()
        .unwrap()
        .claimed_by;
    assert_ne!(first, second);
    let before = replayed_event_types(&store, thread_id);
    assert!(
        store
            .append_events_if_thread_running_owned(thread_id, thread_id, &events, &first)
            .unwrap()
            .is_none()
    );
    assert_eq!(replayed_event_types(&store, thread_id), before);
    assert!(
        store
            .append_events_if_thread_running_owned(thread_id, thread_id, &events, &second)
            .unwrap()
            .is_some()
    );
}

#[test]
fn claimed_created_thread_accepts_exact_launch_attempt_audit() {
    let store = test_store();
    let thread_id = "T-launch-audit";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("create thread");
    let audit = launch_attempt_audit_events();

    // The runtime-authored boundary remains running-only.
    assert!(
        store
            .append_events_if_thread_running(thread_id, thread_id, &audit)
            .expect("runtime append guard")
            .is_none()
    );
    assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

    assert_eq!(
        store
            .claim_thread_launch(thread_id, "claim-audit", "daemon:test")
            .expect("claim launch"),
        runtime_db::LaunchClaimOutcome::Claimed
    );
    let persisted = store
        .append_launch_attempt_audit(thread_id, thread_id, &audit)
        .expect("append claimed launch audit");
    assert_eq!(persisted.len(), 3);
    assert_eq!(
        persisted
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        LAUNCH_ATTEMPT_AUDIT_TYPES
            .iter()
            .map(|event_type| event_type.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        replayed_event_types(&store, thread_id),
        [
            "thread_created",
            ryeos_runtime::RuntimeEventType::AsLaunchedResolution.as_str(),
            ryeos_runtime::RuntimeEventType::AsLaunchedRefBindings.as_str(),
            ryeos_runtime::RuntimeEventType::RuntimeLaunchFacts.as_str(),
        ]
    );
}

#[test]
fn launch_attempt_audit_rejections_do_not_mutate_the_chain() {
    let store = test_store();
    let thread_id = "T-launch-audit-reject";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("create thread");
    let audit = launch_attempt_audit_events();

    let unclaimed = store
        .append_launch_attempt_audit(thread_id, thread_id, &audit)
        .expect_err("unclaimed launch audit must fail");
    assert!(unclaimed.to_string().contains("no active launch claim"));
    assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

    assert_eq!(
        store
            .claim_thread_launch(thread_id, "claim-reject", "daemon:test")
            .expect("claim launch"),
        runtime_db::LaunchClaimOutcome::Claimed
    );
    let mut wrong_event = audit.clone();
    wrong_event.swap(0, 1);
    let wrong_event_error = store
        .append_launch_attempt_audit(thread_id, thread_id, &wrong_event)
        .expect_err("out-of-order audit must fail");
    assert!(wrong_event_error.to_string().contains("event 0"));
    assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

    let mut wrong_storage = audit;
    wrong_storage[0].storage_class = "journal".to_string();
    let wrong_storage_error = store
        .append_launch_attempt_audit(thread_id, thread_id, &wrong_storage)
        .expect_err("non-canonical audit storage must fail");
    assert!(
        wrong_storage_error
            .to_string()
            .contains("canonical storage class")
    );
    assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);
}

#[test]
fn launch_attempt_audit_rejects_terminal_and_process_attached_rows_without_mutation() {
    let audit = launch_attempt_audit_events();

    let terminal_store = test_store();
    let terminal_id = "T-launch-audit-terminal";
    terminal_store
        .create_thread_for_test(&thread_record(terminal_id, terminal_id))
        .expect("create terminal fixture");
    terminal_store
        .claim_thread_launch(terminal_id, "claim-terminal", "daemon:test")
        .expect("claim terminal fixture");
    terminal_store
        .finalize_thread(
            terminal_id,
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
        .expect("finalize fixture");
    let terminal_before = replayed_event_types(&terminal_store, terminal_id);
    let terminal_error = terminal_store
        .append_launch_attempt_audit(terminal_id, terminal_id, &audit)
        .expect_err("terminal launch audit must fail");
    assert!(
        terminal_error
            .to_string()
            .contains("requires a created or running thread")
    );
    assert_eq!(
        replayed_event_types(&terminal_store, terminal_id),
        terminal_before
    );

    // One Store is live per process: close the first before the second.
    terminal_store.close().unwrap();
    let attached_store = test_store();
    let attached_id = "T-launch-audit-attached";
    attached_store
        .create_thread_for_test(&thread_record(attached_id, attached_id))
        .expect("create attached fixture");
    attached_store
        .claim_thread_launch(attached_id, "claim-attached", "daemon:test")
        .expect("claim attached fixture");
    attached_store
        .attach_thread_process(
            attached_id,
            12345,
            67890,
            &crate::process::ExecutionProcessIdentity {
                schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                process_scope: None,
                boot_id: "test-boot".to_string(),
                target_pid: 12345,
                target_start_time_ticks: 10,
                group_leader_pid: 67890,
                group_leader_start_time_ticks: 20,
                resource_selections: Vec::new(),
                resource_settlement_authority: None,
                resource_operations: Vec::new(),
                resource_allocation_limit: None,
                resource_occupancy_start: None,
                resource_occupancy_limit: None,
                resource_cleanup_allowance_ms: None,
            },
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            None,
        )
        .expect("attach fixture process");
    let attached_before = replayed_event_types(&attached_store, attached_id);
    let attached_error = attached_store
        .append_launch_attempt_audit(attached_id, attached_id, &audit)
        .expect_err("process-attached launch audit must fail");
    assert!(attached_error.to_string().contains("process attachment"));
    assert_eq!(
        replayed_event_types(&attached_store, attached_id),
        attached_before
    );
}

#[test]
fn pre_release_attachment_is_strict_and_release_observes_stop_tombstone() {
    let store = test_store();
    let thread_id = "T-pre-release-attachment";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("create attachment fixture");
    let identity = crate::process::ExecutionProcessIdentity {
        schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
        process_scope: None,
        boot_id: "test-boot".to_string(),
        target_pid: 12345,
        target_start_time_ticks: 10,
        group_leader_pid: 12345,
        group_leader_start_time_ticks: 10,
        resource_selections: Vec::new(),
        resource_settlement_authority: None,
        resource_operations: Vec::new(),
        resource_allocation_limit: None,
        resource_occupancy_start: None,
        resource_occupancy_limit: None,
        resource_cleanup_allowance_ms: None,
    };
    store
        .attach_new_thread_process(
            thread_id,
            identity.target_pid,
            identity.group_leader_pid,
            &identity,
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            None,
        )
        .expect("attach exact held identity");
    let repeat = store
        .attach_new_thread_process(
            thread_id,
            identity.target_pid,
            identity.group_leader_pid,
            &identity,
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            None,
        )
        .expect_err("pre-release exact repeat must be rejected");
    assert!(repeat.to_string().contains("already attached"));

    store
        .authorize_attached_process_release(thread_id, &identity, None)
        .expect("release exact attached identity");
    store
        .request_thread_stop(thread_id, StopIntent::Cancel)
        .expect("persist stop tombstone");
    let stopped = store
        .authorize_attached_process_release(thread_id, &identity, None)
        .expect_err("stop tombstone must fence release");
    assert!(stopped.to_string().contains("cancel request"));
}

#[test]
fn attached_process_listing_does_not_wait_for_the_global_store_lock() {
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let thread_id = "T-attached-process-registry";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("create attachment fixture");
    let identity = crate::process::ExecutionProcessIdentity {
        schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
        process_scope: None,
        boot_id: "test-boot".to_string(),
        target_pid: 12346,
        target_start_time_ticks: 11,
        group_leader_pid: 12346,
        group_leader_start_time_ticks: 11,
        resource_selections: Vec::new(),
        resource_settlement_authority: None,
        resource_operations: Vec::new(),
        resource_allocation_limit: None,
        resource_occupancy_start: None,
        resource_occupancy_limit: None,
        resource_cleanup_allowance_ms: None,
    };
    store
        .attach_new_thread_process(
            thread_id,
            identity.target_pid,
            identity.group_leader_pid,
            &identity,
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            None,
        )
        .expect("attach exact held identity");

    let global_guard = store.lock().expect("hold global state lock");
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let listing_store = Arc::clone(&store);
    let listing = std::thread::spawn(move || {
        sender
            .send(listing_store.list_attached_thread_ids())
            .expect("send listing result");
    });
    let ids = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("attached listing must not wait for the global state lock")
        .expect("list attached threads");
    assert_eq!(ids, vec![thread_id.to_string()]);
    drop(global_guard);
    listing.join().expect("join listing thread");

    let wrong_identity = crate::process::ExecutionProcessIdentity {
        target_start_time_ticks: 12,
        ..identity.clone()
    };
    assert!(
        !store
            .clear_thread_process_if_matches(thread_id, &wrong_identity)
            .expect("mismatched clear is a proven no-op")
    );
    assert_eq!(
        store.list_attached_thread_ids().expect("list after no-op"),
        vec![thread_id.to_string()]
    );
    assert!(
        store
            .clear_thread_process_if_matches(thread_id, &identity)
            .expect("clear exact attachment")
    );
    assert!(
        store
            .list_attached_thread_ids()
            .expect("list after clear")
            .is_empty()
    );

    store
        .attach_new_thread_process(
            thread_id,
            identity.target_pid,
            identity.group_leader_pid,
            &identity,
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            None,
        )
        .expect("reattach fixture process");
    {
        let global_guard = store.lock().expect("lock for runtime-row rollback");
        assert_eq!(
            store
                .delete_thread_runtime_locked(&global_guard, thread_id)
                .expect("delete attached runtime row"),
            1
        );
    }
    assert!(
        store
            .list_attached_thread_ids()
            .expect("list after runtime-row deletion")
            .is_empty()
    );
}

#[test]
fn attached_process_registry_is_seeded_from_durable_runtime_state() {
    let tmp = tempdir().expect("tempdir").keep();
    let identity = ryeos_node_runtime::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
        .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let head_trust = Arc::new(head_trust);
    let open_store = || {
        TestStateStore::open(&tmp, Arc::clone(&signer), Arc::clone(&head_trust))
            .expect("state store")
    };

    let thread_id = "T-attached-process-reopen";
    let process_identity = crate::process::ExecutionProcessIdentity {
        schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
        process_scope: None,
        boot_id: "test-boot".to_string(),
        target_pid: 12347,
        target_start_time_ticks: 12,
        group_leader_pid: 12347,
        group_leader_start_time_ticks: 12,
        resource_selections: Vec::new(),
        resource_settlement_authority: None,
        resource_operations: Vec::new(),
        resource_allocation_limit: None,
        resource_occupancy_start: None,
        resource_occupancy_limit: None,
        resource_cleanup_allowance_ms: None,
    };
    let store = open_store();
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .expect("create attachment fixture");
    store
        .attach_new_thread_process(
            thread_id,
            process_identity.target_pid,
            process_identity.group_leader_pid,
            &process_identity,
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            None,
        )
        .expect("attach fixture process");
    drop(store);

    let reopened = open_store();
    assert_eq!(
        reopened
            .list_attached_thread_ids()
            .expect("list seeded attachments"),
        vec![thread_id.to_string()]
    );
    assert!(
        reopened
            .clear_thread_process_if_matches(thread_id, &process_identity)
            .expect("clear seeded attachment")
    );
}

#[test]
fn continuation_snapshot_binds_project_root_and_pin() {
    let source = "T-source";
    let mut record = thread_record("T-successor", "T-root");
    record.upstream_thread_id = Some(source.to_string());
    record.base_project_snapshot_hash = Some("a".repeat(64));
    let resume = continuation_resume_context(ProjectContext::LocalPath {
        path: PathBuf::from("/work/project"),
    });
    record.project_authority = resume.project_authority.clone();
    record.project_root = Some(PathBuf::from("/wrong/caller/path"));
    assert!(build_continuation_snapshot(&record, &resume).is_err());
    record.project_root = Some(PathBuf::from("/work/project"));
    let snapshot = build_continuation_snapshot(&record, &resume).unwrap();
    assert_eq!(snapshot.project_root, Some(PathBuf::from("/work/project")));
    assert_eq!(snapshot.base_project_snapshot_hash, Some("a".repeat(64)));
}
