//! Materialization cache for CAS checkout.
//!
//! Maintains a cache directory where materialized project snapshots
//! are stored. Skips re-materialization if the cache already has the
//! matching manifest hash.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;

/// Materialization cache backed by a directory on disk.
///
/// Layout: `{cache_root}/{manifest_hash}/` contains the materialized files.
pub struct MaterializationCache {
    cache_root: PathBuf,
}

impl MaterializationCache {
    pub fn new(cache_root: PathBuf) -> Self {
        Self { cache_root }
    }

    /// Check if a manifest hash is already cached.
    pub fn has(&self, manifest_hash: &str) -> bool {
        self.cache_dir(manifest_hash).is_dir()
    }

    /// Get the cache directory for a manifest hash.
    pub fn cache_dir(&self, manifest_hash: &str) -> PathBuf {
        self.cache_root.join(manifest_hash)
    }

    /// Record that a manifest has been materialized to a directory.
    ///
    /// Creates a marker file so we know the cache entry is complete.
    pub fn mark_complete(&self, manifest_hash: &str) -> Result<()> {
        let dir = self.cache_dir(manifest_hash);
        fs::create_dir_all(&dir)?;
        let marker = dir.join(".materialized");
        fs::write(&marker, manifest_hash.as_bytes())?;
        Ok(())
    }

    /// Check if a cache entry is fully materialized (not partial).
    pub fn is_complete(&self, manifest_hash: &str) -> bool {
        self.cache_dir(manifest_hash)
            .join(".materialized")
            .is_file()
    }

    /// Evict a cache entry.
    pub fn evict(&self, manifest_hash: &str) -> Result<bool> {
        let dir = self.cache_dir(manifest_hash);
        if dir.is_dir() {
            fs::remove_dir_all(&dir)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all cached manifest hashes.
    pub fn list(&self) -> Result<Vec<String>> {
        if !self.cache_root.is_dir() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.cache_root)? {
            let entry = entry?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if !name.contains(".staging.") {
                        entries.push(name.to_string());
                    }
                }
            }
        }
        Ok(entries)
    }
}
