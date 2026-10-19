use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ryeos_app::state::AppState;
use ryeos_scheduler_contract::{
    ProjectScheduleCaller, ProjectScheduleDeployRequest, ProjectScheduleSession,
    ScheduleDeployReport,
};
use ryeos_state::objects::ProjectTree;

use crate::handler_context::HandlerContext;

#[cfg(all(test, feature = "scheduling"))]
pub mod schedules;

pub struct ProjectDeployContext<'a> {
    pub project_path: &'a Path,
    pub staging_root: &'a Path,
    pub tree: &'a ProjectTree,
    pub snapshot_hash: &'a str,
    pub project_key: &'a str,
    pub caller: &'a HandlerContext,
    pub state: &'a AppState,
}

#[derive(Debug, Default)]
pub struct ProjectDeployPlan {
    pub schedules: ScheduleDeployReport,
    pub has_schedule_actions: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectDeployReport {
    pub schedules: ScheduleDeployReport,
}

struct ProjectScheduleCallerAdapter {
    caller: HandlerContext,
    // Capture the only AppState-derived value needed for authority. Retained
    // sessions are owned by an AppState extension, so retaining AppState here
    // would create an extensions -> session -> AppState strong-reference cycle.
    site_id: String,
}

impl ProjectScheduleCaller for ProjectScheduleCallerAdapter {
    fn require_registration(&self) -> Result<()> {
        self.caller
            .require_verified()
            .map_err(anyhow::Error::from)?;
        require_scope(&self.caller, "ryeos.execute.service.scheduler/register")
    }

    fn require_owner(&self, schedule_id: &str, owner_fingerprint: &str) -> Result<()> {
        use crate::handler_error::HandlerError;
        self.caller
            .require_verified()
            .map_err(anyhow::Error::from)?;
        if self.caller.is_owner(Some(owner_fingerprint)) {
            return Ok(());
        }
        if !self.caller.is_present() {
            return Err(HandlerError::NotFound.into());
        }
        Err(HandlerError::Conflict(format!(
            "schedule '{schedule_id}' in this project is registered by a different principal; deregister it on the remote or run the sync as its owner"
        )).into())
    }

    fn require_capability(&self, capability: &str) -> Result<()> {
        self.caller
            .require_verified()
            .map_err(anyhow::Error::from)?;
        require_scope(&self.caller, capability)
    }

