//! Generic project-content authority consumed by engine subsystems.
//!
//! Discovery policy remains with the domain owner (trust, parser, config,
//! resolution). This interface only enumerates and reads exact
//! project-relative files from an already-admitted content authority.

use std::path::{Path, PathBuf};

use crate::error::EngineError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContentEntry {
    /// Path relative to the prefix passed to [`AuthoritativeProjectContent::list_files`].
    ///
    /// Domain owners join this back to their own prefix when reading the exact
    /// project-relative file. Keeping catalog-relative identity here matches
    /// live secure traversal without leaking a materialization pathname.
    pub relative_path: PathBuf,
    pub content_hash: String,
    pub size: u64,
    pub normalized_mode: u32,
}

pub trait AuthoritativeProjectContent {
    /// List regular files beneath the project-relative `prefix`, returning
    /// each entry relative to that prefix. The authority enforces `max_entries`
    /// while enumerating rather than collecting an unbounded intermediate
    /// result.
    fn list_files(
        &self,
        prefix: &Path,
        recursive: bool,
        max_entries: usize,
    ) -> Result<Vec<ProjectContentEntry>, EngineError>;

    /// Read one exact project-relative file, rejecting an oversized
    /// descriptor before allocating its body and verifying its content
    /// address before returning.
    fn read_file(
        &self,
        relative_path: &Path,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, EngineError>;

    /// Prove a whole-file digest observed during resolution against this
    /// authority's exact admitted tree.
    fn validates_file(&self, relative_path: &Path, content_hash: &str)
        -> Result<bool, EngineError>;

    /// Prove that one exact project-relative file is absent from the
    /// immutable tree.
    fn validates_absence(&self, relative_path: &Path) -> Result<bool, EngineError>;
}

impl AuthoritativeProjectContent for ryeos_state::PinnedProjectMaterialization {
    fn list_files(
        &self,
        prefix: &Path,
        recursive: bool,
        max_entries: usize,
    ) -> Result<Vec<ProjectContentEntry>, EngineError> {
        let prefix = prefix.to_str().ok_or_else(|| {
            EngineError::Internal(format!(
                "authoritative project prefix is not UTF-8: {}",
                prefix.display()
            ))
        })?;
        self.authoritative_entries_under(prefix, recursive, max_entries)
            .map_err(|error| EngineError::Internal(error.to_string()))?
            .into_iter()
            .map(|(relative_path, file)| {
                Ok(ProjectContentEntry {
                    relative_path: PathBuf::from(relative_path),
                    content_hash: file.blob_hash,
                    size: file.size,
                    normalized_mode: file.normalized_mode,
                })
            })
            .collect()
    }

    fn read_file(
        &self,
        relative_path: &Path,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let relative = relative_path.to_str().ok_or_else(|| {
            EngineError::Internal(format!(
                "authoritative project file is not UTF-8: {}",
                relative_path.display()
            ))
        })?;
        self.authoritative_file_bounded(relative, max_bytes)
            .map_err(|error| EngineError::Internal(error.to_string()))
    }

    fn validates_file(
        &self,
        relative_path: &Path,
        content_hash: &str,
    ) -> Result<bool, EngineError> {
        self.validates_observed_file(&self.path().join(relative_path), content_hash)
            .map_err(|error| EngineError::Internal(error.to_string()))
    }

    fn validates_absence(&self, relative_path: &Path) -> Result<bool, EngineError> {
        self.validates_observed_absence(&self.path().join(relative_path))
            .map_err(|error| EngineError::Internal(error.to_string()))
    }
}
