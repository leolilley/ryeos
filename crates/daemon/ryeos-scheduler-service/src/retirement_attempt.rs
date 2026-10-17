//! Same-thread retirement cursor. Every failed step leaves the cursor and its
//! original SchedulerDb owner in place; successful native mutations advance
//! to an acknowledgement-only step so retry never repeats the mutation.

use anyhow::{ensure, Context, Result};
use ryeos_app::execution_history_reset::{SchedulerHistoryRetirement, SchedulerResetRetirement};
use ryeos_scheduler::SchedulerDb;
use ryeos_scheduler_contract::{
    SchedulerFireHistoryDiscardReport, SchedulerRetirementEffect, SchedulerRetirementPhase,
    SchedulerRetirementResolutionAction, SchedulerRetirementResolutionOutcome,
    SchedulerRetirementResolutionState, SchedulerRetirementRole,
};
use std::{marker::PhantomData, rc::Rc};

use crate::retirement_journal::{
    native_resolution_projection, role_identity, ResolutionReservation, RetirementJournal,
};

#[derive(Clone, Copy)]
enum Step {
    Captured(usize),
    DrainAuthorized(usize),
    Drain,
    ValidateDrained(usize),
    Drained(usize),
    QuarantineAuthorized(usize),
    Quarantine(usize),
    QuarantinedAck(usize, lillux_sqlite::SqliteRoleMutationOutcome),
    PurgeAuthorized(usize),
    Purge(usize),
    PurgedAck(usize, lillux_sqlite::SqliteRoleMutationOutcome),
    ResolveRole(usize, SchedulerRetirementResolutionAction),
    Held,
    ReleaseAuthorized(usize),
    Release,
    Released(usize),
    Complete,
}

pub(crate) struct RetirementAttempt<'a> {
    db: SchedulerDb,
    journal: RetirementJournal<'a>,
    generation: [u8; 16],
    step: Step,
    captured: Option<lillux_sqlite::CapturedSqliteRoleFacts>,
    drained: Option<lillux_sqlite::DrainedSqliteRoleFacts>,
    pending_resolution: Option<ResolutionReservation>,
    pending_resolution_ack: Option<(
        SchedulerRetirementResolutionOutcome,
        SchedulerRetirementResolutionState,
        Option<SchedulerRetirementResolutionAction>,
    )>,
    _same_thread: PhantomData<Rc<()>>,
}

impl<'a> RetirementAttempt<'a> {
    pub(crate) fn new_stopped(
        db: SchedulerDb,
        journal: RetirementJournal<'a>,
        generation: [u8; 16],
        drained: lillux_sqlite::DrainedSqliteRoleFacts,
    ) -> Self {
        // Intake already holds actual stopped pins/barriers. Schema3 phases
        // attest metadata, authorization and verified no-live-SQL facts; no
        // ordinary SQL open/close is replayed to reconstruct this cursor.
        let mut captured = drained.clone();
        for role in &mut captured.roles {
            if let Some(identity) = &mut role.original {
                identity.content_sha256 = None;
            }
        }
        let count = usize::from(journal.next_sequence());
        let step = match count {
            0..=2 => Step::Captured(count),
            3..=5 => Step::DrainAuthorized(count - 3),
            6..=8 => Step::ValidateDrained(count - 6),
            9 => Step::QuarantineAuthorized(0),
            _ => Step::Held,
        };
        Self {
            db,
            journal,
            generation,
            step,
            captured: Some(captured),
            drained: Some(drained),
            pending_resolution: None,
            pending_resolution_ack: None,
            _same_thread: PhantomData,
        }
    }

    // This cursor is for a fresh stopped owner after the child has recorded
    // exactly its nine intake facts. An interrupted later cursor is never
    // reconstructed here: its current sources are reconciled into a new child,
    // and its signed unresolved references remain in that child's ancestry.
    pub(crate) fn new_recovered(
        db: SchedulerDb,
        journal: RetirementJournal<'a>,
        generation: [u8; 16],
        drained: lillux_sqlite::DrainedSqliteRoleFacts,
    ) -> Self {
        Self {
            db,
            journal,
            generation,
            step: Step::QuarantineAuthorized(0),
            captured: None,
            drained: Some(drained),
            pending_resolution: None,
            pending_resolution_ack: None,
            _same_thread: PhantomData,
        }
    }

