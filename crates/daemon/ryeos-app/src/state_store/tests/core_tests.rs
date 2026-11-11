//! Store core, selected-metadata claims, fences and direct-owner tests.
use super::*;

#[test]
fn projection_records_match_canonical_events_without_rewriting_evidence() {
    use ryeos_state::objects::EventDurability::{Durable, Journal};
    for (durability, storage_class) in [(Durable, "indexed"), (Journal, "journal")] {
        let (event, row) = projected_event_fixture(durability);
        let authoritative =
            persisted_event_from_authoritative(row.event_hash.clone(), event).unwrap();
        let projected = persisted_event_from_projection_row(row.clone()).unwrap();
        assert_eq!(projected.storage_class, storage_class);
        assert_eq!(
            serde_json::to_value(&projected).unwrap(),
            serde_json::to_value(&authoritative).unwrap(),
        );

        // Conversion translates vocabulary only; it must not repair altered
        // payloads or hashes and thereby conceal a later authentication failure.
        let mut changed = row;
        changed.payload = br#"{"value":8}"#.to_vec();
        let changed = persisted_event_from_projection_row(changed).unwrap();
        assert_eq!(changed.event_hash, authoritative.event_hash);
        assert_ne!(changed.payload, authoritative.payload);
    }
}

#[test]
fn projection_conversion_refuses_invalid_persistent_evidence() {
    use ryeos_state::objects::EventDurability::{Durable, Ephemeral};
    let (_, row) = projected_event_fixture(Durable);
    for durability in ["indexed", "ephemeral", "unknown", "", "Durable", "durable "] {
        let mut changed = row.clone();
        changed.durability = durability.into();
        assert!(persisted_event_from_projection_row(changed).is_err());
    }
    let (event, ephemeral) = projected_event_fixture(Ephemeral);
    assert!(persisted_event_from_authoritative(ephemeral.event_hash, event).is_err());
    let mut malformed = row;
    malformed.payload = b"invalid JSON".to_vec();
    assert!(persisted_event_from_projection_row(malformed).is_err());
}

#[test]
fn anchored_page_reads_ignore_later_appends_and_refuse_unbounded_work() {
    let store = test_store();
    let thread_id = "T-anchored-page";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .unwrap();
    append_indexed(&store, thread_id, 2);
    let anchor = store
        .capture_thread_event_read_anchor(thread_id, thread_id)
        .unwrap()
        .expect("thread anchor exists");
    let (tip_hash, tip_seq) = anchor.tip().expect("anchor has a tip");
    let tip_hash = tip_hash.to_owned();
    let later = append_indexed(&store, thread_id, 1);
    let later_hash = later[0].event_hash.clone().unwrap();

    assert!(
        store
            .read_thread_event_page(&anchor, None, 0, 1024)
            .is_err()
    );
    assert!(store.read_thread_event_page(&anchor, None, 1, 0).is_err());
    assert!(store.read_thread_event_page(&anchor, None, 1, 1).is_err());

    let mut progress = None;
    let mut observed = Vec::new();
    loop {
        let page = store
            .read_thread_event_page(&anchor, progress.as_ref(), 1, 1024 * 1024)
            .unwrap();
        assert!(page.events.len() <= 1);
        observed.extend(page.events.into_iter().map(|(hash, _)| hash));
        progress = page.progress;
        if progress.is_none() {
            break;
        }
    }
    assert_eq!(observed.len() as u64, tip_seq);
    assert_eq!(observed.first(), Some(&tip_hash));
    assert!(!observed.contains(&later_hash));
    assert!(
        store
            .capture_thread_event_read_anchor(thread_id, "T-absent-thread")
            .unwrap()
            .is_none()
    );
}

#[test]
fn recent_thread_events_decode_through_the_durability_helper() {
    let store = test_store();
    let thread_id = "T-recent-events";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .unwrap();
    let appended = append_indexed(&store, thread_id, 3);
    let page = store
        .recent_thread_events(thread_id, thread_id, None, 2, 1024 * 1024)
        .unwrap();
    assert!(page.has_more);
    assert_eq!(page.events.len(), 2);
    assert!(
        page.events
            .iter()
            .all(|event| event.storage_class == "indexed")
    );
    assert_eq!(page.events[1].event_hash, appended[2].event_hash);
    assert!(
        store
            .recent_thread_events(thread_id, thread_id, None, 0, 1024)
            .is_err()
    );
}

#[test]
fn execution_candidate_proof_returns_signed_records_across_bounded_pages() {
    let store = test_store();
    let thread_id = "T-execution-candidate-page";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .unwrap();
    let records = append_indexed(&store, thread_id, 70);
    let candidates = vec![records[0].clone(), records[69].clone(), records[35].clone()];
    let authenticated = store
        .authenticate_execution_event_candidates(thread_id, &candidates)
        .unwrap();
    assert_eq!(authenticated.len(), candidates.len());
    for (candidate, signed) in candidates.iter().zip(&authenticated) {
        assert!(execution_event_candidate_matches(candidate, signed));
    }
    assert_eq!(authenticated[0].payload["index"], 0);
    assert_eq!(authenticated[1].payload["index"], 69);
    assert_eq!(authenticated[2].payload["index"], 35);
}

#[test]
fn execution_candidate_proof_refuses_contradicted_or_malformed_candidates() {
    let store = test_store();
    let thread_id = "T-execution-candidate-refusal";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .unwrap();
    let candidate = append_indexed(&store, thread_id, 1).remove(0);
    let refuses = |mutate: &dyn Fn(&mut PersistedEventRecord)| {
        let mut changed = candidate.clone();
        mutate(&mut changed);
        store
            .authenticate_execution_event_candidates(thread_id, &[changed])
            .is_err()
    };
    assert!(!refuses(&|_| {}));
    assert!(refuses(
        &|c| c.payload["index"] = json!("projection-forgery")
    ));
    assert!(refuses(&|c| c.thread_id = "T-foreign".into()));
    assert!(refuses(&|c| c.chain_root_id = "T-foreign".into()));
    assert!(refuses(&|c| c.storage_class = "journal".into()));
    assert!(refuses(&|c| c.ts = "2001-01-01T00:00:00Z".into()));
    assert!(refuses(&|c| c.event_hash = None));
    assert!(refuses(&|c| c.event_hash = Some("A".repeat(64))));
    assert!(refuses(&|c| c.event_hash = Some("c".repeat(64))));
    assert!(refuses(&|c| c.chain_seq = 0));
    // The same hash named twice in one thread is contradictory input.
    assert!(
        store
            .authenticate_execution_event_candidates(
                thread_id,
                &[candidate.clone(), candidate.clone()]
            )
            .is_err()
    );
    let too_many = vec![candidate; 2_001];
    assert!(
        store
            .authenticate_execution_event_candidates(thread_id, &too_many)
            .is_err()
    );
}

#[test]
fn projected_event_read_surfaces_preserve_indexed_class_and_thread_scope() {
    let store = test_store();
    let thread = "T-projection-read-surfaces";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let foreign = "T-projection-foreign";
    store
        .create_thread_for_test(&thread_record(foreign, foreign))
        .unwrap();
    let expected = store
        .latest_thread_event_by_type(thread, ryeos_state::event_types::THREAD_CREATED)
        .unwrap()
        .unwrap();
    assert_eq!(expected.storage_class, "indexed");
    let scoped = [
        store
            .replay_events(thread, Some(thread), None, 10, 1024 * 1024)
            .unwrap()
            .events,
        store
            .replay_events_through(
                thread,
                Some(thread),
                None,
                expected.chain_seq,
                10,
                1024 * 1024,
            )
            .unwrap()
            .events,
        store.latest_thread_events(thread, 10).unwrap(),
        vec![
            store
                .latest_thread_event_by_type_prefix(thread, "thread_")
                .unwrap()
                .unwrap(),
        ],
    ];
    for events in scoped {
        assert_eq!(events.len(), 1);
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
    }
    let node_events = store.latest_node_events(10, &[]).unwrap();
    assert_eq!(node_events.len(), 2);
    assert!(
        node_events
            .iter()
            .all(|event| event.storage_class == "indexed")
    );
    let owned = node_events
        .iter()
        .find(|event| event.thread_id == thread)
        .unwrap();
    assert_eq!(
        serde_json::to_value(owned).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[test]
fn rooted_bundle_member_proof_rechecks_current_owner_and_signatures() {
    let key = lillux::crypto::SigningKey::from_bytes(&[41; 32]);
    let fingerprint = lillux::crypto::fingerprint(&key.verifying_key());
    let trust = bundle_admission_trust_for(&[&key]);
    let capsule = rooted_bundle_proof_fixture(&key, &fingerprint);
    verify_rooted_bundle_member_source_proof(&capsule, &trust)
        .expect("rooted proof verifies with current owner and source signer");

    let mut bad_signature = capsule.clone();
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executable_identity:
            ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
                member_source_proof,
                ..
            },
        ..
    } = &mut bad_signature.artifact_identity
    else {
        unreachable!("fixture uses BundleExecutor identity");
    };
    let mut sidecar =
        String::from_utf8(member_source_proof.signed_item_source_sidecar.clone()).unwrap();
    let header_end = sidecar.find('\n').unwrap();
    let signer_separator = sidecar[..header_end].rfind(':').unwrap();
    let signature_separator = sidecar[..signer_separator].rfind(':').unwrap();
    let signature_start = signature_separator + 1;
    let replacement = if sidecar.as_bytes()[signature_start] == b'A' {
        b'B'
    } else {
        b'A'
    };
    sidecar.replace_range(
        signature_start..signature_start + 1,
        &(replacement as char).to_string(),
    );
    member_source_proof.signed_item_source_sidecar = sidecar.into_bytes();
    assert!(verify_rooted_bundle_member_source_proof(&bad_signature, &trust).is_err());

    let mut revoked_owner = capsule;
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executable_identity:
            ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
                member_source_proof,
                ..
            },
        ..
    } = &mut revoked_owner.artifact_identity
    else {
        unreachable!("fixture uses BundleExecutor identity");
    };
    member_source_proof
        .runtime_selection
        .definition_owner
        .signer_fingerprint = Some("0".repeat(64));
    assert!(verify_rooted_bundle_member_source_proof(&revoked_owner, &trust).is_err());
}

