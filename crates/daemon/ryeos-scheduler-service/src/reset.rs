use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use ryeos_app::execution_history_reset::{
    SchedulerAcknowledgedReset, SchedulerHistoryResetParticipant, SchedulerHistoryRetirement,
    SchedulerHistoryRetirementAttempt, SchedulerResetPreview, SchedulerResetRetirement,
    SqliteInspectionOutcome,
};
use ryeos_scheduler::{SchedulerDb, db::SchedulerDbOpenFailure};
use ryeos_scheduler_contract::{
    SchedulerFireHistoryAccounting, SchedulerFireHistoryDiscardReport, SchedulerRetirementPhase,
    SchedulerRetirementRole, SchedulerRetirementRoleIdentity, SchedulerRetirementStoppedPlan,
    SchedulerRetirementStoppedRolePlan, SchedulerRetirementStoppedSource,
};

use crate::retirement_attempt::RetirementAttempt;
use crate::retirement_journal::{
    AuthenticatedInterruptedGeneration, ResetNativeReadScope, RetirementJournal,
    authorized_role_sources, reset_native_scope,
};

#[derive(Debug)]
struct InspectionRetryFailure {
    original: anyhow::Error,
    retry: anyhow::Error,
}
impl std::fmt::Display for InspectionRetryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; same-owner scheduler release retry refused: {}",
            self.original, self.retry
        )
    }
}
impl std::error::Error for InspectionRetryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.original.as_ref())
    }
}

/// Offline reset adapter. It is constructed directly by the selected CLI; it
/// does not start a daemon timer or rely on AppState registration.
pub struct SchedulerHistoryReset;

impl SchedulerHistoryReset {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SchedulerHistoryReset {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerHistoryResetParticipant for SchedulerHistoryReset {
    fn preflight(&self, permit: &SchedulerResetPreview<'_>) -> Result<()> {
        let runtime = permit.runtime_directory()?;
        let names = runtime.entry_names_bounded(4096)?;
        ensure!(
            !names
                .iter()
                .any(|name| name == std::ffi::OsStr::new("scheduler.sqlite3-journal")),
            "scheduler rollback journal is outside the tracked retirement roles and requires separate recovery"
        );
        RetirementJournal::validate_original_preview(permit)?;
        let (new_generation, interrupted) = if let Some(identity) = permit.node_identity() {
            validate_reset_admission(runtime, identity)
                .context("validate signed scheduler retirement journal before any reset effects")?;
            let interrupted = RetirementJournal::scan_interrupted_evidence(runtime, identity)?;
            for observed in &interrupted {
                RetirementJournal::validate_for_preview(&observed.generation_intent, permit)?;
            }
            let new_generation = if interrupted.is_empty() {
                names
                    .iter()
                    .any(|name| name == std::ffi::OsStr::new("scheduler.sqlite3"))
            } else {
                let (parent, child) = interrupted_recovery_pair(&interrupted)?;
                child.is_none()
                    && parent.phase_records.len() != 27
                    && !(parent.generation_intent.intake_mode == "stopped-original"
                        && parent.phase_records.len() <= 9)
            };
            (new_generation, interrupted)
        } else {
            ensure!(
                !names.iter().any(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with("scheduler-retirement.")
                        || name.starts_with("execution-history-reset.operation.")
                }),
                "scheduler signed reset evidence requires its selected node identity before effects"
            );
            (
                names
                    .iter()
                    .any(|name| name == std::ffi::OsStr::new("scheduler.sqlite3")),
                Vec::new(),
            )
        };
        RetirementJournal::reserve_pre_effect_capacity(
            runtime,
            permit.node_identity(),
            new_generation,
        )?;
        validate_preview_source_continuity(permit, &interrupted)?;
        Ok(())
    }

    fn preview(
        &self,
        permit: &SchedulerResetPreview<'_>,
    ) -> Result<SchedulerFireHistoryAccounting> {
        self.preflight(permit)?;
        if let Some(original) = permit.original_scheduler_accounting()? {
            // Restart reports the original signed accounting, including when
            // only sidecars remain or every role was already removed.
            return Ok(original.clone());
        }
        let inspected = permit
            .inspect_database(|factory, inputs, schema| {
                match SchedulerDb::inspect_history_reset(factory, inputs, schema) {
                    Ok((accounting, terminal)) => {
                        Ok(SqliteInspectionOutcome { accounting, terminal })
                    }
                    Err(error) => {
                        // A refused acquisition or close retries its same
                        // native owner; the terminal it reaches still goes to
                        // the census, carrying the refusal as its accounting.
                        let recovery = error
                            .chain()
                            .find_map(|cause| {
                                cause.downcast_ref::<ryeos_scheduler::db::SchedulerInspectionCloseFailure>()
                            })
                            .map(|failure| {
                                failure.retry_close().and_then(|()| failure.take_terminal())
                            });
                        match recovery {
                            Some(Ok(terminal)) => Ok(SqliteInspectionOutcome {
                                accounting: Err(error),
                                terminal,
                            }),
                            Some(Err(retry)) => Err(anyhow::Error::new(InspectionRetryFailure {
                                original: error,
                                retry,
                            })),
                            None => Err(error),
                        }
                    }
                }
            })
            .context("inspect disposable scheduler database copy")?;
        Ok(
            inspected.unwrap_or_else(|| SchedulerFireHistoryAccounting::Exact {
                counts: SchedulerFireHistoryDiscardReport::default(),
            }),
        )
    }

    fn begin_retirement<'a>(
        &self,
        permit: &'a SchedulerResetRetirement<'_, '_>,
    ) -> Result<Box<dyn SchedulerHistoryRetirementAttempt + 'a>> {
        let _scope = ResetNativeReadScope::enter(permit.native_factory()?);
        let authority_runtime = permit.runtime_directory()?;
        let interrupted = RetirementJournal::scan_interrupted_evidence(
            authority_runtime,
            permit.node_identity(),
        )?;
        // The signed generation cannot choose its own accounting/provenance:
        // every interrupted owner must join the actual activated App permit.
        for observed in &interrupted {
            RetirementJournal::validate_for_permit(&observed.generation_intent, permit)?;
        }
        // Before any native stopped intake, finish what an earlier
        // acknowledged operation left: that intake admits only this plan's
        // own journal names and refuses a retired lineage's records or an
        // earlier fire-retirement intent as unknown.
        {
            let operation = permit.operation_reference()?;
            RetirementJournal::finish_acknowledged_leftovers(
                authority_runtime,
                permit.node_identity(),
                &operation.operation_id,
                &operation.sha256,
            )
            .context("finish an earlier acknowledged operation's scheduler cleanup")?;
        }
        if !interrupted.is_empty() {
            let (parent, child) = interrupted_recovery_pair(&interrupted)?;
            let leaf = child.as_ref().unwrap_or(&parent);
            if leaf.phase_records.len() == 27 {
                let plan = absent_inventory_plan(authority_runtime, &leaf.known_generations)?;
                verify_fixed_source_inventory(authority_runtime, &plan, [None, None, None], true)?;
                authority_runtime.sync()?;
                let artifact_count = authority_runtime
                    .entry_names_bounded(4096)?
                    .iter()
                    .filter(|name| name.to_string_lossy().starts_with("scheduler-retirement."))
                    .count();
                return Ok(Box::new(CompletedRetirementAttempt(Some(
                    SchedulerHistoryRetirement {
                        journal_artifacts: artifact_count,
                        rows: SchedulerFireHistoryDiscardReport::default(),
                        generation: Some(parse_generation(&leaf.generation_intent.generation)?),
                    },
                ))));
            }
            if child.is_none()
                && parent.phase_records.len() <= 9
                && parent.generation_intent.intake_mode == "stopped-original"
            {
                return begin_original_stopped(permit, authority_runtime, Some(parent), 0);
            }
            return begin_stopped_recovery(permit, authority_runtime, parent, child);
        }
        let completed_artifacts =
            RetirementJournal::validate_admission(authority_runtime, permit.node_identity())?;
        let names = authority_runtime.entry_names_bounded(4096)?;
        let has_main = names
            .iter()
            .any(|name| name == std::ffi::OsStr::new("scheduler.sqlite3"));
        ensure!(
            !names
                .iter()
                .any(|name| name == std::ffi::OsStr::new("scheduler.sqlite3-journal")),
            "scheduler rollback journal is outside the tracked retirement roles and requires separate recovery"
        );
        if !has_main {
            for sidecar in ["scheduler.sqlite3-wal", "scheduler.sqlite3-shm"] {
                ensure!(
                    !names
                        .iter()
                        .any(|name| name == std::ffi::OsStr::new(sidecar)),
                    "scheduler main database is absent while sidecar {sidecar} remains"
                );
            }
            let generation = if permit.original_scheduler_sources()?[0].current.is_some() {
                Some(RetirementJournal::completed_operation_generation(permit)?
                    .context("original scheduler source is absent without its authenticated operation completion")?)
            } else {
                ensure!(
                    permit
                        .original_scheduler_sources()?
                        .iter()
                        .all(|source| source.current.is_none()),
                    "original scheduler absence has retained sidecar provenance"
                );
                None
            };
            return Ok(Box::new(CompletedRetirementAttempt(Some(
                SchedulerHistoryRetirement {
                    journal_artifacts: completed_artifacts,
                    rows: SchedulerFireHistoryDiscardReport::default(),
                    generation,
                },
            ))));
        }

