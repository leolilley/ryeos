//! Daemon-side `SchedulerContext` implementation for `AppState`.
//!
//! The scheduler owns fire identity and durable transition ordering. This
//! adapter resolves the signed, portable execution policy against daemon-local
//! project/isolation authority. A fire first binds one project authority and
//! later dispatches only from that persisted authority; recovery never rereads
//! a moving project HEAD for the same fire.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{EffectivePrincipal, PlanContext, Principal, ProjectContext};
use ryeos_engine::execution_contract::{PinnedSource, ProjectExecutionPolicy};
use ryeos_executor::execution::project_source::{
    PinnedContextRealization, ProjectSource, ResolvedCurrentHeadDestination,
};
use ryeos_scheduler::db::SchedulerDb;
use ryeos_scheduler::types::ScheduleSpecRecord;
use ryeos_scheduler::{
    ScheduledDispatchReceipt, ScheduledProjectBinding, SchedulerContext, ThreadResultOutcome,
};
use ryeos_state::objects::{
    ExecutionProjectAuthority, PinnedProjectRealization, PinnedTerminalPublication,
};

/// Newtype wrapper around `AppState` so daemon-private dispatch can implement
/// the scheduler-owned context trait without moving either public type.
#[derive(Clone)]
pub struct AppSchedulerContext(pub Arc<AppState>);

impl AppSchedulerContext {
    fn validate_fire_spec(
        spec: &ScheduleSpecRecord,
        fire: &ryeos_engine::contracts::ScheduledFireContext,
    ) -> Result<()> {
        ryeos_scheduler::types::validate_schedule_spec_record(spec)?;
        fire.validate()?;
        if fire.schedule_id != spec.schedule_id || fire.schedule_spec_hash != spec.spec_hash {
            bail!("scheduled fire context does not identify the admitted schedule generation");
        }
        Ok(())
    }

    fn project_root(spec: &ScheduleSpecRecord) -> Result<PathBuf> {
        spec.project_root
            .as_deref()
            .map(PathBuf::from)
            .context("project-backed schedule has no project_root")
    }

