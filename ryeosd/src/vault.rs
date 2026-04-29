//! Node-vault: operator-secret store consumed by the existing
//! `vault_bindings` plumbing in `services::thread_lifecycle::spawn_item`.
//!
//! ## Architectural role
//!
//! The daemon owns a single shared secret store. At request-build time
//! ([`dispatch::dispatch_subprocess`] and the runner's resume path), the
//! daemon reads the operator's secrets via [`NodeVault::read_all`] and
//! threads them through `ExecutionParams.vault_bindings` →
//! `spawn_item` → `spec.env` → `Command::env()` so every spawned
//! subprocess (directive runtime, graph runtime, tool primitive, …)
//! sees them.
//!
//! Subprocesses (e.g. `ryeos-directive-runtime`) just call
//! `std::env::var("ZEN_API_KEY")` against their inherited env. They
//! don't know a vault exists. The daemon stays vendor-agnostic — it
//! never enumerates provider names or secret-key conventions; it only
//! moves opaque `String -> String` pairs.
//!
//! ## Trust boundary
//!
//! - The daemon process trusts what's on its filesystem (signed
//!   bundles, etc.). Vault secrets are NOT signed; they're treated as
//!   the operator's plain credentials, scoped by file permissions
//!   (V0). When V1 ports the Python sealed-envelope vault
//!   (`docs/future/node-vault.md`) the trust story tightens, but the
//!   trait stays identical.
//! - Already-set process env on the daemon does NOT poison the vault
//!   — vault output is always layered on top of `daemon_callback_env`
//!   and OS-inherited env at spawn time, but the vault itself is read
//!   solely from disk.
//!
//! ## V0 backend (`PlaintextFileVault`)
//!
//! A line-based `KEY=VALUE` file at `<HOME>/.ai/secrets.env`,
//! permissions enforced by the OS (0600 expected). Same convention as
//! Python `${USER_SPACE}/.ai/auth/`.
//!
//! - File missing → empty vault, request proceeds. (Operator hasn't
//!   provisioned secrets — legitimate state.)
//! - File present but malformed → fail-loud at read time; the request
//!   that triggered the read returns an error.
//! - Key on the OS-protected blocked list (`PATH`, `HOME`, …) →
//!   fail-loud at read time. A poisoned secrets file must never
//!   silently shadow `PATH` for spawned subprocesses.
//! - Empty key, missing `=` → fail-loud.
//!
//! No silent dropping of bad lines: typed-fail-loud, end-to-end.
//!
//! ## Future
//!
//! V1 sealed-envelope backend (X25519 + ChaCha20Poly1305 per Python
//! `ryeos-node/ryeos_node/vault.py`) implements the same trait. V1+
//! per-request `vault_keys` filtering plugs in at the dispatch call
//! site without changing the runtime side of the pipe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

/// Names that the OS or process-bootstrap pre-sets and that no vault
/// is allowed to override. Matches the Python `validate_env_map()`
/// blocked list (`ryeos-node/ryeos_node/vault.py`). A secrets file
/// containing one of these aborts the read with a typed error.
const BLOCKED_NAMES: &[&str] = &[
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

/// Read-only operator-secret store. Daemon-owned, swappable backend.
pub trait NodeVault: Send + Sync + std::fmt::Debug {
    /// Return the secrets the given principal is allowed to see.
    ///
    /// V0 ignores `principal` (single-operator node, no per-principal
    /// scoping). V1 sealed-envelope backend will scope by principal
    /// fingerprint, matching Python `ryeos-node/ryeos_node/vault.py`'s
    /// `<cas_base>/<fingerprint>/vault/<NAME>.json` layout.
    fn read_all(&self, principal: &str) -> Result<HashMap<String, String>>;
}

/// Stub vault — used only when the daemon is constructed for a unit
/// test that doesn't want to depend on the operator's filesystem.
/// Always returns an empty map.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyVault;

impl NodeVault for EmptyVault {
    fn read_all(&self, _principal: &str) -> Result<HashMap<String, String>> {
        Ok(HashMap::new())
    }
}

/// V0: plaintext `KEY=VALUE` file at a fixed path.
#[derive(Debug, Clone)]
pub struct PlaintextFileVault {
    path: PathBuf,
}

impl PlaintextFileVault {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl NodeVault for PlaintextFileVault {
    fn read_all(&self, _principal: &str) -> Result<HashMap<String, String>> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => {
                return Err(anyhow!(
                    "vault: read failed for {}: {e}",
                    self.path.display()
                ))
            }
        };
        parse_secrets(&content, &self.path)
    }
}