#[test]
fn rooted_bundle_member_proof_follows_admitting_trust_not_head_trust() {
    let bundle_key = lillux::crypto::SigningKey::from_bytes(&[41; 32]);
    let node_key = lillux::crypto::SigningKey::from_bytes(&[42; 32]);
    let bundle_fingerprint = lillux::crypto::fingerprint(&bundle_key.verifying_key());
    let capsule = rooted_bundle_proof_fixture(&bundle_key, &bundle_fingerprint);

    verify_rooted_bundle_member_source_proof(
        &capsule,
        &bundle_admission_trust_for(&[&node_key, &bundle_key]),
    )
    .expect("Bundle-signed capsule verifies under the trust that admitted the Bundle");

    let removed = verify_rooted_bundle_member_source_proof(
        &capsule,
        &bundle_admission_trust_for(&[&node_key]),
    )
    .expect_err("Bundle signer removed from admitting trust must refuse");
    assert!(
        removed.to_string().contains("no longer trusted"),
        "unexpected refusal: {removed:#}"
    );

    let unbound =
        verify_rooted_bundle_member_source_proof(&capsule, &BundleAdmissionTrust::unbound())
            .expect_err("a store without admitting trust must refuse Bundle executors");
    assert!(
        unbound.to_string().contains("no Bundle admission trust"),
        "unexpected refusal: {unbound:#}"
    );
}

#[test]
fn abandoned_runtime_snapshot_stage_is_removed_under_pinned_state_authority() {
    let temporary = tempdir().unwrap();
    let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
    let stages = state
        .create_child(std::ffi::OsStr::new("runtime-snapshot-staging"), 0o700)
        .unwrap();
    let name = format!("snapshot-stage-{}", uuid::Uuid::new_v4());
    let stage = stages
        .create_child(std::ffi::OsStr::new(&name), 0o700)
        .unwrap();
    let product = stage
        .create_child(std::ffi::OsStr::new("product"), 0o700)
        .unwrap();
    let mut payload = product
        .open_regular_create(std::ffi::OsStr::new("payload"), true, true, 0o600)
        .unwrap();
    payload.write_all(b"disposable upload bytes").unwrap();
    drop(payload);
    discard_abandoned_runtime_snapshot_staging(&state).unwrap();
    assert!(
        stages
            .open_child_directory(std::ffi::OsStr::new(&name))
            .unwrap()
            .is_none()
    );
    discard_abandoned_runtime_snapshot_staging(&state).unwrap();
}

