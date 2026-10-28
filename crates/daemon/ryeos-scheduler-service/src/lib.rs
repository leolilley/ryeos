//! Selected scheduler runtime ownership and App lifecycle adapters.

/// Owner token carried by the secure temporaries this crate publishes.
pub const SCHEDULER_TEMP_PURPOSE: lillux::TempPurpose = lillux::TempPurpose::new("scheduler");

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
/// Every guarded journal read runs inside the live native factory's scope, so
/// the projection leases that factory already holds answer for their owner.
pub fn validate_retirement_startup(
    factory: &lillux_sqlite::OriginalNativeFactory,
    runtime: &lillux::PinnedDirectory,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> anyhow::Result<usize> {
    retirement_journal::RetirementJournal::validate_admission_in(Some(factory), runtime, identity)
}

#[doc(hidden)]
pub fn validate_project_declarations(
    staging_root: &std::path::Path,
    project_path: &std::path::Path,
) -> anyhow::Result<usize> {
    project::validate_project_declarations(staging_root, project_path)
}

/// A fixed file identity for fixtures that never touch a real file.
#[cfg(test)]
pub(crate) fn test_file_id(device: u64, inode: u64) -> lillux::PinnedRegularFileIdentity {
    serde_json::from_value(serde_json::json!({"filesystem": {"device": device}, "inode": inode}))
        .unwrap()
}
