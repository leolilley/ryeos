//! Retained publisher for the one capacity redo role. Namespace mechanics and
//! recovery belong to Lillux; transition meaning and original funding stay here.

use super::redo::{AccountRedo, ANCHOR_BYTES};
use anyhow::{Context, Result, ensure};
use lillux::{PinnedDirectory, PinnedDirectoryIdentity, PinnedRegularFile, PinnedRegularFileIdentity};
use lillux::time::MonotonicDeadline;
use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;
use std::ffi::OsStr;

pub(super) const PROTOCOL: &str = "ryeos/native-capacity-redo/v1";
const NAMESPACE_ENTRIES: usize = 8;
pub(super) const READ_WORK: u64 = ANCHOR_BYTES as u64 / 64 + 16;
// Fixed-block copying/hashing plus bounded namespace/descriptor/syscall phases;
// these are control-work units, not a wall-clock or I/O completion guarantee.
pub(super) const PUBLICATION_WORK: u64 = (3 * ANCHOR_BYTES as u64 + 16 * 1024) / 64 + 128;
pub(super) const NAMESPACE_WORK: u64 = NAMESPACE_ENTRIES as u64 * 8 + 8;

/// Worst charged work for one retained anchor publication attempt. The
/// mutation-entered recovery route can perform a bounded lower replacement
/// recovery before deciding whether the target already committed or must be
/// republished. Keep this quote beside the actual publication phases so the
/// reservation-time outside-cleanup sum does not duplicate that state machine.
pub(super) fn maximum_publication_work() -> Result<u64> {
    READ_WORK
        .checked_mul(3)
        .and_then(|work| work.checked_add(NAMESPACE_WORK.checked_mul(3)?))
        .and_then(|work| work.checked_add(PUBLICATION_WORK.checked_mul(3)?))
        .and_then(|work| work.checked_add(17))
        .context("capacity anchor publication work quote overflow")
}

struct CurrentAnchor {
    pin: PinnedRegularFile,
    identity: PinnedRegularFileIdentity,
    bytes: [u8; ANCHOR_BYTES],
    redo: AccountRedo,
}

struct PendingAnchor {
    redo: AccountRedo,
    mutation_entered: bool,
}

pub(super) struct RetainedAnchor {
    parent: PinnedDirectory,
    directory_identity: PinnedDirectoryIdentity,
    current: Option<CurrentAnchor>,
    pending: Option<PendingAnchor>,
}

impl RetainedAnchor {
    /// Retain before recovery or publication. A missing target may be an exact
    /// interrupted lower transaction; this constructor creates no replacement.
    pub(super) fn retain(parent: PinnedDirectory, expected: PinnedDirectoryIdentity) -> Result<Self> {
        let owner = Self { parent, directory_identity: expected, current: None, pending: None };
        owner.require_directory()?;
        Ok(owner)
    }

    pub(super) fn directory_identity(&self) -> PinnedDirectoryIdentity { self.directory_identity }

    fn require_directory(&self) -> Result<()> {
        self.parent.ensure_path_binding_fixed().map_err(anyhow::Error::msg)?;
        self.parent.require_owner_private_directory_fixed().map_err(anyhow::Error::msg)?;
        ensure!(self.parent.identity()? == self.directory_identity, "capacity redo directory changed");
        Ok(())
    }

    fn steady_namespace(&self, pay: &dyn Fn(u64) -> Result<()>) -> Result<()> {
        pay(NAMESPACE_WORK)?;
        self.require_directory()?;
        let entries = self.parent.entries_no_follow_bounded(NAMESPACE_ENTRIES)?;
        ensure!(entries.len() == 2 && entries.iter().all(|entry|
            matches!(entry.name.to_str(), Some("account" | "anchor"))
                && entry.entry_type == lillux::PinnedEntryType::Regular),
            "capacity namespace retains unexpected entries; stopped operator resolution required");
        // In particular, an unbound .secure.authoring stage is neither adopted
        // nor deleted. Refuse before another publication can create a stage.
        Ok(())
    }

    fn recover_namespace(&self, pay: &dyn Fn(u64) -> Result<()>) -> Result<()> {
        pay(PUBLICATION_WORK)?;
        self.require_directory()?;
        self.parent.recover_conditional_byte_replacement_atomic(OsStr::new("anchor"))?;
        self.steady_namespace(pay)
    }

    fn read_current(&self, policy: &NodeNativeCapacityPolicy, issuance: &[u8; 32],
        deadline: MonotonicDeadline, pay: &dyn Fn(u64) -> Result<()>) -> Result<CurrentAnchor> {
        pay(READ_WORK)?;
        self.require_directory()?;
        let pin = self.parent.open_pinned_regular(OsStr::new("anchor"), false)?
            .context("capacity redo is absent after exact namespace recovery")?;
        pin.require_effective_user_owned()?;
        self.parent.require_pinned_regular_entry_fixed(&pin).map_err(anyhow::Error::msg)?;
        let identity = pin.identity()?;
        let mut bytes = [0; ANCHOR_BYTES];
        pin.read_exact_stable_into(&mut bytes, deadline)?;
        let redo = AccountRedo::decode(&bytes, policy, issuance)?;
        self.parent.require_pinned_regular_entry_fixed(&pin).map_err(anyhow::Error::msg)?;
        Ok(CurrentAnchor { pin, identity, bytes, redo })
    }

    /// Only the enclosing original bootstrap owner pays this initial read and
    /// exact lower recovery. No account repair or native grant occurs here.
    pub(super) fn load(&mut self, policy: &NodeNativeCapacityPolicy, issuance: &[u8; 32],
        deadline: MonotonicDeadline, pay: &dyn Fn(u64) -> Result<()>) -> Result<()> {
        ensure!(self.pending.is_none(), "capacity redo publication is already retained");
        if self.current.is_some() { return self.require_current(deadline, pay); }
        self.recover_namespace(pay)?;
        self.current = Some(self.read_current(policy, issuance, deadline, pay)?);
        Ok(())
    }