#[test]
fn runtime_snapshot_stage_sweep_refuses_foreign_entry() {
    let temporary = tempdir().unwrap();
    let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
    let stages = state
        .create_child(std::ffi::OsStr::new("runtime-snapshot-staging"), 0o700)
        .unwrap();
    stages
        .create_child(std::ffi::OsStr::new("unrelated"), 0o700)
        .unwrap();
    assert!(discard_abandoned_runtime_snapshot_staging(&state).is_err());
    assert!(
        stages
            .open_child_directory(std::ffi::OsStr::new("unrelated"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn abandoned_external_guest_package_is_removed_under_pinned_state_authority() {
    let temporary = tempdir().unwrap();
    let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
    let packages = state
        .create_child(std::ffi::OsStr::new("external-guest-packages"), 0o700)
        .unwrap();
    let (name, package) = packages
        .create_unique_child("guest-package", 0o700)
        .unwrap();
    let mut payload = package
        .open_regular_create(std::ffi::OsStr::new("payload"), true, true, 0o600)
        .unwrap();
    payload.write_all(b"secret bootstrap").unwrap();
    drop(payload);
    discard_abandoned_external_guest_packages(&state).unwrap();
    assert!(packages.open_child_directory(&name).unwrap().is_none());
    discard_abandoned_external_guest_packages(&state).unwrap();
}

#[test]
fn abandoned_external_guest_package_sweep_refuses_foreign_entry() {
    let temporary = tempdir().unwrap();
    let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
    let packages = state
        .create_child(std::ffi::OsStr::new("external-guest-packages"), 0o700)
        .unwrap();
    packages
        .create_child(std::ffi::OsStr::new("unrelated"), 0o700)
        .unwrap();
    assert!(discard_abandoned_external_guest_packages(&state).is_err());
    assert!(
        packages
            .open_child_directory(std::ffi::OsStr::new("unrelated"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn abandoned_external_guest_package_sweep_progresses_beyond_one_batch() {
    let temporary = tempdir().unwrap();
    let state = lillux::PinnedDirectory::open_or_create(temporary.path()).unwrap();
    let packages = state
        .create_child(std::ffi::OsStr::new("external-guest-packages"), 0o700)
        .unwrap();
    for _ in 0..1025 {
        packages
            .create_unique_child("guest-package", 0o700)
            .unwrap();
    }
    discard_abandoned_external_guest_packages(&state).unwrap();
    assert!(packages.entry_names_bounded(1).unwrap().is_empty());
}

#[test]
fn abandoned_external_guest_package_is_removed_on_state_store_reopen() {
    let temporary = tempdir().unwrap();
    let app_root = temporary.path().to_path_buf();
    let identity =
        ryeos_node_runtime::identity::NodeIdentity::create(&app_root.join("node-key.pem"))
            .expect("test node identity");
    let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let trust = Arc::new(trust);
    let open = || {
        TestStateStore::open(&app_root, Arc::clone(&signer), Arc::clone(&trust))
            .expect("state store")
    };

    let store = open();
    let packages = store.external_guest_package_parent().unwrap();
    let (name, package) = packages
        .create_unique_child("guest-package", 0o700)
        .unwrap();
    let mut payload = package
        .open_regular_create(std::ffi::OsStr::new("payload"), true, true, 0o600)
        .unwrap();
    payload.write_all(b"secret bootstrap").unwrap();
    drop(payload);
    drop(package);
    drop(store);

    let reopened = open();
    assert!(packages.open_child_directory(&name).unwrap().is_none());
    drop(reopened);
}

#[test]
fn remote_follow_terminal_job_distinguishes_pending_request_and_completed_response() {
    let request = remote_follow_delivery_request();
    let pending = remote_follow_delivery_job(
        ryeos_state::SyncJobState::Retryable,
        "delivery_retryable",
        Some(serde_json::to_value(&request).unwrap()),
    );
    assert_eq!(
        classify_retained_remote_follow_terminal_job_result(&pending, &request).unwrap(),
        RetainedRemoteFollowTerminalJobResult::Pending
    );

    let response = crate::federated_follow::RemoteFollowTerminalDeliveryResponse {
        operation_id: request.operation_id.clone(),
        child_chain_root_id: request.child_chain_root_id.clone(),
        parent_chain_root_id: "T-parent".to_owned(),
        parent_successor_thread_id: "T-parent-successor".to_owned(),
        delivery: "settled".to_owned(),
    };
    let completed = remote_follow_delivery_job(
        ryeos_state::SyncJobState::Completed,
        "settled",
        Some(serde_json::to_value(response).unwrap()),
    );
    assert_eq!(
        classify_retained_remote_follow_terminal_job_result(&completed, &request).unwrap(),
        RetainedRemoteFollowTerminalJobResult::Completed
    );

    let completed_with_request = remote_follow_delivery_job(
        ryeos_state::SyncJobState::Completed,
        "settled",
        Some(serde_json::to_value(&request).unwrap()),
    );
    assert!(
        classify_retained_remote_follow_terminal_job_result(&completed_with_request, &request)
            .is_err()
    );
}

#[test]
fn remote_follow_terminal_job_repairs_only_pristine_missing_request() {
    let request = remote_follow_delivery_request();
    let pristine = remote_follow_delivery_job(ryeos_state::SyncJobState::Planned, "planned", None);
    assert_eq!(
        classify_retained_remote_follow_terminal_job_result(&pristine, &request).unwrap(),
        RetainedRemoteFollowTerminalJobResult::RepairMissingRequest
    );

    let mut progressed = pristine;
    progressed.attempt_count = 1;
    assert!(classify_retained_remote_follow_terminal_job_result(&progressed, &request).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocking_store_wait_does_not_starve_unrelated_async_work() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);

    let waiter = tokio::spawn(async move {
        cooperate_while_blocking(|| {
            started_tx.send(()).expect("test still awaits the waiter");
            release_rx.recv().expect("test releases the waiter");
        });
    });
    started_rx.await.expect("blocking boundary started");

    tokio::time::timeout(std::time::Duration::from_millis(100), async {
        tokio::task::yield_now().await;
    })
    .await
    .expect("the sole async worker remains available");

    release_tx.send(()).expect("release blocking boundary");
    waiter.await.expect("waiter task");
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_store_boundary_is_valid_from_spawn_blocking_workers() {
    tokio::task::spawn_blocking(|| cooperate_while_blocking(|| 42usize))
        .await
        .expect("blocking worker must not panic at the cooperative boundary");
}

#[test]
fn state_anchor_publish_wire_contract_requires_typed_subject() {
    let mut request = json!({
        "thread_id": "T-root",
        "subject": {
            "kind": "graph",
            "graph_run_id": "G-test",
            "definition_ref": "graph:test/solve",
            "effective_definition_digest": "d".repeat(64),
            "node": "solve",
            "step": 3
        },
        "contract": "domain.restore.v1",
        "restore": {"contract": "domain.restore.v1"},
        "restore_digest": format!("sha256:{}", "a".repeat(64)),
        "objects": [],
        "anchor": {
            "label": "checkpoint",
            "runtime": {},
            "metadata": {}
        }
    });
    let parsed: StateAnchorPublishParams =
        serde_json::from_value(request.clone()).expect("current state-anchor request");
    assert!(matches!(
        parsed.subject,
        ryeos_state::objects::StateAnchorSubject::Graph { .. }
    ));

    request.as_object_mut().unwrap().remove("subject");
    request["graph_run_id"] = json!("G-test");
    assert!(serde_json::from_value::<StateAnchorPublishParams>(request).is_err());
}

#[test]
fn immutable_execution_schema_mismatch_gets_the_explicit_cutover_command() {
    let mismatch = anyhow::Error::new(ryeos_state::IncompatibleCurrentObjectSchema::new(
        "thread snapshot",
        7,
        8,
    ))
    .context("decode current snapshot");
    let error = with_execution_schema_cutover_hint(mismatch);
    let rendered = format!("{error:#}");
    assert!(rendered.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND));
    assert!(
        error
            .chain()
            .any(|cause| cause.is::<ryeos_state::IncompatibleCurrentObjectSchema>())
    );

    let unrelated = with_execution_schema_cutover_hint(anyhow!("untrusted signed head"));
    assert!(
        !format!("{unrelated:#}")
            .contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
    );

    let newer = with_execution_schema_cutover_hint(anyhow::Error::new(
        ryeos_state::IncompatibleCurrentObjectSchema::new("thread snapshot", 9, 8),
    ));
    assert!(
        !format!("{newer:#}")
            .contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND)
    );
}

#[test]
fn original_state_store_mutex_deadline_refuses_while_actual_guard_remains_held() {
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let original_guard = store.inner.lock().unwrap();
    let original = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(40));
    let waiting = Arc::clone(&store);
    let (sent, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sent.send(waiting.lock_until(original).map(|_| ())).unwrap();
    });
    let result = received.recv_timeout(std::time::Duration::from_secs(2));
    drop(original_guard); // always release actual original mutex before joining/assertion
    worker.join().unwrap();
    assert!(
        result
            .expect("actual StateStore waiter must stop before guard release")
            .is_err()
    );
    assert!(original.has_elapsed());
    assert!(store.lock().is_ok());
}

#[test]
fn original_state_store_expiry_cannot_enter_runtime_sql_operation() {
    let store = test_store();
    let entered = AtomicBool::new(false);
    let result = store.with_runtime_original_deadline(
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
        |db| {
            entered.store(true, Ordering::Release);
            db.process_resource_reservations()
        },
    );
    assert!(result.is_err());
    assert!(!entered.load(Ordering::Acquire));
    assert!(store.lock().is_ok());
}

#[test]
fn store_owned_active_access_and_release_share_actual_guard() {
    let store = test_store();
    let thread = "T-store-active-release";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "claim", "daemon:test")
        .unwrap()
        .unwrap();
    {
        let g = store.lock().unwrap();
        assert!(StateStore::is_launch_owner_active_locked(
            &g,
            &claim.claimed_by
        ));
        assert_eq!(
            g.runtime_db
                .get_launch_claim(thread)
                .unwrap()
                .unwrap()
                .claimed_by,
            claim.claimed_by
        );
    }
    assert!(
        store
            .release_active_thread_launch_claim(thread, &claim.claim_id, &claim.claimed_by)
            .unwrap()
    );
    let g = store.lock().unwrap();
    assert!(g.runtime_db.get_launch_claim(thread).unwrap().is_none());
    assert!(!StateStore::is_launch_owner_active_locked(
        &g,
        &claim.claimed_by
    ));
}

#[test]
fn prepared_rotation_mismatch_rolls_back_epoch_attempt_and_preserves_active_owner() {
    let store = test_store();
    let thread = "T-prepared-rotation";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "first", "daemon:test")
        .unwrap()
        .unwrap();
    let g = store.lock().unwrap();
    let permit = store
        .selected_mutation_permit(
            &g,
            SelectedMutationOperation::Rotate(thread, &claim.claimed_by, "next", "daemon:test", 3),
        )
        .unwrap();
    let wrong = runtime_db::LaunchOwner {
        thread_id: thread.to_owned(),
        monotonic_launch_epoch: claim.owner.monotonic_launch_epoch + 2,
        unpredictable_nonce: "next".to_owned(),
        daemon_generation_id: "daemon:test".to_owned(),
    };
    assert!(
        g.runtime_db
            .rotate_thread_launch_claim_after_runtime_recovery(
                &permit,
                thread,
                &claim.claimed_by,
                "next",
                "daemon:test",
                3,
                &wrong
            )
            .is_err()
    );
    assert_eq!(
        g.runtime_db.fixture_launch_epoch(thread).unwrap(),
        Some(claim.owner.monotonic_launch_epoch as i64)
    );
    assert_eq!(g.runtime_db.get_resume_attempts(thread).unwrap(), 0);
    assert_eq!(
        g.runtime_db
            .get_launch_claim(thread)
            .unwrap()
            .unwrap()
            .claimed_by,
        claim.claimed_by
    );
    assert!(StateStore::is_launch_owner_active_locked(
        &g,
        &claim.claimed_by
    ));
}

#[test]
fn prepared_rotation_publishes_exact_replacement_under_same_store() {
    let store = test_store();
    let thread = "T-prepared-rotation-success";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let first = store
        .claim_thread_launch_active(thread, "first", "daemon:test")
        .unwrap()
        .unwrap();
    let outcome = store
        .rotate_active_thread_launch_claim_after_runtime_recovery(
            thread,
            &first.claimed_by,
            "next",
            "daemon:test",
            3,
        )
        .unwrap();
    let runtime_db::RuntimeRecoveryClaimRotation::Rotated {
        claim,
        next_attempt,
    } = outcome
    else {
        panic!("expected rotation");
    };
    let g = store.lock().unwrap();
    assert_eq!(next_attempt, 1);
    assert_eq!(
        claim.owner.monotonic_launch_epoch,
        first.owner.monotonic_launch_epoch + 1
    );
    assert_eq!(
        g.runtime_db
            .get_launch_claim(thread)
            .unwrap()
            .unwrap()
            .claimed_by,
        claim.claimed_by
    );
    assert!(!StateStore::is_launch_owner_active_locked(
        &g,
        &first.claimed_by
    ));
    assert!(StateStore::is_launch_owner_active_locked(
        &g,
        &claim.claimed_by
    ));
}

#[test]
fn selected_fences_all_phases_and_refusal_before_claim_release_rotation_and_sweep() {
    use crate::selected_invocation_bookkeeping::SelectedFixturePhase::*;
    for phase in [Pending, Occupied, Borrowed, RestoredAwaitingBorrowAck] {
        for refused in [false, true] {
            let (store, claim) = selected_fence_fixture(phase, refused);
            assert!(
                store
                    .release_thread_launch_claim(&claim.thread_id, &claim.claim_id)
                    .is_err()
            );
            assert!(
                store
                    .release_thread_launch_claim(&claim.thread_id, "stale")
                    .is_err()
            );
            assert!(
                store
                    .release_active_thread_launch_claim(&claim.thread_id, "stale", "wrong")
                    .is_err()
            );
            assert!(
                store
                    .release_active_thread_launch_claim(
                        &claim.thread_id,
                        &claim.claim_id,
                        &claim.claimed_by
                    )
                    .is_err()
            );
            assert!(
                store
                    .claim_thread_launch(&claim.thread_id, "new", "foreign")
                    .is_err()
            );
            assert!(
                store
                    .claim_thread_launch_active(
                        &claim.thread_id,
                        "new",
                        runtime_db::daemon_generation_id()
                    )
                    .is_err()
            );
            assert!(
                store
                    .rotate_active_thread_launch_claim_after_runtime_recovery(
                        &claim.thread_id,
                        &claim.claimed_by,
                        "next",
                        runtime_db::daemon_generation_id(),
                        3
                    )
                    .is_err()
            );
            assert!(
                store
                    .clear_stale_launch_claims("foreign-generation")
                    .is_err()
            );
            let retained = store.get_launch_claim(&claim.thread_id).unwrap().unwrap();
            assert_eq!(retained.claim_id, claim.claim_id);
            assert_eq!(retained.claimed_by, claim.claimed_by);
            assert!(
                store
                    .lock()
                    .unwrap()
                    .active_launch_owners
                    .contains(&claim.claimed_by)
            );
            let g = store.lock().unwrap();
            assert_eq!(
                g.runtime_db.get_resume_attempts(&claim.thread_id).unwrap(),
                0
            );
            assert!(
                store
                    .delete_thread_runtime_locked(&g, &claim.thread_id)
                    .is_err()
            );
            assert!(store.require_selected_bulk_exclusion(&g).is_err());
        }
    }
}

#[test]
fn selected_fences_missing_claim_and_unpublished_id_without_new_epoch_or_rollback() {
    use crate::selected_invocation_bookkeeping::SelectedFixturePhase::Pending;
    let (store, claim) = selected_fence_fixture(Pending, false);
    {
        let g = store.lock().unwrap();
        g.runtime_db
            .fixture_release_thread_launch_claim(&claim.thread_id, &claim.claim_id)
            .unwrap();
    }
    let before_epoch = store
        .lock()
        .unwrap()
        .runtime_db
        .fixture_launch_epoch(&claim.thread_id)
        .unwrap();
    assert!(
        store
            .claim_thread_launch(&claim.thread_id, "replacement", "foreign")
            .is_err()
    );
    assert!(store.get_launch_claim(&claim.thread_id).unwrap().is_none());
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .fixture_launch_epoch(&claim.thread_id)
            .unwrap(),
        before_epoch
    );
    let fresh = "T-selected-unpublished";
    let fresh_claim = store
        .reserve_fresh_thread_launch_active(fresh, "fresh", runtime_db::daemon_generation_id())
        .unwrap()
        .unwrap();
    let borrower = store.fixture_selected_restored_metadata(fresh).unwrap();
    {
        let g = store.lock().unwrap();
        g.runtime_db
            .fixture_release_thread_launch_claim(fresh, &fresh_claim.claim_id)
            .unwrap();
    }
    let fresh_epoch = store
        .lock()
        .unwrap()
        .runtime_db
        .fixture_launch_epoch(fresh)
        .unwrap();
    assert!(
        store
            .reserve_fresh_thread_launch(fresh, "new", "foreign")
            .is_err()
    );
    assert!(
        store
            .reserve_fresh_thread_launch_active(fresh, "new", runtime_db::daemon_generation_id())
            .is_err()
    );
    assert!(store.get_launch_claim(fresh).unwrap().is_none());
    assert_eq!(
        store
            .lock()
            .unwrap()
            .runtime_db
            .fixture_launch_epoch(fresh)
            .unwrap(),
        fresh_epoch
    );
    assert!(store.mark_selected_metadata_restored(&borrower).is_err());
    assert!(
        store
            .lock()
            .unwrap()
            .active_launch_owners
            .contains(&fresh_claim.claimed_by)
    );
}

#[test]
fn selected_live_permit_refuses_wrong_store_db_target_and_operation() {
    let store = test_store();
    // One Store is live per process, so the other Store is its identity
    // alone: an address that is not this Store's mutex. The other Runtime
    // is a real store opened beside it on the same test host.
    let other_root = tempdir().unwrap();
    let other_db = test_native::with_store_host(|host| {
        runtime_db::test_native::open_live(host, &other_root.path().join("runtime.sqlite3"))
    });
    let g = store.lock().unwrap();
    assert!(store.owns_store_mutex(g.store_mutex));
    assert!(!store.owns_store_mutex(std::ptr::null()));
    let permit = store
        .selected_mutation_permit(&g, SelectedMutationOperation::Release("T", "claim"))
        .unwrap();
    assert!(
        other_db
            .release_thread_launch_claim(&permit, "T", "claim")
            .is_err()
    );
    assert!(
        g.runtime_db
            .release_thread_launch_claim(&permit, "other", "claim")
            .is_err()
    );
    assert!(
        g.runtime_db
            .claim_thread_launch(&permit, "T", "claim", "generation")
            .is_err()
    );
    assert!(
        !g.runtime_db
            .release_thread_launch_claim(&permit, "T", "claim")
            .unwrap()
    );
    drop(g);
    test_native::with_store_host(|host| runtime_db::test_native::retire(host, other_db));
}

#[test]
fn selected_registry_poison_refuses_without_durable_claim_change() {
    const TEST: &str = concat!(
        module_path!(),
        "::selected_registry_poison_refuses_without_durable_claim_change"
    );
    // A poisoned registry refuses the store's close, so its native owners
    // end only with the process: the scenario runs in a child.
    if !lillux_sqlite::test_host::is_isolated_child(TEST) {
        let outcome = lillux_sqlite::test_host::run_isolated(TEST);
        assert_eq!(outcome, "poisoned registry kept the durable claim");
        return;
    }
    let store = test_store();
    let thread = "T-selected-poison";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
        .unwrap()
        .unwrap();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Deliberate private poison injection isolates registry refusal;
        // no ordinary SQL or selected capability is issued by this fixture.
        let _selected = store.selected_invocations.lock().unwrap();
        panic!("injected selected registry poison");
    }));
    assert!(
        store
            .release_thread_launch_claim(thread, &claim.claim_id)
            .is_err()
    );
    {
        let g = store.lock().unwrap();
        assert_eq!(
            g.runtime_db
                .get_launch_claim(thread)
                .unwrap()
                .unwrap()
                .claim_id,
            claim.claim_id
        );
    }
    let host = crate::state_store::test_native::take_shared_host();
    lillux_sqlite::test_host::end_isolated_process(
        (claim, store, host),
        "poisoned registry kept the durable claim",
    )
}

