//! The explicit history-reset safety of the original Runtime mutation path:
//! which stores the offline reset may claim, what it refuses to erase, and
//! what the schema cutover rebuilds. Each store is seeded the way a live
//! node writes it, then opened only through the native mutation gate a real
//! reset uses, on one test host whose every ticket reaches the census.
use super::*;
use lillux_sqlite::test_host::TestNativeHost;
use lillux_sqlite::OriginalNativeDomain;
use std::sync::Arc;

use super::test_native::{TestRuntime, open_original_mutation, original_mutation_refusal, retire};

const NAME: &str = "runtime.sqlite3";

/// Seed the current-schema store at `path` through a live open, then run
/// `setup` on that same live connection before it closes.
fn seed(path: &Path, setup: impl for<'c> FnOnce(&'c Connection) -> Result<()>) {
    let db = TestRuntime::open(path);
    db.conn.with_sql(setup).expect("seed the Runtime store");
    drop(db);
}

/// Run `work` with a fresh test host and the pinned, exclusively locked
/// parent of the stores, then require that every issued ticket reached the
/// census and release the host.
fn with_reset_host<T>(
    parent: &Path,
    work: impl FnOnce(&TestNativeHost, &Arc<lillux::PinnedDirectory>, &lillux::PinnedDirectoryLock) -> T,
) -> T {
    let host = TestNativeHost::acquire();
    let root = Arc::new(lillux::PinnedDirectory::open(parent).unwrap().unwrap());
    let namespace = root.lock_exclusive().unwrap();
    let value = work(&host, &root, &namespace);
    drop(namespace);
    drop(root);
    let (issued, recorded) = host.ticket_census();
    assert_eq!(issued as usize, recorded, "every issued ticket reaches the census");
    drop(host.release());
    value
}

/// Read the stored application id through the native gate.
fn stored_application_id(
    host: &TestNativeHost,
    root: &Arc<lillux::PinnedDirectory>,
    name: &str,
) -> u32 {
    host.with_existing(Arc::clone(root), OsStr::new(name), OriginalNativeDomain::NodeRuntime, |connection| {
        Ok(connection.query_row("PRAGMA application_id", [], |row| row.get::<_, u32>(0))?)
    })
}

fn predecessor_application_id() -> u32 {
    RUNTIME_OPERATOR_APP_ID_PREFIX | (RUNTIME_OPERATOR_SCHEMA_EPOCH - 1)
}

fn insert_action_intent(connection: &Connection, operation_id: &str, authority: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO runtime_action_intent(
             operation_id, chain_root_id, first_caller_thread_id, mode, request_hash, child_thread_id,
             child_project_authority, created_at_ms
         ) VALUES (?1, 'T-chain', 'T-parent', 'detached', 'request', 'T-child', ?2, 1)",
        params![operation_id, authority],
    )?;
    Ok(())
}

#[test]
fn original_mutation_refuses_newer_operator_epoch_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(NAME);
    let newer = RUNTIME_OPERATOR_APP_ID_PREFIX | (RUNTIME_OPERATOR_SCHEMA_EPOCH + 1);
    seed(&path, |connection| Ok(connection.pragma_update(None, "application_id", newer)?));

    with_reset_host(dir.path(), |host, root, namespace| {
        let refusal = original_mutation_refusal(host, root, namespace, &path);
        assert!(refusal.contains("newer operator epoch"), "{refusal}");
        assert!(refusal.contains("destructive reset refused"), "{refusal}");
        assert!(
            !refusal.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND),
            "{refusal}"
        );
        assert_eq!(stored_application_id(host, root, NAME), newer, "the newer store is not erased");
    });
}

#[test]
fn v1_hook_dispatch_epoch_refuses_live_open_and_is_reset_eligible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(NAME);
    seed(&path, |connection| {
        Ok(connection.pragma_update(None, "application_id", RUNTIME_OPERATOR_APP_ID_PREFIX | 1)?)
    });

    let refusal = super::test_native::live_open_refusal(&path);
    assert!(refusal.contains("stored schema_epoch=1"), "{refusal}");
    assert!(refusal.contains("explicit no-backcompat reset"), "{refusal}");

    with_reset_host(dir.path(), |host, root, namespace| {
        // A proven predecessor is reset-eligible, never migrated: the open
        // classifies it and changes nothing until the reset is applied.
        let mut reset = open_original_mutation(host, root, namespace, &path);
        assert!(reset.requires_explicit_history_reset());
        assert_eq!(reset.operator_schema_epoch().unwrap(), 1);
        reset.fixture_apply_explicit_history_reset(&path).unwrap();
        assert!(!reset.requires_explicit_history_reset());
        assert_eq!(reset.operator_schema_epoch().unwrap(), RUNTIME_OPERATOR_SCHEMA_EPOCH);
        retire(host, reset);
    });
}

