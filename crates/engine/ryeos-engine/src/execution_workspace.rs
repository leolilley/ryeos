//! Canonical kind-neutral execution-workspace layout.
//!
//! The app owns the durable journal and RyeOS owns the canonical project
//! generation. Enforced isolation backends may additionally receive one opaque
//! state directory; only the selected signed backend may interpret its
//! contents.

use std::path::{Path, PathBuf};

use anyhow::Result;

pub const PROJECT_DIR: &str = "project";
pub const BACKEND_STATE_DIR: &str = "backend-state";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLayout {
    pub root: PathBuf,
    pub project: PathBuf,
    /// Opaque state granted only to an enforced signed isolation backend.
    /// Disabled/native execution does not create this directory.
    pub backend_state: PathBuf,
}

impl WorkspaceLayout {
    pub fn create(execution_root: &Path, workspace_id: &str) -> Result<Self> {
        validate_workspace_id(workspace_id)?;
        let execution_root = lillux::PinnedDirectory::open_or_create(execution_root)?;
        execution_root.set_mode(0o700)?;
        let workspace =
            execution_root.open_or_create_child(std::ffi::OsStr::new(workspace_id), 0o700)?;
        workspace.set_mode(0o700)?;
        let project = workspace.open_or_create_child(std::ffi::OsStr::new(PROJECT_DIR), 0o700)?;
        project.set_mode(0o700)?;
        for entry in workspace.entries_no_follow_bounded(3)? {
            if entry.entry_type != lillux::PinnedEntryType::Directory
                || !matches!(entry.name.to_str(), Some(PROJECT_DIR | BACKEND_STATE_DIR))
            {
                anyhow::bail!(
                    "execution workspace contains unexpected entry: {}",
                    workspace.path().join(entry.name).display()
                );
            }
        }
        Ok(Self::from_root(workspace.path().to_path_buf()))
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self {
            project: root.join(PROJECT_DIR),
            backend_state: root.join(BACKEND_STATE_DIR),
            root,
        }
    }

    pub fn from_project(project: &Path) -> Result<Self> {
        if project.file_name().and_then(|name| name.to_str()) != Some(PROJECT_DIR) {
            anyhow::bail!(
                "runtime project path is not a canonical workspace project: {}",
                project.display()
            );
        }
        let root = project
            .parent()
            .ok_or_else(|| anyhow::anyhow!("workspace project has no parent"))?
            .to_path_buf();
        Ok(Self::from_root(root))
    }
}

fn validate_workspace_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("invalid execution workspace id `{value}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_layout_separates_project_from_opaque_backend_state() {
        let root = PathBuf::from("/runtime/executions/workspace-one");
        let layout = WorkspaceLayout::from_root(root.clone());
        assert_eq!(layout.project, root.join("project"));
        assert_eq!(layout.backend_state, root.join("backend-state"));
    }

    #[test]
    fn native_create_has_only_the_project_generation() {
        let parent = tempfile::tempdir().unwrap();
        let created = WorkspaceLayout::create(parent.path(), "workspace-one").unwrap();
        let reopened = WorkspaceLayout::from_project(&created.project).unwrap();
        assert_eq!(created, reopened);
        assert!(!created.backend_state.exists());
    }
}
