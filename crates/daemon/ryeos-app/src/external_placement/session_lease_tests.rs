//! The lease request a placed session asks its binding's adapter to admit,
//! built from the session's admitted profile, the binding and node policy.

use super::*;
use ryeos_node_runtime::node_policy::sections::execution::ExternalLeaseBudget;

fn remote_profile() -> serde_json::Value {
    serde_json::json!({
        "external_candidate": {"kind": "fixture"},
        "requires": {
            "workload_class": "codex.authoring.remote",
            "controller": ["time"],
            "credential_kinds": {"credential_profile": ["credential:private_home"]},
            "lease": [
                {"name": "lease.create_bound", "accept": ["provider_attested"]},
                {"name": "lease.network_policy", "accept": ["provider_attested"]}
            ],
            "guest": [{"name": "guest.exec_route_closed", "accept": ["provider_attested"]}]
        }
    })
}

fn budget() -> ExternalLeaseBudget {
    ExternalLeaseBudget {
        max_concurrent_leases: 2,
        max_lease_lifetime_seconds: 4 * 60 * 60,
        daily_lease_seconds: 8 * 60 * 60,
        turn_end_margin_seconds: 30,
    }
}

fn contract() -> ExternalPlacementBackendContract {
    RetainedExternalExecutionBinding::test_fixture().backend_contract()
}

#[test]
fn a_remote_session_asks_for_one_lease_built_from_signed_data() {
    let (inputs, charge) =
        session_lease_request(&remote_profile(), 120_500, &contract(), Some(&budget()))
            .unwrap()
            .unwrap();
    assert_eq!(inputs.workload, "codex.authoring.remote");
    assert_eq!(
        inputs
            .requires_before_spend
            .iter()
            .map(|entry| entry.label())
            .collect::<Vec<_>>(),
        ["lease.create_bound", "lease.network_policy"]
    );
    assert_eq!(inputs.requires_from_guest.len(), 1);
    // The guest may reach only the binding's controller origin's host.
    assert_eq!(inputs.controller_host, "controller.example");
    // The lease lives as long as the binding's execution timeout and must
    // be activated within the session's readiness budget.
    assert_eq!(inputs.maximum_lifetime_seconds, 60);
    assert_eq!(inputs.activation_deadline_seconds, 121);
    assert_eq!(charge.lifetime_seconds, 60);
    assert_eq!(charge.max_concurrent_leases, 2);
    assert_eq!(charge.daily_lease_seconds, 8 * 60 * 60);
}