        // Reopen the exact held directory inode for the native owner. The
        // offline namespace lock remains anchored to `authority_runtime`.
        validate_original_canonical_sources(permit)?;
        begin_original_stopped(permit, authority_runtime, None, completed_artifacts)
    }

    fn complete(
        &self,
        permit: &SchedulerResetRetirement<'_, '_>,
        retirement: &SchedulerHistoryRetirement,
    ) -> Result<usize> {
        let factory = reset_factory(permit)?;
        let _scope = ResetNativeReadScope::enter(Some(factory));
        // Fire history is retired first, under the same Active operation:
        // its intent is gone before `Complete` is published, so the State
        // acknowledgement never precedes it. A refusal here (a foreign
        // entry, kept and named) leaves the operation Active, and the next
        // reset finishes a begun retirement. No native scope is needed: no
        // SQLite role is read or moved.
        {
            let runtime = permit.runtime_directory()?;
            let operation = permit.operation_reference()?;
            crate::retirement_journal::retire_fire_history(
                runtime,
                permit.node_identity(),
                &operation.operation_id,
                &operation.sha256,
            )
            .context("retire scheduler fire history before terminal completion")?;
        }
        in_service_scope(factory, || {
            let runtime = permit.runtime_directory()?;
            let Some(generation) = retirement.generation else {
                ensure!(
                    permit
                        .original_scheduler_sources()?
                        .iter()
                        .all(|source| source.current.is_none()),
                    "scheduler completion omitted generation for a present original source"
                );
                return retry_while_held("validate scheduler retirement completion", || {
                    permit.runtime_directory()?;
                    RetirementJournal::validate_admission(runtime, permit.node_identity())
                });
            };
            retry_while_held("publish scheduler retirement terminal completion", || {
                permit.runtime_directory()?;
                RetirementJournal::complete_generation(permit, generation)
                    .context("verify scheduler role cleanup and publish terminal completion")
            })
        })
    }

    fn retire_acknowledged_evidence(
        &self,
        permit: &SchedulerAcknowledgedReset<'_>,
    ) -> Result<usize> {
        let runtime = permit.runtime_directory()?;
        let journals =
            RetirementJournal::retire_acknowledged_evidence(runtime, permit.node_identity())
                .context("remove the acknowledged scheduler retirement journals")?;
        permit.runtime_directory()?;
        let fires_root =
            remove_empty_fires_root(runtime).context("remove the empty scheduler fires root")?;
        Ok(journals + usize::from(fires_root))
    }
}

/// Remove the runtime namespace's `schedules` fires root when it holds no
/// entry, returning whether it was removed. A scheduling boot creates it
/// even when no schedule ever fires; an empty root is no history. A root
/// that holds fire journals is left as it is. The caller holds the runtime
/// namespace lock, which excludes every cooperating writer of the root.
fn remove_empty_fires_root(runtime: &lillux::PinnedDirectory) -> Result<bool> {
    let name = std::ffi::OsStr::new("schedules");
    if runtime.entry_no_follow(name)?.is_none() {
        return Ok(false);
    }
    let Some(fires_root) = runtime.open_child_directory(name)? else {
        anyhow::bail!(
            "scheduler fires root {} is not a directory",
            runtime.path().join(name).display()
        );
    };
    if !fires_root.entry_names_bounded(1)?.is_empty() {
        return Ok(false);
    }
    Ok(runtime
        .remove_empty_child_if_same(name, &fires_root)?
        .is_some())
}

fn verify_source_identity(
    runtime: &lillux::PinnedDirectory,
    name: &str,
    expected: Option<&SchedulerRetirementRoleIdentity>,
) -> Result<()> {
    let native = expected.map(|identity| lillux_sqlite::SqliteRoleIdentity {
        device: identity.device,
        inode: identity.inode,
        byte_length: identity.byte_length,
        content_sha256: identity.content_sha256.clone(),
    });
    // The original caller must separately admit aggregate verification work.
    // Neither this signed extent nor a source receipt supplies that grant.
    // Under a reset permit the verification runs in its native factory scope.
    match reset_native_scope()? {
        Some(factory) => factory.verify_regular_without_sqlite_alias(
            runtime,
            std::ffi::OsStr::new(name),
            native.as_ref(),
        ),
        None => lillux_sqlite::verify_regular_without_sqlite_alias(
            runtime,
            std::ffi::OsStr::new(name),
            native.as_ref(),
        ),
    }
}

fn validate_original_canonical_sources(permit: &SchedulerResetRetirement<'_, '_>) -> Result<()> {
    let runtime = permit.runtime_directory()?;
    for (name, original) in [
        "scheduler.sqlite3",
        "scheduler.sqlite3-wal",
        "scheduler.sqlite3-shm",
    ]
    .into_iter()
    .zip(permit.original_scheduler_sources()?)
    {
        verify_source_identity(runtime, name, original.current.as_ref())?;
    }
    Ok(())
}

fn verify_fixed_source_inventory(
    runtime: &lillux::PinnedDirectory,
    plan: &SchedulerRetirementStoppedPlan,
    current: [Option<&SchedulerRetirementRoleIdentity>; 3],
    verify_canonical: bool,
) -> Result<()> {
    plan.validate()?;
    let names = plan.expected_coordinate_names();
    ensure!(
        names.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_RECOVERY_CANDIDATES,
        "scheduler pre-effect fixed inventory exceeds candidate bound"
    );
    for name in names {
        let role = SchedulerRetirementRole::ALL
            .into_iter()
            .position(|role| plan.source_name(role).as_deref() == Some(name.as_str()));
        let canonical = matches!(
            name.as_str(),
            "scheduler.sqlite3" | "scheduler.sqlite3-wal" | "scheduler.sqlite3-shm"
        );
        match role {
            Some(index) if verify_canonical => {
                verify_source_identity(runtime, &name, current[index])?
            }
            Some(_) => {} // Fresh pre-copy accounting has no original digest yet.
            None if !verify_canonical && canonical => {}
            None => verify_source_identity(runtime, &name, None)?,
        }
    }
    Ok(())
}

