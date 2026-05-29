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
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
];

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
        if BLOCKED_NAMES.contains(&key.as_str()) {
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
    if BLOCKED_NAMES.contains(&key) {
        bail!("vault: key name `{key}` is on the blocked list");
    }
    Ok(())
}
