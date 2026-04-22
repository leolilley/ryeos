use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Serialize;

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
    lillux::cas::sha256_hex(project_path.as_bytes())
}

pub struct RefStore {
    cas_root: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct ProjectRef {
    pub snapshot_hash: String,
    pub project_path: String,
}

impl RefStore {
    pub fn new(cas_root: PathBuf) -> Self {
        Self { cas_root }
    }

    fn generic_ref_path(&self, ref_path: &str) -> PathBuf {
        self.cas_root.join("refs").join("generic").join(ref_path)
    }

    pub fn write_ref(&self, ref_path: &str, hash: &str) -> Result<()> {
        anyhow::ensure!(lillux::cas::valid_hash(hash), "invalid hash: {hash}");
        let path = self.generic_ref_path(ref_path);
        let data = serde_json::json!({ "hash": hash });
        lillux::cas::atomic_write(&path, serde_json::to_vec(&data)?.as_slice())
    }

    pub fn read_ref(&self, ref_path: &str) -> Result<Option<String>> {
        let path = self.generic_ref_path(ref_path);
        if !path.exists() {
            return Ok(None);
        }
        let data: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        Ok(data.get("hash").and_then(|v| v.as_str()).map(String::from))
    }

    fn project_ref_dir(&self, user_fp: &str, project_path: &str) -> PathBuf {
        self.cas_root
            .join(user_fp)
            .join("refs")
            .join("projects")
            .join(project_path_hash(project_path))
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
                let now = lillux::time::iso8601_now();
                let meta = serde_json::json!({ "project_path": project_path, "created_at": now });
                lillux::cas::atomic_write(&head_file, new_snapshot_hash.as_bytes())?;
                lillux::cas::atomic_write(
                    &ref_dir.join("meta.json"),
                    serde_json::to_vec(&meta)?.as_slice(),
                )?;
                Ok(true)
            }
            Some(expected) => match &current {
                Some(c) if c == expected => {
                    lillux::cas::atomic_write(&head_file, new_snapshot_hash.as_bytes())?;
                    Ok(true)
                }
                _ => Ok(false),
            },
        }
    }
}
