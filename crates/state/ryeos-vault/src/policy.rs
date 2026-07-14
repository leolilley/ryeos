use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Result};

pub const BLOCKED_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "PWD",
    "USER",
    "SHELL",
    "TERM",
    "PYTHONPATH",
    "PYTHONHOME",
    "RYEOS_APP_ROOT",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

pub const BLOCKED_PREFIXES: &[&str] = &["LD_", "DYLD_", "RYEOS_", "RYEOSD_"];
pub const INTERNAL_RUNTIME_VAULT_PREFIX: &str = "INTERNAL_RUNTIME_VAULT_";

/// The sealed backend decrypts one envelope per operation, so the envelope and
/// its plaintext map are deliberately finite. These are storage invariants,
/// not merely response limits.
pub const MAX_VAULT_ENTRIES: usize = 1024;
pub const MAX_VAULT_KEY_BYTES: usize = 256;
pub const MAX_VAULT_VALUE_BYTES: usize = 256 * 1024;
pub const MAX_VAULT_PLAINTEXT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_VAULT_ENVELOPE_BYTES: u64 = 6 * 1024 * 1024;

pub fn is_blocked_name(key: &str) -> bool {
    BLOCKED_NAMES.contains(&key)
        || BLOCKED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

pub fn is_internal_runtime_vault_key(key: &str) -> bool {
    key.starts_with(INTERNAL_RUNTIME_VAULT_PREFIX)
}

pub fn validate_decrypted_keys(map: &HashMap<String, String>, store_path: &Path) -> Result<()> {
    if map.len() > MAX_VAULT_ENTRIES {
        bail!(
            "vault: sealed store {} has {} entries; maximum is {MAX_VAULT_ENTRIES}",
            store_path.display(),
            map.len()
        );
    }
    let mut plaintext_content_bytes = 0usize;
    for (key, value) in map {
        if key.is_empty() {
            bail!("vault: empty key in sealed store {}", store_path.display());
        }
        if key.len() > MAX_VAULT_KEY_BYTES {
            bail!(
                "vault: key in sealed store {} is {} bytes; maximum is {MAX_VAULT_KEY_BYTES}",
                store_path.display(),
                key.len()
            );
        }
        if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            bail!(
                "vault: invalid key `{key}` in sealed store {} (must match [A-Za-z0-9_]+)",
                store_path.display()
            );
        }
        if is_blocked_name(key) {
            bail!(
                "vault: key `{key}` in sealed store {} is on the OS-protected \
                 blocked list and would shadow inherited environment",
                store_path.display()
            );
        }
        validate_secret_value(value)?;
        plaintext_content_bytes = plaintext_content_bytes
            .checked_add(key.len())
            .and_then(|bytes| bytes.checked_add(value.len()))
            .ok_or_else(|| anyhow::anyhow!("vault: plaintext content size overflow"))?;
        if plaintext_content_bytes > MAX_VAULT_PLAINTEXT_BYTES {
            bail!(
                "vault: sealed store {} content exceeds the {MAX_VAULT_PLAINTEXT_BYTES}-byte maximum",
                store_path.display()
            );
        }
    }
    Ok(())
}

pub fn validate_key_name(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("vault: key name must not be empty");
    }
    if key.len() > MAX_VAULT_KEY_BYTES {
        bail!("vault: key name exceeds the {MAX_VAULT_KEY_BYTES}-byte maximum");
    }
    if is_internal_runtime_vault_key(key) {
        bail!("vault: key name uses the reserved internal runtime vault prefix");
    }
    if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        bail!("vault: invalid key name `{key}` (must match [A-Za-z0-9_]+)");
    }
    if is_blocked_name(key) {
        bail!("vault: key name `{key}` is on the blocked list");
    }
    Ok(())
}

pub fn validate_secret_value(value: &str) -> Result<()> {
    if value.len() > MAX_VAULT_VALUE_BYTES {
        bail!("vault: secret value exceeds the {MAX_VAULT_VALUE_BYTES}-byte maximum");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_key_name_rejects_protected_application_env_names() {
        for key in [
            "LD_AUDIT",
            "LD_DEBUG",
            "DYLD_PRINT_LIBRARIES",
            "PYTHONHOME",
            "RYEOS_PROJECT_SECRET",
            "RYEOSD_THREAD_AUTH_TOKEN",
            "RYEOS_APP_ROOT",
            "HTTP_PROXY",
            "SSL_CERT_FILE",
        ] {
            assert!(validate_key_name(key).is_err(), "{key} should reject");
        }
    }

    #[test]
    fn validate_key_name_rejects_internal_runtime_prefix() {
        assert!(validate_key_name("INTERNAL_RUNTIME_VAULT_FORGED").is_err());
    }

    #[test]
    fn validate_secret_value_enforces_byte_boundary() {
        assert!(validate_secret_value(&"x".repeat(MAX_VAULT_VALUE_BYTES)).is_ok());
        assert!(validate_secret_value(&"x".repeat(MAX_VAULT_VALUE_BYTES + 1)).is_err());
    }

    #[test]
    fn validate_decrypted_keys_enforces_entry_and_content_bounds() {
        let store = Path::new("test.sealed.toml");
        let too_many = (0..=MAX_VAULT_ENTRIES)
            .map(|index| (format!("KEY_{index}"), String::new()))
            .collect::<HashMap<_, _>>();
        assert!(validate_decrypted_keys(&too_many, store).is_err());

        let mut too_large = HashMap::new();
        for index in 0..16 {
            too_large.insert(format!("KEY_{index}"), "x".repeat(MAX_VAULT_VALUE_BYTES));
        }
        too_large.insert("OVERFLOW".to_string(), "x".to_string());
        assert!(validate_decrypted_keys(&too_large, store).is_err());
    }
}
