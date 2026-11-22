//! Mechanical refusal of caller-named project roots that would expose the
//! node's own state.
//!
//! A project root a caller names must never be `/`, the node app root, an
//! ancestor of the app root, or anything inside the app root. Both paths must
//! already be canonical (symlinks resolved); the check is purely lexical on
//! path components.

use std::path::{Path, PathBuf};

/// Why a candidate project root was refused.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum NodeInternalProjectRoot {
    #[error("project root `{}` is the filesystem root", .0.display())]
    FilesystemRoot(PathBuf),
    #[error("project root `{}` contains the node app root", .0.display())]
    ContainsAppRoot(PathBuf),
    #[error("project root `{}` is inside the node app root", .0.display())]
    InsideAppRoot(PathBuf),
}

/// Refuse a canonical `candidate` that is `/`, the canonical `app_root`, an
/// ancestor of it, or inside it.
pub fn refuse_node_internal_project_root(
    candidate: &Path,
    app_root: &Path,
) -> Result<(), NodeInternalProjectRoot> {
    if candidate.parent().is_none() {
        return Err(NodeInternalProjectRoot::FilesystemRoot(candidate.into()));
    }
    if app_root.starts_with(candidate) {
        return Err(NodeInternalProjectRoot::ContainsAppRoot(candidate.into()));
    }
    if candidate.starts_with(app_root) {
        return Err(NodeInternalProjectRoot::InsideAppRoot(candidate.into()));
    }
    Ok(())
}

/// Canonicalize `app_root` and apply [`refuse_node_internal_project_root`].
/// `candidate` must already be canonical.
pub fn refuse_node_internal_project_root_for_app(
    candidate: &Path,
    app_root: &Path,
) -> Result<(), NodeInternalProjectRoot> {
    let app_root = std::fs::canonicalize(app_root).unwrap_or_else(|_| app_root.to_path_buf());
    refuse_node_internal_project_root(candidate, &app_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "/data/ryeos";

    fn check(candidate: &str) -> Result<(), NodeInternalProjectRoot> {
        refuse_node_internal_project_root(Path::new(candidate), Path::new(APP))
    }

    #[test]
    fn filesystem_root_is_refused() {
        let err = check("/").unwrap_err();
        assert!(err.to_string().contains("is the filesystem root"));
    }

    #[test]
    fn app_root_and_its_ancestors_are_refused() {
        for candidate in ["/data/ryeos", "/data", "/"] {
            assert!(check(candidate).is_err(), "{candidate}");
        }
        assert!(
            check("/data")
                .unwrap_err()
                .to_string()
                .contains("contains the node app root")
        );
        assert!(
            check("/data/ryeos")
                .unwrap_err()
                .to_string()
                .contains("contains the node app root")
        );
    }

    #[test]
    fn grandparent_is_refused() {
        let err =
            refuse_node_internal_project_root(Path::new("/srv"), Path::new("/srv/data/ryeos"))
                .unwrap_err();
        assert!(matches!(err, NodeInternalProjectRoot::ContainsAppRoot(_)));
    }

    #[test]
    fn inside_app_root_is_refused() {
        let err = check("/data/ryeos/.ai/state").unwrap_err();
        assert!(err.to_string().contains("is inside the node app root"));
    }

    #[test]
    fn sibling_sharing_a_name_prefix_is_allowed() {
        assert!(check("/data/ryeos-other").is_ok());
    }

    #[test]
    fn unrelated_project_is_allowed() {
        assert!(check("/home/user/project").is_ok());
    }
}
