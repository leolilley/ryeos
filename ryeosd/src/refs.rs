use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Serialize;

/// Acquire an exclusive advisory lock on a per-project lock file.
/// Returns the lock file handle — lock is released when dropped.
fn acquire_project_lock(ref_dir: &Path) -> Result<fs::File> {
    let lock_path = ref_dir.join("ref.lock");
    fs::create_dir_all(ref_dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            bail!("flock failed: {}", std::io::Error::last_os_error());
        }
    }
    Ok(file)
}

fn project_path_hash(project_path: &str) -> String {
    crate::cas::sha256_hex(project_path.as_bytes())
}

pub struct RefStore {
    cas_root: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct ProjectRef {
    pub snapshot_hash: String,
    pub project_path: String,
}

#[derive(Debug, Serialize)]
pub struct UserSpaceRef {
    pub user_manifest_hash: String,
    pub revision: i64,
    pub pushed_at: Option<String>,
}

impl RefStore {
    pub fn new(cas_root: PathBuf) -> Self {
        Self { cas_root }
    }

    // ── Generic refs ────────────────────────────────────────────────

    fn generic_ref_path(&self, ref_path: &str) -> PathBuf {
        self.cas_root.join("refs").join("generic").join(ref_path)
    }

    fn pin_path(&self, name: &str) -> PathBuf {
        self.cas_root.join("refs").join("pins").join(name)
    }

    /// Write a generic ref — atomic JSON write of `{ "hash": "<hash>" }`.
    pub fn write_ref(&self, ref_path: &str, hash: &str) -> Result<()> {
        anyhow::ensure!(lillux::cas::valid_hash(hash), "invalid hash: {hash}");
        let path = self.generic_ref_path(ref_path);
        let data = serde_json::json!({ "hash": hash });
        crate::cas::atomic_write(&path, serde_json::to_vec(&data)?.as_slice())
    }

    /// Read a generic ref — returns the hash if the ref exists.
    pub fn read_ref(&self, ref_path: &str) -> Result<Option<String>> {
        let path = self.generic_ref_path(ref_path);
        if !path.exists() {
            return Ok(None);
        }
        let data: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        Ok(data.get("hash").and_then(|v| v.as_str()).map(String::from))
    }

    /// Write a pin ref for GC roots.
    pub fn write_pin(&self, name: &str, hash: &str) -> Result<()> {
        anyhow::ensure!(lillux::cas::valid_hash(hash), "invalid hash: {hash}");
        let path = self.pin_path(name);
        let data = serde_json::json!({ "hash": hash });
        crate::cas::atomic_write(&path, serde_json::to_vec(&data)?.as_slice())
    }

    /// Read a pin ref.
    pub fn read_pin(&self, name: &str) -> Result<Option<String>> {
        let path = self.pin_path(name);
        if !path.exists() {
            return Ok(None);
        }
        let data: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        Ok(data.get("hash").and_then(|v| v.as_str()).map(String::from))
    }

