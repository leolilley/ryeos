//! Explicit offline retirement of the local execution-history epoch.
//!
//! Normal maintenance GC deliberately preserves authoritative chain heads.
//! This module owns the separate operator-authorized path used when an entire
//! local thread-history epoch is disposable. It coordinates every store while
//! the daemon state lock, runtime namespace lock, and exclusive CAS mutation
//! guard are held.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{Config, ConfigSources};
use crate::runtime_db::{RuntimeDb, RuntimeThreadHistoryDiscardReport};
use crate::state_lock::{StateLock, default_lock_path};

pub const EXECUTION_SCHEMA_CUTOVER_COMMAND: &str =
    "ryeos node reset execution-history --include-project-heads --confirm --confirm-project-heads";

#[derive(Debug, Clone, Default)]
pub struct ExecutionHistoryResetOptions {
    pub app_root: Option<PathBuf>,
    pub dry_run: bool,
    /// Explicit immutable project-schema cutover for principal and deployed
    /// project HEADs. This is accepted only with the all-thread-history discard
    /// because live history may reference either namespace.
    pub discard_project_heads: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionHistoryResetPhase {
    CapturingAuthority,
    InspectingHistory,
    PublishingIntent,
    RetiringChainHeads,
    RetiringProjectHeads,
    RebuildingProjection,
    ClearingRuntime,
    ClearingScheduler,
    Finalizing,
    Complete,
}

impl ExecutionHistoryResetPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CapturingAuthority => "capturing_authority",
            Self::InspectingHistory => "inspecting_history",
            Self::PublishingIntent => "publishing_intent",
            Self::RetiringChainHeads => "retiring_chain_heads",
            Self::RetiringProjectHeads => "retiring_project_heads",
            Self::RebuildingProjection => "rebuilding_projection",
            Self::ClearingRuntime => "clearing_runtime",
            Self::ClearingScheduler => "clearing_scheduler",
            Self::Finalizing => "finalizing",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionHistoryResetProgress {
    pub phase: ExecutionHistoryResetPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectionDiscardReport {
    pub chains_rebuilt: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
    pub superseded_instances_deleted: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RuntimeThreadHistoryAccounting {
    Exact {
        counts: RuntimeThreadHistoryDiscardReport,
    },
    UnavailableIncompatibleSchema,
}

impl RuntimeThreadHistoryAccounting {
    pub fn total_rows(&self) -> Option<usize> {
        match self {
            Self::Exact { counts } => Some(counts.total_rows()),
            Self::UnavailableIncompatibleSchema => None,
        }
    }
}

fn completed_runtime_accounting(
    schema_reset_required: bool,
    counts: RuntimeThreadHistoryDiscardReport,
) -> RuntimeThreadHistoryAccounting {
    if schema_reset_required {
        RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema
    } else {
        RuntimeThreadHistoryAccounting::Exact { counts }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionHistoryResetReport {
    pub app_root: PathBuf,
    pub dry_run: bool,
    pub chain_heads: usize,
    pub project_heads: usize,
    pub chain_ref_artifacts: usize,
    pub pending_transitions: usize,
    pub runtime_rows: RuntimeThreadHistoryAccounting,
    pub thread_runtime_artifacts: usize,
    pub scheduler_journal_artifacts: usize,
    pub scheduler_rows: ryeos_scheduler::db::SchedulerFireHistoryAccounting,
    pub replay_indexes: ryeos_state::operational::ReplayIndexResetReport,
    pub projection: ProjectionDiscardReport,
}

impl ExecutionHistoryResetReport {
    pub fn total_discarded_rows(&self) -> Option<usize> {
        Some(self.runtime_rows.total_rows()? + self.scheduler_rows.total_rows()?)
    }
}

/// Inspect or execute the explicit all-execution-history reset transaction.
pub fn run_execution_history_reset(
    options: &ExecutionHistoryResetOptions,
) -> Result<ExecutionHistoryResetReport> {
    run_execution_history_reset_inner(options, None)
}

pub fn run_execution_history_reset_with_progress(
    options: &ExecutionHistoryResetOptions,
    observer: &mut dyn FnMut(&ExecutionHistoryResetProgress),
) -> Result<ExecutionHistoryResetReport> {
    run_execution_history_reset_inner(options, Some(observer))
}

fn run_execution_history_reset_inner(
    options: &ExecutionHistoryResetOptions,
    mut observer: Option<&mut dyn FnMut(&ExecutionHistoryResetProgress)>,
) -> Result<ExecutionHistoryResetReport> {
    crate::provider_object_contracts::install()
        .context("install application object contracts for execution-history reset")?;
    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::CapturingAuthority,
        None,
    );
    let config = Config::load(&ConfigSources {
        app_root: options.app_root.clone(),
        ..ConfigSources::default()
    })
    .context("load local node configuration for execution-history reset")?;
    let runtime_state_dir = config.runtime_state_dir();

    // This is the outer ownership proof. A running daemon and every
    // cooperating standalone state service hold the same lock.
    let state_lock_path = default_lock_path(&config.app_root);
    let _state_lock = if options.dry_run {
        StateLock::acquire_existing_read_only(&state_lock_path)
    } else {
        StateLock::acquire(&state_lock_path)
    }
    .context("execution-history reset requires the daemon to be stopped")?;
    let runtime_directory =
        lillux::PinnedDirectory::open(&runtime_state_dir)?.ok_or_else(|| {
            anyhow::anyhow!("runtime state is absent: {}", runtime_state_dir.display())
        })?;
    let runtime_directory_lock = runtime_directory
        .lock_exclusive()
        .context("lock offline runtime-state namespace")?;

    // Signed chain heads belong to the node-identity trust domain, matching
    // ordinary daemon startup. Item/publisher trust is a separate domain and
    // must never be substituted for head authority here.
    let identity = crate::identity::NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for signed-head verification")?;
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let head_trust = Arc::new(head_trust);
    let mut state_db = ryeos_state::StateDb::open_for_projection_rebuild(
        &runtime_state_dir,
        Arc::clone(&head_trust),
    )
    .context("open pinned state authority for offline execution-history reset")?;
    let state_authority = state_db.pinned_authority()?;
    if !state_authority
        .runtime_directory()
        .is_same_directory(&runtime_directory)?
    {
        anyhow::bail!(
            "runtime-state path changed while execution-history reset captured authority"
        );
    }
    let cas_guard = state_authority
        .acquire_exclusive_guard(!options.dry_run)
        .context("acquire exclusive head mutation guard for execution-history reset")?;

    let mut runtime_db = open_runtime_db(
        &config,
        &runtime_directory,
        &runtime_directory_lock,
        options.dry_run,
    )?;
    let scheduler_db_path = runtime_state_dir.join("scheduler.sqlite3");
    let scheduler_preview = inspect_scheduler_db(&scheduler_db_path, &runtime_directory)?;

    // Validate every participating namespace before publishing destructive
    // intent. The second pass performs the same descriptor-rooted checks while
    // mutating; no pathname-only deletion is used.
    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::InspectingHistory,
        None,
    );
    let authoritative_preview = state_db
        .discard_authoritative_thread_history_admitted(&cas_guard, true)
        .context("inspect authoritative thread history")?;
    let project_heads_preview = if options.discard_project_heads {
        state_db
            .discard_all_project_heads_admitted(&cas_guard, true)
            .context("inspect principal and deployed project HEADs")?
    } else {
        0
    };
    let runtime_schema_reset_required = runtime_db.requires_explicit_history_reset();
    let runtime_preview = if runtime_schema_reset_required {
        // Strict no-backcompat means an incompatible layout is never decoded
        // merely to estimate destructive counts.
        RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema
    } else {
        RuntimeThreadHistoryAccounting::Exact {
            counts: runtime_db
                .discard_all_thread_history(true)
                .context("inspect daemon runtime thread rows")?,
        }
    };
    let thread_runtime_preview = crate::state_store::discard_all_thread_runtime_files(
        &config.app_root,
        &runtime_directory,
        true,
    )
    .context("inspect per-thread runtime files")?;
    let scheduler_journal_preview = discard_scheduler_fire_journals(&runtime_directory, true)
        .context("inspect scheduler fire journals")?;

    // Replay indexes are disposable execution authority; credentials are not.
    // Inspect both through the existing operational owner before dry-run can
    // succeed. Never use ordinary current-open here: it correctly rejects
    // predecessor replay epochs, even though this command explicitly retires
    // them. The restricted preparation cannot service replay or retire rows.
    let prepared_replay =
        ryeos_state::OperationalDb::prepare_replay_reset_with_namespace_authority(
            &runtime_directory,
            runtime_directory_lock.clone(),
            options.dry_run,
        )
        .context("prepare replay retirement and stable credential preservation")?;
    let replay_indexes = prepared_replay.report();
    let stable_profiles = prepared_replay
        .credential_profiles()
        .context("validate stable credential-profile authority before history retirement")?;

    // Credential-profile lifecycle authority is stable node state, not
    // execution history. OperationalDb is the only authority carried across
    // this cutover; a predecessor RuntimeDb is deliberately opaque and its
    // credential projection is never decoded as a compatibility format.
    // An enrolling ceremony is owned by worker/session history that this
    // operation retires. Its invalidation is itself a monotonic transition in
    // the stable authority and happens before any runtime-schema mutation.
    let stable_profiles = stable_profiles
        .into_iter()
        .map(|profile| -> Result<_> {
            if profile.state != "enrolling" {
                return Ok(profile);
            }
            let mut invalidated = profile;
            invalidated.authority_revision = invalidated
                .authority_revision
                .checked_add(1)
                .context("credential authority revision overflow during history retirement")?;
            invalidated.state = "unauthenticated".to_owned();
            invalidated.active_login_id = None;
            invalidated.login_expires_at_ms = None;
            invalidated.sanitized_account = None;
            invalidated.updated_at_ms = invalidated
                .updated_at_ms
                .max(lillux::time::timestamp_millis() as i64);
            Ok(invalidated)
        })
        .collect::<Result<Vec<_>>>()?;

    if options.dry_run {
        publish_progress(&mut observer, ExecutionHistoryResetPhase::Complete, None);
        return Ok(ExecutionHistoryResetReport {
            app_root: config.app_root,
            dry_run: true,
            chain_heads: authoritative_preview.chain_heads,
            project_heads: project_heads_preview,
            chain_ref_artifacts: authoritative_preview.chain_ref_artifacts,
            pending_transitions: authoritative_preview.pending_transitions,
            runtime_rows: runtime_preview,
            thread_runtime_artifacts: thread_runtime_preview,
            scheduler_journal_artifacts: scheduler_journal_preview,
            scheduler_rows: scheduler_preview,
            replay_indexes,
            projection: ProjectionDiscardReport {
                superseded_instances_deleted: authoritative_preview.superseded_projection_instances,
                ..ProjectionDiscardReport::default()
            },
        });
    }

    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::PublishingIntent,
        None,
    );
    state_db
        .begin_thread_history_discard_admitted(&cas_guard)
        .context("publish offline thread-history discard intent")?;
    // Only after durable intent may any replay rows or enrolling ceremonies
    // be retired. A crash leaves the existing discard marker for safe retry.
    let (operational_db, replay_indexes) = prepared_replay
        .activate()
        .context("activate replay indexes after durable discard intent")?;
    for profile in &stable_profiles {
        operational_db
            .merge_credential_profile(profile)
            .context("preserve stable credential authority and invalidate retired enrollment")?;
    }
    runtime_db
        .apply_explicit_history_reset(&config.db_path)
        .context("apply runtime schema cutover after durable discard intent")?;
    for profile in stable_profiles {
        runtime_db
            .reconcile_credential_profile_projection(&profile)
            .context("restore credential-profile runtime projection")?;
    }

    let authoritative = {
        let mut authoritative_progress = |progress| match progress {
            ryeos_state::AuthoritativeThreadHistoryDiscardProgress::RetiringChainHeads {
                completed,
                total,
            } => publish_progress(
                &mut observer,
                ExecutionHistoryResetPhase::RetiringChainHeads,
                Some((completed, total)),
            ),
            ryeos_state::AuthoritativeThreadHistoryDiscardProgress::RebuildingProjection => {
                publish_progress(
                    &mut observer,
                    ExecutionHistoryResetPhase::RebuildingProjection,
                    None,
                )
            }
        };
        state_db
            .discard_authoritative_thread_history_admitted_with_progress(
                &cas_guard,
                false,
                &mut authoritative_progress,
            )
            .context("discard authoritative thread history")?
    };

    let project_heads = if options.discard_project_heads {
        publish_progress(
            &mut observer,
            ExecutionHistoryResetPhase::RetiringProjectHeads,
            None,
        );
        state_db
            .discard_all_project_heads_admitted(&cas_guard, false)
            .context("discard principal and deployed project HEADs for schema cutover")?
    } else {
        0
    };

    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::ClearingRuntime,
        None,
    );
    let runtime_rows = runtime_db
        .discard_all_thread_history(false)
        .context("discard daemon runtime thread rows")?;
    let thread_runtime_artifacts = crate::state_store::discard_all_thread_runtime_files(
        &config.app_root,
        &runtime_directory,
        false,
    )
    .context("discard per-thread runtime files")?;

    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::ClearingScheduler,
        None,
    );
    let scheduler_journal_artifacts = discard_scheduler_fire_journals(&runtime_directory, false)
        .context("discard scheduler fire journals")?;
    // Rebuild a recognized stale projection only after durable discard intent
    // and retirement of its authoritative fire journals, never during preview.
    ryeos_scheduler::SchedulerDb::open(&scheduler_db_path)
        .context("open scheduler database for confirmed history retirement")?
        .discard_fire_history(false)
        .context("discard scheduler fire projection")?;