    fn pinned_binding(
        authority: &ExecutionProjectAuthority,
    ) -> Result<(&str, Option<ResolvedCurrentHeadDestination>)> {
        let ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            realization,
            ..
        } = authority
        else {
            bail!("scheduled pinned execution was not bound to a pinned generation");
        };
        let destination = match realization {
            PinnedProjectRealization::Cow {
                terminal_publication:
                    PinnedTerminalPublication::RetainCurrentHead {
                        principal_key,
                        project_hash,
                        expected_hash,
                    },
            } => Some(ResolvedCurrentHeadDestination {
                principal_key: principal_key.clone(),
                project_hash: project_hash.clone(),
                expected_hash: expected_hash.clone(),
            }),
            _ => None,
        };
        Ok((snapshot_hash, destination))
    }

    fn verify_bound_authority(
        &self,
        spec: &ScheduleSpecRecord,
        project_root: Option<&Path>,
        authority: &ExecutionProjectAuthority,
    ) -> Result<()> {
        let (snapshot_hash, current_head_destination) = match &spec.execution.policy.project {
            ProjectExecutionPolicy::Pinned { .. } => {
                let (hash, destination) = Self::pinned_binding(authority)?;
                (Some(hash), destination)
            }
            _ => (None, None),
        };
        let expected =
            ryeos_api::routes::response_modes::execute_mode::resolve_execution_project_authority(
                &spec.execution.policy,
                project_root,
                snapshot_hash,
                current_head_destination.as_ref(),
                self.0.threads.site_id(),
                &self.0.isolation,
                &spec.execution.capabilities,
            )?;
        if &expected != authority {
            bail!(
                "persisted scheduler project authority disagrees with the signed execution policy"
            );
        }
        Ok(())
    }

    async fn resolve_dispatch_provenance(
        &self,
        spec: &ScheduleSpecRecord,
        thread_id: &str,
        authority: &ExecutionProjectAuthority,
    ) -> Result<ryeos_app::execution_provenance::ExecutionProvenance> {
        match &spec.execution.policy.project {
            ProjectExecutionPolicy::Projectless => {
                self.verify_bound_authority(spec, None, authority)?;
                let state = self.0.as_ref().clone();
                let workspace_name = format!("schedule-{thread_id}");
                let (workspace, lifeline) = tokio::task::spawn_blocking(move || {
                    ryeos_app::temp_dir_guard::create_projectless_workspace(
                        &state.config.runtime_root().cache(),
                        &workspace_name,
                    )
                })
                .await
                .context("scheduled projectless workspace task stopped")??;
                ryeos_app::execution_provenance::ExecutionProvenance::root_projectless(
                    workspace,
                    self.0.engine.clone(),
                    lifeline,
                    authority.clone(),
                )
            }
            ProjectExecutionPolicy::LiveDirect { .. } => {
                let root = Self::project_root(spec)?;
                let state = self.0.as_ref().clone();
                let principal = spec.execution.principal_id().to_owned();
                let checkout_id = format!("schedule-live-{thread_id}");
                let context = ryeos_api::routes::response_modes::execute_mode::resolve_project_context_off_thread(
                    ryeos_api::routes::response_modes::execute_mode::ResolveProjectContextRequest {
                        state,
                        source: ProjectSource::LiveFs,
                        project_path: root,
                        principal_id: principal,
                        checkout_id,
                        pinned_realization: None,
                        normalization: ryeos_api::routes::response_modes::execute_mode::ProjectRootNormalization::CanonicalizeLive,
                        launch_timings: None,
                    },
                )
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                self.verify_bound_authority(spec, Some(&context.original_path), authority)?;
                ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
                    context.effective_path,
                    context.request_engine,
                    authority.clone(),
                )
            }
            ProjectExecutionPolicy::Pinned { .. } => {
                let original_path = Self::project_root(spec)?;
                self.verify_bound_authority(spec, Some(&original_path), authority)?;
                let (snapshot_hash, _) = Self::pinned_binding(authority)?;
                let snapshot_hash = snapshot_hash.to_owned();
                let realization =
                    ryeos_executor::execution::project_source::pinned_context_realization(
                        authority,
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let state = self.0.as_ref().clone();
                let checkout_id = format!("schedule-{thread_id}");
                let context = tokio::task::spawn_blocking(move || {
                    ryeos_executor::execution::project_source::resolve_pinned_snapshot_context(
                        &state,
                        &snapshot_hash,
                        original_path,
                        &checkout_id,
                        realization,
                    )
                })
                .await
                .context("scheduled pinned materialization task stopped")?
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                ryeos_app::execution_provenance::ExecutionProvenance::root_pushed_head(
                    context.original_path,
                    context.request_engine,
                    context.temp_dir.context(
                        "scheduled pinned project context lost its materialization lease",
                    )?,
                    context.pinned_materialization.context(
                        "scheduled pinned project context lost its materialization authority",
                    )?,
                    authority.clone(),
                )
            }
        }
    }
}

impl SchedulerContext for AppSchedulerContext {
    fn app_root(&self) -> &Path {
        &self.0.config.app_root
    }

    fn scheduler_db(&self) -> Arc<SchedulerDb> {
        self.0.scheduler_db.clone()
    }

    fn scheduler_runtime_gate(&self) -> Arc<tokio::sync::RwLock<()>> {
        self.0.scheduler_runtime_gate.clone()
    }

    fn schedule_trust_store(&self) -> &ryeos_engine::trust::TrustStore {
        &self.0.engine.node_trust_store
    }

