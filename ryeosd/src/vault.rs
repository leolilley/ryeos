use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde_json::Value;

fn is_safe_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
        && !name.starts_with('.')
        && !name.contains("..")
}

pub struct VaultStore {
    cas_root: PathBuf,
}

impl VaultStore {
    pub fn new(cas_root: PathBuf) -> Self {
        Self { cas_root }
    }

    fn vault_dir(&self, user_fp: &str) -> PathBuf {
        self.cas_root.join(user_fp).join("vault")
    }

    pub fn set_secret(&self, user_fp: &str, name: &str, envelope: &Value) -> Result<()> {
        if !is_safe_secret_name(name) {
            bail!("invalid secret name: {name:?}");
        }
        let vdir = self.vault_dir(user_fp);
        fs::create_dir_all(&vdir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&vdir, fs::Permissions::from_mode(0o700))?;
        }

        let now = chrono::Utc::now().to_rfc3339();
        let record = serde_json::json!({
            "schema": "vault_secret/v1",
            "name": name,
            "updated_at": now,
            "envelope": envelope,
        });

        let path = vdir.join(format!("{name}.json"));
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&record)?)?;
        fs::rename(&tmp, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn list_secrets(&self, user_fp: &str) -> Result<Vec<String>> {
        let vdir = self.vault_dir(user_fp);
        if !vdir.is_dir() {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(&vdir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn delete_secret(&self, user_fp: &str, name: &str) -> Result<bool> {
        if !is_safe_secret_name(name) {
            bail!("invalid secret name: {name:?}");
        }
        let path = self.vault_dir(user_fp).join(format!("{name}.json"));
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
