//! The whole explicit execution-history reset composed over a node whose
//! every store is written and read through the native gate. The Runtime
//! store an interrupted attempt held is opened only through the original
//! history-reset mutation path a real reset uses; each reset runs under the
//! process-wide test serial guard, since it issues its own native host.
use super::*;
use std::ffi::OsStr;
use tempfile::TempDir;

/// Run `work` on the existing store `name` beneath `directory` on one bound
/// ticket of this thread's shared test host, released afterwards.
fn with_existing_store<T>(
    directory: &Path,
    name: &str,
    domain: lillux_sqlite::OriginalNativeDomain,
    work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> Result<T>,
) -> T {
    let root = Arc::new(lillux::PinnedDirectory::open(directory).unwrap().unwrap());
    let host = ryeos_state::test_support::shared_test_host();
    let value = host.with_existing(root, OsStr::new(name), domain, work);
    ryeos_state::test_support::release_shared_test_host(host);
    value
}

#[test]
fn history_reset_composes_replay_cutover_and_preserves_stable_credentials() {
    let tmp = TempDir::new().unwrap();
    let config = Config::load(&ConfigSources {
        app_root: Some(tmp.path().to_owned()),
        ..Default::default()
    })
    .unwrap();
    let identity =
        ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
    super::tests::install_offline_policy(&config, &identity, true);
    let identity_bytes = std::fs::read(&config.node_signing_key_path).unwrap();
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    let trust = Arc::new(trust);
    let runtime_state_dir = config.runtime_state_dir();
    // State and a current Runtime store, established the way a first live
    // boot establishes them.
    crate::runtime_db::test_native::seed_node(&config, true);
    drop(StateLock::acquire(&default_lock_path(tmp.path())).unwrap());

    let profile = ryeos_state::operational::OperationalCredentialProfileRecord {
        profile_id: "retained-active".to_owned(),
        owner_principal: "operator".to_owned(),
        home_id: "home".to_owned(),
        authority_revision: 3,
        credential_generation: 2,
        state: "active".to_owned(),
        active_login_id: None,
        login_epoch: 1,
        login_expires_at_ms: None,
        sanitized_account: Some(serde_json::json!({"account_id":"retained-account"})),
        created_at_ms: 1,
        updated_at_ms: 2,
    };
    let mut enrolling = profile.clone();
    enrolling.profile_id = "retired-enrollment".to_owned();
    enrolling.home_id = "enrollment-home".to_owned();
    enrolling.state = "enrolling".to_owned();
    enrolling.active_login_id = Some("old-login".to_owned());
    enrolling.login_expires_at_ms = Some(100);

    // Raw imports are durable CAS staging authority, outside the retired
    // thread and Runtime rows: a qualification recovery reopens these exact
    // uploads under the new source generation.
    let import_owner = identity.fingerprint().to_owned();
    let import_key =
        ryeos_state::DurableCasPublicationKey::external_content_import(&"a".repeat(64)).unwrap();
    let (import_stage_id, import_object_hash) = {
        let state =
            ryeos_state::test_support::TestState::open(&runtime_state_dir, Arc::clone(&trust))
                .unwrap();
        let staged = {
            let authority = state.pinned_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let object_hash = authority
                .cas_store()
                .unwrap()
                .store_object(&serde_json::json!({"kind": "raw-import-reset-fixture"}))
                .unwrap();
            let mut stage = authority
                .require_recovery()
                .unwrap()
                .begin_durable_cas_upload_admitted(
                    &guard,
                    &import_owner,
                    "external-content-import",
                    &import_key,
                    None,
                )
                .unwrap();
            stage
                .protect_cas_closure(&guard, [object_hash.as_str()], std::iter::empty())
                .unwrap();
            (stage.staging_id().to_owned(), object_hash)
        };
        state
            .merge_operational_credential_profile(&profile)
            .unwrap();
        state
            .merge_operational_credential_profile(&enrolling)
            .unwrap();
        state.close().unwrap();
        staged
    };

    // A predecessor replay-index epoch and a predecessor Runtime operator
    // epoch: the reset must cut both over in one composition.
    let operational_name = ryeos_state::operational::OPERATIONAL_DB_FILENAME;
    let operational_path = runtime_state_dir.join(operational_name);
    let current_replay_epoch = with_existing_store(
        &runtime_state_dir,
        operational_name,
        lillux_sqlite::OriginalNativeDomain::NodeOperational,
        |connection| {
            let epoch: i32 =
                connection
                    .query_row("SELECT epoch FROM replay_index_epoch", [], |row| row.get(0))?;
            connection.execute_batch("UPDATE replay_index_epoch SET epoch = epoch - 2;")?;
            Ok(epoch)
        },
    );
    crate::runtime_db::test_native::read_existing(&config.db_path, |connection| {
        let application_id: i32 =
            connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        connection.pragma_update(None, "application_id", application_id - 1)?;
        Ok(())
    });

    let options = ExecutionHistoryResetOptions {
        app_root: Some(tmp.path().to_owned()),
        dry_run: true,
        discard_project_heads: false,
        schema_cut: Some(ExecutionHistorySchemaCut {
            from: RuntimeDb::current_operator_schema_epoch() - 1,
            to: RuntimeDb::current_operator_schema_epoch(),
        }),
        discard_unreadable_lifetime_fence_after_reboot: false,
    };
    let runtime_before = std::fs::read(&config.db_path).unwrap();
    let operational_before = std::fs::read(&operational_path).unwrap();
    let preview = {
        let _serial = lillux_sqlite::test_host::serial();
        run_execution_history_reset(&options, None).unwrap()
    };
    assert!(!preview.performed);
    assert_eq!(
        preview.schema_cut.as_ref().unwrap().status,
        ExecutionHistorySchemaCutStatus::Required
    );
    assert_eq!(
        preview.replay_indexes.scope,
        ryeos_state::operational::ReplayIndexResetScope::AllReplayRecords
    );
    assert_eq!(
        preview.replay_indexes.stored_epoch,
        current_replay_epoch - 2
    );
    assert_eq!(std::fs::read(&config.db_path).unwrap(), runtime_before);
    assert_eq!(
        std::fs::read(&operational_path).unwrap(),
        operational_before
    );
    // The preview leaves the predecessor replay epoch a live node refuses.
    let refusal = {
        let host = ryeos_state::test_support::shared_test_host();
        let refusal = ryeos_state::test_support::live_state_open_refusal(
            &host,
            &runtime_state_dir,
            Arc::clone(&trust),
        );
        ryeos_state::test_support::release_shared_test_host(host);
        refusal
    };
    assert!(
        refusal.contains("replay indexes use predecessor epoch"),
        "{refusal}"
    );

    // Model resumption after a process died with the durable discard intent
    // published but before replay or Runtime activation. The interrupted
    // attempt holds the Runtime store through the original mutation path.
    {
        let host = ryeos_state::test_support::shared_test_host();
        let state_lock = StateLock::acquire(&default_lock_path(tmp.path())).unwrap();
        let directory = lillux::PinnedDirectory::open(&runtime_state_dir)
            .unwrap()
            .unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let state = ryeos_state::test_support::TestOfflineState::open(
            &runtime_state_dir,
            &namespace,
            Arc::clone(&trust),
        )
        .unwrap();
        let runtime_root = Arc::new(
            lillux::PinnedDirectory::open(config.db_path.parent().unwrap())
                .unwrap()
                .unwrap(),
        );
        assert!(directory.is_same_directory(&runtime_root).unwrap());
        let mut db = crate::runtime_db::test_native::open_original_mutation(
            &host,
            &runtime_root,
            &namespace,
            &config.db_path,
        );
        assert!(db.requires_explicit_history_reset());
        {
            let authority = state.pinned_authority().unwrap();
            let guard = authority.acquire_exclusive_guard(true).unwrap();
            let offline =
                OfflineResetAuthority::capture(&state_lock, &directory, &namespace, &config, &db)
                    .unwrap();
            // Before durable intent the captured authority admits no Runtime
            // mutation, and it is never captured for another namespace or
            // another app root.
            assert!(
                db.apply_explicit_history_reset(&offline, &config.db_path)
                    .is_err()
            );
            assert!(db.discard_all_thread_history(&offline).is_err());
            assert!(
                db.requires_explicit_history_reset(),
                "a refused reset changes nothing"
            );
            let foreign = TempDir::new().unwrap();
            let foreign_directory = lillux::PinnedDirectory::open(foreign.path())
                .unwrap()
                .unwrap();
            let foreign_namespace = foreign_directory.lock_exclusive().unwrap();
            assert!(
                OfflineResetAuthority::capture(
                    &state_lock,
                    &directory,
                    &foreign_namespace,
                    &config,
                    &db
                )
                .is_err()
            );
            drop(foreign_namespace);
            let other_root = TempDir::new().unwrap();
            let other_config = Config::load(&ConfigSources {
                app_root: Some(other_root.path().to_owned()),
                ..Default::default()
            })
            .unwrap();
            assert!(
                OfflineResetAuthority::capture(
                    &state_lock,
                    &directory,
                    &namespace,
                    &other_config,
                    &db
                )
                .is_err()
            );

            let policy = OfflineResetPolicyOwner::load(
                &config,
                &identity,
                &state_lock,
                &directory,
                &namespace,
            )
            .unwrap();
            let quota_claim = OriginalQuotaClaim::prepare(
                &policy,
                &identity,
                None,
                inspection_destination::OriginalInspectionMode::Mutating,
                false,
            )
            .unwrap();
            let operation = OriginalOperation::prepare(
                &offline,
                &db,
                &authority,
                &guard,
                &identity,
                &options,
                false,
                &ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                    counts: Default::default(),
                },
                None,
                &quota_claim,
            )
            .unwrap();
            operation.stage(&directory).unwrap();
            state
                .begin_thread_history_discard_admitted(&guard, operation.reference())
                .unwrap();
        }
        assert!(
            db.requires_explicit_history_reset(),
            "the interrupted attempt never reached Runtime"
        );
        crate::runtime_db::test_native::retire(&host, db);
        state.close().unwrap();
        drop(namespace);
        drop(directory);
        drop(state_lock);
        ryeos_state::test_support::release_shared_test_host(host);
    }

    let options = ExecutionHistoryResetOptions {
        dry_run: false,
        ..options
    };
    let report = {
        let _serial = lillux_sqlite::test_host::serial();
        run_execution_history_reset(&options, None).unwrap()
    };
    assert!(report.performed);
    assert_eq!(
        report.schema_cut.as_ref().unwrap().status,
        ExecutionHistorySchemaCutStatus::Completed
    );
    assert_eq!(report.replay_indexes, preview.replay_indexes);

    // Stable credentials survive unchanged; the retired enrollment ceremony
    // is invalidated, and the durable raw-import upload is still reopenable.
    {
        let state =
            ryeos_state::test_support::TestState::open(&runtime_state_dir, Arc::clone(&trust))
                .unwrap();
        let profiles = state.operational_credential_profiles().unwrap();
        let find = |profile_id: &str| {
            profiles
                .iter()
                .find(|candidate| candidate.profile_id == profile_id)
                .cloned()
        };
        assert_eq!(find(&profile.profile_id), Some(profile.clone()));
        let retired = find(&enrolling.profile_id).unwrap();
        assert_eq!(retired.state, "unauthenticated");
        assert_eq!(retired.authority_revision, enrolling.authority_revision + 1);
        assert_eq!(
            retired.credential_generation,
            enrolling.credential_generation
        );
        assert_eq!(retired.home_id, enrolling.home_id);
        assert!(retired.active_login_id.is_none());
        assert!(retired.sanitized_account.is_none());
        {
            let authority = state.pinned_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let stage = authority
                .require_recovery()
                .unwrap()
                .open_durable_cas_upload_admitted(&guard, &import_stage_id, &import_owner)
                .unwrap();
            stage
                .ensure_publication_contract(&import_key, None)
                .unwrap();
            stage.ensure_protects_object(&import_object_hash).unwrap();
        }
        state.close().unwrap();
    }
    // The cut-over Runtime store is the exact current schema a live node opens.
    drop(crate::runtime_db::test_native::TestRuntime::open(
        &config.db_path,
    ));

    let repeated = {
        let _serial = lillux_sqlite::test_host::serial();
        run_execution_history_reset(&options, None).unwrap()
    };
    assert!(!repeated.performed);
    assert!(!repeated.dry_run);
    assert_eq!(
        repeated.schema_cut.as_ref().unwrap().status,
        ExecutionHistorySchemaCutStatus::AlreadyCurrent
    );
    assert_eq!(
        repeated.replay_indexes.scope,
        ryeos_state::operational::ReplayIndexResetScope::Unchanged
    );
    assert_eq!(
        std::fs::read(&config.node_signing_key_path).unwrap(),
        identity_bytes
    );
}
