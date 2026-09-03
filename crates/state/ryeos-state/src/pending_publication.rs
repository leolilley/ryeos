//! Descriptor-bound staging publication retained across asynchronous work.

use anyhow::Result;

use crate::{PinnedStateAuthority, StagedCasRootLease};

/// Immutable CAS writes protected by durable temporary roots until their
/// authoritative consumer has been published.
///
/// This is state-store lifecycle mechanics. Admission layers may carry the
/// value, but they cannot inspect or reconstruct its pinned authority.
pub struct PendingCasPublication {
    authority: PinnedStateAuthority,
    staged_roots: Option<StagedCasRootLease>,
}

impl PendingCasPublication {
    pub fn new(authority: PinnedStateAuthority, staged_roots: StagedCasRootLease) -> Self {
        Self {
            authority,
            staged_roots: Some(staged_roots),
        }
    }

    pub fn publish(mut self) -> Result<()> {
        let guard = self.authority.acquire_shared_guard()?;
        self.authority.ensure_guard(&guard)?;
        self.staged_roots
            .as_mut()
            .expect("pending CAS publication always owns staged roots")
            .finish_admitted(&guard)?;
        self.staged_roots.take();
        Ok(())
    }

    /// Exact pinned state authority retained by this publication.
    pub fn authority(&self) -> &PinnedStateAuthority {
        &self.authority
    }

    /// Extend the same durable stage before its authoritative publication.
    pub fn staged_roots_mut(&mut self) -> &mut StagedCasRootLease {
        self.staged_roots
            .as_mut()
            .expect("pending CAS publication always owns staged roots")
    }
}

impl Drop for PendingCasPublication {
    fn drop(&mut self) {
        let Some(staged_roots) = self.staged_roots.as_mut() else {
            return;
        };
        match self.authority.acquire_shared_guard() {
            Ok(guard) => {
                if let Err(error) = staged_roots.finish_admitted(&guard) {
                    tracing::warn!(%error, "failed to discard staged CAS publication roots");
                }
            }
            Err(error) => {
                // Fail closed: keep the durable recovery roots and lease
                // record rather than resolving a new filesystem authority.
                tracing::warn!(%error, "abandoning staged CAS roots under pinned-authority failure");
            }
        }
    }
}