fn absent_inventory_plan(
    runtime: &lillux::PinnedDirectory,
    generations: &[String],
) -> Result<SchedulerRetirementStoppedPlan> {
    let plan = SchedulerRetirementStoppedPlan {
        generation: generations
            .first()
            .context("scheduler inventory lacks authenticated generation")?
            .clone(),
        generations: generations.to_vec(),
        runtime_identity: serde_json::to_string(&runtime.identity()?)?,
        roles: std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
            source: SchedulerRetirementStoppedSource::Absent,
            current: None,
        }),
    };
    plan.validate()?;
    Ok(plan)
}

fn remaining_source_name(
    role: SchedulerRetirementRole,
    source: &SchedulerRetirementStoppedSource,
) -> Option<String> {
    // Formatting only: no prospective generation or native intake authority.
    Some(match source {
        SchedulerRetirementStoppedSource::Absent => return None,
        SchedulerRetirementStoppedSource::Canonical => match role {
            SchedulerRetirementRole::Main => "scheduler.sqlite3".to_owned(),
            SchedulerRetirementRole::Wal => "scheduler.sqlite3-wal".to_owned(),
            SchedulerRetirementRole::Shm => "scheduler.sqlite3-shm".to_owned(),
        },
        SchedulerRetirementStoppedSource::Quarantine { generation } => format!(
            "scheduler-retirement.role.{generation}.{}.quarantine",
            role.name()
        ),
        SchedulerRetirementStoppedSource::Purge { generation } => format!(
            "scheduler-retirement.role.{generation}.{}.purge",
            role.name()
        ),
        SchedulerRetirementStoppedSource::Resolution {
            generation,
            ordinal,
        } => format!(
            "scheduler-retirement.resolve.{generation}.{}.{ordinal:02}.quarantine",
            role.name()
        ),
    })
}

fn verify_remaining_inventory(
    runtime: &lillux::PinnedDirectory,
    generations: &[String],
    roles: &[SchedulerRetirementStoppedRolePlan; 3],
) -> Result<()> {
    let inventory = absent_inventory_plan(runtime, generations)?;
    let names = inventory.expected_coordinate_names();
    for (index, role) in SchedulerRetirementRole::ALL.into_iter().enumerate() {
        if let Some(name) = remaining_source_name(role, &roles[index].source) {
            ensure!(
                names.contains(&name) && roles[index].current.is_some(),
                "remaining source is outside authenticated fixed inventory"
            );
        } else {
            ensure!(
                roles[index].current.is_none(),
                "absent remaining role has identity"
            );
        }
        for earlier in &roles[..index] {
            if let (Some(left), Some(right)) = (&earlier.current, &roles[index].current) {
                ensure!(
                    (left.device, left.inode) != (right.device, right.inode),
                    "remaining scheduler roles alias one inode"
                );
            }
        }
    }
    for name in names {
        let selected =
            SchedulerRetirementRole::ALL
                .into_iter()
                .enumerate()
                .find(|(index, role)| {
                    remaining_source_name(*role, &roles[*index].source).as_deref()
                        == Some(name.as_str())
                });
        verify_source_identity(
            runtime,
            &name,
            selected.and_then(|(index, _)| roles[index].current.as_ref()),
        )?;
    }
    Ok(())
}

fn select_authenticated_remaining_sources(
    runtime: &lillux::PinnedDirectory,
    evidence: &AuthenticatedInterruptedGeneration,
) -> Result<[SchedulerRetirementStoppedRolePlan; 3]> {
    let mut roles = std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
        source: SchedulerRetirementStoppedSource::Absent,
        current: None,
    });
    for (index, role) in SchedulerRetirementRole::ALL.into_iter().enumerate() {
        let (identity, mut allowed, absent) = authorized_role_sources(evidence, role)?;
        allowed.sort_by_key(|source| remaining_source_name(role, source));
        allowed.dedup();
        let mut selected = None;
        for source in allowed {
            let name = remaining_source_name(role, &source)
                .context("authorized surviving source is absent")?;
            if runtime
                .entry_no_follow(std::ffi::OsStr::new(&name))?
                .is_some()
            {
                ensure!(
                    selected.is_none(),
                    "multiple aliases survive for one authenticated scheduler role"
                );
                verify_source_identity(runtime, &name, identity.as_ref())?;
                selected = Some(SchedulerRetirementStoppedRolePlan {
                    source,
                    current: identity.clone(),
                });
            }
        }
        roles[index] = match selected {
            Some(selected) => selected,
            None => {
                ensure!(
                    absent,
                    "scheduler source vanished without signed removal authorization"
                );
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                }
            }
        };
    }
    verify_remaining_inventory(runtime, &evidence.known_generations, &roles)?;
    Ok(roles)
}

fn validate_preview_source_continuity(
    permit: &SchedulerResetPreview<'_>,
    interrupted: &[AuthenticatedInterruptedGeneration],
) -> Result<()> {
    let runtime = permit.runtime_directory()?;
    let mut generations = match permit.node_identity() {
        Some(identity) => RetirementJournal::authenticated_generation_ids(runtime, identity)?,
        None => Vec::new(), // Preflight already refused every signed/artifact name.
    };
    let Some(original) = permit.original_scheduler_sources()? else {
        ensure!(
            interrupted.is_empty(),
            "interrupted scheduler lacks original source provenance"
        );
        if let Some(generation) = generations.first() {
            let plan = SchedulerRetirementStoppedPlan {
                generation: generation.clone(),
                generations: generations.clone(),
                runtime_identity: serde_json::to_string(&runtime.identity()?)?,
                roles: std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                }),
            };
            verify_fixed_source_inventory(runtime, &plan, [None, None, None], false)?;
        }
        return Ok(());
    };
    if !interrupted.is_empty() {
        let (parent, child) = interrupted_recovery_pair(interrupted)?;
        let leaf = child.as_ref().unwrap_or(&parent);
        if leaf.phase_records.len() == 27 {
            let plan = absent_inventory_plan(runtime, &generations)?;
            return verify_fixed_source_inventory(runtime, &plan, [None, None, None], true);
        }
        if child.is_none()
            && parent.phase_records.len() <= 9
            && parent.generation_intent.intake_mode == "stopped-original"
        {
            let plan = parent
                .generation_intent
                .stopped_plan
                .as_ref()
                .context("original stopped intake plan absent")?;
            ensure!(
                plan.generations == generations,
                "original stopped intake fixed inventory changed"
            );
            return verify_fixed_source_inventory(
                runtime,
                plan,
                std::array::from_fn(|index| plan.roles[index].current.as_ref()),
                true,
            );
        }
        let roles = if let Some(child) = &child {
            child
                .generation_intent
                .recovery
                .as_ref()
                .context("recovery child lacks authenticated remaining plan")?
                .stopped_plan
                .roles
                .clone()
        } else {
            select_authenticated_remaining_sources(runtime, &parent)?
        };
        verify_remaining_inventory(runtime, &generations, &roles)?;
        return Ok(());
    }
    let reference = permit
        .original_operation_reference()?
        .context("original scheduler sources lack operation ref")?;
    let mut current: [Option<&SchedulerRetirementRoleIdentity>; 3] =
        std::array::from_fn(|index| original[index].current.as_ref());
    if original[0].current.is_some()
        && runtime
            .entry_no_follow(std::ffi::OsStr::new("scheduler.sqlite3"))?
            .is_none()
    {
        RetirementJournal::completed_preview_generation(permit)?.context(
            "original scheduler source disappeared without same-operation terminal evidence",
        )?;
        current = [None, None, None];
    }
    // A fresh present original will create its initial generation under this
    // real operation id. Include those prospective fixed names now. Genuine
    // original absence creates no generation and needs only existing names.
    if original[0].current.is_some() && !generations.contains(&reference.operation_id) {
        generations.push(reference.operation_id.clone());
        generations.sort();
    }
    if generations.is_empty() {
        for (name, identity) in [
            "scheduler.sqlite3",
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
        ]
        .into_iter()
        .zip(current)
        {
            verify_source_identity(runtime, name, identity)?;
        }
        return Ok(());
    }
    let plan = SchedulerRetirementStoppedPlan {
        generation: generations[0].clone(),
        generations,
        runtime_identity: serde_json::to_string(&runtime.identity()?)?,
        roles: original.clone(),
    };
    verify_fixed_source_inventory(runtime, &plan, current, true)
}

