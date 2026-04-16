use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

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

        let binding = WebhookBinding {
            hook_id: hook_id.clone(),
            user_id: user_fp.to_string(),
            remote_name: remote_name.to_string(),
            item_id: item_id.to_string(),
            project_path: project_path.to_string(),
            description: description.map(String::from),
            secret_envelope: secret_envelope.cloned(),
            vault_keys: vk.clone(),
            owner: owner.to_string(),
            created_at: now,
            revoked_at: None,
            active: true,
        };

        let mut index = self.read_index()?;
        index.insert(hook_id.clone(), serde_json::to_value(&binding)?);
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
            let binding: WebhookBinding = match serde_json::from_value(val.clone()) {
                Ok(b) => b,
                Err(_) => continue, // skip malformed entries
            };
            if binding.user_id == user_fp && binding.remote_name == remote_name {
                results.push(BindingListItem {
                    hook_id: binding.hook_id,
                    item_id: binding.item_id,
                    project_path: binding.project_path,
                    description: binding.description,
                    created_at: binding.created_at,
                    revoked_at: binding.revoked_at,
                    has_secret_envelope: binding.secret_envelope.is_some(),
                    vault_keys: binding.vault_keys,
                    owner: binding.owner,
                });
            }
        }
        results.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(results)
    }

    /// Resolve a webhook binding by hook_id.
    ///
    /// Returns the binding if it exists and is active.
    pub fn resolve_binding(&self, hook_id: &str) -> Result<Option<WebhookBinding>> {
        let index = self.read_index()?;
        let val = match index.get(hook_id) {
            Some(v) => v,
            None => return Ok(None),
        };
        let binding: WebhookBinding = serde_json::from_value(val.clone())?;
        if !binding.active || binding.revoked_at.is_some() {
            return Ok(None);
        }
        Ok(Some(binding))
    }

    /// Verify an HMAC signature for an inbound webhook request.
    ///
    /// The signature header is expected to be `sha256=<hex>` format.
    pub fn verify_hmac(
        &self,
        hook_id: &str,
        payload: &[u8],
        signature_header: &str,
    ) -> Result<bool> {
        let secret_path = self.secret_path(hook_id);
        if !secret_path.exists() {
            bail!("HMAC secret not found for hook {hook_id}");
        }
        let secret = fs::read_to_string(&secret_path)?;

        let expected_hex = signature_header
            .strip_prefix("sha256=")
            .unwrap_or(signature_header);

        let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
            .map_err(|e| anyhow::anyhow!("HMAC init failed: {e}"))?;
        mac.update(payload);
        let result = mac.finalize().into_bytes();

        let computed_hex = Self::hex_encode_bytes(&result);
        Ok(computed_hex == expected_hex)
    }

    fn hex_encode_bytes(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            use std::fmt::Write;
            let _ = write!(&mut out, "{b:02x}");
        }
        out
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
