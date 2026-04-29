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
//!   bundles, etc.). Vault secrets are encrypted at rest with an
//!   X25519 sealed envelope (see [`SealedEnvelopeVault`] and
//!   [`lillux::vault`]); the daemon's vault X25519 secret key
//!   (auto-generated at boot, separate from the Ed25519 node identity)
//!   is the only thing that can decrypt them.
//! - Already-set process env on the daemon does NOT poison the vault
//!   — vault output is always layered on top of `daemon_callback_env`
//!   and OS-inherited env at spawn time, but the vault itself is read
//!   solely from disk.
//!
//! ## Backend (`SealedEnvelopeVault`)
//!
//! Encrypted store at `<state>/.ai/state/secrets/store.enc` (TOML
//! [`lillux::vault::SealedEnvelope`]); plaintext is a TOML map of
//! `KEY = "VALUE"` after decryption. Vault X25519 secret key lives at
//! `<state>/.ai/node/vault/private_key.pem` (0600).
//!
//! - Store missing → empty vault, request proceeds. (Operator hasn't
//!   provisioned secrets — legitimate state.)
//! - Store present but corrupt / wrong fingerprint / tampered → fail-loud
//!   at read time; the request that triggered the read returns an
//!   error.
//! - Decrypted key on the OS-protected blocked list (`PATH`, `HOME`,
//!   …) → fail-loud at read time via [`validate_decrypted_keys`]. A
//!   poisoned store must never silently shadow `PATH` for spawned
//!   subprocesses.
//! - Empty / non-`[A-Za-z0-9_]+` keys → fail-loud post-decrypt.
//!
//! No silent dropping of bad entries: typed-fail-loud, end-to-end.

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