    publish_progress(&mut observer, ExecutionHistoryResetPhase::Finalizing, None);
    state_db
        .finish_thread_history_discard_admitted(&cas_guard)
        .context("acknowledge completed offline thread-history discard")?;

    let rebuilt = authoritative.rebuilt_projection.unwrap_or_default();
    publish_progress(&mut observer, ExecutionHistoryResetPhase::Complete, None);
    Ok(ExecutionHistoryResetReport {
        app_root: config.app_root,
        dry_run: false,
        chain_heads: authoritative.chain_heads,
        project_heads,
        chain_ref_artifacts: authoritative.chain_ref_artifacts,
        pending_transitions: authoritative.pending_transitions,
        runtime_rows: completed_runtime_accounting(runtime_schema_reset_required, runtime_rows),
        thread_runtime_artifacts,
        scheduler_journal_artifacts,
        scheduler_rows: scheduler_preview,
        replay_indexes,
        projection: ProjectionDiscardReport {
            chains_rebuilt: rebuilt.chains_rebuilt,
            threads_restored: rebuilt.threads_restored,
            events_projected: rebuilt.events_projected,
            superseded_instances_deleted: authoritative.superseded_projection_instances,
        },
    })
}

fn publish_progress(
    observer: &mut Option<&mut dyn FnMut(&ExecutionHistoryResetProgress)>,
    phase: ExecutionHistoryResetPhase,
    counts: Option<(usize, usize)>,
) {
    if let Some(observer) = observer.as_deref_mut() {
        let (completed, total) = counts
            .map(|(completed, total)| (Some(completed), Some(total)))
            .unwrap_or((None, None));
        observer(&ExecutionHistoryResetProgress {
            phase,
            completed,
            total,
        });
    }
}