/// The first native effect is deliberately narrow: only the exact prefix that
/// ends after all three SqlDrained acknowledgements has an unambiguous source
/// (the canonical roles). Later crash points remain held until their resolution
/// records can select a unique source coordinate.
fn native_stopped_plan(
    runtime: &lillux::PinnedDirectory,
    plan: &SchedulerRetirementStoppedPlan,
) -> Result<lillux_sqlite::StoppedSqliteProjectionPlan> {
    plan.validate()?;
    let generation = parse_generation(&plan.generation)?;
    let generations = plan
        .generations
        .iter()
        .map(|id| parse_generation(id))
        .collect::<Result<Vec<_>>>()?;
    let mut roles = Vec::with_capacity(3);
    for role in &plan.roles {
        let source = match &role.source {
            SchedulerRetirementStoppedSource::Absent => {
                lillux_sqlite::StoppedSqliteRoleSource::Absent
            }
            SchedulerRetirementStoppedSource::Canonical => {
                lillux_sqlite::StoppedSqliteRoleSource::Canonical
            }
            SchedulerRetirementStoppedSource::Quarantine { generation } => {
                lillux_sqlite::StoppedSqliteRoleSource::Quarantine {
                    generation: parse_generation(generation)?,
                }
            }
            SchedulerRetirementStoppedSource::Purge { generation } => {
                lillux_sqlite::StoppedSqliteRoleSource::Purge {
                    generation: parse_generation(generation)?,
                }
            }
            SchedulerRetirementStoppedSource::Resolution {
                generation,
                ordinal,
            } => lillux_sqlite::StoppedSqliteRoleSource::Resolution {
                generation: parse_generation(generation)?,
                ordinal: *ordinal,
            },
        };
        roles.push(lillux_sqlite::StoppedSqliteRolePlan {
            source,
            current: role
                .current
                .as_ref()
                .map(|identity| lillux_sqlite::SqliteRoleIdentity {
                    device: identity.device,
                    inode: identity.inode,
                    byte_length: identity.byte_length,
                    content_sha256: identity.content_sha256.clone(),
                }),
        });
    }
    Ok(lillux_sqlite::StoppedSqliteProjectionPlan {
        generation,
        generations,
        runtime_identity: runtime.identity()?,
        roles: roles
            .try_into()
            .map_err(|_| anyhow::anyhow!("stopped intake requires three roles"))?,
    })
}

fn begin_original_stopped<'a>(
    permit: &'a SchedulerResetRetirement<'_, '_>,
    authority_runtime: &'a lillux::PinnedDirectory,
    existing: Option<AuthenticatedInterruptedGeneration>,
    prior_artifacts: usize,
) -> Result<Box<dyn SchedulerHistoryRetirementAttempt + 'a>> {
    let factory = reset_factory(permit)?;
    let generation = permit.initial_scheduler_generation()?;
    let plan = if let Some(evidence) = &existing {
        evidence
            .generation_intent
            .stopped_plan
            .clone()
            .context("original stopped plan absent")?
    } else {
        let mut generations = RetirementJournal::authenticated_generation_ids(
            authority_runtime,
            permit.node_identity(),
        )?;
        generations.push(encode_generation(generation));
        generations.sort();
        generations.dedup();
        SchedulerRetirementStoppedPlan {
            generation: encode_generation(generation),
            generations,
            runtime_identity: permit.runtime_identity()?,
            roles: permit.original_scheduler_sources()?.clone(),
        }
    };
    plan.validate()?;
    ensure!(
        plan.generation == encode_generation(generation)
            && &plan.roles == permit.original_scheduler_sources()?,
        "normal stopped intake differs from original operation copied sources"
    );
    verify_fixed_source_inventory(
        authority_runtime,
        &plan,
        std::array::from_fn(|index| plan.roles[index].current.as_ref()),
        true,
    )?;
    let runtime = retry_while_held("retain original stopped scheduler runtime", || {
        permit.runtime_directory()?;
        let runtime = Arc::new(
            lillux::PinnedDirectory::open(authority_runtime.path())?
                .context("scheduler runtime absent")?,
        );
        ensure!(
            runtime.is_same_directory(authority_runtime)?,
            "original stopped scheduler runtime changed"
        );
        Ok(runtime)
    })?;
    let native_plan = native_stopped_plan(&runtime, &plan)?; // all data validation before ownership acquisition
    let journal = retry_while_held("publish original stopped scheduler intake", || {
        permit.runtime_directory()?;
        match &existing {
            Some(evidence) => RetirementJournal::resume_original_stopped(permit, evidence),
            None => RetirementJournal::begin(permit, plan.clone()),
        }
    })?;
    let db = capture_stopped_intake(factory, &runtime, &native_plan, permit)
        .map_err(intake_owners_end)?;
    let drained = retry_while_held("acknowledge original stopped scheduler intake", || {
        permit.runtime_directory()?;
        in_service_scope(factory, || {
            db.retry_stopped_projection(&native_plan)
                .map_err(anyhow::Error::new)
        })
    })
    .map_err(intake_owners_end)?;
    retry_while_held("sync original stopped scheduler intake", || {
        permit.runtime_directory()?;
        runtime.sync()
    })
    .map_err(intake_owners_end)?;
    Ok(Box::new(ServiceRetirementAttempt {
        attempt: RetirementAttempt::new_stopped(db, journal, generation, drained),
        prior_artifacts,
    }))
}

