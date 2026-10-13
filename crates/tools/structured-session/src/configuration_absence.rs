//! Finite signed absence checks in the actual workload host filesystem.
//! This is a pre-bootstrap check under the existing trusted host/controller
//! boundary, not a snapshot or protection against later privileged mutation.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ryeos_state::objects::{
    ConfigurationAbsence, MAX_CONFIGURATION_ABSENCE_ANCESTORS as MAX_ANCESTORS,
    MAX_CONFIGURATION_ABSENCE_INSPECTIONS as MAX_INSPECTIONS,
    MAX_CONFIGURATION_ABSENCE_PATH_BYTES as MAX_PATH_BYTES,
};

fn canonical_root(root: &str) -> Result<&Path> {
    ensure!(
        root.starts_with('/')
            && root.len() <= MAX_PATH_BYTES
            && !root.chars().any(char::is_control)
            && (root == "/"
                || root[1..]
                    .split('/')
                    .all(|part| { !part.is_empty() && part != "." && part != ".." })),
        "configuration absence workspace must be a bounded canonical absolute path"
    );
    Ok(Path::new(root))
}

fn insert(paths: &mut BTreeSet<PathBuf>, path: PathBuf) -> Result<()> {
    ensure!(
        path.as_os_str().len() <= MAX_PATH_BYTES,
        "configuration absence expanded path exceeds its byte bound"
    );
    paths.insert(path);
    ensure!(
        paths.len() <= MAX_INSPECTIONS,
        "configuration absence expansion exceeds its inspection bound"
    );
    Ok(())
}

/// Pure expansion completes before the first filesystem inspection. Traversal
/// includes root and deliberately does not stop at repository markers.
fn expand(
    inventory: &ConfigurationAbsence,
    startup_workspace: &str,
    route_workspace: &str,
) -> Result<BTreeSet<PathBuf>> {
    inventory.validate()?;
    let mut paths = BTreeSet::new();
    for path in &inventory.absolute_paths {
        insert(&mut paths, PathBuf::from(path))?;
    }
    for (workspace, relative_paths) in [
        (startup_workspace, &inventory.startup_ancestor_paths),
        (route_workspace, &inventory.route_ancestor_paths),
    ] {
        if relative_paths.is_empty() {
            continue;
        }
        for (index, ancestor) in canonical_root(workspace)?.ancestors().enumerate() {
            ensure!(
                index < MAX_ANCESTORS,
                "configuration absence workspace exceeds its ancestry bound"
            );
            for relative in relative_paths {
                insert(&mut paths, ancestor.join(relative))?;
            }
        }
    }
    Ok(paths)
}

