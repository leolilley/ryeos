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

pub(crate) struct SelectedInvocationCompositionBarrier {
    root: Arc<EmptyRoot>,
}

#[derive(Clone)]
pub(crate) struct SelectedInvocationAttachment {
    root: Weak<EmptyRoot>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SelectedInstallationRefusal {
    Unarmed,
    Closed,
    Detached,
}

impl SelectedInvocationCompositionBarrier {
    pub(crate) fn bootstrap() -> (Self, SelectedInvocationAttachment) {
        let root = Arc::new(EmptyRoot {
            slots: std::array::from_fn(|_| None),
            closed: AtomicBool::new(false),
        });
        let attachment = SelectedInvocationAttachment { root: Arc::downgrade(&root) };
        (Self { root }, attachment)
    }

    /// Closes only the structurally empty bootstrap. No native settlement claim.
    pub(crate) fn close_empty_bootstrap(&self) {
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
    pub(crate) fn installation_refusal(&self) -> SelectedInstallationRefusal {
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
        let result = std::panic::catch_unwind(move || {
            let _barrier = barrier;
            panic!("fixture unwind");
        });
        assert!(result.is_err());
        assert_eq!(observer.installation_refusal(), SelectedInstallationRefusal::Detached);
    }
}
