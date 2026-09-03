//! Process-local coordination for hosted-execution authority boundaries.
//!
//! SQLite generation/state CAS operations remain the durable authority. These
//! gates close the in-process interval between the last durable authority
//! check and process contact, and between a hosted root operation and root
//! terminalization. The gates are generic: profile and root identifiers are
//! opaque RyeOS ownership coordinates, never provider or workload kinds.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use anyhow::{Result, bail};

#[derive(Default)]
struct ExclusiveState {
    exclusive_held: bool,
    exclusive_waiters: usize,
    active_contacts: usize,
    primary_contacts_by_session: HashMap<String, usize>,
}

#[derive(Default)]
struct ExclusiveGate {
    state: Mutex<ExclusiveState>,
    changed: Condvar,
}

/// Exclusive profile-operation ownership. The lease is `Send` and may be held
/// across async suspension; acquisition itself runs on the blocking pool.
pub struct CredentialProfileOperationLease {
    gate: Arc<ExclusiveGate>,
}

impl Drop for CredentialProfileOperationLease {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.exclusive_held = false;
        self.gate.changed.notify_all();
    }
}

/// One worker-contact lease for an exact profile/session pair. Primary leases
/// are ordinary command contacts. Causal leases are pushed observations or
/// authority controls that must be allowed to complete an already-active
/// command even when an exclusive revoke/termination operation is waiting.
pub struct CredentialProfileContactLease {
    gate: Arc<ExclusiveGate>,
    session_id: String,
    primary: bool,
}

impl Drop for CredentialProfileContactLease {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active_contacts = state.active_contacts.saturating_sub(1);
        if self.primary {
            let remove =
                if let Some(count) = state.primary_contacts_by_session.get_mut(&self.session_id) {
                    *count = count.saturating_sub(1);
                    *count == 0
                } else {
                    false
                };
            if remove {
                state.primary_contacts_by_session.remove(&self.session_id);
            }
        }
        self.gate.changed.notify_all();
    }
}

fn credential_gate(profile_id: &str) -> Arc<ExclusiveGate> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<ExclusiveGate>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(profile_id).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(ExclusiveGate::default());
    gates.insert(profile_id.to_owned(), Arc::downgrade(&gate));
    gate
}

fn acquire_credential_profile_operation_blocking(
    profile_id: &str,
) -> CredentialProfileOperationLease {
    let gate = credential_gate(profile_id);
    {
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.exclusive_waiters = state.exclusive_waiters.saturating_add(1);
        while state.exclusive_held || state.active_contacts != 0 {
            state = gate
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.exclusive_waiters = state.exclusive_waiters.saturating_sub(1);
        state.exclusive_held = true;
    }
    CredentialProfileOperationLease { gate }
}

fn acquire_credential_profile_contact_blocking(
    profile_id: &str,
    session_id: &str,
    causal: bool,
) -> CredentialProfileContactLease {
    let gate = credential_gate(profile_id);
    {
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.exclusive_held
            || (state.exclusive_waiters != 0
                && !(causal
                    && state
                        .primary_contacts_by_session
                        .get(session_id)
                        .is_some_and(|count| *count != 0)))
        {
            state = gate
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.active_contacts = state.active_contacts.saturating_add(1);
        if !causal {
            let count = state
                .primary_contacts_by_session
                .entry(session_id.to_owned())
                .or_default();
            *count = count.saturating_add(1);
        }
    }
    CredentialProfileContactLease {
        gate,
        session_id: session_id.to_owned(),
        primary: !causal,
    }
}

pub async fn acquire_credential_profile_operation(
    profile_id: &str,
) -> Result<CredentialProfileOperationLease> {
    let profile_id = profile_id.to_owned();
    tokio::task::spawn_blocking(move || acquire_credential_profile_operation_blocking(&profile_id))
        .await
        .map_err(|error| anyhow::anyhow!("join credential-profile operation acquisition: {error}"))
}

pub fn acquire_credential_profile_operation_sync(
    profile_id: &str,
) -> CredentialProfileOperationLease {
    acquire_credential_profile_operation_blocking(profile_id)
}

/// Acquire a primary worker-contact lane. Waiting exclusive operations block
/// new commands, preventing revoke/termination starvation.
pub async fn acquire_credential_profile_contact(
    profile_id: &str,
    session_id: &str,
) -> Result<CredentialProfileContactLease> {
    let profile_id = profile_id.to_owned();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        acquire_credential_profile_contact_blocking(&profile_id, &session_id, false)
    })
    .await
    .map_err(|error| anyhow::anyhow!("join credential-profile contact acquisition: {error}"))
}

/// Acquire a causal full-duplex contact. When an exclusive operation is
/// waiting, this may bypass it only for the same session as an active primary
/// command; it can never start a new command epoch or cross sessions.
pub async fn acquire_credential_profile_causal_contact(
    profile_id: &str,
    session_id: &str,
) -> Result<CredentialProfileContactLease> {
    let profile_id = profile_id.to_owned();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        acquire_credential_profile_contact_blocking(&profile_id, &session_id, true)
    })
    .await
    .map_err(|error| anyhow::anyhow!("join credential-profile causal contact acquisition: {error}"))
}