#[test]
fn selected_pending_birth_veto_retains_reservation_and_runtime_row() {
    let store = test_store();
    let thread = "T-selected-pending-birth";
    let claim = store
        .reserve_fresh_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
        .unwrap()
        .unwrap();
    {
        let g = store.lock().unwrap();
        g.runtime_db
            .reserve_in_process_handler_birth(thread, thread, &in_process_launch_metadata())
            .unwrap();
    }
    store.fixture_selected_restored_metadata(thread).unwrap();
    assert!(
        store
            .discard_uncommitted_in_process_handler_birth(thread)
            .is_err()
    );
    let g = store.lock().unwrap();
    assert!(
        store
            .selected_mutation_permit(&g, SelectedMutationOperation::DeleteThread(thread))
            .is_err()
    );
    assert!(
        g.runtime_db
            .in_process_handler_reservation(thread)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        g.runtime_db
            .get_launch_claim(thread)
            .unwrap()
            .unwrap()
            .claim_id,
        claim.claim_id
    );
    assert_eq!(
        g.runtime_db
            .inspect_thread_history_discard()
            .unwrap()
            .thread_runtime,
        1
    );
}

#[test]
fn selected_first_remove_and_head_absent_replay_preserve_exact_journal_and_claim() {
    let store = test_store();
    let thread = "T-selected-terminal";
    let mut record = thread_record(thread, thread);
    record.captured_history_policy.as_mut().unwrap().retention =
        ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
    store.create_thread_for_test(&record).unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
        .unwrap()
        .unwrap();
    finish_workspace_test_thread(&store, thread);
    store.fixture_selected_restored_metadata(thread).unwrap();
    {
        let g = store.lock().unwrap();
        let lock = g.state_db.acquire_chain_lock(thread).unwrap();
        let chain = g
            .state_db
            .authoritative_terminal_chain_under_lock(thread, &lock)
            .unwrap()
            .unwrap();
        let mut result = TerminalChainRetirement::default();
        assert!(
            store
                .finish_terminal_chain_removal(&g, &chain, &lock, &[], false, &mut result)
                .is_err()
        );
        assert!(
            g.state_db
                .pending_chain_transition(thread)
                .unwrap()
                .is_none()
        );
        assert!(
            g.state_db
                .authoritative_terminal_chain_under_lock(thread, &lock)
                .unwrap()
                .is_some()
        );
        // Deliberately construct an actual crash-boundary contradiction in
        // private unit code; no production issuer or cleanup permission.
        assert!(g.state_db.remove_chain_head_ref(thread, &lock).unwrap());
    }
    let before = store
        .lock()
        .unwrap()
        .state_db
        .pending_chain_transition(thread)
        .unwrap()
        .unwrap()
        .transition_id;
    assert!(
        store
            .recover_pending_terminal_chain_removals("2099-01-01T00:00:00Z", false, |_| Ok(0))
            .is_err()
    );
    let g = store.lock().unwrap();
    assert_eq!(
        g.state_db
            .pending_chain_transition(thread)
            .unwrap()
            .unwrap()
            .transition_id,
        before
    );
    assert_eq!(
        g.runtime_db
            .get_launch_claim(thread)
            .unwrap()
            .unwrap()
            .claim_id,
        claim.claim_id
    );
    assert_eq!(
        g.runtime_db
            .inspect_thread_history_discard()
            .unwrap()
            .thread_runtime,
        1
    );
}