fn begin_stopped_recovery<'a>(
    permit: &'a SchedulerResetRetirement<'_, '_>,
    authority_runtime: &'a lillux::PinnedDirectory,
    parent: AuthenticatedInterruptedGeneration,
    child: Option<AuthenticatedInterruptedGeneration>,
) -> Result<Box<dyn SchedulerHistoryRetirementAttempt + 'a>> {
    ensure!(
        (9..=26).contains(&parent.phase_records.len()),
        "interrupted scheduler generation lacks authenticated SQL-drained restart evidence"
    );
    let factory = reset_factory(permit)?;
    let generation = if let Some(child) = &child {
        parse_generation(&child.generation_intent.generation)?
    } else {
        let mut generated = [0u8; 16];
        lillux::crypto::fill_random_bytes(&mut generated)?;
        generated
    };
    let generation_hex = encode_generation(generation);
    let runtime = retry_while_held("reopen selected scheduler runtime directory", || {
        let runtime = Arc::new(
            lillux::PinnedDirectory::open(authority_runtime.path())?
                .context("selected scheduler runtime directory is absent")?,
        );
        ensure!(
            runtime.is_same_directory(authority_runtime)?,
            "scheduler runtime directory changed while acquiring native owner"
        );
        Ok(runtime)
    })?;

    let runtime_identity = retry_while_held("bind scheduler recovery to runtime identity", || {
        permit.runtime_identity()
    })?;
    let mut generations = parent
        .known_generations
        .iter()
        .map(|generation| parse_generation(generation))
        .collect::<Result<Vec<_>>>()?;
    if child.is_none() {
        generations.push(generation);
    }
    generations.sort_unstable();
    generations.dedup();
    ensure!(
        generations.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_GENERATIONS,
        "scheduler recovery has no bounded generation capacity"
    );

    let roles = if let Some(child) = &child {
        child
            .generation_intent
            .recovery
            .as_ref()
            .context("reusable recovery child lacks admitted remaining plan")?
            .stopped_plan
            .roles
            .clone()
    } else {
        select_authenticated_remaining_sources(&runtime, &parent)?
    };
    verify_remaining_inventory(&runtime, &parent.known_generations, &roles)?;
    let contract_plan = SchedulerRetirementStoppedPlan {
        generation: generation_hex.clone(),
        generations: generations
            .iter()
            .map(|id| encode_generation(*id))
            .collect(),
        runtime_identity: runtime_identity.clone(),
        roles,
    };
    contract_plan.validate()?;
    let mut journal = retry_while_held("publish immutable scheduler recovery intent", || {
        permit.runtime_directory()?;
        RetirementJournal::begin_recovery(permit, &parent, contract_plan.clone())
    })?;
    let native_plan = native_stopped_plan(&runtime, &contract_plan)?;
    let db = capture_stopped_intake(factory, &runtime, &native_plan, permit)
        .map_err(intake_owners_end)?;
    let drained = retry_while_held("acknowledge stopped scheduler recovery intake", || {
        in_service_scope(factory, || {
            db.retry_stopped_projection(&native_plan)
                .map_err(anyhow::Error::new)
        })
    })
    .map_err(intake_owners_end)?;

    // The signed parent authorizes any absent remaining role. The fresh native
    // owner has checked all fixed aliases; sync its retained directory before
    // recording that absence. This is no synthetic old-resolution outcome.
    retry_while_held("sync authenticated stopped scheduler namespace", || {
        permit.runtime_directory()?;
        runtime.sync()
    })
    .map_err(intake_owners_end)?;

    // Rebuild only the already-authenticated, pre-effect prefix under the
    // child intent. The digest-free capture facts are derived from the exact
    // D-owned drained facts; no earlier SQL operation is repeated.
    let existing_phase_count = usize::from(journal.next_sequence());
    let mut phase_index = 0usize;
    for role in SchedulerRetirementRole::ALL {
        let identity = recovery_identity(&drained, role, false)?;
        if phase_index >= existing_phase_count {
            retry_while_held("publish recovered scheduler capture phase", || {
                permit.runtime_directory()?;
                journal.append(role, SchedulerRetirementPhase::Captured, identity.clone())
            })
            .map_err(intake_owners_end)?;
        }
        phase_index += 1;
    }
    for role in SchedulerRetirementRole::ALL {
        let identity = recovery_identity(&drained, role, false)?;
        if phase_index >= existing_phase_count {
            retry_while_held("publish recovered scheduler drain authorization", || {
                permit.runtime_directory()?;
                journal.append(
                    role,
                    SchedulerRetirementPhase::SqlDrainAuthorized,
                    identity.clone(),
                )
            })
            .map_err(intake_owners_end)?;
        }
        phase_index += 1;
    }
    for role in SchedulerRetirementRole::ALL {
        let identity = recovery_identity(&drained, role, true)?;
        if phase_index >= existing_phase_count {
            retry_while_held("publish recovered scheduler drain acknowledgement", || {
                permit.runtime_directory()?;
                journal.append(role, SchedulerRetirementPhase::SqlDrained, identity.clone())
            })
            .map_err(intake_owners_end)?;
        }
        phase_index += 1;
    }
    Ok(Box::new(ServiceRetirementAttempt {
        attempt: RetirementAttempt::new_recovered(db, journal, generation, drained),
        prior_artifacts: 0,
    }))
}

/// The context of a refusal returned once the stopped intake's journal or
/// scheduler projection exists. Both are locals of the intake frame, so they
/// end as the refusal returns: the retry notices that said they were
/// retained held only while retrying. The reset then fails and its process
/// exits; the journal is durable, so the next reset recovers the interrupted
/// generation from it.
fn intake_owners_end(error: anyhow::Error) -> anyhow::Error {
    error.context(
        "the stopped intake's retirement journal and scheduler projection end with this refusal; \
         the next reset recovers the interrupted generation from its durable journal",
    )
}

/// The reset's native factory, which the scheduler's native steps run in.
/// A permit issued without one cannot run them.
fn reset_factory<'a>(
    permit: &'a SchedulerResetRetirement<'_, '_>,
) -> Result<&'a Arc<lillux_sqlite::OriginalNativeFactory>> {
    permit
        .native_factory()?
        .context("scheduler retirement native steps require the reset's native factory")
}

/// Capture the stopped scheduler intake, retrying a held intake on its
/// retained owner up to [`HELD_RETRY_ATTEMPTS`] times. Each native attempt
/// runs alone in the factory's stopped-intake scope, the cleanup allocator
/// scope the native stopped create requires. The permit check before each
/// retry runs outside that scope: its guarded reads enter a service scope,
/// which refuses inside the intake scope. The owner is borrowed into each
/// scope, so a scope refused before its attempt keeps it for the next one,
/// and a scope refused after its attempt hands it back. After the last
/// attempt the refusal carries the retained failed owner.
fn capture_stopped_intake(
    factory: &lillux_sqlite::OriginalNativeFactory,
    runtime: &Arc<lillux::PinnedDirectory>,
    plan: &lillux_sqlite::StoppedSqliteProjectionPlan,
    permit: &SchedulerResetRetirement<'_, '_>,
) -> Result<SchedulerDb> {
    let mut held: Option<std::result::Result<SchedulerDb, SchedulerDbOpenFailure>> = None;
    let mut last_refusal = None;
    for attempt in 0..=HELD_RETRY_ATTEMPTS {
        if attempt > 0
            && let Err(error) = permit.runtime_directory()
        {
            retry_notice(
                "revalidate stopped scheduler intake authority while retaining owner",
                &error,
            );
            last_refusal = Some(error);
            continue;
        }
        let refusal = match factory.with_original_stopped_intake(|_| match held.take() {
            None => SchedulerDb::capture_stopped_projection(Arc::clone(runtime), plan.clone()),
            Some(Err(failure)) => failure.retry_stopped_projection(plan),
            Some(Ok(db)) => Ok(db),
        }) {
            Ok(Ok(db)) => return Ok(db),
            Ok(Err(failure)) => {
                let refusal = anyhow::Error::msg(failure.to_string());
                held = Some(Err(failure));
                refusal
            }
            Err(failure) => {
                let (refusal, owner) = failure.into_parts();
                if let Some(owner) = owner {
                    held = Some(owner);
                }
                refusal
            }
        };
        retry_notice("retry the exact stopped scheduler intake", &refusal);
        last_refusal = Some(refusal);
    }
    let refused = format!(
        "scheduler history reset refused the stopped intake after its first attempt and {HELD_RETRY_ATTEMPTS} retries"
    );
    match held {
        Some(Err(failure)) => {
            Err(anyhow::Error::new(failure).context(format!("{refused}; its owner is retained")))
        }
        Some(Ok(db)) => {
            // Captured, but every later intake scope refused around it. The
            // owner ends here, in a service scope so its native frees run in
            // this arena; a refused scope ends it all the same.
            let ended = factory.with_original_service_scope(move |_| drop(db));
            let refusal =
                last_refusal.unwrap_or_else(|| anyhow::anyhow!("stopped intake scope refused"));
            Err(match ended {
                Ok(()) => refusal.context(refused),
                Err(failure) => refusal.context(format!(
                    "{refused}; its captured owner ended outside a native scope: {}",
                    failure.error()
                )),
            })
        }
        None => Err(last_refusal
            .unwrap_or_else(|| anyhow::anyhow!("stopped intake scope refused"))
            .context(refused)),
    }
}