#[test]
fn other_ryeos_database_ids_are_outside_the_runtime_operator_family() {
    let dir = tempfile::tempdir().unwrap();
    // An unclaimed database, then each other RyeOS store family.
    let stores = [
        ("foreign.sqlite3", 0),
        ("scheduler.sqlite3", 0x5259_5343),
        ("operational.sqlite3", 0x5259_4f50),
        ("projection.sqlite3", 0x5259_504a),
        ("accounting.sqlite3", 0x5259_4143),
    ];
    for (name, application_id) in stores {
        super::test_native::with_created(&dir.path().join(name), |connection| {
            connection.execute_batch("CREATE TABLE foreign_state(value TEXT);")?;
            Ok(connection.pragma_update(None, "application_id", application_id)?)
        });
    }

    with_reset_host(dir.path(), |host, root, namespace| {
        for (name, application_id) in stores {
            let refusal = original_mutation_refusal(host, root, namespace, &dir.path().join(name));
            assert!(refusal.contains("unowned store"), "{name}: {refusal}");
            assert_eq!(stored_application_id(host, root, name), application_id as u32);
        }
    });

    for (name, _) in stores {
        let retained: i64 = super::test_native::read_existing(&dir.path().join(name), |connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='foreign_state'",
                [],
                |row| row.get(0),
            )?)
        });
        assert_eq!(retained, 1, "another RyeOS database stays unowned and intact: {name}");
    }
}

#[test]
fn predecessor_reset_never_decodes_embedded_authority_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(NAME);
    seed(&path, |connection| {
        insert_action_intent(connection, "opaque-row", "not-json-and-never-decoded")?;
        connection.pragma_update(None, "application_id", predecessor_application_id())?;
        Ok(())
    });

    with_reset_host(dir.path(), |host, root, namespace| {
        let mut reset = open_original_mutation(host, root, namespace, &path);
        assert!(reset.requires_explicit_history_reset());
        reset.fixture_apply_explicit_history_reset(&path).unwrap();
        assert_eq!(
            reset.fixture_discard_all_thread_history(true).unwrap().total_rows(),
            Some(0)
        );
        retire(host, reset);
    });
}

#[test]
fn original_mutation_refuses_newer_project_authority_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(NAME);
    let newer = lillux::canonical_json(&serde_json::json!({
        "kind": PROJECT_AUTHORITY_ENVELOPE_KIND,
        "schema_epoch": PROJECT_AUTHORITY_SCHEMA_EPOCH + 1,
        "authority": {"deliberately": "newer"}
    }))
    .unwrap();
    seed(&path, |connection| insert_action_intent(connection, "op-newer", &newer));

    with_reset_host(dir.path(), |host, root, namespace| {
        let refusal = original_mutation_refusal(host, root, namespace, &path);
        assert!(
            refusal.contains(&format!("stored schema_epoch={}", PROJECT_AUTHORITY_SCHEMA_EPOCH + 1)),
            "an older daemon must not erase newer runtime authority: {refusal}"
        );
        assert!(
            !refusal.contains(crate::execution_history_reset::EXECUTION_SCHEMA_CUTOVER_COMMAND),
            "{refusal}"
        );
        let retained: String = host.with_existing(
            Arc::clone(root),
            OsStr::new(NAME),
            OriginalNativeDomain::NodeRuntime,
            |connection| {
                Ok(connection.query_row(
                    "SELECT child_project_authority FROM runtime_action_intent WHERE operation_id='op-newer'",
                    [],
                    |row| row.get(0),
                )?)
            },
        );
        assert_eq!(retained, newer);
    });
}

