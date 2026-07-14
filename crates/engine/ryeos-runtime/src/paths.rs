use std::path::{Path, PathBuf};

pub const AI_DIR: &str = ".ai";

pub fn operator_hooks_path(app_root: &Path) -> PathBuf {
    app_root.join(AI_DIR).join("config/agent/hooks.yaml")
}

pub fn project_hooks_path(project_root: &Path) -> PathBuf {
    project_root.join(AI_DIR).join("config/agent/hooks.yaml")
}
