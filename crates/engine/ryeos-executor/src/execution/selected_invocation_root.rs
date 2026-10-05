//! Empty composition groundwork. This module cannot install or launch an owner.
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};

const CAPACITY: usize = 32;

// Uninhabited until a separately reviewed owner implementation replaces this
// empty storage cut. This is not a fake native owner or settlement witness.
enum UnavailableOwner {}

struct EmptyRoot {
    slots: [Option<UnavailableOwner>; CAPACITY],
    closed: AtomicBool,
}

pub struct SelectedInvocationCompositionBarrier {
    root: Arc<EmptyRoot>,
    registered_generation: Option<(Arc<ryeos_app::state_store::StateStore>, ryeos_app::SelectedGeneration)>,
}

#[derive(Clone)]
pub struct SelectedInvocationAttachment {
    root: Weak<EmptyRoot>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SelectedInstallationRefusal {
    Unarmed,
    Closed,
    Detached,
}

impl SelectedInvocationCompositionBarrier {
    pub fn bootstrap() -> (Self, SelectedInvocationAttachment) {
        let root = Arc::new(EmptyRoot {
            slots: std::array::from_fn(|_| None),
            closed: AtomicBool::new(false),
        });
        let attachment = SelectedInvocationAttachment { root: Arc::downgrade(&root) };
        (Self { root, registered_generation: None }, attachment)
    }

    /// Bind only the actual shared AppState generation; this never arms contact.
    pub fn arm_from_app_generation(&mut self, app: &ryeos_app::state::AppState) -> anyhow::Result<()> {
        self.register_from_store(&app.state_store)
    }

    fn register_from_store(&mut self, store: &Arc<ryeos_app::state_store::StateStore>) -> anyhow::Result<()> {
        if self.root.closed.load(Ordering::Acquire) {
            anyhow::bail!("empty selected composition is closed");
        }
        if self.registered_generation.is_some() {
            anyhow::bail!("selected composition generation is already registered");
        }
        let generation = store.register_selected_bookkeeping_generation()?;
        self.registered_generation = Some((Arc::clone(store), generation));
        Ok(())
    }

    /// Closes only the structurally empty bootstrap. No native settlement claim.
    pub fn close_empty_bootstrap(&self) {
        // The sole constructor fills every slot with None; no owner issuer,
        // storage mutation, extraction or loan exists in this implementation.
        for slot in &self.root.slots {
            if let Some(owner) = slot { match *owner {} }
        }
        self.root.closed.store(true, Ordering::Release);
    }
}

impl Drop for SelectedInvocationCompositionBarrier {
    fn drop(&mut self) { self.close_empty_bootstrap(); }
}

impl SelectedInvocationAttachment {
    pub fn installation_refusal(&self) -> SelectedInstallationRefusal {
        match self.root.upgrade() {
            None => SelectedInstallationRefusal::Detached,
            Some(root) if root.closed.load(Ordering::Acquire) => SelectedInstallationRefusal::Closed,
            Some(_) => SelectedInstallationRefusal::Unarmed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actual_store(root: &std::path::Path) -> Arc<ryeos_app::state_store::StateStore> {
        let identity = ryeos_app::identity::NodeIdentity::create(&root.join("node.pem")).unwrap();
        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let state = root.join(".ai/state");
        Arc::new(ryeos_app::state_store::StateStore::new_with_head_trust(
            root.to_owned(), state.clone(), state.join("runtime.sqlite3"), signer,
            ryeos_app::write_barrier::WriteBarrier::new(), Arc::new(trust),
        ).unwrap())
    }

    #[test]
    fn actual_shared_generation_registration_is_one_shot_and_never_contact() {
        let root = tempfile::tempdir().unwrap();
        let store = actual_store(root.path());
        let (mut barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        barrier.register_from_store(&store).unwrap();
        assert!(Arc::ptr_eq(&barrier.registered_generation.as_ref().unwrap().0, &store));
        assert!(barrier.register_from_store(&Arc::clone(&store)).is_err());
        let other_root = tempfile::tempdir().unwrap();
        let other_store = actual_store(other_root.path());
        assert!(barrier.register_from_store(&other_store).is_err());
        assert!(Arc::ptr_eq(&barrier.registered_generation.as_ref().unwrap().0, &store));
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Unarmed);
        barrier.close_empty_bootstrap();
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Closed);
    }

    #[test]
    fn closed_root_or_store_cannot_publish_generation_association() {
        let root = tempfile::tempdir().unwrap();
        let store = actual_store(root.path());
        let (mut barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        store.close_process_attachment_admission().unwrap();
        assert!(barrier.register_from_store(&store).is_err());
        assert!(barrier.registered_generation.is_none());
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Unarmed);
        barrier.close_empty_bootstrap();
        assert!(barrier.register_from_store(&store).is_err());
        assert!(barrier.registered_generation.is_none());
    }

    #[test]
    fn bootstrap_refuses_and_empty_close_is_idempotent() {
        let (barrier, attachment) = SelectedInvocationCompositionBarrier::bootstrap();
        assert_eq!(barrier.root.slots.len(), CAPACITY);
        assert!(barrier.root.slots.iter().all(Option::is_none));
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Unarmed);
        barrier.close_empty_bootstrap();
        barrier.close_empty_bootstrap();
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Closed);
        drop(barrier);
        assert_eq!(attachment.installation_refusal(), SelectedInstallationRefusal::Detached);
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
        assert_eq!(observer.installation_refusal(), SelectedInstallationRefusal::Detached);
    }
}
