//! Daemon-owned selected invocation generation registration and immutable
//! publication. Publication does not authorize native contact or certify
//! settlement.
use crate::{SelectedApplicationPublication, SelectedRootClose};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

struct SelectedRoot {
    generation: std::sync::Mutex<
        Option<(
            Arc<ryeos_app::state_store::StateStore>,
            ryeos_app::SelectedGeneration,
        )>,
    >,
    closed: AtomicBool,
}

impl SelectedRoot {
    fn has_unresolved_owners(&self) -> bool {
        // Shutdown must not wait for an in-flight registration or slot writer.
        // Contention is unresolved custody, never a positive empty receipt.
        self.generation.try_lock().is_err()
    }
}

pub struct SelectedInvocationCompositionBarrier {
    root: Arc<SelectedRoot>,
    registered_generation: Option<(
        Arc<ryeos_app::state_store::StateStore>,
        ryeos_app::SelectedGeneration,
    )>,
}

#[derive(Clone)]
/// Refusal observation only; no owned root/native extraction or loan issuer.
/// ```compile_fail
/// let (barrier, attachment) = ryeos_executor::SelectedInvocationCompositionBarrier::bootstrap();
/// let owned = attachment.into_inner();
/// ```
/// ```compile_fail
/// let (barrier, _) = ryeos_executor::SelectedInvocationCompositionBarrier::bootstrap();
/// let loan = barrier.borrow_native();
/// ```
pub struct SelectedInvocationAttachment {
    root: Weak<SelectedRoot>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SelectedInstallationRefusal {
    Unarmed,
    Closed,
    Detached,
}

impl SelectedInvocationCompositionBarrier {
    pub fn bootstrap() -> (Self, SelectedInvocationAttachment) {
        let root = Arc::new(SelectedRoot {
            generation: std::sync::Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let attachment = SelectedInvocationAttachment {
            root: Arc::downgrade(&root),
        };
        (
            Self {
                root,
                registered_generation: None,
            },
            attachment,
        )
    }

    /// Bind only the actual shared AppState generation; this never arms contact.
    pub fn arm_from_app_generation(
        &mut self,
        app: &ryeos_app::state::AppState,
    ) -> anyhow::Result<()> {
        self.register_from_store(&app.state_store)
    }

    fn register_from_store(
        &mut self,
        store: &Arc<ryeos_app::state_store::StateStore>,
    ) -> anyhow::Result<()> {
        if self.root.closed.load(Ordering::Acquire) {
            anyhow::bail!("empty selected composition is closed");
        }
        if self.registered_generation.is_some() {
            anyhow::bail!("selected composition generation is already registered");
        }
        let generation = store.register_selected_bookkeeping_generation()?;
        // Store work is complete before acquiring root publication storage.
        let mut registered = self
            .root
            .generation
            .lock()
            .map_err(|_| anyhow::anyhow!("selected root generation storage poisoned"))?;
        anyhow::ensure!(
            !self.root.closed.load(Ordering::Acquire),
            "selected root closed during registration"
        );
        anyhow::ensure!(
            registered.is_none(),
            "selected root generation already registered"
        );
        *registered = Some((Arc::clone(store), generation.clone()));
        self.registered_generation = Some((Arc::clone(store), generation));
        Ok(())
    }

    /// Capture the actual armed App/Executor generation in one immutable object.
    /// This publication is a carrier, not a product or contact admission.
    pub fn publication(
        &self,
        app: Arc<ryeos_app::state::AppState>,
    ) -> anyhow::Result<Arc<SelectedApplicationPublication>> {
        let (store, _) = self
            .registered_generation
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("selected publication generation is unarmed"))?;
        anyhow::ensure!(
            Arc::ptr_eq(store, &app.state_store),
            "selected publication App belongs to another Store generation"
        );
        anyhow::ensure!(
            !self.root.closed.load(Ordering::Acquire),
            "selected publication root is closed"
        );
        Ok(Arc::new(SelectedApplicationPublication {
            app,
            attachment: SelectedInvocationAttachment {
                root: Arc::downgrade(&self.root),
            },
        }))
    }

    pub fn has_unresolved_owners(&self) -> bool {
        self.root.has_unresolved_owners()
    }

    /// Non-owning close handle for a supervisor that cannot borrow the
    /// barrier while its startup future is still alive.
    pub fn admission_fence(&self) -> SelectedRootAdmissionFence {
        SelectedRootAdmissionFence(Arc::downgrade(&self.root))
    }

    /// Close admission before observing retained custody. Contended or
    /// poisoned generation storage requires reconciliation. This never
    /// extracts an owner or treats a stopped waiter as settlement.
    pub fn close(&self) -> SelectedRootClose {
        self.root.closed.store(true, Ordering::Release);
        if self.root.has_unresolved_owners() {
            SelectedRootClose::Unresolved
        } else {
            SelectedRootClose::Closed
        }
    }

    /// After a Closed observation, release the Store generation this barrier
    /// registered, so the sole live Store owner can perform its consuming
    /// close. Refuses while admission is open or any selected owner remains,
    /// and leaves both registrations in place if they disagree.
    pub fn release_closed_store_generation(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.root.closed.load(Ordering::Acquire),
            "selected root admission is still open"
        );
        anyhow::ensure!(
            !self.root.has_unresolved_owners(),
            "selected invocation owners remain unresolved"
        );
        let mut generation = self
            .root
            .generation
            .lock()
            .map_err(|_| anyhow::anyhow!("selected root generation poisoned"))?;
        let consistent = match (generation.as_ref(), self.registered_generation.as_ref()) {
            (None, None) => true,
            (Some((root_store, _)), Some((barrier_store, _))) => {
                Arc::ptr_eq(root_store, barrier_store)
            }
            _ => false,
        };
        anyhow::ensure!(
            consistent,
            "selected root and barrier disagree on the registered Store generation"
        );
        let root_registration = generation.take();
        drop(generation);
        let barrier_registration = self.registered_generation.take();
        drop(root_registration);
        drop(barrier_registration);
        Ok(())
    }
}

/// Closes new selected attachments only. Holds no root, Store or owner; a
/// detached root needs no fence. The barrier's own close still reports
/// unresolved owners.
pub struct SelectedRootAdmissionFence(std::sync::Weak<SelectedRoot>);

impl SelectedRootAdmissionFence {
    pub fn close(&self) {
        if let Some(root) = self.0.upgrade() {
            root.closed.store(true, Ordering::Release);
        }
    }
}

/// The owners of a selected composition barrier dropped while its root still
/// reported unresolved custody.
struct AbandonedSelectedComposition {
    _root: Arc<SelectedRoot>,
    _registered_generation: Option<(
        Arc<ryeos_app::state_store::StateStore>,
        ryeos_app::SelectedGeneration,
    )>,
}

/// The kind under which a selected composition barrier dropped with
/// unresolved owners is retained for the life of the process.
const ABANDONED_SELECTED_COMPOSITION_KIND: &str = "ryeos-executor::selected-composition";

/// Count of selected composition barriers this process retains because they
/// were dropped with unresolved owners. Any nonzero count means a selected
/// root and its Store generation stay held until the process exits.
pub fn abandoned_selected_compositions() -> usize {
    lillux::custody::abandoned_custody().count(ABANDONED_SELECTED_COMPOSITION_KIND)
}

impl Drop for SelectedInvocationCompositionBarrier {
    fn drop(&mut self) {
        if self.close() == SelectedRootClose::Unresolved {
            // Drop cannot run a bounded native or financial reconciliation.
            // Keep the exact root and Store alive even if a caller misses the
            // normal daemon unresolved hold: both move whole into the
            // process-lifetime custody registry; no root mutex re-entry,
            // ledger write or synthetic positive receipt.
            let abandoned = AbandonedSelectedComposition {
                _root: Arc::clone(&self.root),
                _registered_generation: self.registered_generation.take(),
            };
            lillux::custody::abandon(ABANDONED_SELECTED_COMPOSITION_KIND, abandoned);
        }
    }
}

impl SelectedInvocationAttachment {
    pub(crate) fn validate_for_store(
        &self,
        store: &Arc<ryeos_app::state_store::StateStore>,
    ) -> anyhow::Result<()> {
        self.validate_for_store_identity(Arc::as_ptr(store))
    }