pub fn acquire_credential_profile_causal_contact_sync(
    profile_id: &str,
    session_id: &str,
) -> CredentialProfileContactLease {
    acquire_credential_profile_contact_blocking(profile_id, session_id, true)
}

#[derive(Default)]
struct RootState {
    active_operations: usize,
    terminalizing: bool,
}

#[derive(Default)]
struct RootGate {
    state: Mutex<RootState>,
    changed: Condvar,
}

fn root_gate(root_thread_id: &str) -> Arc<RootGate> {
    // The durable thread row is rechecked inside operation admission, so a
    // collected local gate cannot reopen a terminal root. Weak entries bound
    // this map to currently active operations and terminalizers.
    static GATES: OnceLock<Mutex<HashMap<String, Weak<RootGate>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(root_thread_id).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(RootGate::default());
    gates.insert(root_thread_id.to_owned(), Arc::downgrade(&gate));
    gate
}

/// One hosted operation that requires its root to remain appendable.
pub struct HostedRootOperationLease {
    gate: Arc<RootGate>,
}

impl Drop for HostedRootOperationLease {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active_operations = state.active_operations.saturating_sub(1);
        self.gate.changed.notify_all();
    }
}

fn begin_hosted_root_operation_with_check<F>(
    root_thread_id: &str,
    mut durable_root_is_appendable: F,
) -> Result<HostedRootOperationLease>
where
    F: FnMut() -> Result<bool>,
{
    begin_hosted_root_operation_if_appendable_with_check(
        root_thread_id,
        &mut durable_root_is_appendable,
    )?
    .ok_or_else(|| anyhow::anyhow!("hosted execution root is not durably appendable"))
}

fn begin_hosted_root_operation_if_appendable_with_check<F>(
    root_thread_id: &str,
    mut durable_root_is_appendable: F,
) -> Result<Option<HostedRootOperationLease>>
where
    F: FnMut() -> Result<bool>,
{
    let gate = root_gate(root_thread_id);
    let mut state = gate
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if !durable_root_is_appendable()? {
            return Ok(None);
        }
        if !state.terminalizing {
            state.active_operations = state
                .active_operations
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("hosted root operation count overflow"))?;
            drop(state);
            return Ok(Some(HostedRootOperationLease { gate }));
        }
        // A terminalizer owns the only transition that can make this root
        // permanently unappendable. Wait for its commit/abort notification,
        // then re-read durable state under the same gate instead of guessing
        // from process-local state.
        state = gate
            .changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

pub fn begin_hosted_root_operation(
    state_store: &crate::state_store::StateStore,
    root_thread_id: &str,
) -> Result<HostedRootOperationLease> {
    begin_hosted_root_operation_with_check(root_thread_id, || {
        let thread = state_store
            .get_thread(root_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("hosted execution root thread disappeared"))?;
        Ok(!crate::state_store::is_terminal_status(&thread.status)
            && state_store
                .active_source_worker_handoff_for_placement(root_thread_id)?
                .is_none())
    })
}

/// Acquire appendability ownership when the durable root is still live, or
/// return `None` for an already-terminal root. This is for recovery and
/// cleanup paths whose terminal branch is deliberately read-only with respect
/// to the root chain. A concurrent terminalization is waited out and decided
/// from the durable thread row.
pub fn begin_hosted_root_operation_if_appendable(
    state_store: &crate::state_store::StateStore,
    root_thread_id: &str,
) -> Result<Option<HostedRootOperationLease>> {
    begin_hosted_root_operation_if_appendable_with_check(root_thread_id, || {
        let thread = state_store
            .get_thread(root_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("hosted execution root thread disappeared"))?;
        Ok(!crate::state_store::is_terminal_status(&thread.status)
            && state_store
                .active_source_worker_handoff_for_placement(root_thread_id)?
                .is_none())
    })
}

/// Exclusive root-terminalization ownership. Beginning terminalization first
/// prevents new hosted operations, then waits for every pre-existing lease to
/// settle. Dropping an uncommitted guard reopens admission; committing keeps
/// the root permanently fenced in this daemon generation.
pub struct HostedRootTerminalizationGuard {
    gate: Arc<RootGate>,
    committed: bool,
}

impl HostedRootTerminalizationGuard {
    pub fn commit(&mut self) {
        self.committed = true;
        // `terminalizing` intentionally remains set, but optional operation
        // admission must wake to observe the now-terminal durable root.
        let _state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.gate.changed.notify_all();
    }
}

impl Drop for HostedRootTerminalizationGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.terminalizing = false;
        self.gate.changed.notify_all();
    }
}

