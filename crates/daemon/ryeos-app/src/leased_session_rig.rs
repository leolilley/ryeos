//! Seeding for the leased-session startup rig: a node that died mid-lease over
//! a real installed placement binding and the real synthetic lifecycle
//! adapter, so the restarted node's startup reconcile reaches a provider that
//! answers.
//!
//! The fixture program, its stored capsule and the binding composed for it
//! derive from one deterministic capsule, so a test composes the binding body
//! before the node starts and seeds the predecessor's rows after it has been
//! stopped, without either side passing the other a value.

use std::path::Path;

use anyhow::{Context, Result};

use crate::node_config::sections::external_execution::InstalledExternalExecutionBinding;
use crate::state_store::StateStore;

/// What the predecessor daemon left behind for one placed session.
pub struct LeasedSessionSeed<'a> {
    pub placement_thread_id: &'a str,
    pub workspace_id: &'a str,
    pub worker_instance_id: &'a str,
    /// The reservation's contact deadline. A fresh reservation must carry one
    /// in the future (within five minutes), so a stated lease end is moved
    /// into the past only by the provider's clock.
    pub contact_deadline_ms: i64,
    /// The reservation's lease lifetime, which the binding's timeout must
    /// cover.
    pub timeout_seconds: u32,
    /// Whether the reservation retains an admitted lease request. Only such a
    /// reservation states a bounded latest lease end; any other is unbounded.
    pub lease_admitted: bool,
}

/// The one capsule the rig's binding and rows are composed for.
pub fn leased_session_capsule() -> Result<ryeos_state::objects::AdmittedPersistentSessionCapsule> {
    use ryeos_state::external_execution::admission::test_support::{
        fixture_requirement, qualified_external_candidate_capsule,
    };
    qualified_external_candidate_capsule(fixture_requirement(), &"b".repeat(64))
}

/// The admitted program of [`leased_session_capsule`].
pub fn leased_session_program()
-> Result<ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram> {
    leased_session_capsule()?
        .external_candidate
        .context("the leased-session fixture capsule has no candidate program")
}

/// Seed the predecessor's session, credential lock, stored capsule and
/// contacted, quarantine-bound allocation into a Store whose node is not
/// running. The binding is the installed one the restarted node will load, so
/// its backend is the real adapter.
pub fn install_leased_session(
    store: &StateStore,
    app_root: &Path,
    binding: &InstalledExternalExecutionBinding,
    seed: &LeasedSessionSeed<'_>,
) -> Result<()> {
    store.install_leased_session_allocation(app_root, binding, seed)
}