#[test]
fn selected_expected_and_advanced_replay_keep_actual_signed_heads_and_remove_cursor() {
    for advanced in [false, true] {
        let store = test_store();
        let thread = "T-selected-replay";
        store
            .create_thread_for_test(&thread_record(thread, thread))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
            .unwrap()
            .unwrap();
        let head_path = store
            .lock()
            .unwrap()
            .state_db
            .refs_root()
            .join("generic/chains")
            .join(thread)
            .join("head");
        let expected = std::fs::read(&head_path).unwrap();
        finish_workspace_test_thread(&store, thread);
        let newer = std::fs::read(&head_path).unwrap();
        assert_ne!(expected, newer);
        store.fixture_selected_restored_metadata(thread).unwrap();
        let transition = {
            let g = store.lock().unwrap();
            let lock = g.state_db.acquire_chain_lock(thread).unwrap();
            // Private crash fixture restores a genuinely signed earlier
            // head, then records an actual Remove and a visible signed head.
            std::fs::write(&head_path, &expected).unwrap();
            g.state_db.remove_chain_head_ref(thread, &lock).unwrap();
            std::fs::write(&head_path, if advanced { &newer } else { &expected }).unwrap();
            let transition = g
                .state_db
                .pending_chain_transition(thread)
                .unwrap()
                .unwrap();
            let actual = g
                .state_db
                .pending_chain_removal_head_state(thread, &transition.transition_id, &lock)
                .unwrap();
            if advanced {
                assert!(matches!(
                    actual,
                    ryeos_state::PendingRemoveHeadState::AdvancedHeadVisible { .. }
                ));
            } else {
                assert!(matches!(
                    actual,
                    ryeos_state::PendingRemoveHeadState::ExpectedHeadVisible
                ));
            }
            transition.transition_id
        };
        let head_before = std::fs::read(&head_path).unwrap();
        assert!(
            store
                .recover_pending_terminal_chain_removals("2099-01-01T00:00:00Z", false, |_| Ok(0))
                .is_err()
        );
        let g = store.lock().unwrap();
        assert_eq!(
            g.state_db
                .pending_chain_transition(thread)
                .unwrap()
                .unwrap()
                .transition_id,
            transition
        );
        assert_eq!(std::fs::read(&head_path).unwrap(), head_before);
        assert_eq!(
            g.runtime_db
                .get_launch_claim(thread)
                .unwrap()
                .unwrap()
                .claim_id,
            claim.claim_id
        );
        assert_eq!(
            g.runtime_db
                .inspect_thread_history_discard()
                .unwrap()
                .thread_runtime,
            1
        );
    }
}

#[test]
fn selected_bulk_retirement_veto_preserves_unrelated_due_chain() {
    use crate::selected_invocation_bookkeeping::SelectedFixturePhase::Occupied;
    let (store, _) = selected_fence_fixture(Occupied, false);
    let due = "T-unrelated-due";
    let mut record = thread_record(due, due);
    record.captured_history_policy.as_mut().unwrap().retention =
        ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
    store.create_thread_for_test(&record).unwrap();
    finish_workspace_test_thread(&store, due);
    assert!(
        store
            .retire_due_terminal_chains("2099-01-01T00:00:00Z", false, |_| Ok(0))
            .is_err()
    );
    let g = store.lock().unwrap();
    assert!(g.state_db.pending_chain_transition(due).unwrap().is_none());
    let lock = g.state_db.acquire_chain_lock(due).unwrap();
    assert!(
        g.state_db
            .authoritative_terminal_chain_under_lock(due, &lock)
            .unwrap()
            .is_some()
    );
}

#[test]
fn empty_selected_domain_preserves_ordinary_head_absent_cleanup() {
    let store = test_store();
    let thread = "T-ordinary-replay";
    let mut record = thread_record(thread, thread);
    record.captured_history_policy.as_mut().unwrap().retention =
        ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
    store.create_thread_for_test(&record).unwrap();
    finish_workspace_test_thread(&store, thread);
    {
        let g = store.lock().unwrap();
        let lock = g.state_db.acquire_chain_lock(thread).unwrap();
        assert!(g.state_db.remove_chain_head_ref(thread, &lock).unwrap());
    }
    store
        .recover_pending_terminal_chain_removals("2099-01-01T00:00:00Z", false, |_| Ok(0))
        .unwrap();
    let g = store.lock().unwrap();
    assert!(
        g.state_db
            .pending_chain_transition(thread)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        g.runtime_db
            .inspect_thread_history_discard()
            .unwrap()
            .thread_runtime,
        0
    );
}

#[test]
fn ordinary_retention_leaves_fenced_kind_chains_for_the_fenced_path() {
    let store = test_store();
    crate::object_contracts::install_retained_evidence_validator(
        crate::object_contracts::accept_evidence,
    )
    .unwrap();
    let thread = "T-fenced-kind";
    fenced_kind_chain(&store, thread);
    store
        .install_fenced_chain_kinds(&fenced_test_kinds())
        .unwrap();
    let now = "2099-01-01T00:00:00Z";

    let ordinary = store
        .retire_due_terminal_chains(now, false, |_| Ok(0))
        .unwrap();
    assert_eq!(ordinary.retired_chains, 0);
    {
        let g = store.lock().unwrap();
        let lock = g.state_db.acquire_chain_lock(thread).unwrap();
        assert!(
            g.state_db
                .authoritative_terminal_chain_under_lock(thread, &lock)
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(
        store
            .due_fenced_chain_retirement_slice(now, None, 8)
            .unwrap(),
        vec![thread.to_string()]
    );

    // A pinned chain is refused, not retired around its pin.
    let plan = FencedChainRetirementPlan::default();
    assert!(
        store
            .retire_fenced_chain(thread, now, &plan, |_| Ok(1))
            .is_err()
    );
    let retired = store
        .retire_fenced_chain(thread, now, &plan, |_| Ok(0))
        .unwrap();
    assert_eq!(retired.retired_chains, 1);
    let g = store.lock().unwrap();
    let lock = g.state_db.acquire_chain_lock(thread).unwrap();
    assert!(
        g.state_db
            .authoritative_terminal_chain_under_lock(thread, &lock)
            .unwrap()
            .is_none()
    );
    assert!(
        g.state_db
            .pending_chain_transition(thread)
            .unwrap()
            .is_none()
    );
    // The retirement is complete and not repeatable as a fresh one.
    drop(lock);
    drop(g);
    assert!(
        store
            .retire_fenced_chain(thread, now, &plan, |_| Ok(0))
            .is_err()
    );
}

#[test]
fn fenced_retirement_refuses_an_unfenced_kind_and_a_different_kind_set() {
    let store = test_store();
    crate::object_contracts::install_retained_evidence_validator(
        crate::object_contracts::accept_evidence,
    )
    .unwrap();
    let now = "2099-01-01T00:00:00Z";
    let plan = FencedChainRetirementPlan::default();
    // Nothing installed: the fenced path refuses rather than assuming.
    assert!(
        store
            .due_fenced_chain_retirement_slice(now, None, 8)
            .is_err()
    );
    assert!(
        store
            .retire_fenced_chain("T-any", now, &plan, |_| Ok(0))
            .is_err()
    );

    store
        .install_fenced_chain_kinds(&fenced_test_kinds())
        .unwrap();
    store
        .install_fenced_chain_kinds(&fenced_test_kinds())
        .unwrap();
    let other = ryeos_state::FencedChainKinds::new(["other_kind".to_string()]).unwrap();
    assert!(store.install_fenced_chain_kinds(&other).is_err());

    let thread = "T-ordinary-kind";
    let mut record = thread_record(thread, thread);
    record.captured_history_policy.as_mut().unwrap().retention =
        ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: 60 };
    store.create_thread_for_test(&record).unwrap();
    finish_workspace_test_thread(&store, thread);
    assert!(
        store
            .retire_fenced_chain(thread, now, &plan, |_| Ok(0))
            .is_err()
    );
    assert!(
        store
            .due_fenced_chain_retirement_slice(now, None, 8)
            .unwrap()
            .is_empty()
    );
    // The ordinary kind still retires through the ordinary path.
    assert_eq!(
        store
            .retire_due_terminal_chains(now, false, |_| Ok(0))
            .unwrap()
            .retired_chains,
        1
    );
}

#[test]
fn selected_metadata_is_shared_and_ack_requires_actual_claim_and_active_owner() {
    use crate::selected_invocation_bookkeeping::SelectedBorrowReturn;
    let store_owner = test_store();
    let store = Arc::clone(store_owner.store());
    let same_store = Arc::clone(&store);
    let generation = store.register_selected_bookkeeping_generation().unwrap();
    let thread = "T-selected-ack";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
        .unwrap()
        .unwrap();
    let borrower = same_store
        .selected_invocations
        .lock()
        .unwrap()
        .fixture_restored_borrow(&generation, thread, &claim.claim_id, &claim.claimed_by);
    store
        .lock()
        .unwrap()
        .active_launch_owners
        .remove(&claim.claimed_by);
    assert!(same_store.ack_selected_borrow_return(&borrower).is_err());
    assert!(
        store
            .selected_invocations
            .lock()
            .unwrap()
            .fixture_awaiting_ack(&borrower)
    );
    store
        .lock()
        .unwrap()
        .active_launch_owners
        .insert(claim.claimed_by.clone());
    assert_eq!(
        same_store.ack_selected_borrow_return(&borrower).unwrap(),
        SelectedBorrowReturn::Returned
    );
    assert_eq!(
        store.ack_selected_borrow_return(&borrower).unwrap(),
        SelectedBorrowReturn::AlreadyReturned
    );
    // One Store is live per process: the first closes before the other
    // opens, and the borrower is plain data that outlives its Store.
    drop((store, same_store));
    store_owner.close().unwrap();
    let other = test_store();
    other.register_selected_bookkeeping_generation().unwrap();
    assert!(other.ack_selected_borrow_return(&borrower).is_err());
}

#[test]
fn selected_return_retries_after_close_but_new_metadata_borrow_refuses() {
    let store = test_store();
    let generation = store.register_selected_bookkeeping_generation().unwrap();
    let thread = "T-selected-return-close";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
        .unwrap()
        .unwrap();
    let first = store
        .selected_invocations
        .lock()
        .unwrap()
        .fixture_restored_borrow(&generation, thread, &claim.claim_id, &claim.claimed_by);
    let ticket = first.invocation().clone();
    store.mark_selected_metadata_restored(&first).unwrap();
    store.ack_selected_borrow_return(&first).unwrap();
    let second = store.begin_selected_metadata_borrow(&ticket).unwrap();
    assert!(store.mark_selected_metadata_restored(&first).is_err());
    store.close_process_attachment_admission().unwrap();
    store.mark_selected_metadata_restored(&second).unwrap();
    store.mark_selected_metadata_restored(&second).unwrap();
    assert_eq!(
        store.ack_selected_borrow_return(&second).unwrap(),
        crate::SelectedBorrowReturn::Returned
    );
    assert_eq!(
        store.ack_selected_borrow_return(&second).unwrap(),
        crate::SelectedBorrowReturn::AlreadyReturned
    );
    assert!(store.begin_selected_metadata_borrow(&ticket).is_err());
    assert!(store.ack_selected_borrow_return(&first).is_err());
}

#[test]
fn selected_missing_durable_claim_keeps_restored_metadata_pending() {
    let store = test_store();
    let generation = store.register_selected_bookkeeping_generation().unwrap();
    let thread = "T-selected-missing-claim";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "claim", runtime_db::daemon_generation_id())
        .unwrap()
        .unwrap();
    let borrower = store
        .selected_invocations
        .lock()
        .unwrap()
        .fixture_restored_borrow(&generation, thread, &claim.claim_id, &claim.claimed_by);
    // Deliberate private unit-fixture corruption, not a Store bypass API.
    store
        .lock()
        .unwrap()
        .runtime_db
        .fixture_release_thread_launch_claim(thread, &claim.claim_id)
        .unwrap();
    assert!(store.ack_selected_borrow_return(&borrower).is_err());
    assert!(
        store
            .selected_invocations
            .lock()
            .unwrap()
            .fixture_awaiting_ack(&borrower)
    );
}

