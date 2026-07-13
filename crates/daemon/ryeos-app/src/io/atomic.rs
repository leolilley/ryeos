//! Atomic file writes.
//!
//! Provides `atomic_write` which writes through a temporary file, fsyncs,
//! and renames to the target path — same filesystem guaranteed.
//!
//! Used by `node_config::writer` for daemon-issued mutations to
//! `kind: node` items.

use std::path::Path;

use anyhow::{Context, Result};

/// Write `content` to `target_path` atomically.
///
/// 1. Create a unique temporary sibling of the target.
/// 2. Write content to the tmp file.
/// 3. `fsync` the tmp file (data durability).
/// 4. `rename` tmp → target (atomic on same filesystem).
/// 5. `fsync` the parent directory (directory entry durability).
pub fn atomic_write(target_path: &Path, content: &[u8]) -> Result<()> {
    lillux::atomic_write(target_path, content)
        .with_context(|| format!("failed to atomically write {}", target_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_creates_file_with_content() {
        let tmpdir = TempDir::new().unwrap();
        let target = tmpdir.path().join("output.txt");

        atomic_write(&target, b"hello world").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world");
        // No tmp file left behind
        assert!(!target.with_extension("tmp~").exists());
        assert!(std::fs::read_dir(tmpdir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("tmp~")
        }));
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let tmpdir = TempDir::new().unwrap();
        let target = tmpdir.path().join("output.txt");
        std::fs::write(&target, b"old").unwrap();

        atomic_write(&target, b"new").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let tmpdir = TempDir::new().unwrap();
        let target = tmpdir.path().join("a").join("b").join("output.txt");

        atomic_write(&target, b"nested").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "nested");
    }
}