    /// The Store generation check by identity alone: the root must be armed
    /// with exactly the Store at `store`.
    fn validate_for_store_identity(
        &self,
        store: *const ryeos_app::state_store::StateStore,
    ) -> anyhow::Result<()> {
        let root = self
            .root
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("selected invocation root detached"))?;
        anyhow::ensure!(
            !root.closed.load(Ordering::Acquire),
            "selected invocation root closed"
        );
        let generation = root
            .generation
            .lock()
            .map_err(|_| anyhow::anyhow!("selected root generation poisoned"))?;
        let (registered, _) = generation
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("selected invocation root unarmed"))?;
        anyhow::ensure!(
            std::ptr::eq(Arc::as_ptr(registered), store),
            "selected invocation Store generation mismatch"
        );
        Ok(())
    }

    pub fn installation_refusal(&self) -> SelectedInstallationRefusal {
        match self.root.upgrade() {
            None => SelectedInstallationRefusal::Detached,
            Some(root) if root.closed.load(Ordering::Acquire) => {
                SelectedInstallationRefusal::Closed
            }
            Some(_) => SelectedInstallationRefusal::Unarmed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner dereferences to the Store `Arc`, closes the Store when
    /// dropped and stays on the opening thread.
    fn actual_store(root: &std::path::Path) -> ryeos_app::test_support::TestStateStore {
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&root.join("node.pem")).unwrap();
        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        ryeos_app::test_support::TestStateStore::open(root, signer, Arc::new(trust)).unwrap()
    }

    #[test]
    fn actual_shared_generation_registration_is_one_shot_and_never_contact() {
        let root = tempfile::tempdir().unwrap();
        let store = actual_store(root.path());
        let (mut barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        barrier.register_from_store(&store).unwrap();
        assert!(Arc::ptr_eq(
            &barrier.registered_generation.as_ref().unwrap().0,
            store.store()
        ));
        // Registration is one-shot: a registered barrier refuses before it
        // consults whatever Store is offered, this one or any other.
        assert!(
            barrier
                .register_from_store(&Arc::clone(store.store()))
                .is_err()
        );
        assert!(Arc::ptr_eq(
            &barrier.registered_generation.as_ref().unwrap().0,
            store.store()
        ));
        assert!(attachment.validate_for_store(&store).is_ok());
        // One process keeps a single live Store, so another Store generation
        // is offered by identity alone.
        assert!(
            attachment
                .validate_for_store_identity(std::ptr::null())
                .is_err()
        );
        let elsewhere = std::sync::Arc::new(0_u8);
        assert!(
            attachment
                .validate_for_store_identity(std::sync::Arc::as_ptr(&elsewhere).cast())
                .is_err()
        );
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Unarmed
        );
        assert_eq!(barrier.close(), SelectedRootClose::Closed);
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Closed
        );
    }

    #[test]
    fn closed_empty_root_releases_its_store_generation_only_after_close() {
        let root = tempfile::tempdir().unwrap();
        let store = actual_store(root.path());
        let (mut barrier, _attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        barrier.register_from_store(&store).unwrap();
        assert_eq!(Arc::strong_count(store.store()), 3);
        assert!(barrier.release_closed_store_generation().is_err());
        assert_eq!(Arc::strong_count(store.store()), 3);
        assert_eq!(barrier.close(), SelectedRootClose::Closed);
        barrier.release_closed_store_generation().unwrap();
        assert!(barrier.registered_generation.is_none());
        assert!(barrier.root.generation.lock().unwrap().is_none());
        assert_eq!(Arc::strong_count(store.store()), 1);
        barrier.release_closed_store_generation().unwrap();
    }

    #[test]
    fn closed_root_or_store_cannot_publish_generation_association() {
        let root = tempfile::tempdir().unwrap();
        let store = actual_store(root.path());
        let (mut barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        store.close_process_attachment_admission().unwrap();
        assert!(barrier.register_from_store(&store).is_err());
        assert!(barrier.registered_generation.is_none());
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Unarmed
        );
        assert_eq!(barrier.close(), SelectedRootClose::Closed);
        assert!(barrier.register_from_store(&store).is_err());
        assert!(barrier.registered_generation.is_none());
    }

    #[test]
    fn bootstrap_refuses_and_empty_close_is_idempotent() {
        let (barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        assert!(!barrier.has_unresolved_owners());
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Unarmed
        );
        assert_eq!(barrier.close(), SelectedRootClose::Closed);
        assert_eq!(barrier.close(), SelectedRootClose::Closed);
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Closed
        );
        drop(barrier);
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Detached
        );
    }

    #[test]
    fn weak_attachment_does_not_retain_barrier_after_unwind() {
        let (barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        let observer = attachment.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _barrier = barrier;
            panic!("fixture unwind");
        }));
        assert!(result.is_err());
        assert_eq!(
            observer.installation_refusal(),
            SelectedInstallationRefusal::Detached
        );
    }
}