    fn authenticated_execution_authority(
        &self,
        registration_request_hash: String,
    ) -> Result<ryeos_scheduler_contract::ScheduleExecutionAuthority> {
        authenticated_schedule_authority_for_site(
            &self.caller,
            &self.site_id,
            registration_request_hash,
        )
    }
}

pub(crate) fn authenticated_schedule_authority_for_site(
    ctx: &crate::handler_context::HandlerContext,
    site_id: &str,
    registration_request_hash: String,
) -> Result<ryeos_scheduler_contract::ScheduleExecutionAuthority> {
    ctx.require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    let principal_class = ctx
        .authorized_key_class
        .context("schedule registration requires a RyeOS authorized-key principal class")?;
    let grant_authority = ctx.authenticated_grant_authority.clone().context(
        "schedule registration requires stable authorized-key grant-generation evidence",
    )?;
    let origin_site_id = ctx.execution_origin(site_id);
    Ok(
        ryeos_scheduler_contract::ScheduleExecutionAuthority::Authenticated {
            principal_id: ctx.fingerprint.clone(),
            principal_class,
            effective_origin_site_id: origin_site_id,
            grant_authority,
            registration_request_hash,
        },
    )
}

fn require_scope(caller: &HandlerContext, capability: &str) -> Result<()> {
    if caller.scopes.is_empty() {
        anyhow::bail!("verified scheduler caller has no capabilities");
    }
    ryeos_runtime::authorizer::Authorizer::new()
        .authorize(
            &caller.scopes,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require(capability),
        )
        .map_err(|_| anyhow::anyhow!("scheduler capability {capability:?} is not granted"))
}

pub async fn acquire_session(state: &AppState) -> Result<Box<dyn ProjectScheduleSession>> {
    if let Some(participant) = state.scheduler() {
        return participant
            .acquire_project_schedules()
            .await
            .context("acquire selected project schedule session");
    }
    Ok(Box::new(NoSchedulerProjectSession::default()))
}

pub fn try_acquire_session(state: &AppState) -> Result<Option<Box<dyn ProjectScheduleSession>>> {
    if let Some(participant) = state.scheduler() {
        return participant
            .try_acquire_project_schedules()
            .context("try to acquire selected project schedule session");
    }
    Ok(Some(Box::new(NoSchedulerProjectSession::default())))
}

/// In-process custody for project apply recovery. The key is the authenticated
/// job ID; a session retains the exact scheduler gate, directory lock,
/// transaction and before-images. A pending marker covers cancellation while
/// the first session acquisition is still waiting for the scheduler gate.
#[derive(Default)]
pub struct ProjectScheduleRecoveryOwners {
    owners: Mutex<HashMap<String, Vec<ProjectScheduleRecoveryOwner>>>,
}

pub(crate) enum ProjectScheduleRecoveryOwner {
    AcquiringSession,
    PendingSessionAcquisition,
    Session(Box<dyn ProjectScheduleSession>),
}

impl ProjectScheduleRecoveryOwners {
    pub(crate) fn reserve_before_acquisition(&self, job_id: String) {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        owners
            .entry(job_id)
            .or_default()
            .push(ProjectScheduleRecoveryOwner::AcquiringSession);
    }

    pub fn retain(&self, job_id: String, owner: Box<dyn ProjectScheduleSession>) {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        owners
            .entry(job_id)
            .or_default()
            .push(ProjectScheduleRecoveryOwner::Session(owner));
    }

    pub(crate) fn take(&self, job_id: &str) -> Option<ProjectScheduleRecoveryOwner> {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let retained = owners.get_mut(job_id)?.pop();
        if owners
            .get(job_id)
            .is_some_and(|retained| retained.is_empty())
        {
            owners.remove(job_id);
        }
        retained
    }

    pub(crate) fn release_pending(&self, job_id: &str) -> bool {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(retained) = owners.get_mut(job_id) else {
            return false;
        };
        if !matches!(
            retained.last(),
            Some(
                ProjectScheduleRecoveryOwner::PendingSessionAcquisition
                    | ProjectScheduleRecoveryOwner::AcquiringSession
            )
        ) {
            return false;
        }
        retained.pop();
        if retained.is_empty() {
            owners.remove(job_id);
        }
        true
    }

    pub(crate) fn mark_acquisition_pending(&self, job_id: &str) {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(owner) = owners.get_mut(job_id).and_then(|owners| owners.last_mut())
            && matches!(owner, ProjectScheduleRecoveryOwner::AcquiringSession)
        {
            *owner = ProjectScheduleRecoveryOwner::PendingSessionAcquisition;
        }
    }

    pub(crate) fn is_acquiring_session(&self, job_id: &str) -> bool {
        self.owners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(job_id)
            .and_then(|owners| owners.last())
            .is_some_and(|owner| matches!(owner, ProjectScheduleRecoveryOwner::AcquiringSession))
    }