#[test]
fn a_session_without_lease_requirements_needs_no_lease_or_budget() {
    let mut profile = remote_profile();
    let requires = profile["requires"].as_object_mut().unwrap();
    requires.remove("lease");
    requires.remove("guest");
    assert!(
        session_lease_request(&profile, 1_000, &contract(), None)
            .unwrap()
            .is_none()
    );
    assert!(
        session_lease_request(&serde_json::json!({}), 1_000, &contract(), None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_lease_is_refused_plainly_without_a_budget_that_allows_it() {
    let text = |error: anyhow::Error| format!("{error:#}");
    let missing =
        text(session_lease_request(&remote_profile(), 1_000, &contract(), None).unwrap_err());
    assert!(
        missing.contains("workload `codex.authoring.remote` needs a remote lease")
            && missing.contains("declares no external lease budget"),
        "{missing}"
    );

    let short = ExternalLeaseBudget {
        max_lease_lifetime_seconds: 59,
        ..budget()
    };
    let too_long = text(
        session_lease_request(&remote_profile(), 1_000, &contract(), Some(&short)).unwrap_err(),
    );
    assert!(
        too_long.contains("60s lifetime exceeds the node's 59s lease lifetime ceiling"),
        "{too_long}"
    );
}

#[test]
fn a_lease_is_refused_when_the_controller_has_no_dns_host_for_the_guest() {
    let mut contract = contract();
    contract
        .controller_transport
        .http_mut()
        .unwrap()
        .https_origin = "https://192.0.2.1".into();
    let refusal = format!(
        "{:#}",
        session_lease_request(&remote_profile(), 1_000, &contract, Some(&budget())).unwrap_err()
    );
    assert!(refusal.contains("names no HTTPS DNS host"), "{refusal}");
}

/// One app (a thread holds one test store) whose one installed backend is
/// the binding's fixture generation, which admits no leases.
fn lease_app(
    root: &std::path::Path,
) -> (crate::state_store::test_native::TestStateStore, AppState) {
    let (store_owner, mut state) = crate::state::test_support::build(root).unwrap();
    state.external_placement_backends = Arc::new(
        ExternalPlacementBackendRegistry::from_backends(vec![Arc::new(
            super::tests::FixtureBackend {
                artifact: "d".repeat(64),
            },
        )])
        .unwrap(),
    );
    (store_owner, state)
}

/// Give the app an execution policy carrying `budget` and a fresh vault
/// that counts and refuses every operation; returns that vault.
fn with_budget_and_counting_vault(
    state: &mut AppState,
    budget: Option<ExternalLeaseBudget>,
) -> Arc<crate::vault::CountingRefusingVault> {
    let vault = Arc::new(crate::vault::CountingRefusingVault::default());
    state.vault = vault.clone();
    let policy =
        ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy {
            schema: 4,
            max_live_fanout: 1,
            max_private_materialization_copy_bytes: 1024 * 1024,
            host_env_passthrough: vec![],
            workload_client: None,
            resource_authority: None,
            producer_resource_ceiling: None,
            external_lease_budget: budget,
        };
    state.node_policy = Arc::new(
        ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(vec![Arc::new(
            policy,
        )]),
    );
    vault
}

/// The plain refusal a lease admission returned. The credential carries no
/// `Debug`, so a success is reported without it.
fn refusal_of<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("lease admission and credential read unexpectedly succeeded"),
        Err(error) => format!("{error:#}"),
    }
}

fn now_ms() -> i64 {
    i64::try_from(lillux::time::timestamp_millis()).unwrap()
}

/// A deadline ten seconds away, which the fixture binding's 60s lease
/// covers with the 30s margin.
fn fitting_deadline() -> Result<Option<i64>> {
    Ok(Some(now_ms() + 10_000))
}

#[test]
fn a_refused_lease_never_reads_the_vault() {
    let binding = RetainedExternalExecutionBinding::test_fixture();
    let contract = binding.backend_contract();
    let access = binding.credential_access().unwrap();
    let profile = remote_profile();

    let root = tempfile::tempdir().unwrap();
    let (_store_owner, mut state) = lease_app(root.path());

    // The adapter refuses the lease.
    let vault = with_budget_and_counting_vault(&mut state, Some(budget()));
    let refusal = refusal_of(admit_fresh_lease_then_read_credential(
        &state,
        Some(&profile),
        1_000,
        fitting_deadline,
        &contract,
        &access,
    ));
    assert!(refusal.contains("admits no leases"), "{refusal}");
    assert_eq!(vault.calls(), 0, "a refused lease read the vault");

    // The node declares no lease budget.
    let vault = with_budget_and_counting_vault(&mut state, None);
    let refusal = refusal_of(admit_fresh_lease_then_read_credential(
        &state,
        Some(&profile),
        1_000,
        fitting_deadline,
        &contract,
        &access,
    ));
    assert!(
        refusal.contains("declares no external lease budget"),
        "{refusal}"
    );
    assert_eq!(vault.calls(), 0, "a refused lease read the vault");

    // A session that needs no lease goes on to read the credential, which
    // this vault refuses: the counter does see vault reads.
    let mut no_lease = remote_profile();
    let requires = no_lease["requires"].as_object_mut().unwrap();
    requires.remove("lease");
    requires.remove("guest");
    let refusal = refusal_of(admit_fresh_lease_then_read_credential(
        &state,
        Some(&no_lease),
        1_000,
        || -> Result<Option<i64>> {
            panic!("a session that needs no lease was measured against one")
        },
        &contract,
        &access,
    ));
    assert!(
        refusal.contains("vault refused placement_credential"),
        "{refusal}"
    );
    assert_eq!(vault.calls(), 1);
}

#[test]
fn a_session_its_lease_cannot_cover_is_refused_before_the_adapter_or_the_vault() {
    let binding = RetainedExternalExecutionBinding::test_fixture();
    let contract = binding.backend_contract();
    let access = binding.credential_access().unwrap();
    let profile = remote_profile();
    let root = tempfile::tempdir().unwrap();
    let (_store_owner, mut state) = lease_app(root.path());
    let vault = with_budget_and_counting_vault(&mut state, Some(budget()));

    // The fixture binding's lease lives 60s and the margin is 30s, so a
    // deadline 31s away does not fit; one with no deadline never does. The
    // fixture adapter admits no lease at all, so a refusal that names the
    // deadline was decided before the adapter was asked.
    for deadline in [Some(now_ms() + 31_000), None] {
        let refusal = refusal_of(admit_fresh_lease_then_read_credential(
            &state,
            Some(&profile),
            1_000,
            || Ok(deadline),
            &contract,
            &access,
        ));
        assert!(refusal.starts_with("lease refused: "), "{refusal}");
        assert!(
            refusal.contains("30s turn-end margin") || refusal.contains("this one has none"),
            "{refusal}"
        );
        assert!(!refusal.contains("admits no leases"), "{refusal}");
    }
    // A session past its deadline is refused by the deadline itself.
    let refusal = refusal_of(admit_fresh_lease_then_read_credential(
        &state,
        Some(&profile),
        1_000,
        || anyhow::bail!("budget_exhausted: bounded worker absolute lifetime expired"),
        &contract,
        &access,
    ));
    assert!(refusal.contains("budget_exhausted"), "{refusal}");
    assert_eq!(vault.calls(), 0, "an unfit session read the vault");

    // A session that fits goes on to the adapter.
    let refusal = refusal_of(admit_fresh_lease_then_read_credential(
        &state,
        Some(&profile),
        1_000,
        fitting_deadline,
        &contract,
        &access,
    ));
    assert!(refusal.contains("admits no leases"), "{refusal}");
    assert_eq!(vault.calls(), 0);
}

/// A placed session `T-one` holding a 60s lease, reserved and not yet
/// contacted, in a store under `root`, with an app over it whose policy
/// carries `budget`. The store is opened with a node key at
/// `root/node-key.pem`, so it can be reopened as a restarted controller.
fn leased_session(
    root: &std::path::Path,
    budget: Option<ExternalLeaseBudget>,
) -> (
    crate::state_store::test_native::TestStateStore,
    AppState,
    ExternalAllocationReservation,
) {
    let identity =
        ryeos_node_runtime::identity::NodeIdentity::create(&root.join("node-key.pem")).unwrap();
    let (store_owner, store, lifetime) = open_store(root, &identity);
    let binding = RetainedExternalExecutionBinding::test_fixture();
    let channel_authority = ExternalChannelAuthority::test_fixture(&"3".repeat(64));
    let now = now_ms();
    let reservation = ExternalAllocationReservation {
        schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
        placement_thread_id: "T-one".into(),
        admitted_capsule_hash: "a".repeat(64),
        owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
            workspace_id: "W-one".into(),
            worker_instance_id: "worker-one".into(),
            worker_boot_epoch: 1,
        }),
        base_snapshot_hash: "b".repeat(64),
        binding_hash: binding.digest().into(),
        capacity_owner: binding.capacity_owner().into(),
        channel_authority_generation: "3".repeat(64),
        channel_owner_public_key: channel_authority.owner_public_key(),
        channel_bootstrap_capability_hash: channel_authority.bootstrap_capability_hash(),
        admitted_adapter_request_base64: None,
        request_digest: "e".repeat(64),
        max_active: 1,
        timeout_seconds: 60,
        startup_started_at_ms: now,
        startup_deadline_ms: now + 60_000,
        contact_deadline_ms: now + 60_000,
    };
    // The fixture installs the session's thread, workspace and session rows
    // with a lease-free allocation. That allocation is settled and removed so
    // the lease-holding one can be reserved in its place.
    store
        .install_external_placement_test_fixture(&reservation, &binding)
        .unwrap();
    store
        .cancel_uncontacted_external_allocation("T-one")
        .unwrap();
    store
        .with_runtime_sql_for_test(|connection| {
            Ok(connection.execute(
                "DELETE FROM external_execution_allocation WHERE placement_thread_id='T-one'",
                [],
            )?)
        })
        .unwrap();
    let mut leased = reservation;
    leased.admitted_adapter_request_base64 = Some(
        ExternalAllocationReservation::encode_admitted_adapter_request(b"{\"lease\":1}").unwrap(),
    );
    leased.request_digest = "f".repeat(64);
    store
        .reserve_external_lease_allocation(
            &leased,
            &binding,
            &crate::runtime_db::external_execution::ExternalLeaseCharge {
                lifetime_seconds: 60,
                budget_weight: 1,
                max_concurrent_leases: 2,
                daily_lease_seconds: 8 * 60 * 60,
            },
        )
        .unwrap();
    let mut state = crate::state::test_support::build_with_store(root, store, lifetime).unwrap();
    with_budget_and_counting_vault(&mut state, budget);
    (store_owner, state, leased)
}