#[test]
fn active_claim_release_refuses_stale_claim_or_wrong_owner_without_mutation() {
    let store = test_store();
    let thread = "T-release-order";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread, "claim-current", "daemon:test")
        .unwrap()
        .unwrap();
    assert!(
        !store
            .release_active_thread_launch_claim(thread, "claim-stale", &claim.claimed_by)
            .unwrap()
    );
    assert!(store.is_launch_owner_active(&claim.claimed_by));
    let retained = store.get_launch_claim(thread).unwrap().unwrap();
    assert_eq!(retained.claim_id, claim.claim_id);
    assert_eq!(retained.claimed_by, claim.claimed_by);
    assert!(
        !store
            .release_active_thread_launch_claim(thread, &claim.claim_id, "wrong-owner")
            .unwrap()
    );
    assert!(store.is_launch_owner_active(&claim.claimed_by));
    assert_eq!(
        store.get_launch_claim(thread).unwrap().unwrap().claimed_by,
        claim.claimed_by
    );
    // Model an already-absent durable row while the actual active registry
    // still retains this owner. A missing-row refusal must not erase it.
    assert!(
        store
            .release_thread_launch_claim(thread, &claim.claim_id)
            .unwrap()
    );
    assert!(
        !store
            .release_active_thread_launch_claim(thread, &claim.claim_id, &claim.claimed_by)
            .unwrap()
    );
    assert!(store.is_launch_owner_active(&claim.claimed_by));
}

#[test]
fn exact_active_claim_release_acknowledges_both_domains_and_preserves_replacement() {
    let store = test_store();
    let thread = "T-release-replaced";
    store
        .create_thread_for_test(&thread_record(thread, thread))
        .unwrap();
    let first = store
        .claim_thread_launch_active(thread, "claim-first", "daemon:test")
        .unwrap()
        .unwrap();
    assert!(
        store
            .release_active_thread_launch_claim(thread, &first.claim_id, &first.claimed_by)
            .unwrap()
    );
    assert!(store.get_launch_claim(thread).unwrap().is_none());
    assert!(!store.is_launch_owner_active(&first.claimed_by));
    let replacement = store
        .claim_thread_launch_active(thread, "claim-replacement", "daemon:test")
        .unwrap()
        .unwrap();
    assert!(
        !store
            .release_active_thread_launch_claim(thread, &first.claim_id, &first.claimed_by)
            .unwrap()
    );
    assert!(store.is_launch_owner_active(&replacement.claimed_by));
    let retained = store.get_launch_claim(thread).unwrap().unwrap();
    assert_eq!(retained.claim_id, replacement.claim_id);
    assert_eq!(retained.claimed_by, replacement.claimed_by);
}

#[test]
fn signed_storage_external_direct_cannot_complete_without_allocation() {
    let store = test_store();
    let thread = "T-storage-external-direct";
    signed_direct_storage_root(&store, thread, true);
    assert!(store.external_allocation(thread).unwrap().is_none());
    let head = || {
        store
            .lock()
            .unwrap()
            .state_db
            .read_generic_head_ref("chains", thread)
            .unwrap()
            .unwrap()
            .target_hash
    };
    let before = head();
    let events = replayed_event_types(&store, thread);
    let error = store
        .finalize_thread(thread, &storage_direct_success())
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("external direct result has no allocation evidence"),
        "{error:#}"
    );
    assert_eq!(head(), before);
    assert_eq!(replayed_event_types(&store, thread), events);
    assert_eq!(store.get_thread(thread).unwrap().unwrap().status, "created");
    assert!(store.external_allocation(thread).unwrap().is_none());
}

#[test]
fn external_finalization_preserves_signed_storage_local_node_policy() {
    let store = test_store();
    let thread = "T-storage-local-node-policy";
    signed_direct_storage_root(&store, thread, false);
    store
        .finalize_thread(thread, &storage_direct_success())
        .unwrap();
    let inner = store.lock().unwrap();
    let snapshot = authoritative_snapshot_for_transition(&inner, thread, thread).unwrap();
    assert_eq!(snapshot.status, ThreadStatus::Completed);
    assert_eq!(snapshot.result, Some(json!({"answer":42})));
    drop(inner);
    assert!(replayed_event_types(&store, thread).contains(&"thread_completed".into()));
    assert!(store.external_allocation(thread).unwrap().is_none());
}

#[test]
fn external_direct_finalization_binds_exact_ordinary_result_meaning() {
    // Content-comparison fixture, not proof of a born launch, signed
    // transcript, or cleanup. The production caller checks those under its
    // existing writer guard before comparing this projection.
    use ryeos_state::external_execution::{
        ExternalCommandOutputCommitment, ExternalCommandTermination,
        ExternalCommandTerminationReason, ExternalTargetExit,
    };
    let stdout = br#"{"answer":42}"#.to_vec();
    let commitment = |bytes: &[u8]| ExternalCommandOutputCommitment {
        bytes: bytes.len() as u64,
        sha256: lillux::sha256_hex(bytes),
        truncated: false,
    };
    let output = runtime_db::external_execution::ExternalDirectOutput {
        binding_digest: "a".repeat(64),
        terminal_sequence: 3,
        terminal_digest: "b".repeat(64),
        termination: ExternalCommandTermination {
            target_exit: ExternalTargetExit::Code(0),
            reason: ExternalCommandTerminationReason::TargetExited,
            stdout: commitment(&stdout),
            stderr: commitment(b""),
        },
        stdout,
        stderr: Vec::new(),
    };
    let expected = ryeos_engine::dispatch::interpret_external_terminal(
        &output.termination,
        &output.stdout,
        &output.stderr,
    );
    let exact = FinalizeThreadRecord {
        status: expected.status.as_str().into(),
        outcome_code: expected.outcome_code,
        result_json: expected.result,
        error_json: expected.error,
        artifacts: Vec::new(),
        final_cost: None,
        managed_envelope: None,
        result_project_snapshot_hash: None,
        result_workspace_output_capture_hash: None,
    };
    validate_external_direct_result(&exact, &output).unwrap();
    for part in [
        "status", "outcome", "result", "error", "managed", "snapshot", "capture",
    ] {
        let mut changed = exact.clone();
        match part {
            "status" => changed.status = "failed".into(),
            "outcome" => changed.outcome_code = Some("invented".into()),
            "result" => changed.result_json = Some(json!({"answer":99})),
            "error" => changed.error_json = Some(json!({"error":"invented"})),
            "managed" => changed.managed_envelope = Some(json!({})),
            "snapshot" => changed.result_project_snapshot_hash = Some("c".repeat(64)),
            "capture" => changed.result_workspace_output_capture_hash = Some("d".repeat(64)),
            _ => unreachable!(),
        }
        assert!(
            validate_external_direct_result(&changed, &output).is_err(),
            "accepted {part}"
        );
    }
    let mut changed = output.clone();
    changed.termination.reason = ExternalCommandTerminationReason::Cancelled;
    assert!(validate_external_direct_result(&exact, &changed).is_err());
    changed = output.clone();
    changed.termination.target_exit = ExternalTargetExit::Code(7);
    assert!(validate_external_direct_result(&exact, &changed).is_err());
}