fn open_runtime_db(
    config: &Config,
    runtime_directory: &lillux::PinnedDirectory,
    runtime_directory_lock: &lillux::PinnedDirectoryLock,
    dry_run: bool,
) -> Result<RuntimeDb> {
    let parent_path = config.db_path.parent().unwrap_or_else(|| Path::new("."));
    let parent = if dry_run {
        lillux::PinnedDirectory::open(parent_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "runtime database parent is absent: {}",
                parent_path.display()
            )
        })?
    } else {
        lillux::PinnedDirectory::open_or_create(parent_path)?
    };
    if runtime_directory.is_same_directory(&parent)? {
        if dry_run {
            RuntimeDb::open_existing_for_explicit_history_reset_with_namespace_authority(
                &config.db_path,
                parent,
                runtime_directory_lock.clone(),
            )
        } else {
            RuntimeDb::open_for_explicit_history_reset_with_namespace_authority(
                &config.db_path,
                parent,
                runtime_directory_lock.clone(),
            )
        }
    } else if dry_run {
        RuntimeDb::open_existing_for_explicit_history_reset(&config.db_path)
    } else {
        RuntimeDb::open_for_explicit_history_reset(&config.db_path)
    }
    .with_context(|| format!("open runtime database {}", config.db_path.display()))
}

