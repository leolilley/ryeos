use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Recursively collect regular bundle files in stable name order, rejecting
/// every symlink before the parser/signature phase can observe its target.
pub(super) fn collect_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(dir)
        .with_context(|| format!("failed to stat bundle path {}", dir.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("bundle scan encountered symlink at {}", dir.display());
    }
    if !file_type.is_dir() {
        return Ok(());
    }

    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .with_context(|| format!("failed to read bundle dir {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to stat bundle path {}", path.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            bail!("bundle scan encountered symlink at {}", path.display());
        }
        if file_type.is_dir() {
            collect_files_recursive(&path, out)?;
        } else if file_type.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Tool runtime support trees are implementation files, not signed authored
/// items. Other kind directories never receive this exclusion.
pub(super) fn is_runtime_support_file(kind_directory: &str, rel: &Path) -> bool {
    if kind_directory != "tools" {
        return false;
    }

    rel.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name) if name == "lib" || name == "__pycache__"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tool_support_directories_are_excluded() {
        assert!(is_runtime_support_file(
            "tools",
            Path::new("tools/example/lib/helper.py")
        ));
        assert!(is_runtime_support_file(
            "tools",
            Path::new("tools/example/__pycache__/helper.pyc")
        ));
        assert!(!is_runtime_support_file(
            "handlers",
            Path::new("handlers/example/lib/handler.yaml")
        ));
    }

    #[test]
    fn support_names_must_be_whole_path_components() {
        assert!(!is_runtime_support_file(
            "tools",
            Path::new("tools/example/library/helper.py")
        ));
        assert!(!is_runtime_support_file(
            "tools",
            Path::new("tools/example/my__pycache__/helper.py")
        ));
    }
}