#[test]
fn direct_allocation_requires_authoritative_born_capsule_not_runtime_metadata() {
    let store = test_store();
    let reservation = direct_owner_test_reservation(&store);
    let binding=crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
    store
        .seed_launch_metadata(
            &reservation.placement_thread_id,
            &crate::launch_metadata::RuntimeLaunchMetadata::default().with_launch_driver(
                ryeos_state::objects::ExecutionLaunchDriver::DirectItemExecutor,
            ),
        )
        .unwrap();
    let error = store
        .reserve_external_allocation(&reservation, &binding)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("born thread has no admitted capsule")
    );
    assert!(
        store
            .external_allocation(&reservation.placement_thread_id)
            .unwrap()
            .is_none()
    );
    let mut absent = reservation.clone();
    absent.placement_thread_id = "T-no-authoritative-birth".into();
    let runtime_db::external_execution::ExternalAllocationOwner::DirectThread {
        chain_root_id,
        launch_owner,
        ..
    } = &mut absent.owner
    else {
        unreachable!()
    };
    *chain_root_id = absent.placement_thread_id.clone();
    launch_owner.thread_id = absent.placement_thread_id.clone();
    store
        .lock()
        .unwrap()
        .runtime_db
        .insert_thread_runtime(&absent.placement_thread_id, &absent.placement_thread_id)
        .unwrap();
    assert!(
        store
            .reserve_external_allocation(&absent, &binding)
            .unwrap_err()
            .to_string()
            .contains("authoritative snapshot missing")
    );
    assert!(
        store
            .external_allocation(&absent.placement_thread_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn direct_owner_handoff_matches_only_its_exact_tuple() {
    // Exercise tuple comparison only. This hand-built token cannot serve
    // as positive admission evidence; real birth and compiler consumption
    // belong to the public ordinary-execution fixture.
    let store = test_store();
    let reservation = direct_owner_test_reservation(&store);
    let runtime_db::external_execution::ExternalAllocationOwner::DirectThread {
        chain_root_id,
        launch_owner,
        program,
    } = &reservation.owner
    else {
        unreachable!()
    };
    let proof = VerifiedExternalDirectOwner {
        thread_id: reservation.placement_thread_id.clone(),
        chain_root_id: chain_root_id.clone(),
        capsule_hash: reservation.admitted_capsule_hash.clone(),
        snapshot_hash: reservation.base_snapshot_hash.clone(),
        endpoint_binding_hash: reservation.binding_hash.clone(),
        program_digest: program.digest().unwrap(),
        launch_owner: launch_owner.clone(),
    };
    assert!(proof.matches(&reservation));
    for part in ["capsule", "snapshot", "binding", "thread", "chain", "owner"] {
        let mut changed = reservation.clone();
        let runtime_db::external_execution::ExternalAllocationOwner::DirectThread {
            chain_root_id,
            launch_owner,
            ..
        } = &mut changed.owner
        else {
            unreachable!()
        };
        match part {
            "capsule" => changed.admitted_capsule_hash = "f".repeat(64),
            "snapshot" => changed.base_snapshot_hash = "f".repeat(64),
            "binding" => changed.binding_hash = "f".repeat(64),
            "thread" => changed.placement_thread_id = "T-other".into(),
            "chain" => *chain_root_id = "T-other".into(),
            "owner" => launch_owner.monotonic_launch_epoch += 1,
            _ => unreachable!(),
        }
        assert!(!proof.matches(&changed));
    }
}

#[test]
fn direct_reservation_replay_preserves_original_owner_without_fresh_admission() {
    use runtime_db::external_execution::{ExternalAllocationContactClaim, ExternalAllocationPhase};
    // Storage-only retained occurrence: a positive direct capsule launch
    // remains a joined-test gate, not a claim made by this regression.
    let store = test_store();
    let reservation = direct_owner_test_reservation(&store);
    let binding=crate::node_config::sections::external_execution::RetainedExternalExecutionBinding::test_fixture();
    {
        let inner = store.lock().unwrap();
        runtime_db::external_execution::tests::retain_direct_owner_fixture(
            &inner.runtime_db,
            &reservation,
            "contact_pending",
        );
    }
    store
        .release_thread_launch_claim(&reservation.placement_thread_id, "direct-proof-claim")
        .unwrap();
    store
        .claim_thread_launch_active(
            &reservation.placement_thread_id,
            "replacement-claim",
            "daemon:replacement",
        )
        .unwrap()
        .unwrap();
    let replay = store
        .reserve_external_allocation(&reservation, &binding)
        .unwrap();
    assert_eq!(replay.reservation, reservation);
    assert_eq!(replay.phase, ExternalAllocationPhase::ContactPending);
    assert!(matches!(
        store
            .claim_external_allocation_contact(
                &reservation.placement_thread_id,
                &reservation.request_digest
            )
            .unwrap(),
        ExternalAllocationContactClaim::Reconcile(_)
    ));
    let mut changed = reservation.clone();
    changed.base_snapshot_hash = "f".repeat(64);
    assert!(
        store
            .reserve_external_allocation(&changed, &binding)
            .unwrap_err()
            .to_string()
            .contains("changed its exact reservation")
    );
    assert_eq!(
        store
            .external_allocation(&reservation.placement_thread_id)
            .unwrap()
            .unwrap()
            .reservation,
        reservation
    );

    // One Store is live per process: close the first before the second.
    store.close().unwrap();
    let reserved_store = test_store();
    let reserved = direct_owner_test_reservation(&reserved_store);
    {
        let inner = reserved_store.lock().unwrap();
        runtime_db::external_execution::tests::retain_direct_owner_fixture(
            &inner.runtime_db,
            &reserved,
            "reserved",
        );
    }
    assert!(
        reserved_store
            .reserve_external_allocation(&reserved, &binding)
            .unwrap_err()
            .to_string()
            .contains("born thread has no admitted capsule")
    );
    assert_eq!(
        reserved_store
            .external_allocation(&reserved.placement_thread_id)
            .unwrap()
            .unwrap()
            .phase,
        ExternalAllocationPhase::Reserved
    );
}

#[test]
fn owned_completed_terminal_signs_only_its_exact_cleared_process_attempt() {
    let store = test_store();
    let thread_id = "T-signed-settlement";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .unwrap();
    let claim = store
        .claim_thread_launch_active(thread_id, "claim-settlement", "daemon:test")
        .unwrap()
        .unwrap();
    let identity = crate::process::ExecutionProcessIdentity {
        schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
        process_scope: None,
        boot_id: "test-boot".into(),
        target_pid: 12347,
        target_start_time_ticks: 11,
        group_leader_pid: 12347,
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
            Some(&claim.claimed_by),
        )
        .unwrap();
    assert!(
        store
            .clear_thread_process_if_matches_owned(thread_id, &identity, &claim.claimed_by)
            .unwrap()
    );
    let settlement = store
        .latest_thread_process_settlement(thread_id)
        .unwrap()
        .unwrap();
    assert_eq!(settlement.launch_owner, claim.owner);
    assert_eq!(settlement.process_identity, identity);
    let (events, _) = store
        .finalize_thread_effective_owned(
            thread_id,
            &claim.claimed_by,
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_owned(),
                outcome_code: Some("completed".into()),
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
    let terminal = events
        .iter()
        .find(|event| event.event_type == ryeos_state::event_types::THREAD_COMPLETED)
        .unwrap();
    assert_eq!(
        terminal.payload["process_settlement_witness_digest"],
        settlement.digest().unwrap()
    );
    assert!(terminal.event_hash.is_some());

    for (thread_id, owned, status) in [
        ("T-unowned-settlement", false, ThreadStatus::Completed),
        ("T-failed-settlement", true, ThreadStatus::Failed),
    ] {
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .unwrap();
        let claim = store
            .claim_thread_launch_active(thread_id, "claim-settlement", "daemon:test")
            .unwrap()
            .unwrap();
        store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                Some(&claim.claimed_by),
            )
            .unwrap();
        assert!(
            store
                .clear_thread_process_if_matches_owned(thread_id, &identity, &claim.claimed_by)
                .unwrap()
        );
        let update = FinalizeThreadRecord {
            status: status.as_str().to_owned(),
            outcome_code: Some(status.as_str().to_owned()),
            result_json: None,
            error_json: None,
            artifacts: Vec::new(),
            final_cost: None,
            managed_envelope: None,
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
        };
        let (events, _) = if owned {
            store
                .finalize_thread_effective_owned(thread_id, &claim.claimed_by, &update)
                .unwrap()
        } else {
            store.finalize_thread_effective(thread_id, &update).unwrap()
        };
        let terminal = events
            .iter()
            .find(|event| event.event_type == terminal_event_type(status.as_str()).unwrap())
            .unwrap();
        assert!(
            terminal
                .payload
                .get("process_settlement_witness_digest")
                .is_none(),
            "{thread_id} must not inherit a completion settlement witness"
        );
    }

    let thread_id = "T-replaced-settlement";
    store
        .create_thread_for_test(&thread_record(thread_id, thread_id))
        .unwrap();
    let first_claim = store
        .claim_thread_launch_active(thread_id, "claim-first", "daemon:test")
        .unwrap()
        .unwrap();
    store
        .attach_new_thread_process(
            thread_id,
            identity.target_pid,
            identity.group_leader_pid,
            &identity,
            &crate::launch_metadata::RuntimeLaunchMetadata::default(),
            Some(&first_claim.claimed_by),
        )
        .unwrap();
    assert!(
        store
            .clear_thread_process_if_matches_owned(thread_id, &identity, &first_claim.claimed_by,)
            .unwrap()
    );
    assert!(
        store
            .release_active_thread_launch_claim(thread_id, "claim-first", &first_claim.claimed_by,)
            .unwrap()
    );
    let replacement = store
        .claim_thread_launch_active(thread_id, "claim-second", "daemon:test")
        .unwrap()
        .unwrap();
    assert_ne!(first_claim.owner, replacement.owner);
    let (events, _) = store
        .finalize_thread_effective_owned(
            thread_id,
            &replacement.claimed_by,
            &FinalizeThreadRecord {
                status: ThreadStatus::Completed.as_str().to_owned(),
                outcome_code: Some("completed".into()),
                result_json: None,
                error_json: None,
                artifacts: Vec::new(),
                final_cost: None,
                managed_envelope: None,
                result_project_snapshot_hash: None,
                result_workspace_output_capture_hash: None,
            },
        )
        .unwrap();
    let terminal = events
        .iter()
        .find(|event| event.event_type == ryeos_state::event_types::THREAD_COMPLETED)
        .unwrap();
    assert!(
        terminal
            .payload
            .get("process_settlement_witness_digest")
            .is_none()
    );
}

#[test]
fn workspace_binding_quarantine_requires_retained_authority_not_missing_pid() {
    let (store, binding) = workspace_binding_fixture();
    let root = "T-workspace-root";
    assert!(!store.has_retained_workspace_quarantine("T-absent").unwrap());
    assert!(!store.has_retained_workspace_quarantine(root).unwrap());
    let claim = store.get_launch_claim(root).unwrap().unwrap();
    store
        .release_active_thread_launch_claim(root, &claim.claim_id, &claim.claimed_by)
        .unwrap();
    assert!(store.has_retained_workspace_quarantine(root).unwrap());
    assert_eq!(store.thread_workspace_binding(root).unwrap(), Some(binding));

    // One Store is live per process: close the first before the second.
    store.close().unwrap();
    let (settled, binding) = workspace_binding_fixture();
    let identity = attach_workspace_test_process(&settled, &binding);
    assert!(
        settled
            .settle_reaped_thread_workspace_owned(root, &binding, &identity)
            .unwrap()
    );
    let claim = settled.get_launch_claim(root).unwrap().unwrap();
    settled
        .release_active_thread_launch_claim(root, &claim.claim_id, &claim.claimed_by)
        .unwrap();
    // Missing process identity plus an ordinary journal is insufficient.
    assert!(!settled.has_retained_workspace_quarantine(root).unwrap());
    settled
        .transition_execution_workspace(
            &binding.workspace_id,
            &[runtime_db::WorkspaceState::Active],
            runtime_db::WorkspaceState::Freezing,
            None,
        )
        .unwrap();
    assert!(settled.has_retained_workspace_quarantine(root).unwrap());
}

#[test]
fn workspace_binding_refuses_process_only_clear_until_atomic_settlement() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-process-only-clear";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    let identity = attach_workspace_test_process(&store, &binding);
    let claim = store.get_launch_claim(child).unwrap().unwrap();
    assert!(
        !store
            .clear_thread_process_if_matches(child, &identity)
            .unwrap()
    );
    assert!(
        !store
            .clear_thread_process_if_matches_owned(child, &identity, &claim.claimed_by)
            .unwrap()
    );
    assert_eq!(
        store
            .get_thread(child)
            .unwrap()
            .unwrap()
            .runtime
            .process_identity,
        Some(identity.clone())
    );
    assert_eq!(
        store.thread_workspace_binding(child).unwrap(),
        Some(binding.clone())
    );
    assert!(
        store
            .list_attached_thread_ids()
            .unwrap()
            .iter()
            .any(|id| id == child)
    );
    // Shutdown's abandoned-owner path retains the same coordinates after
    // the waiting task drops its claim; only joint settlement releases them.
    store
        .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
        .unwrap();
    assert!(
        store
            .settle_dead_thread_workspace_if_matches(child, &binding, &identity)
            .unwrap()
    );
    assert!(store.thread_workspace_binding(child).unwrap().is_none());
    assert!(
        store
            .get_thread(child)
            .unwrap()
            .unwrap()
            .runtime
            .process_identity
            .is_none()
    );
}

#[test]
fn workspace_binding_reaped_attempt_clears_exact_attachment_atomically_before_retry() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-reaped-attempt";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    let identity = attach_workspace_test_process(&store, &binding);
    let mut wrong = identity.clone();
    wrong.target_start_time_ticks += 1;
    assert!(
        !store
            .settle_reaped_thread_workspace_owned(child, &binding, &wrong)
            .unwrap()
    );
    assert_eq!(
        store
            .get_thread(child)
            .unwrap()
            .unwrap()
            .runtime
            .process_identity,
        Some(identity.clone())
    );
    assert_eq!(
        store.thread_workspace_binding(child).unwrap(),
        Some(binding.clone())
    );
    // The test supplies the owner's completed-wait proof at this boundary;
    // a nonterminal attempt may settle without changing thread lifecycle.
    assert!(!is_terminal_status(
        &store.get_thread(child).unwrap().unwrap().status
    ));
    assert!(
        store
            .settle_reaped_thread_workspace_owned(child, &binding, &identity)
            .unwrap()
    );
    assert!(
        store
            .get_thread(child)
            .unwrap()
            .unwrap()
            .runtime
            .process_identity
            .is_none()
    );
    assert!(store.thread_workspace_binding(child).unwrap().is_none());
    assert!(
        !store
            .list_attached_thread_ids()
            .unwrap()
            .iter()
            .any(|id| id == child)
    );
    assert!(
        !store
            .settle_reaped_thread_workspace_owned(child, &binding, &identity)
            .unwrap()
    );
    let claim = store.get_launch_claim(child).unwrap().unwrap();
    assert!(matches!(
        store
            .rotate_active_thread_launch_claim_after_runtime_recovery(
                child,
                &claim.claimed_by,
                "next-attempt",
                "daemon:workspace-test",
                2,
            )
            .unwrap(),
        runtime_db::RuntimeRecoveryClaimRotation::Rotated { .. }
    ));
}