    /// Delete a pin ref. Returns true if removed, false if not found.
    pub fn delete_pin(&self, name: &str) -> Result<bool> {
        let path = self.pin_path(name);
        if path.exists() {
            fs::remove_file(&path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all pin names.
    pub fn list_pins(&self) -> Result<Vec<String>> {
        let pins_dir = self.cas_root.join("refs").join("pins");
        if !pins_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&pins_dir)? {
            let entry = entry?;
            if entry.path().is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Collect all GC root hashes from every ref source.
    ///
    /// Centralizes root enumeration so new ref types cannot silently
    /// escape GC. Currently scans: project heads, user-space heads,
    /// pin refs, and generic refs.
    pub fn gc_root_hashes(&self) -> Result<Vec<String>> {
        let mut roots = HashSet::new();

        // Project heads
        for principal_entry in read_dir_safe(&self.cas_root)? {
            let projects_dir = principal_entry.join("refs").join("projects");
            if !projects_dir.is_dir() {
                continue;
            }
            for project_entry in read_dir_safe(&projects_dir)? {
                let head = project_entry.join("head");
                if head.is_file() {
                    if let Ok(hash) = fs::read_to_string(&head) {
                        let hash = hash.trim().to_string();
                        if !hash.is_empty() {
                            roots.insert(hash);
                        }
                    }
                }
            }
        }

        // User-space heads
        for principal_entry in read_dir_safe(&self.cas_root)? {
            let head = principal_entry.join("refs").join("user-space").join("head");
            if head.is_file() {
                if let Ok(hash) = fs::read_to_string(&head) {
                    let hash = hash.trim().to_string();
                    if !hash.is_empty() {
                        roots.insert(hash);
                    }
                }
            }
        }

        // Pin refs
        if let Ok(pins) = self.list_pins() {
            for name in &pins {
                if let Ok(Some(hash)) = self.read_pin(name) {
                    roots.insert(hash);
                }
            }
        }

        // Generic refs (recursively walks subdirectories)
        let generic_dir = self.cas_root.join("refs").join("generic");
        if generic_dir.is_dir() {
            collect_generic_ref_hashes(&generic_dir, &mut roots)?;
        }

        Ok(roots.into_iter().collect())
    }

    // ── Project / user-space refs ───────────────────────────────────

    fn project_ref_dir(&self, user_fp: &str, project_path: &str) -> PathBuf {
        self.cas_root
            .join(user_fp)
            .join("refs")
            .join("projects")
            .join(project_path_hash(project_path))
    }

    fn user_space_ref_dir(&self, user_fp: &str) -> PathBuf {
        self.cas_root.join(user_fp).join("refs").join("user-space")
    }

    pub fn resolve_project_ref(
        &self,
        user_fp: &str,
        project_path: &str,
    ) -> Result<Option<ProjectRef>> {
        let ref_dir = self.project_ref_dir(user_fp, project_path);
        let head_file = ref_dir.join("head");
        if !head_file.exists() {
            return Ok(None);
        }
        let snapshot_hash = fs::read_to_string(&head_file)?.trim().to_string();
        let mut project_path_value = project_path.to_string();
        let meta_file = ref_dir.join("meta.json");
        if meta_file.exists() {
            let meta: serde_json::Value = serde_json::from_slice(&fs::read(&meta_file)?)?;
            if let Some(pp) = meta.get("project_path").and_then(|v| v.as_str()) {
                project_path_value = pp.to_string();
            }
        }
        Ok(Some(ProjectRef {
            snapshot_hash,
            project_path: project_path_value,
        }))
    }

    pub fn advance_project_ref(
        &self,
        user_fp: &str,
        project_path: &str,
        new_snapshot_hash: &str,
        expected_snapshot_hash: Option<&str>,
    ) -> Result<bool> {
        let ref_dir = self.project_ref_dir(user_fp, project_path);
        let _lock = acquire_project_lock(&ref_dir)?;
        let head_file = ref_dir.join("head");

        let current = if head_file.exists() {
            Some(fs::read_to_string(&head_file)?.trim().to_string())
        } else {
            None
        };

        match expected_snapshot_hash {
            None => {
                if current.is_some() {
                    bail!("project ref already exists; expected_snapshot_hash required");
                }
                let now = chrono::Utc::now().to_rfc3339();
                let meta = serde_json::json!({ "project_path": project_path, "created_at": now });
                crate::cas::atomic_write(&head_file, new_snapshot_hash.as_bytes())?;
                crate::cas::atomic_write(
                    &ref_dir.join("meta.json"),
                    serde_json::to_vec(&meta)?.as_slice(),
                )?;
                Ok(true)
            }
            Some(expected) => match &current {
                Some(c) if c == expected => {
                    crate::cas::atomic_write(&head_file, new_snapshot_hash.as_bytes())?;
                    Ok(true)
                }
                _ => Ok(false),
            },
        }
    }

    pub fn resolve_user_space_ref(&self, user_fp: &str) -> Result<Option<UserSpaceRef>> {
        let ref_dir = self.user_space_ref_dir(user_fp);
        let head_file = ref_dir.join("head");
        if !head_file.exists() {
            return Ok(None);
        }
        let user_manifest_hash = fs::read_to_string(&head_file)?.trim().to_string();
        let mut revision = 1i64;
        let mut pushed_at = None;
        let meta_file = ref_dir.join("meta.json");
        if meta_file.exists() {
            let meta: serde_json::Value = serde_json::from_slice(&fs::read(&meta_file)?)?;
            revision = meta.get("revision").and_then(|v| v.as_i64()).unwrap_or(1);
            pushed_at = meta
                .get("pushed_at")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        Ok(Some(UserSpaceRef {
            user_manifest_hash,
            revision,
            pushed_at,
        }))
    }

    pub fn advance_user_space_ref(
        &self,
        user_fp: &str,
        new_manifest_hash: &str,
        expected_revision: Option<i64>,
    ) -> Result<UserSpaceRef> {
        let ref_dir = self.user_space_ref_dir(user_fp);
        let _lock = acquire_project_lock(&ref_dir)?;
        let head_file = ref_dir.join("head");
        let meta_file = ref_dir.join("meta.json");
        let now = chrono::Utc::now().to_rfc3339();

        let current = self.resolve_user_space_ref(user_fp)?;

        let new_revision = match expected_revision {
            None => {
                if current.is_some() {
                    bail!("user space ref already exists; expected_revision required");
                }
                1
            }
            Some(expected) => {
                let cur = current.ok_or_else(|| anyhow::anyhow!("user space ref not found"))?;
                if cur.revision != expected {
                    bail!(
                        "revision mismatch: expected {expected}, current {}",
                        cur.revision
                    );
                }
                expected + 1
            }
        };

        let meta = serde_json::json!({ "revision": new_revision, "pushed_at": now });
        crate::cas::atomic_write(&head_file, new_manifest_hash.as_bytes())?;
        crate::cas::atomic_write(&meta_file, serde_json::to_vec(&meta)?.as_slice())?;

        Ok(UserSpaceRef {
            user_manifest_hash: new_manifest_hash.to_string(),
            revision: new_revision,
            pushed_at: Some(now),
        })
    }
}

fn read_dir_safe(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    paths.sort();
    Ok(paths)
}

fn collect_generic_ref_hashes(dir: &Path, hashes: &mut HashSet<String>) -> Result<()> {
    for entry in read_dir_safe(dir)? {
        if entry.is_file() {
            if let Ok(data) = fs::read(&entry) {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&data) {
                    if let Some(hash) = val.get("hash").and_then(|v| v.as_str()) {
                        hashes.insert(hash.to_string());
                    }
                }
            }
        } else if entry.is_dir() {
            collect_generic_ref_hashes(&entry, hashes)?;
        }
    }
    Ok(())
}