    pub fn job_ids(&self) -> Vec<String> {
        let mut job_ids = self
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        job_ids.sort();
        job_ids
    }
}

pub fn recovery_owners(state: &AppState) -> Result<Arc<ProjectScheduleRecoveryOwners>> {
    state
        .extensions
        .get::<ProjectScheduleRecoveryOwners>()
        .ok_or_else(|| anyhow::anyhow!("project schedule recovery owner registry is unavailable"))
}

#[derive(Default)]
struct NoSchedulerProjectSession {
    report: ScheduleDeployReport,
    phase: NoSchedulerPhase,
}

#[derive(Default, PartialEq, Eq)]
enum NoSchedulerPhase {
    #[default]
    Acquired,
    Planned,
    RecoveryPrepared,
    Prepared,
    Resolved,
}

impl ProjectScheduleSession for NoSchedulerProjectSession {
    fn plan(&mut self, request: ProjectScheduleDeployRequest) -> Result<ScheduleDeployReport> {
        if self.phase != NoSchedulerPhase::Acquired {
            anyhow::bail!("schedule-free session cannot plan in its current phase");
        }
        let schedule_root = request.staging_root.join(".ai/config/schedules");
        match std::fs::symlink_metadata(&schedule_root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!("schedule declaration root is a symlink")
            }
            Ok(metadata) if metadata.file_type().is_dir() => {
                if std::fs::read_dir(&schedule_root)?
                    .next()
                    .transpose()?
                    .is_some()
                {
                    anyhow::bail!(
                        "this project declares schedules but the selected composition has no scheduler service"
                    );
                }
            }
            Ok(_) => anyhow::bail!("schedule declaration root is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.phase = NoSchedulerPhase::Planned;
        Ok(self.report.clone())
    }

    fn has_actions(&self) -> Result<bool> {
        if self.phase == NoSchedulerPhase::Acquired {
            anyhow::bail!("schedule-free session has not completed planning");
        }
        Ok(false)
    }

    fn prepare_recovery(&mut self) -> ryeos_scheduler_contract::ProjectScheduleFuture<'_, ()> {
        Box::pin(async move {
            if self.phase != NoSchedulerPhase::Planned {
                anyhow::bail!("schedule-free session has not completed planning");
            }
            self.phase = NoSchedulerPhase::RecoveryPrepared;
            Ok(())
        })
    }

    fn before_images(&self) -> Result<&[ryeos_scheduler_contract::ScheduleRecoveryBeforeImage]> {
        if self.phase != NoSchedulerPhase::RecoveryPrepared
            && self.phase != NoSchedulerPhase::Prepared
        {
            anyhow::bail!("schedule-free recovery images are not prepared");
        }
        Ok(&[])
    }

    fn prepare(&mut self) -> Result<ScheduleDeployReport> {
        if self.phase != NoSchedulerPhase::RecoveryPrepared {
            anyhow::bail!("schedule-free session cannot prepare in its current phase");
        }
        self.phase = NoSchedulerPhase::Prepared;
        Ok(self.report.clone())
    }

    fn finalize(&mut self) -> Result<()> {
        if self.phase == NoSchedulerPhase::Resolved {
            return Ok(());
        }
        if self.phase != NoSchedulerPhase::Prepared
            && self.phase != NoSchedulerPhase::RecoveryPrepared
        {
            anyhow::bail!("schedule-free session cannot finalize in its current phase");
        }
        self.phase = NoSchedulerPhase::Resolved;
        Ok(())
    }
    fn rollback(&mut self) -> Result<()> {
        if self.phase != NoSchedulerPhase::RecoveryPrepared
            && self.phase != NoSchedulerPhase::Prepared
        {
            anyhow::bail!("schedule-free session cannot rollback in its current phase");
        }
        self.phase = NoSchedulerPhase::Resolved;
        Ok(())
    }
    fn retain_for_recovery(&mut self) -> Result<()> {
        self.phase = NoSchedulerPhase::Resolved;
        Ok(())
    }

