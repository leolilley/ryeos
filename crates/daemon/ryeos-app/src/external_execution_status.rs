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
//!
//! `retained_generations` lists each runtime snapshot binding generation a
//! recompose has superseded while open work (a qualification occurrence or a
//! bootstrap source) still references it. The node retains such a generation
//! with the work and releases it when the work settles.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::runtime_db::CredentialProfileRecord;
use crate::runtime_db::external_execution::{
    ExternalAllocationOwner, ExternalAllocationPhase, HeldCredentialProfileOwner,
    HeldCredentialProfileRow, QualificationLocalStartPhase, QualificationLocalStartRecord,
    UnsettledAllocationRow,
};
use crate::runtime_db::runtime_snapshot_qualification::{
    OpenSnapshotGenerationReference, SnapshotWorkKind,
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
pub struct RetainedGenerationReference {
    pub kind: SnapshotWorkKind,
    pub operation_id: String,
    pub occurrence_bound: bool,
}

/// A superseded runtime snapshot binding generation that open work still
/// references, so the node keeps it to settle that work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedGenerationStatus {
    /// `runtime_snapshot_production` or `runtime_snapshot_qualification`.
    pub section: String,
    pub binding_id: String,
    pub retained_digest: String,
    /// What node config pins for this binding now; `None` once the binding
    /// is no longer installed at all.
    pub current_digest: Option<String>,
    pub referenced_by: Vec<RetainedGenerationReference>,
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
    pub retained_generations: Vec<RetainedGenerationStatus>,
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
            .chain(self.retained_generations.iter().map(|row| row.attention))
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

/// Read the node's live view. The journals are read under one store lock,
/// so the rows agree with each other; `node_config` says which snapshot
/// binding generations are current.
pub fn read(
    store: &StateStore,
    node_config: &crate::node_config::NodeConfigSnapshot,
) -> Result<ExternalExecutionStatus> {
    let now = lillux::time::timestamp_millis();
    let rows = store.external_execution_status_rows(now)?;
    Ok(classify(
        rows,
        &current_snapshot_bindings(node_config),
        crate::runtime_db::daemon_generation_id(),
        now,
    ))
}

/// The raw journal rows the status is built from.
pub struct StatusRows {
    pub allocations: Vec<UnsettledAllocationRow>,
    pub local_starts: Vec<QualificationLocalStartRecord>,
    pub profiles: Vec<HeldCredentialProfileRow>,
    pub snapshot_references: Vec<OpenSnapshotGenerationReference>,
}

/// Current digest of every installed snapshot binding, by section and id.
pub type CurrentSnapshotBindings = BTreeMap<(String, String), String>;

pub fn current_snapshot_bindings(
    node_config: &crate::node_config::NodeConfigSnapshot,
) -> CurrentSnapshotBindings {
    let production = node_config
        .runtime_snapshot_production
        .iter()
        .map(|binding| {
            (
                (
                    crate::node_config::sections::runtime_snapshot_production::SECTION_NAME
                        .to_owned(),
                    binding.id().to_owned(),
                ),
                binding.digest().to_owned(),
            )
        });
    let qualification = node_config
        .runtime_snapshot_qualification
        .iter()
        .map(|binding| {
            (
                (
                    crate::node_config::sections::runtime_snapshot_qualification::SECTION_NAME
                        .to_owned(),
                    binding.id().to_owned(),
                ),
                binding.digest().to_owned(),
            )
        });
    production.chain(qualification).collect()
}

/// Group open references by superseded generation. A generation that is
/// still current is ordinary live work and is not listed.
fn retained_generations(
    references: &[OpenSnapshotGenerationReference],
    current: &CurrentSnapshotBindings,
) -> Vec<RetainedGenerationStatus> {
    let mut grouped: BTreeMap<(String, String, String), Vec<RetainedGenerationReference>> =
        BTreeMap::new();
    for reference in references {
        let key = (reference.section.to_owned(), reference.binding_id.clone());
        if current.get(&key) == Some(&reference.binding_digest) {
            continue;
        }
        grouped
            .entry((key.0, key.1, reference.binding_digest.clone()))
            .or_default()
            .push(RetainedGenerationReference {
                kind: reference.kind,
                operation_id: reference.operation_id.clone(),
                occurrence_bound: reference.occurrence_bound,
            });
    }
    grouped
        .into_iter()
        .map(|((section, binding_id, retained_digest), referenced_by)| {
            let waits_on_operator = referenced_by.iter().any(|reference| {
                reference.kind == SnapshotWorkKind::QualificationOccurrence
                    && reference.occurrence_bound
            });
            let (attention, fix) = if waits_on_operator {
                let commands = referenced_by
                    .iter()
                    .filter(|reference| {
                        reference.kind == SnapshotWorkKind::QualificationOccurrence
                            && reference.occurrence_bound
                    })
                    .map(|reference| {
                        format!(
                            "`ryeos external-content terminate-runtime-snapshot-qualification {}`",
                            reference.operation_id
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    Attention::Operator,
                    format!(
                        "a recompose superseded this generation while a qualification occurrence                          made under it is still running; the node keeps the generation so it can                          terminate the occurrence. {commands} ends it and releases the generation"
                    ),
                )
            } else {
                (
                    Attention::Settling,
                    "a recompose superseded this generation while open work made under it has                      not settled; the node keeps the generation for that work and releases it                      when the work settles (bootstrap sources are terminated by the daemon's                      recovery pass)"
                        .to_owned(),
                )
            };
            let current_digest = current.get(&(section.clone(), binding_id.clone())).cloned();
            RetainedGenerationStatus {
                section,
                binding_id,
                retained_digest,
                current_digest,
                referenced_by,
                attention,
                fix,
            }
        })
        .collect()
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

pub fn classify(
    rows: StatusRows,
    current_bindings: &CurrentSnapshotBindings,
    current_generation: &str,
    now: i64,
) -> ExternalExecutionStatus {
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
        retained_generations: retained_generations(&rows.snapshot_references, current_bindings),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::sections::{
        runtime_snapshot_production, runtime_snapshot_qualification,
    };

    fn reference(
        section: &'static str,
        id: &str,
        digest: char,
        kind: SnapshotWorkKind,
        operation: char,
        occurrence_bound: bool,
    ) -> OpenSnapshotGenerationReference {
        OpenSnapshotGenerationReference {
            section,
            binding_id: id.to_owned(),
            binding_digest: digest.to_string().repeat(64),
            kind,
            operation_id: operation.to_string().repeat(64),
            occurrence_bound,
        }
    }

    fn rows(snapshot_references: Vec<OpenSnapshotGenerationReference>) -> StatusRows {
        StatusRows {
            allocations: Vec::new(),
            local_starts: Vec::new(),
            profiles: Vec::new(),
            snapshot_references,
        }
    }

    #[test]
    fn superseded_generations_are_listed_with_what_references_them() {
        let production = runtime_snapshot_production::SECTION_NAME;
        let qualification = runtime_snapshot_qualification::SECTION_NAME;
        let current: CurrentSnapshotBindings = [
            (
                (production.to_owned(), "producer".to_owned()),
                "b".repeat(64),
            ),
            (
                (qualification.to_owned(), "qualify".to_owned()),
                "d".repeat(64),
            ),
        ]
        .into_iter()
        .collect();
        let status = classify(
            rows(vec![
                // A running occurrence made before the recompose.
                reference(
                    qualification,
                    "qualify",
                    'c',
                    SnapshotWorkKind::QualificationOccurrence,
                    '1',
                    true,
                ),
                reference(
                    production,
                    "producer",
                    'a',
                    SnapshotWorkKind::QualificationOccurrence,
                    '1',
                    true,
                ),
                // A bootstrap source made before the recompose.
                reference(
                    production,
                    "producer",
                    'a',
                    SnapshotWorkKind::BootstrapSource,
                    '2',
                    true,
                ),
                // Live work under the current generation is not retention.
                reference(
                    production,
                    "producer",
                    'b',
                    SnapshotWorkKind::BootstrapSource,
                    '3',
                    true,
                ),
            ]),
            &current,
            "generation",
            10,
        );
        assert_eq!(status.retained_generations.len(), 2);
        let producer = status
            .retained_generations
            .iter()
            .find(|row| row.section == production)
            .unwrap();
        assert_eq!(producer.retained_digest, "a".repeat(64));
        assert_eq!(producer.current_digest, Some("b".repeat(64)));
        assert_eq!(producer.referenced_by.len(), 2);
        assert_eq!(producer.attention, Attention::Operator);
        assert!(producer.fix.contains(&"1".repeat(64)));
        let qualifier = status
            .retained_generations
            .iter()
            .find(|row| row.section == qualification)
            .unwrap();
        assert_eq!(qualifier.retained_digest, "c".repeat(64));
        assert_eq!(status.capacity_attention(), Some(Attention::Operator));
    }

    #[test]
    fn a_retained_generation_only_recovery_settles_warns() {
        let production = runtime_snapshot_production::SECTION_NAME;
        let status = classify(
            rows(vec![reference(
                production,
                "removed",
                'a',
                SnapshotWorkKind::BootstrapSource,
                '2',
                true,
            )]),
            &CurrentSnapshotBindings::new(),
            "generation",
            10,
        );
        assert_eq!(status.retained_generations.len(), 1);
        assert_eq!(status.retained_generations[0].current_digest, None);
        assert_eq!(
            status.retained_generations[0].attention,
            Attention::Settling
        );
        assert_eq!(status.capacity_attention(), Some(Attention::Settling));
        let empty = classify(
            rows(Vec::new()),
            &CurrentSnapshotBindings::new(),
            "generation",
            10,
        );
        assert!(empty.retained_generations.is_empty());
        assert_eq!(empty.capacity_attention(), None);
    }
}