    fn get_thread_status(&self, thread_id: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .threads
            .get_thread(thread_id)?
            .map(|thread| thread.status))
    }

    fn get_thread_result_outcome(&self, thread_id: &str) -> Result<Option<ThreadResultOutcome>> {
        let Some(record) = self.0.threads.get_thread_result(thread_id)? else {
            return Ok(None);
        };
        let Some(result) = record.result.as_ref() else {
            return Ok(Some(ThreadResultOutcome::Success));
        };
        Ok(Some(ryeos_scheduler::classify_result_payload(result)))
    }

    fn get_admitted_capsule_hash(&self, thread_id: &str) -> Result<Option<String>> {
        self.0.state_store.admitted_launch_capsule_hash(thread_id)
    }

    fn submit_cancel(&self, thread_id: &str) -> Result<()> {
        self.0
            .commands
            .submit(&ryeos_app::command_service::CommandSubmitParams {
                thread_id: thread_id.to_string(),
                command_type: "cancel".to_string(),
                requested_by: None,
                params: None,
            })?;
        let (_report, cancelled_roots) = ryeos_app::cascade::stop_thread_and_descendants(
            &self.0,
            thread_id,
            ryeos_app::cascade::CascadeMode::Graceful,
        )?;
        if !cancelled_roots.is_empty() {
            ryeos_executor::execution::launch::kick_launch_window_after_discard(&self.0);
        }
        for root in cancelled_roots {
            ryeos_executor::execution::launch::kick_follow_resume_if_ready(&self.0, &root);
        }
        Ok(())
    }

    async fn wait_for_recovery_execution_release(&self) -> bool {
        ryeos_app::recovery_execution_gate::wait_if_armed().await
    }

    async fn bind_scheduled_project_authority(
        &self,
        spec: &ScheduleSpecRecord,
        fire: &ryeos_engine::contracts::ScheduledFireContext,
        thread_id: &str,
    ) -> Result<ScheduledProjectBinding> {
        Self::validate_fire_spec(spec, fire)?;
        let handler_context =
            ryeos_app::scheduled_execution_authority::revalidate_scheduled_execution(
                &self.0,
                &spec.execution,
            )?;
        ryeos_api::routes::response_modes::execute_mode::preauthorize_execution_policy(
            &spec.execution.policy,
            &handler_context.scopes,
            &self.0,
        )?;

        match &spec.execution.policy.project {
            ProjectExecutionPolicy::Projectless => {
                let authority = ryeos_api::routes::response_modes::execute_mode::resolve_execution_project_authority(
                    &spec.execution.policy,
                    None,
                    None,
                    None,
                    self.0.threads.site_id(),
                    &self.0.isolation,
                    &handler_context.scopes,
                )?;
                Ok(ScheduledProjectBinding {
                    authority,
                    pending_publication: None,
                })
            }
            ProjectExecutionPolicy::LiveDirect { .. } => {
                let root = Self::project_root(spec)?;
                let display = root.display().to_string();
                let canonical = tokio::task::spawn_blocking(move || std::fs::canonicalize(&root))
                    .await
                    .context("scheduled live-root canonicalization task stopped")?
                    .with_context(|| format!("canonicalize scheduled project {display}"))?;
                let authority = ryeos_api::routes::response_modes::execute_mode::resolve_execution_project_authority(
                    &spec.execution.policy,
                    Some(&canonical),
                    None,
                    None,
                    self.0.threads.site_id(),
                    &self.0.isolation,
                    &handler_context.scopes,
                )?;
                Ok(ScheduledProjectBinding {
                    authority,
                    pending_publication: None,
                })
            }
            ProjectExecutionPolicy::Pinned { source, .. } => {
                let project_root = Self::project_root(spec)?;
                let project_source = match source {
                    PinnedSource::CurrentHead => ProjectSource::PushedHead,
                    PinnedSource::Snapshot { hash } => {
                        ProjectSource::Snapshot { hash: hash.clone() }
                    }
                    PinnedSource::CaptureLive { .. } => {
                        bail!("scheduled execution cannot bind capture_live")
                    }
                };
                let state = self.0.clone();
                let execution = spec.execution.clone();
                let checkout_id = format!("schedule-bind-{thread_id}");
                // Select, verify, and stage reachability under one CAS guard.
                // The read-only cache lease alone does not protect CAS when
                // HEAD moves before the asynchronous fire journal binding.
                // This does not reserve the operational COW workspace; the
                // signed realization is still applied exactly once at dispatch.
                ryeos_executor::execution::run_bounded_project_capture(move || {
                    let state_authority = state.state_store.pinned_state_authority()?;
                    let guard = state_authority.acquire_shared_guard()?;
                    state_authority.ensure_guard(&guard)?;
                    let context = ryeos_executor::execution::project_source::resolve_project_context(
                        &state,
                        &project_source,
                        &project_root,
                        execution.principal_id(),
                        &checkout_id,
                        Some(PinnedContextRealization::ReadOnly),
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    let authority = ryeos_api::routes::response_modes::execute_mode::resolve_execution_project_authority(
                        &execution.policy,
                        Some(&context.original_path),
                        context.snapshot_hash.as_deref(),
                        context.current_head_destination.as_ref(),
                        state.threads.site_id(),
                        &state.isolation,
                        &handler_context.scopes,
                    )?;
                    ryeos_api::routes::response_modes::execute_mode::authorize_terminal_publication(
                        &execution.policy,
                        &context.original_path,
                        execution.principal_id(),
                        &handler_context.scopes,
                        context.snapshot_hash.as_deref(),
                        &state,
                    )?;
                    let ExecutionProjectAuthority::PinnedGeneration {
                        base_snapshot_hash,
                        snapshot_hash,
                        ..
                    } = &authority
                    else {
                        bail!("scheduled pinned selection did not bind a generation");
                    };
                    let mut roots = state_authority
                        .require_recovery()?
                        .begin_staged_cas_roots_admitted(&guard, "scheduled-project-binding")?;
                    roots.protect_object_hash_admitted(&guard, base_snapshot_hash)?;
                    roots.protect_object_hash_admitted(&guard, snapshot_hash)?;
                    Ok(ScheduledProjectBinding {
                        authority,
                        pending_publication: Some(ryeos_state::PendingCasPublication::new(
                            state_authority,
                            roots,
                        )),
                    })
                })
                .await
            }
        }
    }

    async fn dispatch_scheduled_item(
        &self,
        spec: &ScheduleSpecRecord,
        fire: &ryeos_engine::contracts::ScheduledFireContext,
        thread_id: &str,
        project_authority: &ExecutionProjectAuthority,
    ) -> Result<ScheduledDispatchReceipt> {
        Self::validate_fire_spec(spec, fire)?;
        project_authority.validate()?;
        let handler_context =
            ryeos_app::scheduled_execution_authority::revalidate_scheduled_execution(
                &self.0,
                &spec.execution,
            )?;
        ryeos_api::routes::response_modes::execute_mode::preauthorize_execution_policy(
            &spec.execution.policy,
            &handler_context.scopes,
            &self.0,
        )?;
        let provenance = self
            .resolve_dispatch_provenance(spec, thread_id, project_authority)
            .await?;
        let params: serde_json::Value = serde_json::from_str(&spec.params)?;
        let original_root_kind = CanonicalRef::parse(&spec.item_ref)
            .with_context(|| format!("invalid scheduled item ref `{}`", spec.item_ref))?
            .kind;
        let current_site_id = self.0.threads.site_id().to_owned();
        let origin_site_id = handler_context.execution_origin(&current_site_id);
        handler_context.validate_execution_authority(
            spec.execution.principal_id(),
            &spec.execution.capabilities,
            &current_site_id,
            &origin_site_id,
        )?;
        let project_context = if matches!(
            spec.execution.policy.project,
            ProjectExecutionPolicy::Projectless
        ) {
            ProjectContext::None
        } else {
            ProjectContext::LocalPath {
                path: provenance.effective_path().to_path_buf(),
            }
        };
        let plan_ctx = PlanContext {
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: spec.execution.principal_id().to_owned(),
                scopes: spec.execution.capabilities.clone(),
            }),
            project_context,
            subject_resolution_authority: provenance.subject_resolution_authority(),
            current_site_id,
            origin_site_id,
            execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
            scheduled_fire: Some(fire.clone()),
            validate_only: false,
        };
        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: spec.execution.principal_id().to_owned(),
            caller_scopes: spec.execution.capabilities.clone(),
            engine: provenance.request_engine().clone(),
            plan_ctx,
            requested_call: None,
        };
        let project_binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
            &exec_ctx.engine,
            &exec_ctx.plan_ctx,
            &provenance,
        )?;
        let preflight = ryeos_executor::dispatch::preflight_root_dispatch(
            &spec.item_ref,
            &original_root_kind,
            &params,
            &spec.ref_bindings,
            &Vec::new(),
            None,
            None,
            &project_binding,
            &exec_ctx,
            &self.0,
            None,
        )?;
        if !preflight.class.produces_admitted_launch_capsule() {
            bail!(
                "scheduled item `{}` resolves to `{}` execution, which cannot publish an admitted launch capsule at the scheduled durable-handoff boundary",
                spec.item_ref,
                preflight.class.as_str(),
            );
        }
        let root_admission = preflight.root_admission.context(format!(
            "scheduled item `{}` has no verified root admission",
            spec.item_ref
        ))?;
        if !matches!(
            exec_ctx.plan_ctx.subject_resolution_authority,
            ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs
        ) {
            ryeos_executor::dispatch::admit_launch_contract(
                preflight.root_dispatch_evidence.applicability(),
                &root_admission,
                &spec.ref_bindings,
                &spec.execution.policy.lifecycle_authority(),
                &provenance,
                &exec_ctx,
                &self.0,
            )
            .await?;
        }

        let (handoff, receiver) = ryeos_executor::execution::launch::LaunchHandoff::channel();
        let item_ref = spec.item_ref.clone();
        let ref_bindings = spec.ref_bindings.clone();
        let acting_principal = spec.execution.principal_id().to_owned();
        let lifecycle_authority = spec.execution.policy.lifecycle_authority();
        let project_path = provenance.effective_path().to_path_buf();
        let state = self.0.as_ref().clone();
        let expected_thread_id = thread_id.to_owned();
        let dispatch_context = exec_ctx.clone();
        let root_dispatch_evidence = preflight.root_dispatch_evidence;
        let dispatch_task = tokio::spawn(async move {
            let request = ryeos_executor::dispatch::DispatchRequest {
                launch_mode: "wait",
                target_site_id: None,
                validate_only: false,
                params,
                ref_bindings,
                product_selections: Vec::new(),
                acting_principal: &acting_principal,
                project_path: &project_path,
                provenance,
                lifecycle_authority,
                launch_timings: None,
                original_root_kind: &original_root_kind,
                pre_minted_thread_id: Some(expected_thread_id),
                usage_subject: None,
                usage_subject_asserted_by: None,
                previous_thread_id: None,
                root_admission: Some(root_admission),
                root_dispatch_evidence: Some(root_dispatch_evidence),
                parent_execution_context: None,
                effect_authority: None,
            };
            ryeos_executor::dispatch::dispatch_with_launch_handoff(
                &item_ref,
                Some(handler_context),
                &request,
                &dispatch_context,
                &state,
                &handoff,
            )
            .await
        });
        let handed_thread_id = receiver
            .await
            .context("scheduled launch task ended before durable handoff")?
            .map_err(|failure| {
                anyhow::anyhow!(
                    "scheduled launch handoff failed ({}): {}",
                    failure.code,
                    failure.message
                )
            })?;
        if handed_thread_id != thread_id {
            bail!("scheduled launch handoff changed deterministic thread identity");
        }
        tokio::spawn(async move {
            match dispatch_task.await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(%error, "scheduled dispatch settled with error"),
                Err(error) => tracing::error!(%error, "scheduled dispatch task stopped"),
            }
        });

        let capsule_hash = self
            .0
            .state_store
            .admitted_launch_capsule_hash(thread_id)?
            .context("scheduled handoff has no admitted launch capsule")?;
        let capsule = self
            .0
            .state_store
            .admitted_launch_capsule(thread_id)?
            .context("scheduled handoff launch capsule is not readable")?;
        if &capsule.project_authority != project_authority
            || capsule.lifecycle_authority != spec.execution.policy.lifecycle_authority()
            || capsule.sealed_invocation.get("scheduled_fire") != Some(&serde_json::to_value(fire)?)
        {
            bail!("scheduled launch capsule disagrees with its bound fire authority");
        }
        Ok(ScheduledDispatchReceipt {
            thread_id: handed_thread_id,
            admitted_capsule_hash: capsule_hash,
        })
    }
}