pub(super) fn require_absent(
    inventory: &ConfigurationAbsence,
    startup_workspace: &str,
    route_workspace: &str,
) -> Result<()> {
    let paths = expand(inventory, startup_workspace, route_workspace)?;
    for path in paths {
        let entry = lillux::inspect_optional_entry_no_follow(&path)
            .with_context(|| format!("inspect configuration absence at {}", path.display()))?;
        ensure!(
            entry.is_none(),
            "configuration absence requires an absent entry: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> ConfigurationAbsence {
        ConfigurationAbsence {
            absolute_paths: Vec::new(),
            startup_ancestor_paths: Vec::new(),
            route_ancestor_paths: Vec::new(),
        }
    }

    #[test]
    fn both_workspace_chains_include_root_and_deduplicate_overlap() {
        let mut inventory = empty();
        inventory.absolute_paths = vec!["/fixed/config".into()];
        inventory.startup_ancestor_paths = vec![".config/settings".into()];
        inventory.route_ancestor_paths = vec![".config/settings".into()];
        let paths = expand(&inventory, "/private/B/project", "/workspace").unwrap();
        assert_eq!(
            paths,
            BTreeSet::from([
                PathBuf::from("/fixed/config"),
                PathBuf::from("/private/B/project/.config/settings"),
                PathBuf::from("/private/B/.config/settings"),
                PathBuf::from("/private/.config/settings"),
                PathBuf::from("/workspace/.config/settings"),
                PathBuf::from("/.config/settings"),
            ])
        );
    }

    #[test]
    fn expansion_accepts_exact_limits_and_refuses_overflow_without_truncation() {
        let mut inventory = empty();
        inventory.startup_ancestor_paths = (0..16).map(|n| format!("config-{n:02}")).collect();
        let root = format!("/{}", vec!["d"; 63].join("/"));
        assert_eq!(expand(&inventory, &root, "/unused").unwrap().len(), 1024);
        assert!(expand(&inventory, &format!("{root}/extra"), "/unused").is_err());
        let wide_root = format!("/{}", "a".repeat(4090));
        assert!(expand(&inventory, &wide_root, "/unused").is_err());
        for root in ["relative", "/a/../b", "/a/./b", "/a//b", "/a/"] {
            assert!(expand(&inventory, root, "/unused").is_err(), "{root}");
        }
        inventory.route_ancestor_paths = vec!["one-more".into()];
        assert!(expand(&inventory, "/", "/").is_err());
        assert!(expand(&empty(), "unused", "unused").unwrap().is_empty());
    }

    #[test]
    fn missing_parent_is_absent_but_existing_files_and_directories_refuse() {
        let root = tempfile::tempdir().unwrap();
        let candidate = root.path().join("missing-parent/config");
        let mut inventory = empty();
        inventory.absolute_paths = vec![candidate.to_str().unwrap().into()];
        require_absent(&inventory, "/unused", "/unused").unwrap();
        std::fs::create_dir(candidate.parent().unwrap()).unwrap();
        std::fs::write(&candidate, b"must never be read").unwrap();
        assert!(require_absent(&inventory, "/unused", "/unused").is_err());
        std::fs::remove_file(&candidate).unwrap();
        std::fs::create_dir(&candidate).unwrap();
        assert!(require_absent(&inventory, "/unused", "/unused").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn links_unsafe_ancestors_and_fifo_refuse_without_reading_contents() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let root = tempfile::tempdir().unwrap();
        let candidate = root.path().join("config");
        let mut inventory = empty();
        inventory.absolute_paths = vec![candidate.to_str().unwrap().into()];
        symlink(root.path().join("absent-target"), &candidate).unwrap();
        assert!(require_absent(&inventory, "/unused", "/unused").is_err());
        std::fs::remove_file(&candidate).unwrap();
        let fifo = std::ffi::CString::new(candidate.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(require_absent(&inventory, "/unused", "/unused").is_err());
        std::fs::remove_file(&candidate).unwrap();
        let parent = root.path().join("parent");
        symlink(root.path().join("absent-parent"), &parent).unwrap();
        inventory.absolute_paths = vec![parent.join("config").to_str().unwrap().into()];
        assert!(require_absent(&inventory, "/unused", "/unused").is_err());
        std::fs::remove_file(&parent).unwrap();
        std::fs::write(&parent, b"not a directory").unwrap();
        assert!(require_absent(&inventory, "/unused", "/unused").is_err());
    }

    #[test]
    fn actual_route_and_startup_ancestors_are_checked_above_repository_markers() {
        let root = tempfile::tempdir().unwrap();
        let startup = root.path().join("controller/repo/project");
        let route = root.path().join("remote/workspace");
        std::fs::create_dir_all(&startup).unwrap();
        std::fs::create_dir_all(&route).unwrap();
        std::fs::create_dir(startup.parent().unwrap().join(".git")).unwrap();
        // Unique relative leaf avoids relying on ambient ancestors being empty.
        let leaf = format!(
            "absence-{}",
            root.path().file_name().unwrap().to_str().unwrap()
        );
        let mut inventory = empty();
        inventory.startup_ancestor_paths = vec![leaf.clone()];
        inventory.route_ancestor_paths = vec![leaf.clone()];
        let check = || {
            require_absent(
                &inventory,
                startup.to_str().unwrap(),
                route.to_str().unwrap(),
            )
        };
        check().unwrap();
        let startup_config = root.path().join("controller").join(&leaf);
        std::fs::write(&startup_config, b"private").unwrap();
        assert!(check().is_err());
        std::fs::remove_file(startup_config).unwrap();
        let route_config = route.parent().unwrap().join(&leaf);
        std::fs::write(route_config, b"private").unwrap();
        assert!(check().is_err());
    }
}
