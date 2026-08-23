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
    held: bool,
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
        state.held = false;
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
        while state.held {
            state = gate
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.held = true;
    }
    CredentialProfileOperationLease { gate }
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
    // Strong entries intentionally retain committed terminal fences for the
    // daemon generation. Re-creating a weakly collected gate would reopen a
    // root after its terminalization guard had been dropped.
    static GATES: OnceLock<Mutex<HashMap<String, Arc<RootGate>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(gate) = gates.get(root_thread_id) {
        return Arc::clone(gate);
    }
    let gate = Arc::new(RootGate::default());
    gates.insert(root_thread_id.to_owned(), Arc::clone(&gate));
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

pub fn begin_hosted_root_operation(root_thread_id: &str) -> Result<HostedRootOperationLease> {
    let gate = root_gate(root_thread_id);
    {
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.terminalizing {
            bail!("hosted execution root is terminalizing");
        }
        state.active_operations = state
            .active_operations
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("hosted root operation count overflow"))?;
    }
    Ok(HostedRootOperationLease { gate })
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

pub fn begin_hosted_root_terminalization(
    root_thread_id: &str,
) -> Result<HostedRootTerminalizationGuard> {
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
    }
    Ok(HostedRootTerminalizationGuard {
        gate,
        committed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_terminalization_fences_new_operations_and_waits_for_old_ones() {
        let operation = begin_hosted_root_operation("root-operation-fixture").unwrap();
        let joined = std::thread::spawn(|| {
            let mut terminal = begin_hosted_root_terminalization("root-operation-fixture").unwrap();
            assert!(begin_hosted_root_operation("root-operation-fixture").is_err());
            terminal.commit();
        });
        while begin_hosted_root_operation("root-operation-fixture").is_ok() {
            std::thread::yield_now();
        }
        drop(operation);
        joined.join().unwrap();
        assert!(begin_hosted_root_operation("root-operation-fixture").is_err());
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
}
