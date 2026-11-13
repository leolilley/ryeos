//! What still holds external capacity or a credential profile on this node,
//! as the running daemon sees it, with the step that releases each row.
//!
//! Every row carries an `attention`:
//! - `in_use`: an owner of this daemon generation holds it and releases it
//!   when its work ends;
//! - `settling`: the daemon's own recovery owns it and retries it;
//! - `operator`: nothing on the node will release it without the operator.
//!
//! The view is read only. It grants no release: every settlement keeps its
//! own proof.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::runtime_db::CredentialProfileRecord;
use crate::runtime_db::external_execution::{
    ExternalAllocationOwner, ExternalAllocationPhase, HeldCredentialProfileOwner,
    HeldCredentialProfileRow, QualificationLocalStartPhase, QualificationLocalStartRecord,
    UnsettledAllocationRow,
};
use crate::state_store::StateStore;

pub const STATUS_SCHEMA: &str = "ryeos.external_execution_status.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    InUse,
    Settling,
    Operator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonGeneration {
    Current,
    Prior,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationStatus {
    pub placement_thread_id: String,
    pub phase: ExternalAllocationPhase,
    /// `dedicated_session`, `qualification_candidate` or `direct_thread`.
    pub owner_kind: String,
    pub capacity_owner: String,
    pub occurrence_bound: bool,
    pub age_ms: i64,
    pub updated_at_ms: i64,
    pub attention: Attention,
    pub fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationStartStatus {
    pub placement_thread_id: String,
    pub phase: QualificationLocalStartPhase,
    pub daemon_generation: DaemonGeneration,
    pub credential_profile_id: Option<String>,
    pub credential_held: bool,
    pub cleanup_reason: Option<String>,
    pub age_ms: i64,
    pub attention: Attention,
    pub fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileLockOwner {
    QualificationStart {
        placement_thread_id: String,
    },
    DedicatedSession {
        chain_root_id: String,
        placement_thread_id: String,
        session_state: String,
    },
    ContinuationReservation {
        reservation_id: String,
        successor_thread_id: String,
    },
    Unresolved {
        lock_owner: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialProfileStatus {
    pub profile_id: String,
    pub state: String,
    pub credential_generation: u64,
    pub login_expires_at_ms: Option<i64>,
    /// Last change to the profile row; the lock is at least this old.
    pub held_since_ms: i64,
    pub owner: ProfileLockOwner,
    pub attention: Attention,
    pub fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalExecutionStatus {
    pub schema: String,
    pub observed_at_ms: i64,
    pub allocations: Vec<AllocationStatus>,
    pub qualification_local_starts: Vec<QualificationStartStatus>,
    pub credential_profiles: Vec<CredentialProfileStatus>,
}

impl ExternalExecutionStatus {
    /// The most demanding attention among the capacity rows (allocations and
    /// Q local starts), or `None` when nothing holds capacity.
    pub fn capacity_attention(&self) -> Option<Attention> {
        self.allocations
            .iter()
            .map(|row| row.attention)
            .chain(
                self.qualification_local_starts
                    .iter()
                    .map(|row| row.attention),
            )
            .max()
    }

    /// The most demanding attention among held credential profiles.
    pub fn credential_attention(&self) -> Option<Attention> {
        self.credential_profiles
            .iter()
            .map(|row| row.attention)
            .max()
    }
}

/// Read the node's live view. The three journals are read under one store
/// lock, so the rows agree with each other.
pub fn read(store: &StateStore) -> Result<ExternalExecutionStatus> {
    let rows = store.external_execution_status_rows()?;
    Ok(classify(
        rows,
        crate::runtime_db::daemon_generation_id(),
        lillux::time::timestamp_millis() as i64,
    ))
}

/// The raw journal rows the status is built from.
pub struct StatusRows {
    pub allocations: Vec<UnsettledAllocationRow>,
    pub local_starts: Vec<QualificationLocalStartRecord>,
    pub profiles: Vec<HeldCredentialProfileRow>,
}

fn start_generation(record: &QualificationLocalStartRecord, current: &str) -> DaemonGeneration {
    if record.daemon_generation_id == current {
        DaemonGeneration::Current
    } else {
        DaemonGeneration::Prior
    }
}

const CLEANUP_UNPROVED_FIX: &str = "this Q start's cleanup is unproved: the node cannot prove its \
Worker is gone without a qualified cgroup scope. A restart settles the rows a dead daemon \
generation left; on a node without a qualified scope the start stays held";

fn start_status(
    record: &QualificationLocalStartRecord,
    current: &str,
    now: i64,
) -> QualificationStartStatus {
    let generation = start_generation(record, current);
    let (attention, fix) = if record.phase == QualificationLocalStartPhase::CleanupUnproved {
        (Attention::Operator, CLEANUP_UNPROVED_FIX.to_owned())
    } else if generation == DaemonGeneration::Prior {
        (
            Attention::Settling,
            "a previous daemon generation left this Q start open; the running daemon's cleanup \
             pass settles it by recorded coordinate and retries it. If it stays, the daemon log \
             names the settlement failure for this placement"
                .to_owned(),
        )
    } else {
        (
            Attention::InUse,
            "a running qualification owns this start; it closes when the qualification ends"
                .to_owned(),
        )
    };
    let custody = record.credential_custody.as_ref();
    QualificationStartStatus {
        placement_thread_id: record.placement_thread_id.clone(),
        phase: record.phase,
        daemon_generation: generation,
        credential_profile_id: custody.map(|c| c.profile_id.clone()),
        credential_held: custody.is_some_and(|c| c.released_at_ms.is_none()),
        cleanup_reason: record.cleanup_reason.clone(),
        age_ms: now.saturating_sub(record.created_at_ms),
        attention,
        fix,
    }
}

fn allocation_status(
    row: &UnsettledAllocationRow,
    starts: &[QualificationStartStatus],
    now: i64,
) -> AllocationStatus {
    let reservation = &row.record.reservation;
    let placement = &reservation.placement_thread_id;
    let owner_kind = match &reservation.owner {
        ExternalAllocationOwner::DedicatedSession(_) => "dedicated_session",
        ExternalAllocationOwner::QualificationCandidate(_) => "qualification_candidate",
        ExternalAllocationOwner::DirectThread { .. } => "direct_thread",
    };
    let occurrence_bound = row.record.occurrence.is_some();
    let (attention, fix) = match row.record.phase {
        ExternalAllocationPhase::Quarantined if !occurrence_bound => (
            Attention::Operator,
            format!(
                "the provider outcome of this allocation is unknown. Check the provider for \
                 placement {placement}, then run `ryeos external-execution \
                 settle-unknown-allocation {placement} <reason> <evidence>`"
            ),
        ),
        ExternalAllocationPhase::Quarantined => (
            Attention::Settling,
            "the allocation bound a provider occurrence; the node's cleanup terminates it \
             through its adapter and retries. If it stays, the daemon log names the cleanup \
             failure for this placement"
                .to_owned(),
        ),
        _ => match starts
            .iter()
            .find(|start| &start.placement_thread_id == placement)
        {
            Some(start) if start.attention != Attention::InUse => {
                (start.attention, start.fix.clone())
            }
            _ => (
                Attention::InUse,
                match &reservation.owner {
                    ExternalAllocationOwner::DedicatedSession(_) => {
                        let chain = row.session_chain_root_id.as_deref().unwrap_or(placement);
                        format!(
                            "a dedicated session holds this capacity; `ryeos worker session \
                             terminate {chain} <reason>` ends it"
                        )
                    }
                    ExternalAllocationOwner::DirectThread { chain_root_id, .. } => format!(
                        "a running thread holds this capacity; `ryeos thread cancel \
                         {chain_root_id}` ends it"
                    ),
                    ExternalAllocationOwner::QualificationCandidate(_) => {
                        "a running qualification holds this capacity; it settles when the \
                         qualification ends"
                            .to_owned()
                    }
                },
            ),
        },
    };
    AllocationStatus {
        placement_thread_id: placement.clone(),
        phase: row.record.phase,
        owner_kind: owner_kind.to_owned(),
        capacity_owner: reservation.capacity_owner.clone(),
        occurrence_bound,
        age_ms: now.saturating_sub(row.created_at_ms),
        updated_at_ms: row.updated_at_ms,
        attention,
        fix,
    }
}

fn profile_status(
    row: &HeldCredentialProfileRow,
    starts: &[QualificationStartStatus],
) -> CredentialProfileStatus {
    let profile: &CredentialProfileRecord = &row.profile;
    let (owner, attention, fix) = match &row.owner {
        HeldCredentialProfileOwner::QualificationStart {
            placement_thread_id,
        } => {
            let (attention, fix) = starts
                .iter()
                .find(|start| &start.placement_thread_id == placement_thread_id)
                .map(|start| (start.attention, start.fix.clone()))
                .unwrap_or((
                    Attention::Settling,
                    "the Q start holding this profile is closed but its credential custody is \
                     not released; the next daemon start releases custody whose Worker \
                     settlement is proved"
                        .to_owned(),
                ));
            (
                ProfileLockOwner::QualificationStart {
                    placement_thread_id: placement_thread_id.clone(),
                },
                attention,
                fix,
            )
        }
        HeldCredentialProfileOwner::DedicatedSession {
            placement_thread_id,
            chain_root_id,
            state,
        } => {
            let terminate =
                format!("`ryeos worker session terminate {chain_root_id} <reason>` releases it");
            let (attention, fix) = match state.as_str() {
                "terminal" => (
                    Attention::Settling,
                    "the session that held this profile is terminal; the daemon releases the \
                     lock once the session's Worker is proved reaped"
                        .to_owned(),
                ),
                "outcome_unknown" => (
                    Attention::Operator,
                    format!(
                        "the session's outcome is unknown after a daemon restart and nothing \
                         else will end it; {terminate}"
                    ),
                ),
                _ => (
                    Attention::InUse,
                    format!("a dedicated session holds this profile; {terminate}"),
                ),
            };
            (
                ProfileLockOwner::DedicatedSession {
                    chain_root_id: chain_root_id.clone(),
                    placement_thread_id: placement_thread_id.clone(),
                    session_state: state.clone(),
                },
                attention,
                fix,
            )
        }
        HeldCredentialProfileOwner::ContinuationReservation {
            reservation_id,
            successor_thread_id,
        } => (
            ProfileLockOwner::ContinuationReservation {
                reservation_id: reservation_id.clone(),
                successor_thread_id: successor_thread_id.clone(),
            },
            Attention::InUse,
            format!(
                "a cross-site continuation reserved this profile for thread \
                 {successor_thread_id}; the reservation releases it when the continuation \
                 is admitted or abandoned"
            ),
        ),
        HeldCredentialProfileOwner::Unresolved => (
            ProfileLockOwner::Unresolved {
                lock_owner: profile.lock_owner.clone().unwrap_or_default(),
            },
            Attention::Operator,
            "no session, Q start or continuation reservation names this lock owner; a daemon \
             restart releases a lock whose owner is gone"
                .to_owned(),
        ),
    };
    CredentialProfileStatus {
        profile_id: profile.profile_id.clone(),
        state: profile.state.clone(),
        credential_generation: profile.credential_generation,
        login_expires_at_ms: profile.login_expires_at_ms,
        held_since_ms: profile.updated_at_ms,
        owner,
        attention,
        fix,
    }
}

pub fn classify(rows: StatusRows, current_generation: &str, now: i64) -> ExternalExecutionStatus {
    let starts: Vec<_> = rows
        .local_starts
        .iter()
        .map(|record| start_status(record, current_generation, now))
        .collect();
    let allocations = rows
        .allocations
        .iter()
        .map(|row| allocation_status(row, &starts, now))
        .collect();
    let credential_profiles = rows
        .profiles
        .iter()
        .map(|row| profile_status(row, &starts))
        .collect();
    ExternalExecutionStatus {
        schema: STATUS_SCHEMA.to_owned(),
        observed_at_ms: now,
        allocations,
        qualification_local_starts: starts,
        credential_profiles,
    }
}
