//! Selected scheduler runtime ownership and App lifecycle adapters.

mod project;
mod reset;
mod retention;
mod retirement_attempt;
mod retirement_journal;
mod terminal;

pub use reset::SchedulerHistoryReset;
pub use retention::SchedulerService;

/// Validate the bounded scheduler-retirement journal before startup opens the
/// scheduler database. An interrupted or unknown artifact blocks admission;
/// startup never treats an old role snapshot as current native authority.
pub fn validate_retirement_startup(
    runtime: &lillux::PinnedDirectory,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> anyhow::Result<usize> {
    retirement_journal::RetirementJournal::validate_admission(runtime, identity)
}

#[doc(hidden)]
pub fn validate_project_declarations(
    staging_root: &std::path::Path,
    project_path: &std::path::Path,
) -> anyhow::Result<usize> {
    project::validate_project_declarations(staging_root, project_path)
}
