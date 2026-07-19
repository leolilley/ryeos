use std::path::Path;

use anyhow::Result;
use ryeos_app::state::AppState;
use ryeos_state::objects::ProjectTree;

use crate::handler_context::HandlerContext;

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
    pub schedules: schedules::ScheduleDeployPlan,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectDeployReport {
    pub schedules: schedules::ScheduleDeployReport,
}

pub fn plan(ctx: &ProjectDeployContext<'_>) -> Result<ProjectDeployPlan> {
    let _ = ctx.tree;
    Ok(ProjectDeployPlan {
        schedules: schedules::plan(ctx)?,
    })
}

pub fn commit(
    plan: &ProjectDeployPlan,
    ctx: &ProjectDeployContext<'_>,
) -> Result<ProjectDeployReport> {
    let mut prepared = prepare_commit(plan, ctx)?;
    let report = prepared.report.clone();
    prepared.finalize(ctx);
    Ok(report)
}

pub struct PreparedProjectDeploy {
    schedules: schedules::PreparedScheduleDeploy,
    pub report: ProjectDeployReport,
    finalized: bool,
}

impl PreparedProjectDeploy {
    pub fn finalize(&mut self, ctx: &ProjectDeployContext<'_>) {
        self.schedules.finalize(ctx);
        self.finalized = true;
    }

    pub fn rollback(&mut self, ctx: &ProjectDeployContext<'_>) {
        self.schedules.rollback(ctx);
        self.finalized = true;
    }
}

impl Drop for PreparedProjectDeploy {
    fn drop(&mut self) {
        // Callers must explicitly finalize or rollback so rollback has access
        // to the deploy context/state. Dropping without either is a bug, but
        // there is no context available here for safe restoration.
        debug_assert!(
            self.finalized,
            "prepared project deploy dropped unfinalized"
        );
    }
}

pub fn prepare_commit(
    plan: &ProjectDeployPlan,
    ctx: &ProjectDeployContext<'_>,
) -> Result<PreparedProjectDeploy> {
    let schedules = schedules::prepare_commit(&plan.schedules, ctx)?;
    let report = ProjectDeployReport {
        schedules: schedules.report.clone(),
    };
    Ok(PreparedProjectDeploy {
        schedules,
        report,
        finalized: false,
    })
}
