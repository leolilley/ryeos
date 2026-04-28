use std::path::PathBuf;

/// Derive the single EffectiveProjectRoot from explicit flag or cwd.
pub fn effective_project_root(explicit: Option<PathBuf>) -> Result<PathBuf, crate::error::CliError> {
    match explicit {
        Some(p) => Ok(p),
        None => std::env::current_dir()
            .map_err(|e| crate::error::CliError::Internal {
                detail: format!("failed to determine cwd: {e}"),
            }),
    }
}