fn open_store(
    root: &std::path::Path,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> (
    crate::state_store::test_native::TestStateStore,
    Arc<crate::state_store::StateStore>,
    Arc<StateLockLease>,
) {
    let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
        identity,
    ));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    let owner =
        crate::state_store::test_native::TestStateStore::open(root, signer, Arc::new(trust))
            .unwrap();
    let lifetime = owner.controller_lifetime();
    let store = Arc::clone(owner.store());
    (owner, store, lifetime)
}

fn phase(state: &AppState) -> ExternalAllocationPhase {
    state
        .state_store
        .external_allocation("T-one")
        .unwrap()
        .unwrap()
        .phase
}

#[test]
fn a_route_its_lease_cannot_carry_fences_the_lease_only_when_the_session_is_idle() {
    let root = tempfile::tempdir().unwrap();
    let (_store_owner, state, _) = leased_session(root.path(), Some(budget()));
    assert!(
        state
            .state_store
            .held_external_lease("T-one")
            .unwrap()
            .is_some()
    );

    // A deadline the 60s lease covers with the 30s margin is admitted.
    admit_turn_on_external_lease(&state, "T-one", Some(now_ms() + 10_000), true).unwrap();

    // A deadline it does not cover is refused; with a turn in progress the
    // lease is left to it.
    let late = Some(now_ms() + 10 * 60 * 1_000);
    let refusal = format!(
        "{:#}",
        admit_turn_on_external_lease(&state, "T-one", late, false).unwrap_err()
    );
    assert!(
        refusal.contains("left to the turn in progress")
            && refusal.contains("turn refused: the remote lease ends in"),
        "{refusal}"
    );
    assert_eq!(phase(&state), ExternalAllocationPhase::Reserved);
    assert!(
        state
            .state_store
            .held_external_lease("T-one")
            .unwrap()
            .is_some()
    );

    // On an idle session the same refusal fences the lease, which settles
    // this uncontacted allocation and releases its budget.
    let refusal = format!(
        "{:#}",
        admit_turn_on_external_lease(&state, "T-one", late, true).unwrap_err()
    );
    assert!(
        refusal.contains("the lease is being terminated")
            && refusal.contains("30s turn-end margin"),
        "{refusal}"
    );
    assert_eq!(phase(&state), ExternalAllocationPhase::NoContact);
    assert!(
        state
            .state_store
            .held_external_lease("T-one")
            .unwrap()
            .is_none()
    );
    let refusal = format!(
        "{:#}",
        admit_turn_on_external_lease(&state, "T-one", late, true).unwrap_err()
    );
    assert!(refusal.contains("remote lease has ended"), "{refusal}");
}