#[test]
fn explicit_history_reset_replaces_any_owned_predecessor_schema_without_interpretation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(NAME);
    seed(&path, |connection| {
        connection.execute_batch(
            "CREATE TABLE \"retired parent\" (id INTEGER PRIMARY KEY);
             CREATE TABLE \"retired \"\"runtime\"\" table\" (
                 parent_id INTEGER NOT NULL REFERENCES \"retired parent\"(id),
                 value TEXT NOT NULL
             );
             CREATE INDEX \"retired index\"
                 ON \"retired \"\"runtime\"\" table\"(value);
             CREATE VIEW \"retired view\" AS
                 SELECT value FROM \"retired \"\"runtime\"\" table\";
             CREATE TRIGGER \"retired trigger\"
                 AFTER INSERT ON \"retired \"\"runtime\"\" table\"
                 BEGIN
                     UPDATE \"retired \"\"runtime\"\" table\"
                        SET value = NEW.value
                      WHERE rowid = NEW.rowid;
                 END;
             INSERT INTO \"retired parent\"(id) VALUES (1);
             INSERT INTO \"retired \"\"runtime\"\" table\"(parent_id, value)
                 VALUES (1, 'opaque predecessor state');",
        )?;
        connection.pragma_update(None, "application_id", predecessor_application_id())?;
        Ok(())
    });

    with_reset_host(dir.path(), |host, root, namespace| {
        let mut reset = open_original_mutation(host, root, namespace, &path);
        assert!(reset.requires_explicit_history_reset());
        reset.fixture_apply_explicit_history_reset(&path).unwrap();
        assert!(!reset.requires_explicit_history_reset());
        reset
            .conn
            .with_sql(|connection| assert_current_runtime_schema(connection, &path, reset.conn.schema_reference()))
            .unwrap();
        assert_eq!(
            reset.fixture_discard_all_thread_history(true).unwrap().total_rows(),
            Some(0)
        );
        assert!(reset.open_workspaces().unwrap().is_empty());
        retire(host, reset);
    });

    // The rebuilt store is the exact current schema a live node opens.
    let reopened = TestRuntime::open(&path);
    assert_eq!(
        reopened.fixture_discard_all_thread_history(true).unwrap().total_rows(),
        Some(0)
    );
    drop(reopened);
}

#[test]
fn explicit_history_reset_rebuilds_credentials_only_from_stable_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(NAME);
    seed(&path, |connection| {
        connection.execute_batch(
            "DROP INDEX idx_credential_profile_owner_state;
             DROP TABLE credential_profile;
             CREATE TABLE credential_profile (
                 deliberately_unknown_predecessor_shape BLOB NOT NULL
             );
             INSERT INTO credential_profile VALUES (x'00');",
        )?;
        connection.pragma_update(None, "application_id", predecessor_application_id())?;
        Ok(())
    });
    let stable = ryeos_state::OperationalCredentialProfileRecord {
        profile_id: "profile-stable".to_owned(),
        owner_principal: "fp:operator".to_owned(),
        home_id: "credential-stable".to_owned(),
        authority_revision: 7,
        credential_generation: 4,
        state: "active".to_owned(),
        active_login_id: None,
        login_epoch: 3,
        login_expires_at_ms: None,
        sanitized_account: Some(serde_json::json!({"account_id":"account-stable"})),
        created_at_ms: 1,
        updated_at_ms: 2,
    };

    with_reset_host(dir.path(), |host, root, namespace| {
        let mut reset = open_original_mutation(host, root, namespace, &path);
        assert!(reset.requires_explicit_history_reset());
        reset.fixture_apply_explicit_history_reset(&path).unwrap();
        // The predecessor projection row is never interpreted: the rebuilt
        // projection holds only what the stable authority restores.
        assert!(reset.credential_profile_projections().unwrap().is_empty());
        reset.reconcile_credential_profile_projection(&stable).unwrap();
        let restored = reset.credential_profile("profile-stable").unwrap().unwrap();
        assert_eq!(restored.state, "active");
        assert_eq!(restored.credential_generation, 4);
        assert_eq!(restored.authority_revision, 7);
        retire(host, reset);
    });
}

#[test]
fn original_mutation_open_never_creates_an_absent_store() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing-runtime.sqlite3");
    let current = dir.path().join(NAME);
    seed(&current, |_connection| Ok(()));

    with_reset_host(dir.path(), |host, root, namespace| {
        original_mutation_refusal(host, root, namespace, &missing);
        assert!(!missing.exists(), "an existing-only open creates nothing");
        let db = open_original_mutation(host, root, namespace, &current);
        assert!(!db.requires_explicit_history_reset());
        retire(host, db);
    });
    assert!(!missing.exists());
}