/// Default V0 secrets path: `<HOME>/.ai/secrets.env`.
///
/// Falls back to `<state_dir>/.ai/secrets.env` only if `HOME` cannot
/// be resolved (e.g. headless CI without a real user). The fallback
/// is fail-explicit, not a "convenience" — it lets the daemon still
/// start in environments without a real home, with the operator
/// provisioning secrets into `state_dir` as a deliberate choice.
pub fn default_vault_path(state_dir: &Path) -> PathBuf {
    if let Some(base) = directories::BaseDirs::new() {
        return base.home_dir().join(".ai").join("secrets.env");
    }
    state_dir.join(".ai").join("secrets.env")
}

fn parse_secrets(content: &str, path: &Path) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (idx, raw) in content.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(eq) = line.find('=') else {
            bail!(
                "vault: malformed line at {}:{lineno} (no `=`): {line:?}",
                path.display()
            );
        };
        let key = line[..eq].trim();
        if key.is_empty() {
            bail!("vault: empty key at {}:{lineno}", path.display());
        }
        if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            bail!(
                "vault: invalid key `{key}` at {}:{lineno} \
                 (must match [A-Za-z0-9_]+)",
                path.display()
            );
        }
        if BLOCKED_NAMES.contains(&key) {
            bail!(
                "vault: key `{key}` at {}:{lineno} is on the OS-protected \
                 blocked list and would shadow inherited environment",
                path.display()
            );
        }
        let value = line[eq + 1..].trim();
        let value = strip_matching_quotes(value);
        if out.insert(key.to_string(), value.to_string()).is_some() {
            bail!(
                "vault: duplicate key `{key}` at {}:{lineno}",
                path.display()
            );
        }
    }
    Ok(out)
}

fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[0] == bytes[bytes.len() - 1]
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpfile(name: &str, content: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ryeosd-vault-test-{}-{}",
            std::process::id(),
            name
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parses_simple_pairs() {
        let p = tmpfile("simple.env", "FOO=bar\nBAZ=qux\n");
        let v = PlaintextFileVault::at(p.clone()).read_all("op").unwrap();
        assert_eq!(v.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(v.get("BAZ"), Some(&"qux".to_string()));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn skips_comments_and_blanks() {
        let p = tmpfile("comments.env", "# c\n\nFOO=bar\n# x\n");
        let v = PlaintextFileVault::at(p.clone()).read_all("op").unwrap();
        assert_eq!(v.len(), 1);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn strips_export_prefix() {
        let p = tmpfile("export.env", "export FOO=bar\n");
        let v = PlaintextFileVault::at(p.clone()).read_all("op").unwrap();
        assert_eq!(v.get("FOO"), Some(&"bar".to_string()));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn strips_matching_quotes() {
        let p = tmpfile(
            "quotes.env",
            "DOUBLE=\"hello world\"\nSINGLE='hi'\nBARE=plain\n",
        );
        let v = PlaintextFileVault::at(p.clone()).read_all("op").unwrap();
        assert_eq!(v.get("DOUBLE"), Some(&"hello world".to_string()));
        assert_eq!(v.get("SINGLE"), Some(&"hi".to_string()));
        assert_eq!(v.get("BARE"), Some(&"plain".to_string()));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn missing_file_is_empty() {
        let p = std::env::temp_dir()
            .join(format!("ryeosd-vault-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let v = PlaintextFileVault::at(p).read_all("op").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn malformed_no_equals_fails_loud() {
        let p = tmpfile("bad-eq.env", "JUSTAKEY\n");
        let err = PlaintextFileVault::at(p.clone())
            .read_all("op")
            .unwrap_err();
        assert!(format!("{err:#}").contains("no `=`"));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn empty_key_fails_loud() {
        let p = tmpfile("empty-key.env", "=value\n");
        let err = PlaintextFileVault::at(p.clone())
            .read_all("op")
            .unwrap_err();
        assert!(format!("{err:#}").contains("empty key"));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn blocked_name_fails_loud() {
        let p = tmpfile("blocked.env", "PATH=/evil\n");
        let err = PlaintextFileVault::at(p.clone())
            .read_all("op")
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("PATH"));
        assert!(msg.contains("blocked list"));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn invalid_key_chars_fail_loud() {
        let p = tmpfile("bad-key.env", "FOO-BAR=baz\n");
        let err = PlaintextFileVault::at(p.clone())
            .read_all("op")
            .unwrap_err();
        assert!(format!("{err:#}").contains("invalid key"));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn duplicate_key_fails_loud() {
        let p = tmpfile("dup.env", "FOO=a\nFOO=b\n");
        let err = PlaintextFileVault::at(p.clone())
            .read_all("op")
            .unwrap_err();
        assert!(format!("{err:#}").contains("duplicate key"));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn empty_vault_trait_returns_empty() {
        assert!(EmptyVault.read_all("op").unwrap().is_empty());
    }
}