#[test]
fn a_route_without_a_lease_budget_fences_only_an_idle_session() {
    let root = tempfile::tempdir().unwrap();
    let (_store_owner, state, _) = leased_session(root.path(), None);
    let deadline = Some(now_ms() + 10_000);
    let refusal = format!(
        "{:#}",
        admit_turn_on_external_lease(&state, "T-one", deadline, false).unwrap_err()
    );
    assert!(
        refusal.contains("no longer declares an external lease budget"),
        "{refusal}"
    );
    assert_eq!(phase(&state), ExternalAllocationPhase::Reserved);
    admit_turn_on_external_lease(&state, "T-one", deadline, true).unwrap_err();
    assert_eq!(phase(&state), ExternalAllocationPhase::NoContact);
}

#[test]
fn a_new_worker_fences_only_its_own_sessions_earlier_lease() {
    let root = tempfile::tempdir().unwrap();
    let (_store_owner, state, _) = leased_session(root.path(), Some(budget()));
    let record = state
        .state_store
        .external_allocation("T-one")
        .unwrap()
        .unwrap();
    let refused = |result: Result<()>| format!("{:#}", result.unwrap_err());

    // Another workspace's allocation is refused and left untouched.
    let other = refused(lease_lifecycle::refuse_ended_session_lease(
        &state,
        &record,
        "W-two",
        "worker-two",
        2,
    ));
    assert!(other.contains("left untouched"), "{other}");
    assert_eq!(phase(&state), ExternalAllocationPhase::Reserved);

    // So is another owner's.
    let mut foreign = record.clone();
    foreign.reservation.owner = ExternalAllocationOwner::DirectThread {
        chain_root_id: "T-one".into(),
        launch_owner: crate::runtime_db::LaunchOwner {
            thread_id: "T-one".into(),
            monotonic_launch_epoch: 1,
            unpredictable_nonce: "nonce".into(),
            daemon_generation_id: "daemon".into(),
        },
        program: crate::thread_lifecycle::external_direct_program_test_fixture(),
    };
    let foreign = refused(lease_lifecycle::refuse_ended_session_lease(
        &state,
        &foreign,
        "W-one",
        "worker-two",
        2,
    ));
    assert!(
        foreign.contains("another owner's remote execution"),
        "{foreign}"
    );
    assert_eq!(phase(&state), ExternalAllocationPhase::Reserved);

    // The allocation's own worker runs on it.
    lease_lifecycle::refuse_ended_session_lease(&state, &record, "W-one", "worker-one", 1).unwrap();

    // A later worker of the same session fences it.
    let earlier = refused(lease_lifecycle::refuse_ended_session_lease(
        &state,
        &record,
        "W-one",
        "worker-two",
        2,
    ));
    assert!(
        earlier.contains("bound to worker epoch 1, which has ended"),
        "{earlier}"
    );
    assert_eq!(phase(&state), ExternalAllocationPhase::NoContact);
    assert!(
        state
            .state_store
            .held_external_lease("T-one")
            .unwrap()
            .is_none()
    );
}

