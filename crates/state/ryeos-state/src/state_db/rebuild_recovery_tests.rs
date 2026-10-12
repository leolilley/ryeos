//! Interrupted projection rebuilds on a live State's native build context:
//! the factory, generation leases and generation publication a node's own
//! rebuild uses, on this thread's shared test host. Each refusal is settled
//! into the host's census before the State closes, so the host still releases.

use std::sync::atomic::{AtomicBool, Ordering};

use lillux_sqlite::test_host::TestNativeHost;

use super::*;
use crate::objects::thread_snapshot::{
    CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
    CapturedThreadHistoryPolicy, ThreadHistoryRetention, ThreadSnapshot, ThreadSnapshotBuilder,
};
use crate::signer::TestSigner;
use crate::test_support::TestState;

fn test_trust_store() -> Arc<TrustStore> {
    let signer = TestSigner::default();
    let mut trust_store = TrustStore::new();
    trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
    Arc::new(trust_store)
}

fn test_root_snapshot(item_ref: &str) -> ThreadSnapshot {
    ThreadSnapshotBuilder::new(
        "T-root",
        "T-root",
        "directive",
        item_ref,
        "directive-runtime",
    )
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

fn create_test_root_chain(db: &StateDb) -> String {
    db.create_chain(
        "T-root",
        test_root_snapshot("directive:system/test"),
        &TestSigner::default(),
    )
    .unwrap()
    .value
    .chain_state_hash
}

/// Cancelled from the start: the rebuild stops while it stages, before it
/// publishes anything.
struct AlwaysCancelled;
impl ProjectionRecoveryObserver for AlwaysCancelled {
    fn update(&self, _progress: ProjectionRecoveryProgress) {}

    fn is_cancelled(&self) -> bool {
        true
    }
}

/// Cancels once acknowledging replay reports progress. A rebuild stages its
/// replay under a cancellation-only observer, so the first replay progress
/// it reports belongs to the selected instance, after the generation pointer
/// is published and before any pending transition is acknowledged.
#[derive(Default)]
struct CancelAtAcknowledgingReplay {
    cancelled: AtomicBool,
}
impl ProjectionRecoveryObserver for CancelAtAcknowledgingReplay {
    fn update(&self, progress: ProjectionRecoveryProgress) {
        if progress.stage == ProjectionRecoveryStage::ReplayingPending {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// The generation pointer as `state` reads it, inside its native scope.
fn selected_generation(state: &StateDb) -> ProjectionRecoveryGeneration {
    state
        .recovery
        .read_generation_in(state.guarded_read_factory())
        .unwrap()
        .expect("selected projection generation present")
}

/// Rebuild a fresh generation on `state`'s own native build context, the
/// one its admitted offline rebuild uses, observed by `observer`.
fn rebuild_on_live_state(
    state: &mut StateDb,
    observer: &dyn ProjectionRecoveryObserver,
) -> anyhow::Result<(ProjectionDb, crate::rebuild::RebuildReport)> {
    let runtime_state_dir = state
        .cas_root
        .parent()
        .expect("CAS root has a runtime-state parent")
        .to_path_buf();
    let context = state
        .original_projection_context
        .as_mut()
        .expect("live State native build context");
    rebuild_and_install_projection(
        &runtime_state_dir,
        &state.cas_root,
        &state.refs_root,
        &state.recovery,
        state.trust_store.as_ref(),
        None,
        Some(observer),
        None,
        context,
    )
}

/// Close the projection a refused rebuild retained into `host`'s census and
/// return the refusal text. A publication refusal also carries the staged
/// generation's durable owner; that generation's closed terminal, its
/// durability proof and its lease already belong to the State's build
/// context, which hands them to the census when the State closes.
fn settle_refused_rebuild(host: &TestNativeHost, error: anyhow::Error) -> String {
    let message = format!("{error:#}");
    let error = match error.downcast::<GeneratedProjectionPublicationFailure>() {
        Ok(GeneratedProjectionPublicationFailure {
            error,
            _staged,
            _recovery,
            _instance_id,
            _projection_file,
        }) => {
            drop((_staged, _recovery, _instance_id, _projection_file));
            error
        }
        Err(error) => error,
    };
    let failure = match error.downcast::<crate::projection::OriginalProjectionFailure>() {
        Ok(failure) => failure,
        Err(error) => panic!("refused rebuild retained no projection owner: {error:#}"),
    };
    let released = match failure.retry_close(host.factory()) {
        Ok(released) => released,
        Err(failure) => panic!("retained rebuild projection close refused: {failure}"),
    };
    let (filesystem, terminal, _refusal) = released.into_outside_custody();
    host.retain_terminal(terminal);
    // The generation directory and a lease alias; the build context's lease
    // registry keeps the lease itself and closes it with the State.
    drop(filesystem);
    message
}

#[test]
fn interrupted_rebuild_never_replaces_the_selected_generation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let mut db = TestState::open(&root, test_trust_store()).unwrap();
    create_test_root_chain(&db);
    let old_path = db.selected_projection_path().to_path_buf();
    let old_generation = selected_generation(&db);

    let error = match rebuild_on_live_state(&mut db, &AlwaysCancelled) {
        Ok(_) => panic!("a cancelled rebuild was published"),
        Err(error) => error,
    };
    let refusal = settle_refused_rebuild(db.host(), error);
    assert!(
        refusal.contains("projection recovery cancelled"),
        "{refusal}"
    );
    assert_eq!(selected_generation(&db), old_generation);
    assert_eq!(
        db.projection().projection_instance_id().unwrap().as_deref(),
        Some(old_generation.projection_instance_id.as_str())
    );
    db.close().unwrap();

    // Recovery still selects the instance the pointer named before.
    let reopened = TestState::open(&root, test_trust_store()).unwrap();
    assert_eq!(reopened.selected_projection_path(), old_path);
    assert_eq!(selected_generation(&reopened), old_generation);
    assert_eq!(
        reopened
            .projection()
            .projection_instance_id()
            .unwrap()
            .as_deref(),
        Some(old_generation.projection_instance_id.as_str())
    );
    assert!(reopened.get_thread("T-root").unwrap().is_some());
    reopened.close().unwrap();
}

#[test]
fn generation_pointer_published_before_ack_recovers_selected_instance() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    let mut db = TestState::open(&root, test_trust_store()).unwrap();
    let target_hash = create_test_root_chain(&db);
    let transition = {
        let chain_lock = crate::chain::ChainLock::acquire(db.refs_root(), "T-root").unwrap();
        db.recovery
            .prepare_set(&chain_lock, "T-root", None, &target_hash)
            .unwrap()
    };
    let old_generation = selected_generation(&db);

    let observer = CancelAtAcknowledgingReplay::default();
    let error = match rebuild_on_live_state(&mut db, &observer) {
        Ok(_) => panic!("a rebuild cancelled before acknowledgement completed"),
        Err(error) => error,
    };
    let refusal = settle_refused_rebuild(db.host(), error);
    assert!(
        refusal.contains("projection recovery cancelled"),
        "{refusal}"
    );
    let published = selected_generation(&db);
    assert_ne!(
        published.projection_instance_id, old_generation.projection_instance_id,
        "the rebuilt generation must be published before the cancellation"
    );
    assert_eq!(
        db.recovery
            .read_pending("T-root")
            .unwrap()
            .expect("the pending transition must survive")
            .transition_id,
        transition.transition_id,
        "publishing generation.json must not acknowledge staged replay"
    );
    db.close().unwrap();

    let reopened = TestState::open(&root, test_trust_store()).unwrap();
    assert_eq!(
        reopened.selected_projection_path(),
        root.join(&published.projection_file)
    );
    assert_eq!(
        reopened
            .projection()
            .projection_instance_id()
            .unwrap()
            .as_deref(),
        Some(published.projection_instance_id.as_str())
    );
    assert!(reopened.recovery.read_pending("T-root").unwrap().is_none());
    assert_eq!(
        reopened
            .projection()
            .get_projection_meta("T-root")
            .unwrap()
            .unwrap()
            .indexed_chain_state_hash,
        target_hash
    );
    reopened.close().unwrap();
}