#[test]
fn workspace_binding_dead_settlement_requires_abandoned_exact_attachment() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-dead-attempt";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    let identity = attach_workspace_test_process(&store, &binding);
    assert!(
        !store
            .settle_dead_thread_workspace_if_matches(child, &binding, &identity)
            .unwrap()
    );
    let claim = store.get_launch_claim(child).unwrap().unwrap();
    store
        .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
        .unwrap();
    let mut wrong = identity.clone();
    wrong.target_start_time_ticks += 1;
    assert!(
        !store
            .settle_dead_thread_workspace_if_matches(child, &binding, &wrong)
            .unwrap()
    );
    let mut wrong_binding = binding.clone();
    wrong_binding.view_identity = "replacement-view".to_owned();
    assert!(
        !store
            .settle_dead_thread_workspace_if_matches(child, &wrong_binding, &identity)
            .unwrap()
    );
    assert_eq!(
        store.thread_workspace_binding(child).unwrap(),
        Some(binding.clone())
    );
    assert!(
        store
            .settle_dead_thread_workspace_if_matches(child, &binding, &identity)
            .unwrap()
    );
    assert!(
        store
            .get_thread(child)
            .unwrap()
            .unwrap()
            .runtime
            .process_identity
            .is_none()
    );
    assert!(store.thread_workspace_binding(child).unwrap().is_none());
}

#[test]
fn workspace_binding_reaped_settlement_refuses_replacement_claim_and_unattached_contact() {
    let (store, root_binding) = workspace_binding_fixture();
    let child = "T-replaced-attempt";
    let binding = workspace_child_fixture(&store, "T-workspace-root", child, &root_binding);
    store.bind_thread_workspace(child, &binding).unwrap();
    let identity = attach_workspace_test_process(&store, &binding);
    let claim = store.get_launch_claim(child).unwrap().unwrap();
    store
        .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
        .unwrap();
    store
        .claim_thread_launch(child, "replacement", "daemon:replacement")
        .unwrap();
    assert!(
        !store
            .settle_reaped_thread_workspace_owned(child, &binding, &identity)
            .unwrap()
    );
    assert!(
        !store
            .settle_dead_thread_workspace_if_matches(child, &binding, &identity)
            .unwrap()
    );
    assert_eq!(
        store.thread_workspace_binding(child).unwrap(),
        Some(binding.clone())
    );
    assert_eq!(
        store
            .get_thread(child)
            .unwrap()
            .unwrap()
            .runtime
            .process_identity,
        Some(identity.clone())
    );

    let unknown = "T-unattached-contact";
    let unknown_binding =
        workspace_child_fixture(&store, "T-workspace-root", unknown, &root_binding);
    store
        .bind_thread_workspace(unknown, &unknown_binding)
        .unwrap();
    assert!(
        !store
            .settle_reaped_thread_workspace_owned(unknown, &unknown_binding, &identity)
            .unwrap()
    );
    let unknown_claim = store.get_launch_claim(unknown).unwrap().unwrap();
    store
        .release_active_thread_launch_claim(
            unknown,
            &unknown_claim.claim_id,
            &unknown_claim.claimed_by,
        )
        .unwrap();
    assert!(
        !store
            .settle_dead_thread_workspace_if_matches(unknown, &unknown_binding, &identity)
            .unwrap()
    );
    assert_eq!(
        store.thread_workspace_binding(unknown).unwrap(),
        Some(unknown_binding)
    );
}
