use std::path::Path;

use anyhow::Result;

pub(crate) fn validate_declarations_for_test(
    staging_root: &Path,
    project_path: &Path,
) -> Result<usize> {
    ryeos_scheduler_service::validate_project_declarations(staging_root, project_path)
}