fn inspect_scheduler_db(
    path: &Path,
    runtime_directory: &lillux::PinnedDirectory,
) -> Result<ryeos_scheduler::db::SchedulerFireHistoryAccounting> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if runtime_directory.path() != parent {
        anyhow::bail!(
            "scheduler database namespace authority path mismatch: selected={}, requested={}",
            runtime_directory.path().display(),
            parent.display()
        );
    }
    let name = path.file_name().ok_or_else(|| {
        anyhow::anyhow!(
            "scheduler database path has no filename: {}",
            path.display()
        )
    })?;
    let existing = runtime_directory
        .open_regular(name, false)
        .with_context(|| {
            format!(
                "scheduler database must be a regular non-symlink file: {}",
                path.display()
            )
        })?;

    match existing {
        None => Ok(ryeos_scheduler::db::SchedulerFireHistoryAccounting::Exact {
            counts: Default::default(),
        }),
        Some(file) => {
            drop(file);
            let (inspection_directory, _inspection_guard) =
                crate::runtime_db::create_sqlite_inspection_copy(
                    runtime_directory,
                    name,
                    "scheduler",
                )?;
            let inspection_path = inspection_directory.path().join(name);
            ryeos_scheduler::SchedulerDb::inspect_history_reset(&inspection_path)
                .context("inspect disposable scheduler database copy for history retirement")
        }
    }
}

