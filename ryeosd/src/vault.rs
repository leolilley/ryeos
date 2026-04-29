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

// Vault key-name policy + write helpers live in
// `ryeos_tools::actions::vault` so they can be shared with the CLI
// `rye vault {put,list,remove,rewrap}` verbs without a circular
// crate dependency. We re-export the pieces public callers (tests,
// fixtures, dispatch) need so this module's surface is unchanged.
pub use ryeos_tools::actions::vault::{
    default_sealed_store_path, validate_decrypted_keys, write_sealed_secrets,
    BLOCKED_NAMES,
};

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
/// projects the source(s) to that subset.
///
/// ## Sources, in precedence order (highest wins)
///
/// 1. The sealed-envelope vault (`vault.read_all(principal)`).
/// 2. `.env` files under each entry of `dotenv_search_dirs`, with
///    later directories overriding earlier ones (typical caller
///    passes `[user_home, project_root]` so project beats user).
///
/// `.env` is intentionally lower precedence than the vault — the
/// vault is the operator's authoritative store, the project's
/// `.env` is convenience for declared secrets the operator hasn't
/// (yet) baked into the vault. Vault entries always win.
///
/// ## Refusal semantics
///
/// Refuses on any declared secret missing from BOTH sources — that's
/// a misconfiguration the caller wants surfaced, not silently
/// absorbed (the alternative is a tool calling a provider with
/// `None` and emitting an opaque upstream auth error).
///
/// Empty `required_secrets` ⇒ empty map. Neither vault nor `.env`
/// files are read in that case (the dispatch fast-path).
pub fn read_required_secrets(
    vault: &dyn NodeVault,
    principal: &str,
    required_secrets: &[String],
    dotenv_search_dirs: &[PathBuf],
) -> Result<HashMap<String, String>> {
    if required_secrets.is_empty() {
        return Ok(HashMap::new());
    }
    let vault_map = vault.read_all(principal)?;
    let dotenv_map = ryeos_tools::actions::vault::read_dotenv_overlay(dotenv_search_dirs)
        .map_err(|e| anyhow!("vault: dotenv overlay: {e:#}"))?;
    let mut out = HashMap::with_capacity(required_secrets.len());
    let mut missing: Vec<&str> = Vec::new();
    for key in required_secrets {
        // Vault wins on conflict; .env is the fallback source.
        if let Some(v) = vault_map.get(key.as_str()) {
            out.insert(key.clone(), v.clone());
        } else if let Some(v) = dotenv_map.get(key.as_str()) {
            out.insert(key.clone(), v.clone());
        } else {
            missing.push(key.as_str());
        }
    }
    if !missing.is_empty() {
        bail!(
            "vault: missing declared secret(s) for principal `{principal}`: [{}]. \
             The item declares these in `required_secrets` but neither the \
             operator vault nor any `.env` overlay provides them. Add them via \
             `rye vault put`, drop them into a `.env` next to the project, or \
             remove the declaration.",
            missing.join(", ")
        );
    }
    Ok(out)
}

/// Compute the conventional `.env` search directories for the
/// dispatch path: `[user_home, project_root]` when both are
/// available, falling back to whichever is present.
///
/// Order matters — later entries win on key collision (see
/// [`read_required_secrets`]). The operator's user-wide `.env` is
/// loaded first; the project's `.env` overrides on top.
///
/// Returns an empty vector when neither is resolvable. The dispatch
/// path then degenerates to vault-only (the pre-step-7c behavior).
pub fn dotenv_search_dirs(project_path: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
        dirs.push(home);
    }
    if let Some(p) = project_path {
        dirs.push(p.to_path_buf());
    }
    dirs
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
        let bindings = read_required_secrets(&PanicVault, "op", &[], &[]).unwrap();
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
        let bindings = read_required_secrets(&v, "op", &required, &[]).unwrap();
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
        let err = read_required_secrets(&v, "op", &required, &[]).unwrap_err();
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
        let err = read_required_secrets(&v, "op", &required, &[]).unwrap_err();
        let msg = format!("{err:#}");
        for k in &["A", "B", "C"] {
            assert!(msg.contains(k), "expected {k} in error: {msg}");
        }
    }

    // ── .env overlay layering ────────────────────────────────────

    #[test]
    fn dotenv_overlay_supplies_missing_declared_secret() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "FROM_DOTENV=hello\n").unwrap();

        let v = FixedVault(HashMap::new());
        let required = vec!["FROM_DOTENV".to_string()];
        let dirs = vec![tmp.path().to_path_buf()];
        let bindings = read_required_secrets(&v, "op", &required, &dirs).unwrap();
        assert_eq!(bindings.get("FROM_DOTENV"), Some(&"hello".to_string()));
    }

    #[test]
    fn dotenv_overlay_loses_to_vault_on_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "FOO=from-dotenv\n").unwrap();

        let mut all = HashMap::new();
        all.insert("FOO".to_string(), "from-vault".to_string());
        let v = FixedVault(all);

        let required = vec!["FOO".to_string()];
        let dirs = vec![tmp.path().to_path_buf()];
        let bindings = read_required_secrets(&v, "op", &required, &dirs).unwrap();
        assert_eq!(
            bindings.get("FOO"),
            Some(&"from-vault".to_string()),
            "vault must win on key collision with .env"
        );
    }

    #[test]
    fn project_dotenv_overrides_user_dotenv() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(user.path().join(".env"), "API_KEY=user-default\n").unwrap();
        std::fs::write(project.path().join(".env"), "API_KEY=project-override\n")
            .unwrap();

        let v = FixedVault(HashMap::new());
        let required = vec!["API_KEY".to_string()];
        let dirs = vec![user.path().to_path_buf(), project.path().to_path_buf()];
        let bindings = read_required_secrets(&v, "op", &required, &dirs).unwrap();
        assert_eq!(
            bindings.get("API_KEY"),
            Some(&"project-override".to_string()),
            "later (project) .env must override earlier (user) .env"
        );
    }

    #[test]
    fn dotenv_overlay_rejects_blocked_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "PATH=/evil\n").unwrap();

        let v = FixedVault(HashMap::new());
        // Even with no required_secrets touching PATH, the parser
        // bails because the file itself is poisoned. The dispatcher
        // should not silently absorb a project's attempt to shadow
        // PATH. (Empty required_secrets short-circuits before the
        // .env read, so we declare a different secret to force the
        // .env walk.)
        let required = vec!["UNRELATED".to_string()];
        let dirs = vec![tmp.path().to_path_buf()];
        let err = read_required_secrets(&v, "op", &required, &dirs).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("PATH") && msg.contains("blocked"),
            "expected blocked PATH error, got: {msg}"
        );
    }

    #[test]
    fn dotenv_overlay_skipped_when_required_empty() {
        // `read_required_secrets` short-circuits on empty required;
        // no .env walk happens, so a poisoned .env doesn't trip the
        // parser when nothing was declared.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "PATH=/evil\n").unwrap();

        let v = FixedVault(HashMap::new());
        let dirs = vec![tmp.path().to_path_buf()];
        let bindings = read_required_secrets(&v, "op", &[], &dirs).unwrap();
        assert!(bindings.is_empty());
    }
}
