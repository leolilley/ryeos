//! Project root auto-discovery.
//!
//! Walks up from a starting directory looking for project markers.
//! Used by `remote execute` and other verbs that need to determine
//! the project root from the caller's working directory.

use std::io;
use std::path::{Path, PathBuf};

/// Walk up from `start`. At each level, in priority order, check for:
///   1. `.ai/`            (Rye space marker)
///   2. `.ryeos-project`  (explicit opt-in marker file; the answer
///                         for monorepo subpackages and non-git dirs)
///   3. `.git`            (entry — directory OR file; git worktrees
///                         and submodules use a `.git` file pointing
///                         at the real gitdir)
///
/// Returns `Ok(Some(root))` on first match, `Ok(None)` if none found
/// before the filebundle root, or `Err` on IO.
pub fn discover_project_root(start: &Path) -> io::Result<Option<PathBuf>> {
    let mut current = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()?.join(start)
    };

    // Canonicalize to resolve symlinks and verify existence.
    // If the path doesn't exist, we can't discover anything.
    current = match current.canonicalize() {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    loop {
        // Priority 1: .ai/ directory
        if current.join(".ai").is_dir() {
            return Ok(Some(current));
        }

        // Priority 2: .ryeos-project file (empty opt-in marker)
        if current.join(".ryeos-project").exists() {
            return Ok(Some(current));
        }

        // Priority 3: .git (file or directory — worktrees use a file)
        let git_path = current.join(".git");
        if git_path.exists() {
            return Ok(Some(current));
        }

        // Walk up
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return Ok(None), // reached filebundle root
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn finds_git_root_from_subdir() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let subdir = tmp.path().join("src").join("lib");
        std::fs::create_dir_all(&subdir).unwrap();

        let found = discover_project_root(&subdir).unwrap();
        assert_eq!(found, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn prefers_dot_ai_over_dot_git() {
        let tmp = TempDir::new().unwrap();
        // Both markers at same level — .ai wins
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".ai")).unwrap();

        let found = discover_project_root(tmp.path()).unwrap();
        assert_eq!(found, Some(tmp.path().to_path_buf()));

        // Now create an inner .ai in a subdirectory
        let inner = tmp.path().join("packages").join("myapp");
        std::fs::create_dir_all(inner.join(".ai")).unwrap();
        // outer still has .git, but inner .ai should be found first
        let found = discover_project_root(&inner).unwrap();
        assert_eq!(found, Some(inner.to_path_buf()));
    }

    #[test]
    fn finds_ryeos_project_marker() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".ryeos-project"), "").unwrap();
        let subdir = tmp.path().join("services").join("foo");
        std::fs::create_dir_all(&subdir).unwrap();

        let found = discover_project_root(&subdir).unwrap();
        assert_eq!(found, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn dot_ai_beats_ryeos_project_at_same_level() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".ai")).unwrap();
        std::fs::write(tmp.path().join(".ryeos-project"), "").unwrap();

        // .ai is checked first
        let found = discover_project_root(tmp.path()).unwrap();
        assert_eq!(found, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn dot_ryeos_project_beats_dot_git_at_same_level() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".ryeos-project"), "").unwrap();

        // .ryeos-project is checked before .git
        let found = discover_project_root(tmp.path()).unwrap();
        assert_eq!(found, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn git_file_counts_as_marker() {
        // Git worktrees use a .git file, not a directory
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: /some/where").unwrap();

        let found = discover_project_root(tmp.path()).unwrap();
        assert_eq!(found, Some(tmp.path().to_path_buf()));
    }

    #[test]
    fn returns_none_outside_any_project() {
        // Build an isolated dir tree under /tmp with NO markers
        // anywhere in the chain. We do this by creating two temp dirs:
        // the outer represents "above any project root", the inner is
        // the start path; neither has .ai/.ryeos-project/.git, and
        // because TempDir uses /tmp directly we know that path chain
        // is also free of markers in normal CI/dev environments.
        //
        // To make this deterministic regardless of the test runner's
        // own environment, we explicitly assert by canonicalizing the
        // chain and verifying no marker exists at any level up to /.
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("deep").join("nested").join("subdir");
        std::fs::create_dir_all(&nested).unwrap();

        // Pre-flight: verify no marker exists anywhere from `nested`
        // up to /. If the host happened to have one, skip the assertion
        // rather than fail spuriously.
        let mut probe = nested.canonicalize().unwrap();
        let mut host_has_marker = false;
        loop {
            if probe.join(".ai").is_dir()
                || probe.join(".ryeos-project").exists()
                || probe.join(".git").exists()
            {
                host_has_marker = true;
                break;
            }
            match probe.parent() {
                Some(p) => probe = p.to_path_buf(),
                None => break,
            }
        }
        if host_has_marker {
            eprintln!("skipping: host filesystem has a project marker in /tmp chain");
            return;
        }

        let found = discover_project_root(&nested).unwrap();
        assert!(
            found.is_none(),
            "expected None when no marker exists anywhere in chain, got {:?}",
            found
        );
    }

    #[test]
    fn nonexistent_start_returns_none() {
        let result = discover_project_root(Path::new("/this/path/does/not/exist")).unwrap();
        assert!(result.is_none(), "nonexistent path should yield None");
    }
}