    pub(super) fn current(&self) -> Result<&AccountRedo> {
        ensure!(self.pending.is_none(), "capacity redo publication remains unresolved");
        Ok(&self.current.as_ref().context("capacity redo has not been loaded")?.redo)
    }

    pub(super) fn require_finalization_source_ready(
        &self,
        expected: &AccountRedo,
        deadline: MonotonicDeadline,
        pay: &dyn Fn(u64) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.pending.is_none(), "capacity redo publication remains unresolved");
        ensure!(self.current()? == expected, "capacity finalization selected another redo record");
        self.require_current(deadline, pay)
    }

    /// Move the exact settled anchor parent and current pinned record into the
    /// node finalizer. The caller first establishes `require_finalization_source_ready`
    /// while it still owns this complete anchor; this transfer performs no I/O.
    pub(super) fn into_finalization_parts(self) -> (PinnedDirectory, PinnedRegularFile) {
        let Self { parent, directory_identity: _, current, pending } = self;
        assert!(pending.is_none(), "unresolved capacity redo cannot transfer to finalization");
        let current = current.expect("settled capacity anchor was verified before transfer");
        (parent, current.pin)
    }

    pub(super) fn require_current(&self, deadline: MonotonicDeadline,
        pay: &dyn Fn(u64) -> Result<()>) -> Result<()> {
        ensure!(self.pending.is_none(), "capacity redo publication remains unresolved");
        pay(READ_WORK)?;
        self.require_directory()?;
        let current = self.current.as_ref().context("capacity redo is not retained")?;
        self.parent.require_pinned_regular_entry_fixed(&current.pin).map_err(anyhow::Error::msg)?;
        current.pin.require_effective_user_owned()?;
        ensure!(current.pin.identity()? == current.identity, "capacity redo inode changed");
        let mut bytes = [0; ANCHOR_BYTES];
        current.pin.read_exact_stable_into(&mut bytes, deadline)?;
        ensure!(bytes == current.bytes, "capacity redo bytes changed outside original publication");
        Ok(())
    }

    /// Cache before entering Lillux; every error retains the original pin and
    /// same target here. A retry resolves only that target, never an older valid
    /// record or an arbitrary replacement inode with matching predecessor bytes.
    pub(super) fn publish(&mut self, next: AccountRedo, policy: &NodeNativeCapacityPolicy,
        issuance: &[u8; 32], deadline: MonotonicDeadline, pay: &dyn Fn(u64) -> Result<()>) -> Result<()> {
        let target = next.encode(policy)?;
        if let Some(pending) = &self.pending {
            ensure!(pending.redo == next, "capacity redo retry changed its original target");
        } else {
            if self.current()? == &next { return self.require_current(deadline, pay); }
            self.require_current(deadline, pay)?;
            self.pending = Some(PendingAnchor { redo: next, mutation_entered: false });
        }

        if self.pending.as_ref().expect("retained publication").mutation_entered {
            self.recover_namespace(pay)?;
            let observed = self.read_current(policy, issuance, deadline, pay)?;
            if observed.bytes == target {
                // A previous call can have failed after rename/fsync. Rejoin
                // durability of this SAME target before allowing account writes.
                self.finish_publication(observed, deadline, pay)?;
                return Ok(());
            }
            let before = self.current.as_ref().context("capacity redo predecessor absent")?;
            ensure!(observed.identity == before.identity && observed.bytes == before.bytes,
                "capacity redo recovery selected neither original predecessor nor target");
        }
        self.steady_namespace(pay)?;
        pay(PUBLICATION_WORK)?;
        self.require_directory()?;
        let before = self.current.as_ref().context("capacity redo predecessor absent")?;
        self.parent.require_pinned_regular_entry_fixed(&before.pin).map_err(anyhow::Error::msg)?;
        self.pending.as_mut().expect("retained publication").mutation_entered = true;
        self.parent.replace_pinned_bytes_if_matches_atomic(
            OsStr::new("anchor"), Some(&before.pin),
            |incumbent| {
                pay(READ_WORK)?;
                incumbent.require_effective_user_owned()?;
                ensure!(incumbent.identity()? == before.identity, "capacity redo incumbent replaced");
                let mut bytes = [0; ANCHOR_BYTES];
                incumbent.read_exact_stable_into(&mut bytes, deadline)?;
                ensure!(bytes == before.bytes, "capacity redo incumbent bytes changed");
                Ok(())
            }, &target, 0o600,
        )?;
        self.steady_namespace(pay)?;
        let observed = self.read_current(policy, issuance, deadline, pay)?;
        ensure!(observed.bytes == target, "capacity redo target readback changed");
        self.finish_publication(observed, deadline, pay)
    }

    fn finish_publication(&mut self, observed: CurrentAnchor, deadline: MonotonicDeadline,
        pay: &dyn Fn(u64) -> Result<()>) -> Result<()> {
        pay(16)?;
        self.require_directory()?;
        self.parent.require_pinned_regular_entry_fixed(&observed.pin).map_err(anyhow::Error::msg)?;
        observed.pin.sync_exact(deadline)?;
        self.parent.sync()?;
        // The fresh exact pin takes over only after durable publication. These
        // ordinary record pins are not native loans or physical-close receipts.
        self.current = Some(observed);
        self.pending = None;
        // Cache completed physical publication before any late observation can
        // refuse. A retry must retain that progress and the same spent counter.
        pay(1)?;
        ensure!(!deadline.has_elapsed(), "capacity redo publication finished after its interval");
        Ok(())
    }
}
