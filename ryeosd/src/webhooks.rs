use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn random_hex(len: usize) -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..len).map(|_| rng.gen()).collect();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in &bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub struct WebhookStore {
    cas_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct WebhookBinding {
    pub hook_id: String,
    pub user_id: String,
    pub remote_name: String,
    pub item_id: String,
    pub project_path: String,
    pub description: Option<String>,
    pub secret_envelope: Option<Value>,
    pub vault_keys: Vec<String>,
    pub owner: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub active: bool,
}

#[derive(Debug, Serialize)]
pub struct CreateBindingResult {
    pub hook_id: String,
    pub hmac_secret: String,
    pub item_id: String,
    pub project_path: String,
    pub has_secret_envelope: bool,
    pub vault_keys: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BindingListItem {
    pub hook_id: String,
    pub item_id: String,
    pub project_path: String,
    pub description: Option<String>,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub has_secret_envelope: bool,
    pub vault_keys: Vec<String>,
    pub owner: String,
}

impl WebhookStore {
    pub fn new(cas_root: PathBuf) -> Self {
        Self { cas_root }
    }

    fn bindings_path(&self) -> PathBuf {
        self.cas_root.join("webhooks").join("bindings.json")
    }

    fn secret_path(&self, hook_id: &str) -> PathBuf {
        self.cas_root
            .join("webhooks")
            .join("secrets")
            .join(format!("{hook_id}.key"))
    }

    fn read_index(&self) -> Result<serde_json::Map<String, Value>> {
        let path = self.bindings_path();
        if !path.exists() {
            return Ok(serde_json::Map::new());
        }
        let data = fs::read(&path)?;
        let v: Value = serde_json::from_slice(&data)?;
        match v {
            Value::Object(map) => Ok(map),
            _ => Ok(serde_json::Map::new()),
        }
    }

    fn write_index(&self, index: &serde_json::Map<String, Value>) -> Result<()> {
        let data = serde_json::to_vec_pretty(&Value::Object(index.clone()))?;
        atomic_write(&self.bindings_path(), &data)
    }

    pub fn create_binding(
        &self,
        user_fp: &str,
        remote_name: &str,
        item_id: &str,
        project_path: &str,
        description: Option<&str>,
        secret_envelope: Option<&Value>,
        owner: &str,
        vault_keys: Option<&[String]>,
    ) -> Result<CreateBindingResult> {
        let hook_id = format!("wh_{}", random_hex(16));
        let hmac_secret = format!("whsec_{}", random_hex(32));
        let now = chrono::Utc::now().to_rfc3339();
        let vk = vault_keys.map(|v| v.to_vec()).unwrap_or_default();

        let record = serde_json::json!({
            "hook_id": hook_id,
            "user_id": user_fp,
            "remote_name": remote_name,
            "item_id": item_id,
            "project_path": project_path,
            "description": description,
            "secret_envelope": secret_envelope,
            "vault_keys": vk,
            "owner": owner,
            "created_at": now,
            "revoked_at": null,
            "active": true,
        });

        let mut index = self.read_index()?;
        index.insert(hook_id.clone(), record);
        self.write_index(&index)?;

        let secret_path = self.secret_path(&hook_id);
        if let Some(parent) = secret_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&secret_path, &hmac_secret)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600))?;
        }

        Ok(CreateBindingResult {
            hook_id,
            hmac_secret,
            item_id: item_id.to_string(),
            project_path: project_path.to_string(),
            has_secret_envelope: secret_envelope.is_some(),
            vault_keys: vk,
        })
    }

    pub fn list_bindings(
        &self,
        user_fp: &str,
        remote_name: &str,
    ) -> Result<Vec<BindingListItem>> {
        let index = self.read_index()?;
        let mut results: Vec<BindingListItem> = Vec::new();
        for val in index.values() {
            if val.get("user_id").and_then(|v| v.as_str()) == Some(user_fp)
                && val.get("remote_name").and_then(|v| v.as_str()) == Some(remote_name)
            {
                results.push(BindingListItem {
                    hook_id: val["hook_id"].as_str().unwrap_or("").to_string(),
                    item_id: val["item_id"].as_str().unwrap_or("").to_string(),
                    project_path: val["project_path"].as_str().unwrap_or("").to_string(),
                    description: val
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    created_at: val["created_at"].as_str().unwrap_or("").to_string(),
                    revoked_at: val
                        .get("revoked_at")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    has_secret_envelope: val
                        .get("secret_envelope")
                        .map_or(false, |v| !v.is_null()),
                    vault_keys: val
                        .get("vault_keys")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    owner: val
                        .get("owner")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
        results.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(results)
    }

    pub fn revoke_binding(
        &self,
        hook_id: &str,
        user_fp: &str,
        remote_name: &str,
    ) -> Result<bool> {
        let mut index = self.read_index()?;
        let binding = match index.get_mut(hook_id) {
            Some(Value::Object(map)) => map,
            _ => return Ok(false),
        };
        if binding.get("user_id").and_then(|v| v.as_str()) != Some(user_fp) {
            return Ok(false);
        }
        if binding.get("remote_name").and_then(|v| v.as_str()) != Some(remote_name) {
            return Ok(false);
        }
        if binding
            .get("revoked_at")
            .map_or(false, |v| !v.is_null())
        {
            return Ok(false);
        }

        let now = chrono::Utc::now().to_rfc3339();
        binding.insert("revoked_at".to_string(), Value::String(now));
        binding.insert("active".to_string(), Value::Bool(false));
        self.write_index(&index)?;

        let _ = fs::remove_file(self.secret_path(hook_id));
        Ok(true)
    }
}