    fn prepare_restoration<'a>(
        &'a mut self,
        entries: &'a [ryeos_scheduler_contract::ScheduleRecoveryBeforeImage],
    ) -> ryeos_scheduler_contract::ProjectScheduleFuture<'a, ()> {
        Box::pin(async move {
            if !entries.is_empty() {
                anyhow::bail!("schedule recovery requires the selected scheduler service");
            }
            if self.phase == NoSchedulerPhase::Resolved {
                self.phase = NoSchedulerPhase::RecoveryPrepared;
                return Ok(());
            }
            if self.phase != NoSchedulerPhase::Acquired {
                anyhow::bail!(
                    "schedule-free session cannot prepare restoration in its current phase"
                );
            }
            self.phase = NoSchedulerPhase::RecoveryPrepared;
            Ok(())
        })
    }

    fn restore(&mut self) -> Result<()> {
        if self.phase == NoSchedulerPhase::Resolved {
            return Ok(());
        }
        if self.phase != NoSchedulerPhase::RecoveryPrepared {
            anyhow::bail!("schedule-free session cannot restore in its current phase");
        }
        self.phase = NoSchedulerPhase::Resolved;
        Ok(())
    }
}

pub fn plan(
    ctx: &ProjectDeployContext<'_>,
    session: &mut dyn ProjectScheduleSession,
) -> Result<ProjectDeployPlan> {
    let _ = ctx.tree;
    let request = ProjectScheduleDeployRequest {
        staging_root: ctx.staging_root.to_path_buf(),
        project_path: ctx.project_path.to_path_buf(),
        project_key: ctx.project_key.to_owned(),
        snapshot_hash: ctx.snapshot_hash.to_owned(),
        caller: Arc::new(ProjectScheduleCallerAdapter {
            caller: ctx.caller.clone(),
            site_id: ctx.state.threads.site_id().to_owned(),
        }),
    };
    let schedules = session.plan(request)?;
    Ok(ProjectDeployPlan {
        schedules,
        has_schedule_actions: session.has_actions()?,
    })
}

pub fn prepare_recovery<'a>(
    session: &'a mut dyn ProjectScheduleSession,
) -> ryeos_scheduler_contract::ProjectScheduleFuture<'a, ()> {
    session.prepare_recovery()
}

pub fn schedule_before_images(
    session: &dyn ProjectScheduleSession,
) -> Result<Vec<ryeos_scheduler_contract::ScheduleRecoveryBeforeImage>> {
    Ok(session.before_images()?.to_vec())
}

pub fn prepare_commit_with_recovery<'a>(
    plan: &ProjectDeployPlan,
    session: &'a mut dyn ProjectScheduleSession,
) -> Result<PreparedProjectDeploy<'a>> {
    let schedules = session.prepare()?;
    if plan.has_schedule_actions != (schedules.created + schedules.updated + schedules.deleted > 0)
    {
        anyhow::bail!("project schedule actions changed between plan and prepare");
    }
    Ok(PreparedProjectDeploy {
        session,
        report: ProjectDeployReport { schedules },
        resolved: false,
    })
}

pub struct PreparedProjectDeploy<'a> {
    session: &'a mut dyn ProjectScheduleSession,
    pub report: ProjectDeployReport,
    resolved: bool,
}

impl PreparedProjectDeploy<'_> {
    pub fn finalize(&mut self) -> Result<()> {
        self.session.finalize()?;
        self.resolved = true;
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<()> {
        self.session.rollback()?;
        self.resolved = true;
        Ok(())
    }

    pub fn retain_for_recovery(&mut self) -> Result<()> {
        self.session.retain_for_recovery()?;
        self.resolved = true;
        Ok(())
    }
}