/// Run one scheduler native step in the reset factory's service allocator
/// scope, where its allocations and frees run in that arena. A scope refused
/// after the step ran drops the step's value: every step this runs keeps its
/// progress in the owner it mutated and returns only data, so the caller
/// retries it.
fn in_service_scope<T>(
    factory: &lillux_sqlite::OriginalNativeFactory,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    match factory.with_original_service_scope(|_| operation()) {
        Ok(result) => result,
        Err(failure) => {
            let (refusal, completed) = failure.into_parts();
            Err(match completed {
                Some(Err(error)) => {
                    refusal.context(format!("the scheduler native step also refused: {error:#}"))
                }
                Some(Ok(_)) | None => refusal,
            })
        }
    }
}

fn validate_reset_admission(
    runtime: &lillux::PinnedDirectory,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<usize> {
    let interrupted = RetirementJournal::scan_interrupted_evidence(runtime, identity)?;
    if interrupted.is_empty() {
        return RetirementJournal::validate_admission(runtime, identity);
    }
    interrupted_recovery_pair(&interrupted)?;
    Ok(0)
}

fn interrupted_recovery_pair(
    interrupted: &[AuthenticatedInterruptedGeneration],
) -> Result<(
    AuthenticatedInterruptedGeneration,
    Option<AuthenticatedInterruptedGeneration>,
)> {
    ensure!(
        !interrupted.is_empty()
            && interrupted.len() <= ryeos_scheduler_contract::SCHEDULER_RETIREMENT_MAX_GENERATIONS,
        "scheduler recovery interrupted generation inventory is outside its bound"
    );
    let parents = interrupted
        .iter()
        .filter_map(|e| {
            e.generation_intent
                .recovery
                .as_ref()
                .map(|binding| binding.parent_generation.as_str())
        })
        .collect::<std::collections::BTreeSet<_>>();
    let leaves = interrupted
        .iter()
        .filter(|e| !parents.contains(e.generation_intent.generation.as_str()))
        .collect::<Vec<_>>();
    ensure!(
        leaves.len() == 1,
        "scheduler recovery has unrelated or conflicting interrupted leaves"
    );
    let leaf = leaves[0];
    let mut cursor = leaf;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        ensure!(
            seen.insert(cursor.generation_intent.generation.clone()),
            "scheduler recovery contains a cycle"
        );
        cursor
            .generation_intent
            .validate_same_operation(&leaf.generation_intent)?;
        let Some(binding) = &cursor.generation_intent.recovery else {
            break;
        };
        cursor = interrupted
            .iter()
            .find(|e| e.generation_intent.generation == binding.parent_generation)
            .context("scheduler interrupted lineage lacks its parent")?;
    }
    ensure!(
        seen.len() == interrupted.len(),
        "scheduler interrupted inventory is not one complete lineage"
    );
    if leaf.phase_records.len() <= 9 && leaf.generation_intent.recovery.is_some() {
        let binding = leaf.generation_intent.recovery.as_ref().unwrap();
        let parent = interrupted
            .iter()
            .find(|e| e.generation_intent.generation == binding.parent_generation)
            .context("scheduler reusable child lacks authenticated parent")?;
        ensure!(
            leaf.resolution_slots.is_empty(),
            "pre-effect recovery child has resolution effects"
        );
        return Ok((parent.clone(), Some(leaf.clone())));
    }
    if leaf.phase_records.len() <= 9 && leaf.generation_intent.intake_mode == "stopped-original" {
        ensure!(
            leaf.generation_intent.recovery.is_none() && leaf.resolution_slots.is_empty(),
            "original stopped intake-only prefix has effect/resolution evidence"
        );
        return Ok((leaf.clone(), None));
    }
    ensure!(
        (9..=27).contains(&leaf.phase_records.len()),
        "initial scheduler recovery before SQL-drained evidence requires its separate native reconciliation path"
    );
    Ok((leaf.clone(), None))
}

fn recovery_identity(
    facts: &lillux_sqlite::DrainedSqliteRoleFacts,
    role: SchedulerRetirementRole,
    include_digest: bool,
) -> Result<Option<SchedulerRetirementRoleIdentity>> {
    let native = facts.roles[match role {
        SchedulerRetirementRole::Main => 0,
        SchedulerRetirementRole::Wal => 1,
        SchedulerRetirementRole::Shm => 2,
    }]
    .original
    .as_ref();
    Ok(native.map(|identity| {
        let mut digest = identity.content_sha256.clone();
        if !include_digest {
            digest = None;
        }
        SchedulerRetirementRoleIdentity {
            device: identity.device,
            inode: identity.inode,
            byte_length: identity.byte_length,
            content_sha256: digest,
        }
    }))
}

fn encode_generation(generation: [u8; 16]) -> String {
    generation
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse_generation(value: &str) -> Result<[u8; 16]> {
    ensure!(
        value.len() == 32,
        "scheduler generation must be 32 hex digits"
    );
    let mut generation = [0u8; 16];
    for (index, byte) in generation.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(generation)
}

struct ServiceRetirementAttempt<'a> {
    attempt: RetirementAttempt<'a>,
    prior_artifacts: usize,
}

impl SchedulerHistoryRetirementAttempt for ServiceRetirementAttempt<'_> {
    fn advance(
        &mut self,
        permit: &SchedulerResetRetirement<'_, '_>,
    ) -> Result<Option<SchedulerHistoryRetirement>> {
        let factory = reset_factory(permit)?;
        let _scope = ResetNativeReadScope::enter(Some(factory));
        Ok(
            in_service_scope(factory, || self.attempt.advance(permit))?.map(|retirement| {
                SchedulerHistoryRetirement {
                    journal_artifacts: self.prior_artifacts + retirement.journal_artifacts,
                    ..retirement
                }
            }),
        )
    }
}

struct CompletedRetirementAttempt(Option<SchedulerHistoryRetirement>);

impl SchedulerHistoryRetirementAttempt for CompletedRetirementAttempt {
    fn advance(
        &mut self,
        _permit: &SchedulerResetRetirement<'_, '_>,
    ) -> Result<Option<SchedulerHistoryRetirement>> {
        Ok(self.0.take())
    }
}

/// Attempts of one held stage before it refuses: about one minute at the
/// retry interval. A refusal that outlasts this is not a transient one.
const HELD_RETRY_ATTEMPTS: u32 = 240;

/// Retry one stage while the caller keeps its original locks and owners. A
/// stage still refused after [`HELD_RETRY_ATTEMPTS`] returns its last
/// refusal, so a persistent refusal ends the call instead of spinning.
fn retry_while_held<T>(stage: &str, mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempts = 0u32;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                attempts += 1;
                if attempts >= HELD_RETRY_ATTEMPTS {
                    return Err(error.context(format!(
                        "scheduler history reset refused to {stage} after {attempts} attempts"
                    )));
                }
                retry_notice(stage, &error);
            }
        }
    }
}

fn retry_notice(stage: &str, error: &anyhow::Error) {
    static LAST_NOTICE: OnceLock<std::sync::Mutex<Option<Instant>>> = OnceLock::new();
    let last_notice = LAST_NOTICE.get_or_init(|| std::sync::Mutex::new(None));
    let mut last_notice = last_notice
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last_notice
        .as_ref()
        .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
    {
        eprintln!(
            "scheduler history reset is retaining its original locks and owner while retrying {stage}: {error:#}"
        );
        *last_notice = Some(Instant::now());
    }
    drop(last_notice);
    std::thread::sleep(Duration::from_millis(250));
}

