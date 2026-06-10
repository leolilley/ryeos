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

pub fn is_blocked_name(key: &str) -> bool {
    BLOCKED_NAMES.contains(&key)
        || BLOCKED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

pub fn validate_decrypted_keys(map: &HashMap<String, String>, store_path: &Path) -> Result<()> {
    for key in map.keys() {
        if key.is_empty() {
            bail!("vault: empty key in sealed store {}", store_path.display());
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
    }
    Ok(())
}

pub fn validate_key_name(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("vault: key name must not be empty");
    }
    if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        bail!("vault: invalid key name `{key}` (must match [A-Za-z0-9_]+)");
    }
    if is_blocked_name(key) {
        bail!("vault: key name `{key}` is on the blocked list");
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
}