impl Drop for PreparedProjectDeploy<'_> {
    fn drop(&mut self) {
        if !self.resolved {
            if let Err(error) = self.session.retain_for_recovery() {
                tracing::error!(%error, "unresolved project schedule session could not retain recovery");
            }
            self.resolved = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SessionProbe {
        calls: Arc<AtomicUsize>,
        fail_prepare: bool,
    }

    impl ProjectScheduleSession for SessionProbe {
        fn plan(&mut self, _: ProjectScheduleDeployRequest) -> Result<ScheduleDeployReport> {
            unreachable!("probe is used after plan")
        }

        fn has_actions(&self) -> Result<bool> {
            unreachable!("probe is used after plan")
        }

        fn prepare_recovery(&mut self) -> ryeos_scheduler_contract::ProjectScheduleFuture<'_, ()> {
            unreachable!("probe is used after plan")
        }

        fn before_images(
            &self,
        ) -> Result<&[ryeos_scheduler_contract::ScheduleRecoveryBeforeImage]> {
            unreachable!("probe is used after plan")
        }

        fn prepare(&mut self) -> Result<ScheduleDeployReport> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_prepare {
                anyhow::bail!("synthetic prepare failure after session ownership is established");
            }
            Ok(ScheduleDeployReport::default())
        }

        fn finalize(&mut self) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn rollback(&mut self) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn retain_for_recovery(&mut self) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn prepare_restoration<'a>(
            &'a mut self,
            _: &'a [ryeos_scheduler_contract::ScheduleRecoveryBeforeImage],
        ) -> ryeos_scheduler_contract::ProjectScheduleFuture<'a, ()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn restore(&mut self) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn prepare_failure_keeps_the_original_session_for_recovery() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut session = SessionProbe {
            calls: calls.clone(),
            fail_prepare: true,
        };
        assert!(prepare_commit_with_recovery(&ProjectDeployPlan::default(), &mut session).is_err());
        session.prepare_restoration(&[]).await.unwrap();
        session.restore().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn dropping_unresolved_prepared_deploy_retains_recovery() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut session = SessionProbe {
            calls: calls.clone(),
            fail_prepare: false,
        };
        let prepared =
            prepare_commit_with_recovery(&ProjectDeployPlan::default(), &mut session).unwrap();
        drop(prepared);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn recovery_owner_registry_reclaims_the_same_operation_session() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = ProjectScheduleRecoveryOwners::default();
        registry.retain(
            "project-snapshot-apply:operation-1".to_owned(),
            Box::new(SessionProbe {
                calls: calls.clone(),
                fail_prepare: false,
            }),
        );
        assert_eq!(
            registry.job_ids(),
            vec!["project-snapshot-apply:operation-1".to_owned()]
        );
        let mut session = match registry
            .take("project-snapshot-apply:operation-1")
            .expect("exact operation owner is retained")
        {
            ProjectScheduleRecoveryOwner::Session(session) => session,
            ProjectScheduleRecoveryOwner::AcquiringSession => {
                panic!("expected a completed session acquisition")
            }
            ProjectScheduleRecoveryOwner::PendingSessionAcquisition => {
                panic!("expected the retained session owner")
            }
        };
        session.prepare_restoration(&[]).await.unwrap();
        session.restore().unwrap();
        assert!(registry.job_ids().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A caller whose grants are never consulted: omission decides before any
    /// authority question is asked.
    struct UnconsultedCaller;

    impl ProjectScheduleCaller for UnconsultedCaller {
        fn require_registration(&self) -> Result<()> {
            unreachable!("omission never asks for registration authority")
        }

        fn require_owner(&self, _: &str, _: &str) -> Result<()> {
            unreachable!("omission never asks for ownership")
        }

        fn require_capability(&self, _: &str) -> Result<()> {
            unreachable!("omission never asks for capabilities")
        }

        fn authenticated_execution_authority(
            &self,
            _: String,
        ) -> Result<ryeos_scheduler_contract::ScheduleExecutionAuthority> {
            unreachable!("omission never mints execution authority")
        }
    }

    fn omission_request(staging: &Path) -> ProjectScheduleDeployRequest {
        ProjectScheduleDeployRequest {
            staging_root: staging.to_path_buf(),
            project_path: staging.to_path_buf(),
            project_key: "0".repeat(64),
            snapshot_hash: "1".repeat(64),
            caller: Arc::new(UnconsultedCaller),
        }
    }

    #[tokio::test]
    async fn core_without_scheduler_refuses_a_deploy_declaring_schedules() {
        let (_root, store_owner, state) = shared_test_state::build_test_state();
        assert!(
            state.scheduler().is_none(),
            "Core composes no scheduler participant"
        );
        let staging = tempfile::TempDir::new().unwrap();
        let declarations = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&declarations).unwrap();
        std::fs::write(
            declarations.join("project.yaml"),
            r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: project-alpha
    item_ref: graph:project/project-alpha
    ref_bindings: {}
    schedule_type: interval
    expression: "60"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    capabilities:
      - ryeos.execute.graph.project/project-alpha
    execution_policy:
      schema_version: 2
      ownership: daemon_owned
      recovery: restart_recoverable
      response: accepted
      target:
        kind: here
      environment:
        kind: none
      project:
        kind: live_direct
        access: read_write
        child_policy:
          kind: inherit
    params: {}
"#,
        )
        .unwrap();

        let mut session = acquire_session(&state).await.unwrap();
        let error = session
            .plan(omission_request(staging.path()))
            .expect_err("declared schedules need the scheduler service");
        assert!(
            format!("{error:#}").contains("has no scheduler service"),
            "got: {error:#}"
        );
        assert!(
            session.has_actions().is_err(),
            "a refused plan never reaches preparation"
        );
        assert!(session.prepare().is_err());
        drop(session);

        // The non-blocking probe used by recovery selects the same omission.
        let mut session = try_acquire_session(&state).unwrap().unwrap();
        assert!(session.plan(omission_request(staging.path())).is_err());
        drop(session);
        drop(state);
        store_owner.close().unwrap();
    }

    #[tokio::test]
    async fn core_without_scheduler_accepts_a_deploy_declaring_none() {
        let (_root, store_owner, state) = shared_test_state::build_test_state();
        assert!(
            state.scheduler().is_none(),
            "Core composes no scheduler participant"
        );
        // Both shapes of "no schedules": no declaration root, or an empty one.
        let absent = tempfile::TempDir::new().unwrap();
        let empty = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(empty.path().join(".ai/config/schedules")).unwrap();

        for staging in [absent.path(), empty.path()] {
            let mut session = acquire_session(&state).await.unwrap();
            let report = session.plan(omission_request(staging)).unwrap();
            assert_eq!(
                (
                    report.declared,
                    report.created,
                    report.updated,
                    report.deleted
                ),
                (0, 0, 0, 0)
            );
            assert!(!session.has_actions().unwrap());
            session.prepare_recovery().await.unwrap();
            assert!(session.before_images().unwrap().is_empty());
            let plan = ProjectDeployPlan {
                schedules: report,
                has_schedule_actions: false,
            };
            let mut prepared = prepare_commit_with_recovery(&plan, session.as_mut()).unwrap();
            prepared.finalize().unwrap();
            drop(prepared);
            // Restart recovery of a schedule-free journal resolves without a
            // scheduler; a journal naming schedules cannot be restored here.
            session.prepare_restoration(&[]).await.unwrap();
            session.restore().unwrap();
        }

        let mut session = acquire_session(&state).await.unwrap();
        let images = [ryeos_scheduler_contract::ScheduleRecoveryBeforeImage {
            schedule_id: "project-alpha".to_owned(),
            signed_yaml: None,
        }];
        assert!(session.prepare_restoration(&images).await.is_err());
        drop(session);
        drop(state);
        store_owner.close().unwrap();
    }

    #[test]
    fn schedule_free_session_refuses_a_non_directory_declaration_root() {
        let staging = tempfile::TempDir::new().unwrap();
        let config = staging.path().join(".ai/config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("schedules"), "not a directory\n").unwrap();
        let mut session = NoSchedulerProjectSession::default();
        assert!(session.plan(omission_request(staging.path())).is_err());
    }

    mod shared_test_state {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test_state.rs"));
    }
}