/// Read only the secrets declared on the spawning item's
/// `ItemMetadata.required_secrets`, refusing if any declared secret
/// is missing.
///
/// This is the **only** vault entry point the dispatcher should use.
/// Calling [`NodeVault::read_all`] directly and pouring the entire
/// vault into a subprocess env was the v0 leak pattern: every spawn,
/// regardless of what the item actually needed, got every secret the
/// operator owned. Items now declare what they need; this function
/// projects the vault to that subset.
///
/// Refuses on any missing declared secret — that's a misconfiguration
/// the caller wants surfaced, not silently absorbed (the alternative
/// is a tool calling a provider with `None` and emitting an opaque
/// upstream auth error).
///
/// Empty `required_secrets` ⇒ empty map (no vault read happens).
pub fn read_required_secrets(
    vault: &dyn NodeVault,
    principal: &str,
    required_secrets: &[String],
) -> Result<HashMap<String, String>> {
    if required_secrets.is_empty() {
        return Ok(HashMap::new());
    }
    let all = vault.read_all(principal)?;
    let mut out = HashMap::with_capacity(required_secrets.len());
    let mut missing: Vec<&str> = Vec::new();
    for key in required_secrets {
        match all.get(key.as_str()) {
            Some(v) => {
                out.insert(key.clone(), v.clone());
            }
            None => missing.push(key.as_str()),
        }
    }
    if !missing.is_empty() {
        bail!(
            "vault: missing declared secret(s) for principal `{principal}`: [{}]. \
             The item declares these in `required_secrets` but the operator vault \
             does not provide them. Add them to the secrets file or remove the \
             declaration.",
            missing.join(", ")
        );
    }
    Ok(out)
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

// ── V1 sealed-envelope vault ────────────────────────────────────────

/// Default sealed-envelope store path: `<state_dir>/.ai/state/secrets/store.enc`.
pub fn default_sealed_store_path(state_dir: &Path) -> PathBuf {
    state_dir
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("secrets")
        .join("store.enc")
}

/// V1: encrypted secret store backed by an X25519 sealed envelope.
///
/// Storage layout:
///   - Vault X25519 secret key: `<state>/.ai/node/vault/private_key.pem`
///     (generated at `rye init` time, file mode 0600).
///   - Encrypted store: `<state>/.ai/state/secrets/store.enc`. Single
///     TOML file containing the [`lillux::vault::SealedEnvelope`].
///     The decrypted plaintext is a TOML map of `KEY = "VALUE"`.
///
/// Trust boundary: the daemon process holds the vault secret key in
/// memory for the lifetime of the daemon. Subprocesses inherit
/// secrets via env, NOT the key itself. Vault-key rotation is an
/// explicit `rye vault rewrap` operation; the daemon refuses to read
/// envelopes whose `vault_pubkey_fingerprint` doesn't match.
#[derive(Debug, Clone)]
pub struct SealedEnvelopeVault {
    store_path: PathBuf,
    secret_key: lillux::vault::VaultSecretKey,
}

impl SealedEnvelopeVault {
    /// Build a vault from an in-memory key + on-disk store path.
    pub fn new(store_path: PathBuf, secret_key: lillux::vault::VaultSecretKey) -> Self {
        Self {
            store_path,
            secret_key,
        }
    }

    /// Load the vault secret key from `<state>/.ai/node/vault/private_key.pem`
    /// and bind it to `<state>/.ai/state/secrets/store.enc`.
    pub fn load(state_dir: &Path) -> Result<Self> {
        let secret_path = state_dir
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("vault")
            .join("private_key.pem");
        let secret_key = lillux::vault::read_secret_key(&secret_path)
            .map_err(|e| anyhow!("vault: load secret key {}: {e:#}", secret_path.display()))?;
        Ok(Self::new(default_sealed_store_path(state_dir), secret_key))
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub fn public_key(&self) -> lillux::vault::VaultPublicKey {
        self.secret_key.public_key()
    }
}

impl NodeVault for SealedEnvelopeVault {
    fn read_all(&self, _principal: &str) -> Result<HashMap<String, String>> {
        if !self.store_path.exists() {
            return Ok(HashMap::new());
        }
        let raw = std::fs::read_to_string(&self.store_path).map_err(|e| {
            anyhow!(
                "vault: read sealed store {}: {e}",
                self.store_path.display()
            )
        })?;
        let envelope: lillux::vault::SealedEnvelope = toml::from_str(&raw).map_err(|e| {
            anyhow!(
                "vault: sealed store {} is not a valid envelope TOML: {e}",
                self.store_path.display()
            )
        })?;
        let plaintext = lillux::vault::open(&self.secret_key, &envelope)
            .map_err(|e| anyhow!("vault: open envelope: {e:#}"))?;
        let plaintext_str = std::str::from_utf8(&plaintext)
            .map_err(|e| anyhow!("vault: decrypted plaintext is not UTF-8: {e}"))?;
        let map: HashMap<String, String> = toml::from_str(plaintext_str).map_err(|e| {
            anyhow!(
                "vault: decrypted plaintext is not a valid TOML map: {e}"
            )
        })?;
        validate_decrypted_keys(&map, &self.store_path)?;
        Ok(map)
    }
}

/// Apply the same key-name policy to decrypted secret-store contents
/// that [`parse_secrets`] applies to plaintext `secrets.env`. A
/// poisoned sealed store is just as dangerous as a poisoned env file.
fn validate_decrypted_keys(map: &HashMap<String, String>, store_path: &Path) -> Result<()> {
    for key in map.keys() {
        if key.is_empty() {
            bail!(
                "vault: empty key in sealed store {}",
                store_path.display()
            );
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

/// Atomically write a sealed envelope containing `secrets` to
/// `store_path`, sealing to `vault_pk`. Used by CLI verbs (e.g.
/// `rye vault put`) and by tests; the daemon NEVER writes the store.
///
/// Refuses on any key that fails [`validate_decrypted_keys`] so a bad
/// write at authoring time fails before the secrets are encrypted.
pub fn write_sealed_secrets(
    store_path: &Path,
    vault_pk: &lillux::vault::VaultPublicKey,
    secrets: &HashMap<String, String>,
) -> Result<()> {
    validate_decrypted_keys(secrets, store_path)?;

    // Serialize the secret map deterministically: sorted keys.
    let mut sorted: Vec<(&String, &String)> = secrets.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut plaintext_toml = String::new();
    for (k, v) in &sorted {
        // toml-rs would also work, but a direct format avoids a
        // round-trip and keeps the on-the-wire format obvious.
        plaintext_toml.push_str(&format!(
            "{k} = {}\n",
            toml_quote(v)
        ));
    }

    let envelope = lillux::vault::seal(vault_pk, plaintext_toml.as_bytes())
        .map_err(|e| anyhow!("vault: seal failed: {e:#}"))?;
    let envelope_toml = toml::to_string(&envelope)
        .map_err(|e| anyhow!("vault: serialize envelope: {e}"))?;

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

/// Quote a TOML string value. We deliberately keep this minimal: the
/// secret-store plaintext is always written by us, so we don't need
/// to handle every TOML edge case — only the ones our values can
/// contain. Strings with backslashes, double-quotes, control chars,
/// or non-ASCII fall back to escaped form via toml-rs.
fn toml_quote(s: &str) -> String {
    if s.bytes().all(|b| {
        b.is_ascii() && b != b'"' && b != b'\\' && !b.is_ascii_control()
    }) {
        format!("\"{s}\"")
    } else {
        // Safe escape via toml-rs: serialize a single-key table and
        // pull out the value side. Slightly heavyweight but correct.
        let mut tmp = std::collections::BTreeMap::new();
        tmp.insert("v", s);
        let serialized = toml::to_string(&tmp).unwrap_or_else(|_| format!("\"{s}\""));
        // Format is `v = "..."`; strip the `v = ` prefix and trailing
        // newline.
        serialized
            .trim()
            .strip_prefix("v = ")
            .unwrap_or(&serialized)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_vault_trait_returns_empty() {
        assert!(EmptyVault.read_all("op").unwrap().is_empty());
    }

    /// Test fixture: a vault that returns a fixed map.
    #[derive(Debug)]
    struct FixedVault(HashMap<String, String>);
    impl NodeVault for FixedVault {
        fn read_all(&self, _principal: &str) -> Result<HashMap<String, String>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn read_required_empty_required_skips_vault_read() {
        // Use a vault that would panic if read; assert no read happens.
        #[derive(Debug)]
        struct PanicVault;
        impl NodeVault for PanicVault {
            fn read_all(&self, _: &str) -> Result<HashMap<String, String>> {
                panic!("read_all should not be called when required is empty");
            }
        }
        let bindings = read_required_secrets(&PanicVault, "op", &[]).unwrap();
        assert!(bindings.is_empty());
    }

    #[test]
    fn read_required_returns_only_declared_keys() {
        let mut all = HashMap::new();
        all.insert("OPENAI_API_KEY".to_string(), "sk-1".to_string());
        all.insert("DATABASE_URL".to_string(), "postgres://".to_string());
        all.insert("UNRELATED".to_string(), "secret-not-declared".to_string());
        let v = FixedVault(all);

        let required = vec!["OPENAI_API_KEY".to_string()];
        let bindings = read_required_secrets(&v, "op", &required).unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings.get("OPENAI_API_KEY"), Some(&"sk-1".to_string()));
        assert!(!bindings.contains_key("DATABASE_URL"));
        assert!(!bindings.contains_key("UNRELATED"));
    }

    #[test]
    fn read_required_fails_on_missing_declared_secret() {
        let mut all = HashMap::new();
        all.insert("FOO".to_string(), "bar".to_string());
        let v = FixedVault(all);

        let required = vec!["FOO".to_string(), "MISSING_KEY".to_string()];
        let err = read_required_secrets(&v, "op", &required).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("MISSING_KEY"), "expected MISSING_KEY in error: {msg}");
        assert!(
            msg.contains("missing declared secret"),
            "expected scoping note in error: {msg}"
        );
    }

    // ── SealedEnvelopeVault tests ────────────────────────────────

    #[test]
    fn sealed_vault_missing_store_returns_empty() {
        let tmp = std::env::temp_dir().join(format!(
            "ryeosd-sealed-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let sk = lillux::vault::VaultSecretKey::generate();
        let v = SealedEnvelopeVault::new(tmp.join("store.enc"), sk);
        assert!(v.read_all("op").unwrap().is_empty());
    }

    #[test]
    fn sealed_vault_roundtrip_via_write_helper() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let pk = sk.public_key();

        let mut secrets = HashMap::new();
        secrets.insert("OPENAI_API_KEY".to_string(), "sk-1".to_string());
        secrets.insert("DATABASE_URL".to_string(), "postgres://u@h/db".to_string());
        write_sealed_secrets(&store_path, &pk, &secrets).unwrap();

        let v = SealedEnvelopeVault::new(store_path.clone(), sk);
        let got = v.read_all("op").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got.get("OPENAI_API_KEY").unwrap(), "sk-1");
        assert_eq!(got.get("DATABASE_URL").unwrap(), "postgres://u@h/db");
    }

    #[test]
    fn sealed_vault_read_with_wrong_key_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk_writer = lillux::vault::VaultSecretKey::generate();
        let sk_reader = lillux::vault::VaultSecretKey::generate();

        let mut secrets = HashMap::new();
        secrets.insert("FOO".to_string(), "bar".to_string());
        write_sealed_secrets(&store_path, &sk_writer.public_key(), &secrets).unwrap();

        let v = SealedEnvelopeVault::new(store_path, sk_reader);
        let err = v.read_all("op").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fingerprint") || msg.contains("AEAD"),
            "expected fingerprint/AEAD failure, got: {msg}"
        );
    }

    #[test]
    fn sealed_vault_write_rejects_blocked_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let pk = lillux::vault::VaultSecretKey::generate().public_key();

        let mut secrets = HashMap::new();
        secrets.insert("PATH".to_string(), "/evil".to_string());
        let err = write_sealed_secrets(&store_path, &pk, &secrets).unwrap_err();
        assert!(
            format!("{err:#}").contains("PATH"),
            "expected PATH in error: {err}"
        );
    }

    #[test]
    fn sealed_vault_write_rejects_invalid_key_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let pk = lillux::vault::VaultSecretKey::generate().public_key();

        let mut secrets = HashMap::new();
        secrets.insert("FOO-BAR".to_string(), "x".to_string());
        let err = write_sealed_secrets(&store_path, &pk, &secrets).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid key"),
            "expected invalid key in error: {err}"
        );
    }

    #[test]
    fn sealed_vault_round_trip_with_quoting_edge_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let pk = sk.public_key();

        let mut secrets = HashMap::new();
        secrets.insert("WITH_QUOTE".to_string(), "abc\"def".to_string());
        secrets.insert("WITH_BACKSLASH".to_string(), "line\\path".to_string());
        secrets.insert("WITH_NEWLINE".to_string(), "a\nb".to_string());
        secrets.insert("WITH_UNICODE".to_string(), "naïve".to_string());
        write_sealed_secrets(&store_path, &pk, &secrets).unwrap();

        let v = SealedEnvelopeVault::new(store_path, sk);
        let got = v.read_all("op").unwrap();
        for (k, v) in &secrets {
            assert_eq!(got.get(k), Some(v), "round-trip mismatch for {k}");
        }
    }

    #[test]
    fn sealed_vault_load_from_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        let key_path = state
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("vault")
            .join("private_key.pem");
        let sk = lillux::vault::VaultSecretKey::generate();
        lillux::vault::write_secret_key(&key_path, &sk).unwrap();

        let v = SealedEnvelopeVault::load(state).unwrap();
        assert_eq!(
            v.public_key().fingerprint(),
            sk.public_key().fingerprint()
        );
        assert_eq!(v.store_path(), default_sealed_store_path(state));
    }

    #[test]
    fn read_required_fails_on_multiple_missing_listed_together() {
        let v = FixedVault(HashMap::new());
        let required = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let err = read_required_secrets(&v, "op", &required).unwrap_err();
        let msg = format!("{err:#}");
        for k in &["A", "B", "C"] {
            assert!(msg.contains(k), "expected {k} in error: {msg}");
        }
    }
}
