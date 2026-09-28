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

/// Canonical journal/layout coordinate, separate from host component safety.
pub fn validate_workspace_id(value: &str) -> Result<()> {
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
    fn workspace_ids_are_bounded_canonical_coordinates() {
        validate_workspace_id("W-native_workspace-01").unwrap();
        validate_workspace_id(&"x".repeat(160)).unwrap();
        for invalid in [
            "",
            ".",
            "..",
            "a/b",
            "a.b",
            "with space",
            "雪",
            &"x".repeat(161),
        ] {
            assert!(validate_workspace_id(invalid).is_err(), "{invalid}");
        }
    }
}