fn discard_scheduler_fire_journals(
    runtime_directory: &lillux::PinnedDirectory,
    dry_run: bool,
) -> Result<usize> {
    let Some(schedules) =
        runtime_directory.open_child_directory(std::ffi::OsStr::new("schedules"))?
    else {
        return Ok(0);
    };
    let mut artifacts = 0usize;
    for schedule_name in schedules.entry_names()? {
        let schedule_id = schedule_name
            .to_str()
            .context("scheduler fire directory name is not UTF-8")?;
        ryeos_scheduler::crontab::validate_schedule_id(schedule_id)?;
        let schedule = schedules
            .open_child_directory(&schedule_name)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler fire entry is not a directory: {}",
                    schedules.path().join(&schedule_name).display()
                )
            })?;
        for entry_name in schedule.entry_names()? {
            if !matches!(
                entry_name.to_str(),
                Some("fires.jsonl" | ".fires.jsonl.lock")
            ) {
                anyhow::bail!(
                    "unexpected scheduler fire-history entry: {}",
                    schedule.path().join(&entry_name).display()
                );
            }
            let file = schedule.open_regular(&entry_name, false)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler fire-history entry is not a regular file: {}",
                    schedule.path().join(&entry_name).display()
                )
            })?;
            if !dry_run {
                schedule.remove_if_same(&entry_name, &file)?;
            }
            artifacts = artifacts
                .checked_add(1)
                .context("scheduler fire-history artifact count overflow")?;
        }
        if !dry_run && !schedules.remove_empty_child_if_same(&schedule_name, &schedule)? {
            anyhow::bail!(
                "scheduler fire directory changed during discard: {}",
                schedule.path().display()
            );
        }
        artifacts = artifacts
            .checked_add(1)
            .context("scheduler fire-history artifact count overflow")?;
    }
    Ok(artifacts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_effect_contract::EffectClass;
    use ryeos_provider_contract::{
        AdmittedLocalWorkerFinal, AdmittedLocalWorkerUsage, FirstObservation,
        LocalWorkerObservation, ObservationClass, PreparedRequestProjection, ProviderCallAnswer,
        ProviderCallRecord, RecordedMessage, RequestAuthority, RequestCoordinate,
        TransportCoordinate,
    };
    use tempfile::TempDir;

    fn local_coordinate() -> RequestCoordinate {
        RequestCoordinate::build(
            RequestAuthority {
                outer_effective_definition_digest: "1".repeat(64),
                provider_family: "chat_completions".to_owned(),
                provider_config_hash: "provider-config".to_owned(),
                provider_config_value_digest: "2".repeat(64),
                provider_id: "fixture-local-recorded".to_owned(),
                profile_id: None,
                model_name: "qwen3-0.6b".to_owned(),
                credential_binding_hmac: "3".repeat(64),
                credential_authority_generation: "none".to_owned(),
                authority_digest: "4".repeat(64),
                admitted_effect_class: Some(EffectClass::Recorded),
            },
            TransportCoordinate::AdmittedLocalWorker {
                worker_ref: "worker:fixtures/local-recorded".to_owned(),
                effective_definition_digest: "5".repeat(64),
                capsule_hash: "6".repeat(64),
                execution_realization_hash: "7".repeat(64),
            },
            PreparedRequestProjection::new(
                std::iter::empty(),
                std::iter::empty(),
                "8".repeat(64),
                16,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn local_terminal() -> AdmittedLocalWorkerFinal {
        AdmittedLocalWorkerFinal {
            answer: ProviderCallAnswer {
                message: RecordedMessage {
                    role: "assistant".to_owned(),
                    content: Some(serde_json::Value::String("done".to_owned())),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_owned()),
            },
            usage: AdmittedLocalWorkerUsage {
                input_tokens: 5,
                output_tokens: 1,
                reasoning_tokens: None,
            },
            response_id: Some("response-1".to_owned()),
        }
    }

    fn write_object(cas_root: &Path, value: &serde_json::Value) -> String {
        let canonical = lillux::canonical_json(value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        hash
    }

    #[test]
    fn incompatible_runtime_accounting_is_explicitly_unavailable_not_zero() {
        let accounting = RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema;
        assert_eq!(accounting.total_rows(), None);
        assert_eq!(
            serde_json::to_value(accounting).unwrap(),
            serde_json::json!({"status": "unavailable_incompatible_schema"})
        );
    }

    #[test]
    fn current_runtime_accounting_reports_exact_counts() {
        let accounting = RuntimeThreadHistoryAccounting::Exact {
            counts: RuntimeThreadHistoryDiscardReport {
                thread_runtime: 3,
                follow_waiters: 2,
                ..RuntimeThreadHistoryDiscardReport::default()
            },
        };
        assert_eq!(accounting.total_rows(), Some(5));
        assert_eq!(serde_json::to_value(accounting).unwrap()["status"], "exact");
    }

    #[test]
    fn execution_history_reset_closure_knows_provider_records_and_local_observations() {
        crate::provider_object_contracts::install().unwrap();
        let coordinate = local_coordinate();
        let terminal = local_terminal();
        let observation = LocalWorkerObservation {
            schema: ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_SCHEMA_VERSION,
            kind: ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_KIND.to_owned(),
            attempt_id: "attempt-1".to_owned(),
            request_hash: "d".repeat(64),
            coordinate_key: coordinate.cache_key().unwrap(),
            capsule_hash: "6".repeat(64),
            admitted_execution_realization_hash: "7".repeat(64),
            observed_execution_realization_hash: None,
            observed_at: "2026-08-09T00:00:00.000Z".to_owned(),
            terminal_digest: terminal.digest().unwrap(),
            terminal: terminal.clone(),
            execution_identity_digest: "9".repeat(64),
            execution_identity_attestation_hash: "a".repeat(64),
        };
        let answer_digest = terminal.answer.digest().unwrap();
        let record = ProviderCallRecord {
            schema: ryeos_provider_contract::PROVIDER_CALL_RECORD_SCHEMA_VERSION,
            kind: ryeos_provider_contract::PROVIDER_CALL_RECORD_KIND.to_owned(),
            cache_key: coordinate.cache_key().unwrap(),
            coordinate,
            answer_digest: answer_digest.clone(),
            answer: terminal.answer,
            first_observation: FirstObservation {
                produced_by_thread: "T-local".to_owned(),
                attempt_id: "attempt-1".to_owned(),
                response_digest: answer_digest,
                observed_at: "2026-08-09T00:00:00.000Z".to_owned(),
                observation_class: ObservationClass::DaemonWorkerObserved,
                provider_accounting: serde_json::json!({
                    "input_tokens": 5,
                    "output_tokens": 1,
                }),
                execution_identity_digest: Some("9".repeat(64)),
                execution_identity_attestation_hash: Some("a".repeat(64)),
                admitted_execution_realization_hash: Some("7".repeat(64)),
                observed_execution_realization_hash: None,
            },
        };

        let temp = TempDir::new().unwrap();
        let cas_root = temp.path().join("cas");
        let observation_hash = write_object(&cas_root, &observation.to_value().unwrap());
        let record_hash = write_object(&cas_root, &record.to_value().unwrap());
        let report = ryeos_state::object_closure::collect_object_closure(
            &cas_root,
            [observation_hash.clone(), record_hash.clone()],
        )
        .unwrap();

        assert!(report.unsupported_objects.is_empty(), "{report:?}");
        assert!(report.malformed_objects.is_empty(), "{report:?}");
        assert!(report.object_hashes.contains(&observation_hash));
        assert!(report.object_hashes.contains(&record_hash));
        let missing = report
            .missing_objects
            .iter()
            .map(|entry| entry.hash.clone())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            missing,
            std::collections::BTreeSet::from(["6".repeat(64), "7".repeat(64), "a".repeat(64),])
        );
    }

    #[test]
    fn confirmed_schema_reset_never_reports_post_reset_zero_as_exact() {
        let accounting =
            completed_runtime_accounting(true, RuntimeThreadHistoryDiscardReport::default());
        assert!(matches!(
            accounting,
            RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema
        ));
    }

    #[test]
    fn scheduler_dry_run_opens_only_a_disposable_copy() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("scheduler.sqlite3");
        let scheduler = ryeos_scheduler::SchedulerDb::open(&path).unwrap();
        scheduler.finish_fire_projection_rebuild().unwrap();
        drop(scheduler);
        let source_snapshot = || {
            std::fs::read_dir(tmp.path())
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let name = entry.file_name();
                    let bytes = std::fs::read(entry.path()).unwrap();
                    (name, bytes)
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = source_snapshot();
        let directory = lillux::PinnedDirectory::open(tmp.path())
            .unwrap()
            .expect("scheduler parent exists");
        let report = inspect_scheduler_db(&path, &directory).unwrap();
        assert_eq!(report.total_rows(), Some(0));
        assert_eq!(source_snapshot(), before);
    }

    #[test]
    fn history_reset_composes_replay_cutover_and_preserves_stable_credentials() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity =
            crate::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
        let identity_bytes = std::fs::read(&config.node_signing_key_path).unwrap();
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let trust = Arc::new(trust);
        drop(ryeos_state::StateDb::open(&config.runtime_state_dir(), trust.clone()).unwrap());
        drop(RuntimeDb::open(&config.db_path).unwrap());
        drop(StateLock::acquire(&default_lock_path(tmp.path())).unwrap());
        let operational_path = config
            .runtime_state_dir()
            .join(ryeos_state::operational::OPERATIONAL_DB_FILENAME);
        let profile = ryeos_state::operational::OperationalCredentialProfileRecord {
            profile_id: "retained-active".to_owned(),
            owner_principal: "operator".to_owned(),
            home_id: "home".to_owned(),
            authority_revision: 3,
            credential_generation: 2,
            state: "active".to_owned(),
            active_login_id: None,
            login_epoch: 1,
            login_expires_at_ms: None,
            sanitized_account: Some(serde_json::json!({"account_id":"retained-account"})),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let mut enrolling = profile.clone();
        enrolling.profile_id = "retired-enrollment".to_owned();
        enrolling.home_id = "enrollment-home".to_owned();
        enrolling.state = "enrolling".to_owned();
        enrolling.active_login_id = Some("old-login".to_owned());
        enrolling.login_expires_at_ms = Some(100);
        {
            let db = ryeos_state::OperationalDb::open_existing_current(&operational_path).unwrap();
            db.merge_credential_profile(&profile).unwrap();
            db.merge_credential_profile(&enrolling).unwrap();
        }
        let current_replay_epoch = {
            let conn = rusqlite::Connection::open(&operational_path).unwrap();
            let epoch: i32 = conn
                .query_row("SELECT epoch FROM replay_index_epoch", [], |r| r.get(0))
                .unwrap();
            conn.execute_batch("UPDATE replay_index_epoch SET epoch = epoch - 2;")
                .unwrap();
            epoch
        };
        {
            let conn = rusqlite::Connection::open(&config.db_path).unwrap();
            let app_id: i32 = conn
                .query_row("PRAGMA application_id", [], |r| r.get(0))
                .unwrap();
            conn.pragma_update(None, "application_id", app_id - 1)
                .unwrap();
        }
        let options = ExecutionHistoryResetOptions {
            app_root: Some(tmp.path().to_owned()),
            dry_run: true,
            discard_project_heads: false,
        };
        let runtime_before = std::fs::read(&config.db_path).unwrap();
        let operational_before = std::fs::read(&operational_path).unwrap();
        let preview = run_execution_history_reset(&options).unwrap();
        assert_eq!(
            preview.replay_indexes.scope,
            ryeos_state::operational::ReplayIndexResetScope::AllReplayRecords
        );
        assert_eq!(
            preview.replay_indexes.stored_epoch,
            current_replay_epoch - 2
        );
        assert_eq!(std::fs::read(&config.db_path).unwrap(), runtime_before);
        assert_eq!(
            std::fs::read(&operational_path).unwrap(),
            operational_before
        );
        assert!(ryeos_state::OperationalDb::open_existing_current(&operational_path).is_err());

        // Model resumption after a process dies with the existing durable
        // discard intent published but before replay/runtime activation.
        {
            let state = ryeos_state::StateDb::open_for_projection_rebuild(
                &config.runtime_state_dir(),
                trust.clone(),
            )
            .unwrap();
            let authority = state.pinned_authority().unwrap();
            let guard = authority.acquire_exclusive_guard(true).unwrap();
            state.begin_thread_history_discard_admitted(&guard).unwrap();
        }
        let options = ExecutionHistoryResetOptions {
            dry_run: false,
            ..options
        };
        let report = run_execution_history_reset(&options).unwrap();
        assert_eq!(report.replay_indexes, preview.replay_indexes);
        let db = ryeos_state::OperationalDb::open_existing_current(&operational_path).unwrap();
        assert_eq!(
            db.credential_profile(&profile.profile_id).unwrap(),
            Some(profile)
        );
        let retired = db
            .credential_profile(&enrolling.profile_id)
            .unwrap()
            .unwrap();
        assert_eq!(retired.state, "unauthenticated");
        assert_eq!(retired.authority_revision, enrolling.authority_revision + 1);
        assert_eq!(
            retired.credential_generation,
            enrolling.credential_generation
        );
        assert_eq!(retired.home_id, enrolling.home_id);
        assert!(retired.active_login_id.is_none());
        assert!(retired.sanitized_account.is_none());
        drop(db);
        drop(RuntimeDb::open(&config.db_path).unwrap());
        drop(ryeos_state::StateDb::open(&config.runtime_state_dir(), trust).unwrap());
        let repeated = run_execution_history_reset(&options).unwrap();
        assert_eq!(
            repeated.replay_indexes.scope,
            ryeos_state::operational::ReplayIndexResetScope::Unchanged
        );
        assert_eq!(
            std::fs::read(&config.node_signing_key_path).unwrap(),
            identity_bytes
        );
    }

    #[test]
    fn scheduler_dry_run_preserves_missing_source() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("scheduler.sqlite3");
        let directory = lillux::PinnedDirectory::open(tmp.path())
            .unwrap()
            .expect("scheduler parent exists");
        assert_eq!(
            inspect_scheduler_db(&path, &directory)
                .unwrap()
                .total_rows(),
            Some(0)
        );
        assert!(!path.exists());
    }

    #[test]
    fn scheduler_dry_run_reports_stale_schema_without_changing_source() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("scheduler.sqlite3");
        drop(ryeos_scheduler::SchedulerDb::open(&path).unwrap());
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("ALTER TABLE schedule_specs ADD COLUMN retired_field TEXT;")
            .unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let report = inspect_scheduler_db(&path, &directory).unwrap();
        assert_eq!(report.total_rows(), None);
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            serde_json::json!({"status": "unavailable_incompatible_schema"})
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
