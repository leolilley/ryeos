//! Thread-affine mutation admission for the actual original Operational owner.
//! Verification borrows the real caller frame; it cannot move a CAS guard into
//! a native Send/Sync authority callback or replace it with decoded identity.
use super::*;
use lillux_sqlite::{OriginalNativeDomain, OriginalNativeFactory};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;

pub struct OriginalOperationalMutationAuthority<'frame> {
    root: &'frame lillux::PinnedDirectory,
    factory: &'frame Arc<OriginalNativeFactory>,
    verify: &'frame dyn Fn() -> Result<()>,
    _thread: PhantomData<Rc<()>>,
}
impl<'frame> OriginalOperationalMutationAuthority<'frame> {
    /// # Safety
    /// The verifier must revalidate the caller's actual stopped-node mutation
    /// authority on every call. For execution-history reset that is the
    /// authenticated staged OriginalOperation, pointer-identical original
    /// funded claim, original offline mutation frame and exclusive State/CAS
    /// guard, plus the exact durable discard marker under that same State
    /// incarnation. For explicit replay-index activation it is the exclusive
    /// node StateLock, the runtime namespace lock and the operation window of
    /// the native host that funded `factory` under that exclusion. A phase
    /// enum, decoded operation reference or fresh claim is insufficient.
    /// All borrowed owners must remain live throughout this capability's use.
    pub unsafe fn issued(
        root: &'frame lillux::PinnedDirectory,
        factory: &'frame Arc<OriginalNativeFactory>,
        verify: &'frame dyn Fn() -> Result<()>,
    ) -> Result<Self> {
        verify()?;
        root.ensure_path_binding()?;
        factory.require_runtime_binding(root)?;
        Ok(Self {
            root,
            factory,
            verify,
            _thread: PhantomData,
        })
    }

    fn require(&self, db: &OperationalDb) -> Result<()> {
        // Actual intent/funding/State/CAS checks precede any admitted SQL or
        // mutable Operational action. They are repeated on every retry.
        (self.verify)()?;
        let OperationalConnection::Original {
            owner,
            factory,
            reference: _,
        } = &db.conn
        else {
            anyhow::bail!(
                "original mutation authority cannot authorize an ordinary Operational owner"
            )
        };
        anyhow::ensure!(
            self.factory.same_original(factory)
                && owner.domain() == OriginalNativeDomain::OperationalReplayReset,
            "Operational mutation authority selects a foreign factory or readonly owner"
        );
        anyhow::ensure!(
            self.root.is_same_directory(&db._runtime_directory)?,
            "Operational mutation authority selects a different runtime incarnation"
        );
        self.root.ensure_path_binding()?;
        db._directory_lock.ensure_protects(self.root)?;
        ensure_operational_bindings(db)
    }
}

impl PreparedReplayIndexReset {
    pub fn activate_original(
        &mut self,
        authority: &OriginalOperationalMutationAuthority<'_>,
    ) -> Result<ReplayIndexResetReport> {
        authority.require(
            self.db
                .as_ref()
                .context("replay reset owner already transferred")?,
        )?;
        self.activate_after_authority()
    }

    pub fn take_activated_original(
        &mut self,
        authority: &OriginalOperationalMutationAuthority<'_>,
    ) -> Result<(OperationalDb, ReplayIndexResetReport)> {
        authority.require(
            self.db
                .as_ref()
                .context("replay reset owner already transferred")?,
        )?;
        self.take_activated_after_authority()
    }
}
impl OperationalDb {
    pub fn merge_credential_profile_original(
        &self,
        authority: &OriginalOperationalMutationAuthority<'_>,
        candidate: &OperationalCredentialProfileRecord,
    ) -> Result<OperationalCredentialProfileRecord> {
        authority.require(self)?;
        self.merge_credential_profile_after_authority(candidate)
    }
}
