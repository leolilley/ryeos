use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::policy::validate_decrypted_keys;

pub fn write_sealed_secrets(
    store_path: &Path,
    vault_pk: &lillux::vault::VaultPublicKey,
    secrets: &HashMap<String, String>,
) -> Result<()> {
    validate_decrypted_keys(secrets, store_path)?;

    let mut sorted: Vec<(&String, &String)> = secrets.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut plaintext_toml = String::new();
    for (k, v) in &sorted {
        plaintext_toml.push_str(&format!("{k} = {}\n", toml_quote(v)));
    }

    let envelope = lillux::vault::seal(vault_pk, plaintext_toml.as_bytes())
        .map_err(|e| anyhow!("vault: seal failed: {e:#}"))?;
    let envelope_toml =
        toml::to_string(&envelope).map_err(|e| anyhow!("vault: serialize envelope: {e}"))?;

    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("vault: create parent {}: {e}", parent.display()))?;
    }
    let tmp = store_path.with_extension("tmp");
    std::fs::write(&tmp, envelope_toml.as_bytes())
        .map_err(|e| anyhow!("vault: write tmp {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow!("vault: chmod 0600 {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, store_path)
        .map_err(|e| anyhow!("vault: rename tmp -> {}: {e}", store_path.display()))?;
    Ok(())
}

pub fn read_sealed_secrets(
    store_path: &Path,
    sk: &lillux::vault::VaultSecretKey,
) -> Result<HashMap<String, String>> {
    if !store_path.exists() {
        return Ok(HashMap::new());
    }
    let raw = std::fs::read_to_string(store_path)
        .with_context(|| format!("read {}", store_path.display()))?;
    let envelope: lillux::vault::SealedEnvelope = toml::from_str(&raw)
        .with_context(|| format!("parse envelope TOML at {}", store_path.display()))?;
    let plaintext =
        lillux::vault::open(sk, &envelope).map_err(|e| anyhow!("open envelope: {e:#}"))?;
    let plaintext_str =
        std::str::from_utf8(&plaintext).context("decrypted plaintext is not UTF-8")?;
    let map: HashMap<String, String> =
        toml::from_str(plaintext_str).context("decrypted plaintext is not a TOML map")?;
    validate_decrypted_keys(&map, store_path)?;
    Ok(map)
}

fn toml_quote(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii() && b != b'"' && b != b'\\' && !b.is_ascii_control())
    {
        format!("\"{s}\"")
    } else {
        let mut tmp = std::collections::BTreeMap::new();
        tmp.insert("v", s);
        let serialized = toml::to_string(&tmp).unwrap_or_else(|_| format!("\"{s}\""));
        serialized
            .trim()
            .strip_prefix("v = ")
            .unwrap_or(&serialized)
            .to_string()
    }
}