fn begin_hosted_root_terminalization_with_check<F>(
    root_thread_id: &str,
    mut durable_disposition_is_allowed: F,
) -> Result<HostedRootTerminalizationGuard>
where
    F: FnMut() -> Result<bool>,
{
    let gate = root_gate(root_thread_id);
    {
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.terminalizing {
            bail!("hosted execution root terminalization is already reserved");
        }
        state.terminalizing = true;
        while state.active_operations != 0 {
            state = gate
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        match durable_disposition_is_allowed() {
            Ok(true) => {}
            Ok(false) => {
                state.terminalizing = false;
                gate.changed.notify_all();
                bail!("hosted execution root has an active source handoff");
            }
            Err(error) => {
                state.terminalizing = false;
                gate.changed.notify_all();
                return Err(error);
            }
        }
    }
    Ok(HostedRootTerminalizationGuard {
        gate,
        committed: false,
    })
}

/// Acquire exclusive disposition ownership after every prior root mutation
/// settles. A durable source-role handoff owns that placement's disposition,
/// so unrelated cancellation/finalization cannot race its writer cut.
pub fn begin_hosted_root_terminalization(
    state_store: &crate::state_store::StateStore,
    root_thread_id: &str,
) -> Result<HostedRootTerminalizationGuard> {
    begin_hosted_root_terminalization_with_check(root_thread_id, || {
        Ok(state_store
            .active_source_worker_handoff_for_placement(root_thread_id)?
            .is_none())
    })
}

/// Reacquire exclusive source disposition for recovery of the exact durable
/// handoff that already owns it. This is the only bypass for the ordinary
/// active-handoff fence; a different or absent operation fails closed.
pub fn begin_hosted_root_handoff_recovery(
    state_store: &crate::state_store::StateStore,
    root_thread_id: &str,
    operation_id: &str,
) -> Result<HostedRootTerminalizationGuard> {
    begin_hosted_root_terminalization_with_check(root_thread_id, || {
        let Some((_job, operation)) =
            state_store.active_source_worker_handoff_for_placement(root_thread_id)?
        else {
            return Ok(false);
        };
        if operation.operation_id != operation_id {
            bail!("source placement is fenced by another worker handoff operation");
        }
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_terminalization_fences_new_operations_and_waits_for_old_ones() {
        let root_id = "root-operation-fixture";
        let appendable = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let operation = begin_hosted_root_operation_with_check(root_id, || {
            Ok(appendable.load(std::sync::atomic::Ordering::Acquire))
        })
        .unwrap();
        let terminal_appendable = Arc::clone(&appendable);
        let joined = std::thread::spawn(move || {
            let mut terminal =
                begin_hosted_root_terminalization_with_check(root_id, || Ok(true)).unwrap();
            terminal_appendable.store(false, std::sync::atomic::Ordering::Release);
            terminal.commit();
        });
        let gate = root_gate(root_id);
        while !gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .terminalizing
        {
            std::thread::yield_now();
        }
        let waiter_appendable = Arc::clone(&appendable);
        let (checked_tx, checked_rx) = std::sync::mpsc::sync_channel(1);
        let waiter = std::thread::spawn(move || {
            let mut checked_tx = Some(checked_tx);
            begin_hosted_root_operation_if_appendable_with_check(root_id, || {
                if let Some(sender) = checked_tx.take() {
                    sender.send(()).unwrap();
                }
                Ok(waiter_appendable.load(std::sync::atomic::Ordering::Acquire))
            })
        });
        checked_rx.recv().unwrap();
        assert!(!waiter.is_finished());
        drop(operation);
        joined.join().unwrap();
        assert!(waiter.join().unwrap().unwrap().is_none());
        assert!(
            begin_hosted_root_operation_with_check(root_id, || {
                Ok(appendable.load(std::sync::atomic::Ordering::Acquire))
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn credential_profile_operations_are_exclusive() {
        let first = acquire_credential_profile_operation("profile-fixture")
            .await
            .unwrap();
        let second = tokio::spawn(async {
            acquire_credential_profile_operation("profile-fixture")
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        drop(first);
        drop(second.await.unwrap());
    }

    #[tokio::test]
    async fn causal_contact_completes_active_command_without_reopening_new_contact() {
        let profile_id = "profile-full-duplex-fixture";
        let session_id = "session-full-duplex-fixture";
        let primary = acquire_credential_profile_contact(profile_id, session_id)
            .await
            .unwrap();
        let writer = tokio::spawn(async move {
            acquire_credential_profile_operation(profile_id)
                .await
                .unwrap()
        });
        let gate = credential_gate(profile_id);
        loop {
            let waiting = gate
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .exclusive_waiters;
            if waiting != 0 {
                break;
            }
            tokio::task::yield_now().await;
        }

        let causal = acquire_credential_profile_causal_contact(profile_id, session_id)
            .await
            .unwrap();
        let unrelated = tokio::spawn(async move {
            acquire_credential_profile_contact(profile_id, "session-unrelated")
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(!writer.is_finished());
        assert!(!unrelated.is_finished());

        drop(causal);
        drop(primary);
        let writer = writer.await.unwrap();
        assert!(!unrelated.is_finished());
        drop(writer);
        drop(unrelated.await.unwrap());
    }
}
