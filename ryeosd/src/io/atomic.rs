//! Atomic file writes.
//!
//! Provides `atomic_write` which writes through a temporary file, fsyncs,
//! and renames to the target path — same filesystem guaranteed.
//!
//! Used by `node_config::writer` for daemon-issued mutations to
//! `kind: node` items.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Write `content` to `target_path` atomically.
///
/// 1. Create a `.tmp` sibling of the target.
/// 2. Write content to the tmp file.
/// 3. `fsync` the tmp file (data durability).
/// 4. `fsync` the parent directory (directory entry durability).
/// 5. `rename` tmp → target (atomic on same filesystem).
pub fn atomic_write(target_path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir {}", parent.display()))?;
    }

    let tmp_path = target_path.with_extension("tmp~");
    {
        let mut file = File::create(&tmp_path)
            .with_context(|| format!("failed to create tmp file {}", tmp_path.display()))?;
        file.write_all(content)
            .with_context(|| format!("failed to write tmp file {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to fsync tmp file {}", tmp_path.display()))?;
    }

    // Fsync the parent directory to ensure the rename entry is durable.
    if let Some(parent) = target_path.parent() {
        if let Ok(dir_file) = File::open(parent) {
            let _ = dir_file.sync_all();
        }
    }

    std::fs::rename(&tmp_path, target_path)
        .with_context(|| format!("failed to rename {} → {}", tmp_path.display(), target_path.display()))?;

    Ok(())
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