    pub(crate) fn advance(
        &mut self,
        permit: &SchedulerResetRetirement<'_, '_>,
    ) -> Result<Option<SchedulerHistoryRetirement>> {
        use SchedulerRetirementPhase as Phase;
        use SchedulerRetirementRole as Role;

        #[cfg(feature = "crash-qualification-test-support")]
        crate::retirement_journal::install_native_removal_cut();
        match self.step {
            Step::Captured(index) => {
                let role = Role::ALL[index];
                let identity = role_identity(
                    self.captured.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                permit.runtime_directory()?;
                self.journal.append(role, Phase::Captured, identity)?;
                self.step = if index + 1 == Role::ALL.len() {
                    Step::DrainAuthorized(0)
                } else {
                    Step::Captured(index + 1)
                };
            }
            Step::DrainAuthorized(index) => {
                let role = Role::ALL[index];
                let identity = role_identity(
                    self.captured.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                permit.runtime_directory()?;
                self.journal
                    .append(role, Phase::SqlDrainAuthorized, identity)?;
                self.step = if index + 1 == Role::ALL.len() {
                    Step::Drain
                } else {
                    Step::DrainAuthorized(index + 1)
                };
            }
            Step::Drain => {
                // The domain owner is already stopped/Drained. This retrieves
                // same-owner facts; it never constructs or closes live SQL.
                permit.runtime_directory()?;
                self.drained = Some(
                    self.db
                        .drain_projection_retirement()
                        .map_err(anyhow::Error::new)?,
                );
                self.step = Step::ValidateDrained(0);
            }
            Step::ValidateDrained(index) => {
                let before = &self.captured.as_ref().unwrap().roles[index].original;
                let after = &self.drained.as_ref().unwrap().roles[index].original;
                ensure_same_role_inode(before, after)?;
                let identity = role_identity(after.as_ref())?;
                ensure!(
                    identity
                        .as_ref()
                        .is_none_or(|identity| identity.content_sha256.is_some()),
                    "drained scheduler role lacks its stable content digest"
                );
                self.step = Step::Drained(index);
            }
            Step::Drained(index) => {
                let identity = role_identity(
                    self.drained.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                permit.runtime_directory()?;
                self.journal
                    .append(Role::ALL[index], Phase::SqlDrained, identity)?;
                self.step = if index + 1 == Role::ALL.len() {
                    Step::QuarantineAuthorized(0)
                } else {
                    Step::ValidateDrained(index + 1)
                };
            }
            Step::QuarantineAuthorized(index) => {
                let identity = role_identity(
                    self.drained.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                permit.runtime_directory()?;
                self.journal
                    .append(Role::ALL[index], Phase::QuarantineAuthorized, identity)?;
                self.step = Step::Quarantine(index);
            }
            Step::Quarantine(index) => {
                permit.runtime_directory()?;
                match self
                    .db
                    .quarantine_projection_role(sqlite_role(Role::ALL[index]))
                {
                    Ok(outcome) => self.step = Step::QuarantinedAck(index, outcome),
                    Err(error) => {
                        let action = match self.recovery_action(permit, Role::ALL[index]) {
                            Ok(action) => action,
                            Err(recovery_error) => {
                                self.step = Step::Held;
                                return Err(recovery_error.context(error.to_string()));
                            }
                        };
                        self.step = Step::ResolveRole(index, action);
                        return Err(anyhow::Error::new(error));
                    }
                }
            }
            Step::QuarantinedAck(index, outcome) => {
                let identity = role_identity(
                    self.drained.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                ensure_mutation_outcome(identity.is_some(), outcome, true)?;
                permit.runtime_directory()?;
                self.journal
                    .append(Role::ALL[index], Phase::Quarantined, identity)?;
                self.step = Step::PurgeAuthorized(index);
            }
            Step::PurgeAuthorized(index) => {
                let identity = role_identity(
                    self.drained.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                permit.runtime_directory()?;
                self.journal
                    .append(Role::ALL[index], Phase::PurgeAuthorized, identity)?;
                self.step = Step::Purge(index);
            }
            Step::Purge(index) => {
                permit.runtime_directory()?;
                match self.db.purge_projection_role(sqlite_role(Role::ALL[index])) {
                    Ok(outcome) => self.step = Step::PurgedAck(index, outcome),
                    Err(error) => {
                        let action = match self.recovery_action(permit, Role::ALL[index]) {
                            Ok(action) => action,
                            Err(recovery_error) => {
                                self.step = Step::Held;
                                return Err(recovery_error.context(error.to_string()));
                            }
                        };
                        self.step = Step::ResolveRole(index, action);
                        return Err(anyhow::Error::new(error));
                    }
                }
            }
            Step::PurgedAck(index, outcome) => {
                let identity = role_identity(
                    self.drained.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                ensure_mutation_outcome(identity.is_some(), outcome, false)?;
                permit.runtime_directory()?;
                self.journal
                    .append(Role::ALL[index], Phase::Purged, identity)?;
                self.step = if index + 1 == Role::ALL.len() {
                    Step::ReleaseAuthorized(0)
                } else {
                    Step::QuarantineAuthorized(index + 1)
                };
            }
            Step::ResolveRole(index, action) => {
                permit.runtime_directory()?;
                let (state, next_action) = self.resolve_role(permit, Role::ALL[index], action)?;
                match state {
                    SchedulerRetirementResolutionState::Held => {
                        self.step = next_action
                            .map(|next| Step::ResolveRole(index, next))
                            .unwrap_or(Step::Held)
                    }
                    SchedulerRetirementResolutionState::RearmedOriginal => {
                        self.step = Step::Quarantine(index)
                    }
                    SchedulerRetirementResolutionState::RearmedQuarantined => {
                        self.step = Step::Purge(index)
                    }
                    SchedulerRetirementResolutionState::QuarantinedDurable => {
                        self.step = Step::QuarantinedAck(
                            index,
                            lillux_sqlite::SqliteRoleMutationOutcome::QuarantinedDurable,
                        )
                    }
                    SchedulerRetirementResolutionState::RemovedDurable => {
                        self.step = Step::PurgedAck(
                            index,
                            lillux_sqlite::SqliteRoleMutationOutcome::RemovedDurable,
                        )
                    }
                }
            }
            Step::Held => anyhow::bail!(
                "scheduler retirement resolution is held; the retained owner and barriers remain active"
            ),
            Step::ReleaseAuthorized(index) => {
                let identity = role_identity(
                    self.drained.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                permit.runtime_directory()?;
                self.journal
                    .append(Role::ALL[index], Phase::ReleaseAuthorized, identity)?;
                self.step = if index + 1 == Role::ALL.len() {
                    Step::Release
                } else {
                    Step::ReleaseAuthorized(index + 1)
                };
            }
            Step::Release => {
                permit.runtime_directory()?;
                self.db
                    .release_projection_retirement()
                    .map_err(anyhow::Error::new)?;
                self.step = Step::Released(0);
            }
            Step::Released(index) => {
                let identity = role_identity(
                    self.drained.as_ref().unwrap().roles[index]
                        .original
                        .as_ref(),
                )?;
                permit.runtime_directory()?;
                self.journal
                    .append(Role::ALL[index], Phase::Released, identity)?;
                self.step = if index + 1 == Role::ALL.len() {
                    Step::Complete
                } else {
                    Step::Released(index + 1)
                };
            }
            Step::Complete => {
                return Ok(Some(SchedulerHistoryRetirement {
                    journal_artifacts: self.journal.artifact_count(),
                    rows: SchedulerFireHistoryDiscardReport::default(),
                    generation: Some(self.generation),
                }));
            }
        }
        Ok(None)
    }

    fn recovery_action(
        &self,
        permit: &SchedulerResetRetirement<'_, '_>,
        role: SchedulerRetirementRole,
    ) -> Result<SchedulerRetirementResolutionAction> {
        use ryeos_scheduler_contract::{
            SchedulerRetirementFailureDisposition as Disposition,
            SchedulerRetirementFailureStage as Stage,
        };
        permit.runtime_directory()?;
        let observation = self
            .db
            .observe_projection_role_resolution(sqlite_role(role))
            .map_err(anyhow::Error::new)?;
        let projection = native_resolution_projection(&observation)?;
        let failure = projection
            .latest_failure
            .as_ref()
            .context("scheduler role recovery lacks exact failure testimony")?;
        ensure!(
            failure.substep_observed,
            "scheduler role failure lacks observed native substep testimony"
        );
        let action = match (failure.operation, failure.disposition, failure.stage) {
            (
                SchedulerRetirementEffect::Quarantine,
                Disposition::DurabilityUncertain,
                Stage::QuarantinedOriginal,
            ) => SchedulerRetirementResolutionAction::SyncQuarantine,
            (
                SchedulerRetirementEffect::Quarantine,
                Disposition::BeforeCommit,
                Stage::Unchanged | Stage::RestoredUnexpected,
            )
            | (
                SchedulerRetirementEffect::Quarantine,
                Disposition::DurabilityUncertain,
                Stage::RestoredUnexpected,
            ) => SchedulerRetirementResolutionAction::RearmOriginal,
            (
                SchedulerRetirementEffect::Purge,
                Disposition::BeforeCommit,
                Stage::Unchanged | Stage::RestoredUnexpected,
            )
            | (
                SchedulerRetirementEffect::Purge,
                Disposition::DurabilityUncertain,
                Stage::RestoredUnexpected,
            ) => SchedulerRetirementResolutionAction::RearmQuarantined,
            (
                SchedulerRetirementEffect::Purge,
                Disposition::DurabilityUncertain,
                Stage::QuarantinedOriginal,
            ) => SchedulerRetirementResolutionAction::FinishPurgeAtU,
            (
                SchedulerRetirementEffect::Purge,
                Disposition::DurabilityUncertain,
                Stage::UnlinkedOriginal,
            ) => SchedulerRetirementResolutionAction::SyncRemoved,
            _ => anyhow::bail!(
                "scheduler role failure testimony has no safe bounded resolution action"
            ),
        };
        Ok(action)
    }

    fn resolve_role(
        &mut self,
        permit: &SchedulerResetRetirement<'_, '_>,
        role: SchedulerRetirementRole,
        action: SchedulerRetirementResolutionAction,
    ) -> Result<(
        SchedulerRetirementResolutionState,
        Option<SchedulerRetirementResolutionAction>,
    )> {
        if let Some((outcome, state, next_action)) = self.pending_resolution_ack.clone() {
            let reservation = self
                .pending_resolution
                .as_ref()
                .context("retained resolution outcome lost its signed intent")?
                .clone();
            self.journal
                .acknowledge_resolution(&reservation, &outcome)?;
            self.pending_resolution_ack = None;
            self.pending_resolution = None;
            return Ok((state, next_action));
        }
        let reservation = if let Some(pending) = self.pending_resolution.as_ref() {
            ensure!(
                pending.intent.role == role && pending.intent.action == action,
                "pending scheduler resolution changed role/action"
            );
            pending.clone()
        } else {
            permit.runtime_directory()?;
            let expected = self
                .db
                .observe_projection_role_resolution(sqlite_role(role))
                .map_err(anyhow::Error::new)?;
            let reservation = self.journal.authorize_resolution(role, expected, action)?;
            self.pending_resolution = Some(reservation.clone());
            reservation
        };
        let request = lillux_sqlite::SqliteRoleResolutionRequest {
            expected: reservation.host_expected.clone(),
            ordinal: reservation.intent.ordinal,
            action: native_action(action),
        };
        let native_result = self.db.resolve_projection_role(request);
        let state = match native_result {
            Ok(outcome) => native_resolution_state(outcome),
            Err(ryeos_scheduler::db::SchedulerDbRetirementError::Resolution(
                lillux_sqlite::SqliteRoleResolutionError::Held(_),
            )) => SchedulerRetirementResolutionState::Held,
            Err(error) => return Err(anyhow::Error::new(error)),
        };
        permit.runtime_directory()?;
        let observed = self
            .db
            .observe_projection_role_resolution(sqlite_role(role))
            .map_err(anyhow::Error::new)?;
        let observed_projection = native_resolution_projection(&observed)?;
        let next_action = if state == SchedulerRetirementResolutionState::Held {
            observed_projection
                .resolutions
                .last()
                .filter(|attempt| attempt.ordinal == reservation.intent.ordinal)
                .and_then(|attempt| {
                    let failure = attempt.effect_failure.as_ref()?;
                    if failure.operation != SchedulerRetirementEffect::Purge
                        || !failure.substep_observed
                    {
                        return None;
                    }
                    if attempt.original_unlink_observed
                        && failure.disposition
                            == ryeos_scheduler_contract::SchedulerRetirementFailureDisposition::DurabilityUncertain
                        && failure.stage
                            == ryeos_scheduler_contract::SchedulerRetirementFailureStage::UnlinkedOriginal
                    {
                        return Some(SchedulerRetirementResolutionAction::SyncRemoved);
                    }
                    if failure.disposition
                        == ryeos_scheduler_contract::SchedulerRetirementFailureDisposition::DurabilityUncertain
                        && failure.stage
                            == ryeos_scheduler_contract::SchedulerRetirementFailureStage::QuarantinedOriginal
                    {
                        return Some(
                            SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
                                source_ordinal: reservation.intent.ordinal,
                            },
                        );
                    }
                    let source_ordinal = match action {
                        SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
                            source_ordinal,
                        } if matches!(
                            (failure.disposition, failure.stage),
                            (
                                ryeos_scheduler_contract::SchedulerRetirementFailureDisposition::BeforeCommit,
                                ryeos_scheduler_contract::SchedulerRetirementFailureStage::Unchanged
                                    | ryeos_scheduler_contract::SchedulerRetirementFailureStage::RestoredUnexpected
                            ) | (
                                ryeos_scheduler_contract::SchedulerRetirementFailureDisposition::DurabilityUncertain,
                                ryeos_scheduler_contract::SchedulerRetirementFailureStage::RestoredUnexpected
                            )
                        ) => source_ordinal,
                        _ => return None,
                    };
                    Some(
                        SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
                            source_ordinal,
                        },
                    )
                })
        } else {
            None
        };
        let outcome = SchedulerRetirementResolutionOutcome {
            schema: 1,
            purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: reservation.intent.generation.clone(),
            owner_fingerprint: reservation.intent.owner_fingerprint.clone(),
            runtime_identity: reservation.intent.runtime_identity.clone(),
            role,
            ordinal: reservation.intent.ordinal,
            intent_sha256: reservation.signed_intent_sha256.clone(),
            state,
            observed: observed_projection,
        };
        self.pending_resolution_ack = Some((outcome.clone(), state, next_action));
        self.journal
            .acknowledge_resolution(&reservation, &outcome)?;
        self.pending_resolution_ack = None;
        self.pending_resolution = None;
        Ok((state, next_action))
    }
}

fn native_action(
    action: SchedulerRetirementResolutionAction,
) -> lillux_sqlite::SqliteRoleResolutionAction {
    match action {
        SchedulerRetirementResolutionAction::SyncQuarantine => {
            lillux_sqlite::SqliteRoleResolutionAction::SyncQuarantine
        }
        SchedulerRetirementResolutionAction::SyncRemoved => {
            lillux_sqlite::SqliteRoleResolutionAction::SyncRemoved
        }
        SchedulerRetirementResolutionAction::RearmOriginal => {
            lillux_sqlite::SqliteRoleResolutionAction::RearmOriginal
        }
        SchedulerRetirementResolutionAction::RearmQuarantined => {
            lillux_sqlite::SqliteRoleResolutionAction::RearmQuarantined
        }
        SchedulerRetirementResolutionAction::FinishPurgeAtU => {
            lillux_sqlite::SqliteRoleResolutionAction::FinishPurgeAtU
        }
        SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
            source_ordinal,
        } => lillux_sqlite::SqliteRoleResolutionAction::FinishPurgeAtResolution { source_ordinal },
    }
}

fn native_resolution_state(
    outcome: lillux_sqlite::SqliteRoleResolutionOutcome,
) -> SchedulerRetirementResolutionState {
    match outcome {
        lillux_sqlite::SqliteRoleResolutionOutcome::QuarantinedDurable => {
            SchedulerRetirementResolutionState::QuarantinedDurable
        }
        lillux_sqlite::SqliteRoleResolutionOutcome::RemovedDurable => {
            SchedulerRetirementResolutionState::RemovedDurable
        }
        lillux_sqlite::SqliteRoleResolutionOutcome::RearmedOriginal => {
            SchedulerRetirementResolutionState::RearmedOriginal
        }
        lillux_sqlite::SqliteRoleResolutionOutcome::RearmedQuarantined => {
            SchedulerRetirementResolutionState::RearmedQuarantined
        }
    }
}

fn ensure_same_role_inode(
    captured: &Option<lillux_sqlite::SqliteRoleIdentity>,
    drained: &Option<lillux_sqlite::SqliteRoleIdentity>,
) -> Result<()> {
    match (captured, drained) {
        (None, None) => Ok(()),
        (Some(before), Some(after)) => {
            ensure!(
                before.device == after.device && before.inode == after.inode,
                "scheduler SQLite role inode changed while SQL drained"
            );
            Ok(())
        }
        _ => anyhow::bail!("scheduler SQLite role presence changed while SQL drained"),
    }
}

fn ensure_mutation_outcome(
    originally_present: bool,
    outcome: lillux_sqlite::SqliteRoleMutationOutcome,
    quarantine: bool,
) -> Result<()> {
    use lillux_sqlite::SqliteRoleMutationOutcome as Outcome;
    ensure!(
        matches!(
            (originally_present, quarantine, outcome),
            (false, _, Outcome::OriginallyAbsent)
                | (true, true, Outcome::QuarantinedDurable)
                | (true, false, Outcome::RemovedDurable)
        ),
        "scheduler role mutation outcome differs from captured identity"
    );
    Ok(())
}

fn sqlite_role(role: SchedulerRetirementRole) -> lillux_sqlite::SqliteRole {
    match role {
        SchedulerRetirementRole::Main => lillux_sqlite::SqliteRole::Main,
        SchedulerRetirementRole::Wal => lillux_sqlite::SqliteRole::Wal,
        SchedulerRetirementRole::Shm => lillux_sqlite::SqliteRole::Shm,
    }
}