#[cfg(test)]
mod selected_close_controls {
    use super::*;

    #[test]
    fn empty_close_is_positive_and_prevents_publication() {
        let (barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        assert_eq!(barrier.close(), SelectedRootClose::Closed);
        assert_eq!(barrier.close(), SelectedRootClose::Closed);
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Closed
        );
    }
    #[test]
    fn contended_generation_storage_reports_unresolved_without_blocking() {
        let (barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        let held = barrier.root.generation.lock().unwrap();
        assert_eq!(barrier.close(), SelectedRootClose::Unresolved);
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Closed
        );
        drop(held);
        assert_eq!(barrier.close(), SelectedRootClose::Closed);
    }

    #[test]
    fn poisoned_generation_storage_never_certifies_empty_and_drop_abandons_root() {
        let (barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        let root = Arc::clone(&barrier.root);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _generation = root.generation.lock().unwrap();
            panic!("storage uncertainty");
        }));
        assert!(barrier.has_unresolved_owners());
        assert_eq!(barrier.close(), SelectedRootClose::Unresolved);
        assert_eq!(
            attachment.installation_refusal(),
            SelectedInstallationRefusal::Closed
        );
        let observer = Arc::downgrade(&root);
        let abandoned_before = abandoned_selected_compositions();
        drop(root);
        drop(barrier);
        // Drop retains the SAME root in the counted registry; it does not
        // invent closure.
        assert!(observer.upgrade().is_some());
        assert!(abandoned_selected_compositions() > abandoned_before);
    }
}