/// Drop the app and store under `root` and reopen them as a restarted
/// controller over the same node key.
fn restart(
    root: &std::path::Path,
    previous: (crate::state_store::test_native::TestStateStore, AppState),
    budget: Option<ExternalLeaseBudget>,
) -> (crate::state_store::test_native::TestStateStore, AppState) {
    let (store_owner, state) = previous;
    drop(state);
    drop(store_owner);
    let identity =
        ryeos_node_runtime::identity::NodeIdentity::load(&root.join("node-key.pem")).unwrap();
    let (store_owner, store, lifetime) = open_store(root, &identity);
    // The app's own identity keys are minted afresh for the new app.
    std::fs::remove_dir_all(root.join("identity")).unwrap();
    let mut state = crate::state::test_support::build_with_store(root, store, lifetime).unwrap();
    with_budget_and_counting_vault(&mut state, budget);
    (store_owner, state)
}

#[test]
fn after_a_restart_an_uncontacted_predecessor_lease_is_settled_and_released() {
    let root = tempfile::tempdir().unwrap();
    let (store_owner, state, _) = leased_session(root.path(), Some(budget()));
    let (_store_owner, state) = restart(root.path(), (store_owner, state), Some(budget()));
    let report = terminate_predecessor_external_leases(&state).unwrap();
    assert_eq!(report.discovered, 1);
    assert_eq!(report.terminated, ["T-one"]);
    assert!(
        report.pending.is_empty() && report.failures.is_empty(),
        "{report:?}"
    );
    assert_eq!(phase(&state), ExternalAllocationPhase::NoContact);
    assert!(
        state
            .state_store
            .held_external_lease("T-one")
            .unwrap()
            .is_none()
    );
}

#[test]
fn after_a_restart_a_contacted_session_lease_is_prepared_for_cleanup_from_its_reservation() {
    let root = tempfile::tempdir().unwrap();
    let (store_owner, state, leased) = leased_session(root.path(), Some(budget()));
    assert!(matches!(
        state
            .state_store
            .claim_external_allocation_contact("T-one", &leased.request_digest)
            .unwrap(),
        ExternalAllocationContactClaim::Contact(_)
    ));
    let (_store_owner, state) = restart(root.path(), (store_owner, state), Some(budget()));
    // A possibly contacted allocation cannot be settled without the
    // provider, so the fence quarantines it and keeps its lease charged.
    request_external_candidate_cleanup(&state, "T-one").unwrap();
    assert_eq!(phase(&state), ExternalAllocationPhase::Quarantined);
    assert!(
        state
            .state_store
            .held_external_lease("T-one")
            .unwrap()
            .is_some()
    );
    // Cleanup preparation works from the retained reservation under the
    // restarted controller: it passes the controller lease, the owner and the
    // retained binding, and stops only at the admitted session capsule, which
    // this storage fixture does not hold.
    let refusal =
        match ExternalPlacementOwner::new(&state).prepare_retained_session_for_cleanup("T-one") {
            Ok(_) => panic!("cleanup preparation found a capsule the fixture never stored"),
            Err(error) => format!("{error:#}"),
        };
    assert!(
        refusal.contains("admitted session capsule disappeared"),
        "{refusal}"
    );
    assert!(
        state
            .state_store
            .held_external_lease("T-one")
            .unwrap()
            .is_some()
    );
}
