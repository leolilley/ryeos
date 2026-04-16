//! Writer epoch management.
//!
//! Epochs protect in-flight writes from GC sweep. A writer registers
//! an epoch before writing, and completes it when done. The sweep phase
//! uses the oldest active epoch time as a grace period cutoff.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// An active writer epoch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Epoch {
    pub epoch_id: String,
    pub node_id: String,
    pub root_hashes: Vec<String>,
    pub created_at: String,
}

fn inflight_dir(cas_root: &Path) -> PathBuf {
    cas_root.join("inflight")
}

/// Register a new writer epoch. Returns the epoch ID.
pub fn register_epoch(
    cas_root: &Path,
    node_id: &str,
    root_hashes: Vec<String>,
) -> Result<String> {
    let dir = inflight_dir(cas_root);
    fs::create_dir_all(&dir)?;

    let epoch_id = format!(
        "{}-{}-{}",
        node_id,
        chrono::Utc::now().timestamp_millis(),
        std::process::id()
    );

    let epoch = Epoch {
        epoch_id: epoch_id.clone(),
        node_id: node_id.to_string(),
        root_hashes,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    let path = dir.join(format!("{epoch_id}.json"));
    let data = serde_json::to_vec_pretty(&epoch)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &data)?;
    fs::rename(&tmp, &path)?;

    Ok(epoch_id)
}

/// Complete (delete) a writer epoch.
pub fn complete_epoch(cas_root: &Path, epoch_id: &str) -> Result<()> {
    let path = inflight_dir(cas_root).join(format!("{epoch_id}.json"));
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// List all active epochs.
pub fn list_active_epochs(cas_root: &Path) -> Result<Vec<Epoch>> {
    let dir = inflight_dir(cas_root);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut epochs = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match fs::read(&path) {
            Ok(data) => {
                if let Ok(epoch) = serde_json::from_slice::<Epoch>(&data) {
                    epochs.push(epoch);
                }
            }
            Err(_) => continue,
        }
    }

    Ok(epochs)
}

/// Clean up stale epochs older than `max_age_seconds`.
pub fn cleanup_stale_epochs(cas_root: &Path, max_age_seconds: u64) -> Result<usize> {
    let dir = inflight_dir(cas_root);
    if !dir.is_dir() {
        return Ok(0);
    }

    let now = SystemTime::now();
    let max_age = std::time::Duration::from_secs(max_age_seconds);
    let mut cleaned = 0;

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(metadata) = path.metadata() {
            if let Ok(modified) = metadata.modified() {
                if let Ok(age) = now.duration_since(modified) {
                    if age > max_age {
                        fs::remove_file(&path)?;
                        cleaned += 1;
                    }
                }
            }
        }
    }

    Ok(cleaned)
}

/// Get the modification time of the oldest active epoch file.
///
/// Used as the grace period cutoff for sweep — files newer than this
/// are not deleted (they may be part of an in-flight write).
pub fn oldest_epoch_time(cas_root: &Path) -> Result<SystemTime> {
    let dir = inflight_dir(cas_root);
    if !dir.is_dir() {
        return Ok(SystemTime::now());
    }

    let mut oldest = SystemTime::now();
    let mut found = false;

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(metadata) = path.metadata() {
            if let Ok(modified) = metadata.modified() {
                if modified < oldest {
                    oldest = modified;
                    found = true;
                }
            }
        }
    }

    if found {
        Ok(oldest)
    } else {
        Ok(SystemTime::now())
    }
}