#[cfg(all(test, target_os = "linux"))]
mod source_continuity_controls {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn complete_fixed_inventory_refuses_each_unauthorized_noncanonical_coordinate_before_release()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let runtime =
            lillux::PinnedDirectory::open(directory.path())?.context("test runtime absent")?;
        let data = b"current authenticated canonical bytes";
        assert!(
            runtime
                .atomic_create_regular(
                    std::ffi::OsStr::new("scheduler.sqlite3"),
                    data,
                    0o600,
                    crate::SCHEDULER_TEMP_PURPOSE,
                )?
                .is_some()
        );
        let metadata = std::fs::metadata(directory.path().join("scheduler.sqlite3"))?;
        let current = SchedulerRetirementRoleIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            byte_length: metadata.len(),
            content_sha256: Some(lillux::sha256_hex(data)),
        };
        let plan = SchedulerRetirementStoppedPlan {
            generation: format!("{:032x}", 4),
            generations: (1..=4).map(|id| format!("{id:032x}")).collect(),
            runtime_identity: serde_json::to_string(&runtime.identity()?)?,
            roles: std::array::from_fn(|index| SchedulerRetirementStoppedRolePlan {
                source: if index == 0 {
                    SchedulerRetirementStoppedSource::Canonical
                } else {
                    SchedulerRetirementStoppedSource::Absent
                },
                current: (index == 0).then(|| current.clone()),
            }),
        };
        assert_eq!(plan.expected_coordinate_names().len(), 100);
        verify_fixed_source_inventory(&runtime, &plan, [Some(&current), None, None], true)?;
        let forbidden = plan
            .expected_coordinate_names()
            .into_iter()
            .filter(|name| {
                ![
                    "scheduler.sqlite3",
                    "scheduler.sqlite3-wal",
                    "scheduler.sqlite3-shm",
                ]
                .contains(&name.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(forbidden.len(), 97);
        for name in &forbidden {
            assert!(
                runtime
                    .atomic_create_regular(
                        std::ffi::OsStr::new(name),
                        b"foreign fixed candidate",
                        0o600,
                        crate::SCHEDULER_TEMP_PURPOSE
                    )?
                    .is_some()
            );
            assert!(
                verify_fixed_source_inventory(&runtime, &plan, [Some(&current), None, None], true)
                    .is_err(),
                "pre-effect inventory accepted {name}"
            );
            std::fs::remove_file(directory.path().join(name))?;
        }
        // The authenticated release-complete shortcut requires absence at
        // EVERY fixed name, even if original/drained source facts were present.
        std::fs::remove_file(directory.path().join("scheduler.sqlite3"))?;
        verify_fixed_source_inventory(&runtime, &plan, [None, None, None], true)?;
        for name in plan.expected_coordinate_names() {
            assert!(
                runtime
                    .atomic_create_regular(
                        std::ffi::OsStr::new(&name),
                        b"foreign after release",
                        0o600,
                        crate::SCHEDULER_TEMP_PURPOSE
                    )?
                    .is_some()
            );
            assert!(
                verify_fixed_source_inventory(&runtime, &plan, [None, None, None], true).is_err(),
                "release-complete inventory accepted {name}"
            );
            std::fs::remove_file(directory.path().join(&name))?;
        }
        Ok(())
    }

    #[test]
    fn source_gate_checks_exact_bytes_extent_inode_and_original_absence() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let runtime =
            lillux::PinnedDirectory::open(directory.path())?.context("test runtime absent")?;
        let name = "scheduler.sqlite3";
        let path = directory.path().join(name);
        verify_source_identity(&runtime, name, None)?;
        let bytes = b"original preview bytes";
        assert!(
            runtime
                .atomic_create_regular(
                    std::ffi::OsStr::new(name),
                    bytes,
                    0o600,
                    crate::SCHEDULER_TEMP_PURPOSE,
                )?
                .is_some()
        );
        let metadata = std::fs::metadata(&path)?;
        let original = SchedulerRetirementRoleIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            byte_length: metadata.len(),
            content_sha256: Some(lillux::sha256_hex(bytes)),
        };
        verify_source_identity(&runtime, name, Some(&original))?;
        assert!(verify_source_identity(&runtime, name, None).is_err());
        // Same length, same inode, and a valid signed-style original digest;
        // metadata identity alone cannot distinguish this in-place edit.
        std::fs::write(&path, vec![b'x'; bytes.len()])?;
        assert_eq!(std::fs::metadata(&path)?.ino(), original.inode);
        assert_eq!(std::fs::metadata(&path)?.len(), original.byte_length);
        assert!(verify_source_identity(&runtime, name, Some(&original)).is_err());
        std::fs::write(&path, bytes)?;
        verify_source_identity(&runtime, name, Some(&original))?;
        std::fs::write(&path, b"short")?;
        assert!(verify_source_identity(&runtime, name, Some(&original)).is_err());
        // Keep the original inode alive under the fixture's own different
        // name; identical replacement bytes still lack its physical lineage.
        std::fs::rename(&path, directory.path().join("held-original"))?;
        assert!(
            runtime
                .atomic_create_regular(
                    std::ffi::OsStr::new(name),
                    bytes,
                    0o600,
                    crate::SCHEDULER_TEMP_PURPOSE,
                )?
                .is_some()
        );
        assert!(verify_source_identity(&runtime, name, Some(&original)).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod late_source_controls {
    use super::*;
    use crate::retirement_journal::late_recovery_controls::{initial_intent, signed_prefix};
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn original_stopped_early_prefix_uses_exact_original_digest_and_never_assumes_absence()
    -> Result<()> {
        for count in 0..=9 {
            let directory = tempfile::tempdir()?;
            let identity = ryeos_node_runtime::identity::NodeIdentity::create(
                &directory.path().join("identity.key"),
            )?;
            let path = directory.path().join("runtime");
            std::fs::create_dir(&path)?;
            let runtime =
                lillux::PinnedDirectory::open(&path)?.context("fixture runtime absent")?;
            let canonical = path.join("scheduler.sqlite3");
            let bytes = b"original signed stopped bytes";
            std::fs::write(&canonical, bytes)?;
            let metadata = std::fs::metadata(&canonical)?;
            let roles = [
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Canonical,
                    current: Some(SchedulerRetirementRoleIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                        byte_length: metadata.len(),
                        content_sha256: Some(lillux::sha256_hex(bytes)),
                    }),
                },
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                },
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                },
            ];
            let intent = initial_intent(&runtime, &identity, roles)?;
            signed_prefix(&runtime, &identity, &intent, count)?;
            let evidence = RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?;
            let (original, child) = interrupted_recovery_pair(&evidence)?;
            assert!(child.is_none());
            assert_eq!(
                original.generation_intent.operation_sha256,
                intent.operation_sha256
            );
            let plan = original.generation_intent.stopped_plan.as_ref().unwrap();
            verify_fixed_source_inventory(
                &runtime,
                plan,
                std::array::from_fn(|index| plan.roles[index].current.as_ref()),
                true,
            )?;
            let mut changed = bytes.to_vec();
            changed[0] ^= 1;
            std::fs::write(&canonical, changed)?;
            assert!(
                verify_fixed_source_inventory(
                    &runtime,
                    plan,
                    std::array::from_fn(|index| plan.roles[index].current.as_ref()),
                    true
                )
                .is_err()
            );
            std::fs::remove_file(&canonical)?;
            assert!(
                verify_fixed_source_inventory(
                    &runtime,
                    plan,
                    std::array::from_fn(|index| plan.roles[index].current.as_ref()),
                    true
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn pending_real_signed_purge_resolution_authorizes_u_r_or_synced_absence() -> Result<()> {
        use ryeos_scheduler_contract::{
            SchedulerRetirementEffect as Effect,
            SchedulerRetirementFailureDisposition as Disposition,
            SchedulerRetirementFailureStage as Stage,
            SchedulerRetirementObservedCoordinate as Coordinate,
            SchedulerRetirementResolutionAction as Action,
        };
        for remaining in ["u", "r", "absent"] {
            let directory = tempfile::tempdir()?;
            let identity = ryeos_node_runtime::identity::NodeIdentity::create(
                &directory.path().join("identity.key"),
            )?;
            let path = directory.path().join("runtime");
            std::fs::create_dir(&path)?;
            let runtime =
                lillux::PinnedDirectory::open(&path)?.context("fixture runtime absent")?;
            let bytes = b"uncertain purge remaining bytes";
            let canonical = path.join("scheduler.sqlite3");
            std::fs::write(&canonical, bytes)?;
            let metadata = std::fs::metadata(&canonical)?;
            let original = SchedulerRetirementRoleIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                byte_length: metadata.len(),
                content_sha256: Some(lillux::sha256_hex(bytes)),
            };
            let roles = [
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Canonical,
                    current: Some(original.clone()),
                },
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                },
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                },
            ];
            let native =
                crate::retirement_journal::compact_native_projection_controls::native_observation(
                    runtime.identity()?,
                );
            let mut observation = crate::retirement_journal::native_resolution_projection(&native)?;
            let mut parent = initial_intent(&runtime, &identity, roles)?;
            parent.generation = observation.generation.clone();
            parent.operation_id = observation.generation.clone();
            parent.stopped_plan.as_mut().unwrap().generation = parent.generation.clone();
            parent.stopped_plan.as_mut().unwrap().generations = vec![parent.generation.clone()];
            signed_prefix(&runtime, &identity, &parent, 12)?;
            observation.stopped_plan = parent.stopped_plan.clone();
            observation.selected_source = Some(SchedulerRetirementStoppedSource::Canonical);
            observation.resolutions.clear();
            observation.resolution_quarantines.clear();
            observation.original = Some(original.clone());
            observation.original.as_mut().unwrap().content_sha256 = None;
            observation.original_link_count = Some(1);
            observation.attempted = Effect::Purge;
            observation.public = Coordinate::Absent;
            observation.source = Coordinate::Absent;
            observation.quarantine = Coordinate::Absent;
            observation.purge = Coordinate::SameOriginal {
                device: original.device,
                inode: original.inode,
            };
            let failure = ryeos_scheduler_contract::SchedulerRetirementFailureFact {
                operation: Effect::Purge,
                disposition: Disposition::DurabilityUncertain,
                stage: Stage::QuarantinedOriginal,
                substep_observed: true,
                error_sha256: "cd".repeat(32),
            };
            observation.stopped_coordinates = parent
                .stopped_plan
                .as_ref()
                .unwrap()
                .expected_coordinate_inventory()
                .into_iter()
                .map(|(name, role)| {
                    ryeos_scheduler_contract::SchedulerRetirementObservedStoppedCoordinate {
                        coordinate: if name == observation.purge_name {
                            Coordinate::SameOriginal {
                                device: original.device,
                                inode: original.inode,
                            }
                        } else {
                            Coordinate::Absent
                        },
                        name,
                        role,
                    }
                })
                .collect();
            observation.original_failure = Some(failure.clone());
            observation.latest_failure = Some(failure);
            let journal_name = format!("scheduler-retirement.phase.{}.11", parent.generation);
            let intent = ryeos_scheduler_contract::SchedulerRetirementResolutionIntent {
                schema: 2,
                operation_id: parent.operation_id.clone(),
                operation_sha256: parent.operation_sha256.clone(),
                purpose: ryeos_scheduler_contract::SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE
                    .to_owned(),
                generation: parent.generation.clone(),
                role: SchedulerRetirementRole::Main,
                ordinal: 0,
                owner_fingerprint: identity.fingerprint().to_owned(),
                runtime_identity: parent.runtime_identity.clone(),
                parent_journal_sha256: crate::retirement_journal::fixture_signed_file_hash(
                    &runtime,
                    &journal_name,
                )?,
                previous_outcome_sha256: None,
                action: Action::FinishPurgeAtU,
                expected: observation,
            };
            crate::retirement_journal::fixture_publish_resolution(
                &runtime, &identity, &parent, &intent,
            )?;
            let target = match remaining {
                "u" => path.join(format!(
                    "scheduler-retirement.role.{}.main.purge",
                    parent.generation
                )),
                "r" => path.join(intent.quarantine_filename()?),
                _ => canonical.clone(),
            };
            if remaining == "absent" {
                std::fs::remove_file(&canonical)?;
            } else {
                std::fs::rename(&canonical, &target)?;
            }
            let evidence =
                RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.remove(0);
            let selected = select_authenticated_remaining_sources(&runtime, &evidence)?;
            assert!(evidence.resolution_slots[0].outcome.is_none());
            match remaining {
                "u" => assert!(matches!(
                    selected[0].source,
                    SchedulerRetirementStoppedSource::Purge { .. }
                )),
                "r" => assert!(matches!(
                    selected[0].source,
                    SchedulerRetirementStoppedSource::Resolution { ordinal: 0, .. }
                )),
                _ => assert!(selected[0].current.is_none()),
            }
            let mut unauthorized = evidence.clone();
            unauthorized.resolution_slots.clear();
            if remaining == "r" {
                assert!(select_authenticated_remaining_sources(&runtime, &unauthorized).is_err());
            }
            // No native owner or directory acknowledgement is fabricated here.
            // Production syncs its real retained directory after stopped intake.
        }
        Ok(())
    }

    #[test]
    fn late_purge_accepts_exact_survivor_refuses_alias_and_keeps_original_accounting() -> Result<()>
    {
        for count in [10, 11, 12, 13, 21, 27] {
            let directory = tempfile::tempdir()?;
            let identity = ryeos_node_runtime::identity::NodeIdentity::create(
                &directory.path().join("identity.key"),
            )?;
            let path = directory.path().join("runtime");
            std::fs::create_dir(&path)?;
            let runtime =
                lillux::PinnedDirectory::open(&path)?.context("fixture runtime absent")?;
            let bytes = b"remaining signed role bytes";
            let canonical = path.join("scheduler.sqlite3");
            std::fs::write(&canonical, bytes)?;
            let metadata = std::fs::metadata(&canonical)?;
            let original = SchedulerRetirementRoleIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                byte_length: metadata.len(),
                content_sha256: Some(lillux::sha256_hex(bytes)),
            };
            let roles = [
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Canonical,
                    current: Some(original.clone()),
                },
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                },
                SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                },
            ];
            let intent = initial_intent(&runtime, &identity, roles)?;
            signed_prefix(&runtime, &identity, &intent, count)?;
            let generation = intent.generation.clone();
            let quarantine = path.join(format!(
                "scheduler-retirement.role.{generation}.main.quarantine"
            ));
            if (11..=12).contains(&count) {
                std::fs::rename(&canonical, &quarantine)?;
            }
            if count >= 13 {
                std::fs::remove_file(&canonical)?;
            }
            let evidence =
                RetirementJournal::scan_interrupted_evidence(&runtime, &identity)?.remove(0);
            let remaining = select_authenticated_remaining_sources(&runtime, &evidence)?;
            assert_eq!(
                evidence.generation_intent.scheduler_accounting,
                intent.scheduler_accounting
            );
            if count >= 13 {
                assert!(remaining[0].current.is_none());
            } else {
                assert_eq!(remaining[0].current, Some(original));
            }
            let unexpected = path.join(format!(
                "scheduler-retirement.resolve.{generation}.main.03.quarantine"
            ));
            std::fs::write(&unexpected, b"foreign surviving alias")?;
            assert!(select_authenticated_remaining_sources(&runtime, &evidence).is_err());
            std::fs::remove_file(unexpected)?;
            if count == 10 {
                std::fs::hard_link(&canonical, &quarantine)?;
                assert!(select_authenticated_remaining_sources(&runtime, &evidence).is_err());
                std::fs::remove_file(&quarantine)?;
                std::fs::remove_file(&canonical)?;
                assert!(select_authenticated_remaining_sources(&runtime, &evidence).is_err());
            }
        }
        Ok(())
    }
}
