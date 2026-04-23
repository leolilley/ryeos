//! Thread locator hints.
//!
//! Rebuildable lookup hints: thread_id → chain_root_id
//! Located at `.ai/state/locators/threads/<thread_id>.json`
//!
//! These are NOT trust anchors — they're just hints for thread → chain lookup.
//! Verification always goes through the signed chain head ref.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// A thread locator hint — maps a thread_id to its chain_root_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadLocator {
    pub chain_root_id: String,
}

impl ThreadLocator {
    /// Create a new locator.
    pub fn new(chain_root_id: String) -> Self {
        Self { chain_root_id }
    }

    /// Validate the locator.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.chain_root_id.is_empty() {
            anyhow::bail!("chain_root_id must not be empty");
        }
        Ok(())
    }
}

/// Write a thread locator hint to a file.
///
/// Locators are stored at `.ai/state/locators/threads/<thread_id>.json`
pub fn write_locator(
    locators_root: &Path,
    thread_id: &str,
    locator: &ThreadLocator,
) -> anyhow::Result<()> {
    locator.validate()?;

    let path = locators_root.join("threads").join(format!("{}.json", thread_id));

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create locator directory")?;
    }

    let json = serde_json::to_string(locator).context("failed to serialize locator")?;
    lillux::atomic_write(&path, json.as_bytes()).context("failed to write locator")?;

    Ok(())
}

/// Read a thread locator hint from a file.
///
/// Returns None if the locator doesn't exist.
pub fn read_locator(locators_root: &Path, thread_id: &str) -> anyhow::Result<Option<ThreadLocator>> {
    let path = locators_root.join("threads").join(format!("{}.json", thread_id));

    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&path).context("failed to read locator")?;
    let locator: ThreadLocator =
        serde_json::from_str(&content).context("failed to parse locator")?;
    locator.validate()?;

    Ok(Some(locator))
}

/// Delete a thread locator hint.
pub fn delete_locator(locators_root: &Path, thread_id: &str) -> anyhow::Result<()> {
    let path = locators_root.join("threads").join(format!("{}.json", thread_id));

    if path.exists() {
        fs::remove_file(&path).context("failed to delete locator")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_validation_passes() {
        let locator = ThreadLocator::new("T-root".to_string());
        assert!(locator.validate().is_ok());
    }

    #[test]
    fn locator_validation_rejects_empty_chain_root_id() {
        let locator = ThreadLocator::new(String::new());
        assert!(locator.validate().is_err());
    }

    #[test]
    fn write_and_read_locator() {
        let tempdir = tempfile::tempdir().unwrap();
        let locators_root = tempdir.path();

        let locator = ThreadLocator::new("T-root".to_string());
        write_locator(locators_root, "T-abc123", &locator).unwrap();

        let read_back = read_locator(locators_root, "T-abc123")
            .unwrap()
            .unwrap();
        assert_eq!(read_back.chain_root_id, "T-root");
    }

    #[test]
    fn read_missing_locator_returns_none() {
        let tempdir = tempfile::tempdir().unwrap();
        let locators_root = tempdir.path();

        let result = read_locator(locators_root, "T-missing").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn write_locator_creates_parent_dirs() {
        let tempdir = tempfile::tempdir().unwrap();
        let locators_root = tempdir.path();

        let locator = ThreadLocator::new("T-root".to_string());
        write_locator(locators_root, "T-abc123", &locator).unwrap();

        let path = locators_root.join("threads/T-abc123.json");
        assert!(path.exists());
    }

    #[test]
    fn delete_locator_succeeds() {
        let tempdir = tempfile::tempdir().unwrap();
        let locators_root = tempdir.path();

        let locator = ThreadLocator::new("T-root".to_string());
        write_locator(locators_root, "T-abc123", &locator).unwrap();

        delete_locator(locators_root, "T-abc123").unwrap();

        let path = locators_root.join("threads/T-abc123.json");
        assert!(!path.exists());
    }

    #[test]
    fn delete_missing_locator_succeeds() {
        let tempdir = tempfile::tempdir().unwrap();
        let locators_root = tempdir.path();

        // Should not error even if locator doesn't exist
        let result = delete_locator(locators_root, "T-missing");
        assert!(result.is_ok());
    }

    #[test]
    fn locator_serialization_roundtrip() {
        let locator = ThreadLocator::new("T-root".to_string());
        let json = serde_json::to_string(&locator).unwrap();
        let locator2: ThreadLocator = serde_json::from_str(&json).unwrap();
        assert_eq!(locator.chain_root_id, locator2.chain_root_id);
    }
}
