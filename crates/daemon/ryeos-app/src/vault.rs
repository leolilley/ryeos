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
//! - Already-set process env on the daemon does NOT poison the vault.
//!   Host env is only consulted for item-declared `required_secrets`,
//!   and only those declared names are projected into subprocess env.
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

use std::collections::{HashMap, HashSet};
use std::env::VarError;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};

use ryeos_engine::roots;

// Vault key-name policy + write helpers live in
// `ryeos_core_tools::actions::vault` so they can be shared with the CLI
// `ryeos vault {put,list,remove,rewrap}` verbs without a circular
// crate dependency. We re-export the pieces public callers (tests,
// fixtures, dispatch) need so this module's surface is unchanged.
pub use ryeos_vault::paths::default_sealed_store_path;
pub use ryeos_vault::policy::{validate_decrypted_keys, validate_key_name, BLOCKED_NAMES};
pub use ryeos_vault::sealed::{recover_rewrap, with_store_lock, write_sealed_secrets};

pub const INTERNAL_RUNTIME_VAULT_PREFIX: &str = "INTERNAL_RUNTIME_VAULT_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultScope {
    OperatorEnv {
        principal: String,
    },
    RuntimeBundle {
        bundle_id: String,
        namespace: String,
    },
}

impl VaultScope {
    pub fn operator_env(principal: impl Into<String>) -> Self {
        Self::OperatorEnv {
            principal: principal.into(),
        }
    }

    pub fn runtime_bundle(
        bundle_id: impl Into<String>,
        namespace: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let bundle_id = bundle_id.into();
        let namespace = namespace.into();
        ryeos_state::objects::validate_bundle_identifier("bundle_id", &bundle_id)?;
        validate_runtime_vault_segment("namespace", &namespace)?;
        Ok(Self::RuntimeBundle {
            bundle_id,
            namespace,
        })
    }
}

pub fn runtime_vault_ref(bundle_id: &str, namespace: &str, key: &str) -> String {
    format!("vault://bundle/{bundle_id}/{namespace}/{key}")
}

pub fn validate_runtime_vault_segment(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("runtime vault {label} must not be empty");
    }
    if value.len() > 64 {
        bail!("runtime vault {label} is too long");
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        bail!("runtime vault {label} must match [A-Za-z0-9_]+");
    }
    Ok(())
}

pub fn is_internal_runtime_vault_key(name: &str) -> bool {
    name.starts_with(INTERNAL_RUNTIME_VAULT_PREFIX)
}

fn validate_operator_secret_name(name: &str) -> Result<()> {
    if is_internal_runtime_vault_key(name) {
        bail!("vault: secret name uses reserved internal runtime vault prefix");
    }
    Ok(())
}

fn runtime_physical_key(bundle_id: &str, namespace: &str, key: &str) -> Result<String> {
    ryeos_state::objects::validate_bundle_identifier("bundle_id", bundle_id)?;
    validate_runtime_vault_segment("namespace", namespace)?;
    validate_runtime_vault_segment("key", key)?;
    Ok(format!(
        "{INTERNAL_RUNTIME_VAULT_PREFIX}{}_{}_{namespace}_{key}",
        lillux::cas::sha256_hex(bundle_id.as_bytes()),
        namespace.len()
    ))
}

fn runtime_physical_prefix(bundle_id: &str, namespace: &str) -> Result<String> {
    ryeos_state::objects::validate_bundle_identifier("bundle_id", bundle_id)?;
    validate_runtime_vault_segment("namespace", namespace)?;
    Ok(format!(
        "{INTERNAL_RUNTIME_VAULT_PREFIX}{}_{}_{namespace}_",
        lillux::cas::sha256_hex(bundle_id.as_bytes()),
        namespace.len()
    ))
}

/// Read-only operator-secret store. Daemon-owned, swappable backend.
pub trait NodeVault: Send + Sync + std::fmt::Debug {
    /// Return the secrets the given principal is allowed to see.
    ///
    /// V0 ignores `principal` (single-operator node, no per-principal
    /// scoping). V1 sealed-envelope backend will scope by principal
    /// fingerprint, matching Python `ryeos-node/ryeos_node/vault.py`'s
    /// `<cas_base>/<fingerprint>/vault/<NAME>.json` layout.
    fn read_all(&self, principal: &str) -> Result<HashMap<String, String>>;

    /// Set a secret. Server-side sealing: the daemon re-encrypts the
    /// entire store. The `principal` param is accepted but ignored in v1
    /// (single shared store per trust boundary).
    fn set_secret(&self, principal: &str, name: &str, value: &str) -> Result<()>;

    /// List secret key names (never values).
    fn list_keys(&self, principal: &str) -> Result<Vec<String>>;

    /// Delete a secret by name. Returns `true` if the key existed.
    fn delete_secret(&self, principal: &str, name: &str) -> Result<bool>;

    fn put_scoped_secret(&self, scope: &VaultScope, key: &str, value: &str) -> Result<()> {
        match scope {
            VaultScope::OperatorEnv { principal } => self.set_secret(principal, key, value),
            VaultScope::RuntimeBundle {
                bundle_id,
                namespace,
            } => self.set_secret("", &runtime_physical_key(bundle_id, namespace, key)?, value),
        }
    }

    fn get_scoped_secret(&self, scope: &VaultScope, key: &str) -> Result<Option<String>> {
        match scope {
            VaultScope::OperatorEnv { principal } => {
                validate_operator_secret_name(key)?;
                Ok(self.read_all(principal)?.get(key).cloned())
            }
            VaultScope::RuntimeBundle {
                bundle_id,
                namespace,
            } => Ok(self
                .read_all("")?
                .get(&runtime_physical_key(bundle_id, namespace, key)?)
                .cloned()),
        }
    }

    fn delete_scoped_secret(&self, scope: &VaultScope, key: &str) -> Result<bool> {
        match scope {
            VaultScope::OperatorEnv { principal } => self.delete_secret(principal, key),
            VaultScope::RuntimeBundle {
                bundle_id,
                namespace,
            } => self.delete_secret("", &runtime_physical_key(bundle_id, namespace, key)?),
        }
    }

    fn list_scoped_secret_keys(&self, scope: &VaultScope) -> Result<Vec<String>> {
        match scope {
            VaultScope::OperatorEnv { principal } => self.list_keys(principal),
            VaultScope::RuntimeBundle {
                bundle_id,
                namespace,
            } => {
                let prefix = runtime_physical_prefix(bundle_id, namespace)?;
                let mut keys: Vec<String> = self
                    .list_keys("")?
                    .into_iter()
                    .filter_map(|name| name.strip_prefix(&prefix).map(str::to_string))
                    .collect();
                keys.sort();
                Ok(keys)
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VaultReadError {
    #[error(
        "missing declared secret(s) for principal `{principal}`: [{}]. \
         The item declares these in `required_secrets` but none of the checked \
         sources provides them: sealed vault, daemon host environment, or \
         `.env` overlay. For hosted deployments, configure them as service \
         variables. For local/dev, use `ryeos vault put` or a project/user \
         `.env`, or remove the declaration.",
        names.join(", ")
    )]
    MissingSecrets {
        principal: String,
        names: Vec<String>,
    },
    #[error("vault read error: {0}")]
    Internal(#[from] anyhow::Error),
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
/// 2. Declared names from the daemon host environment (for hosted
///    platforms like Railway/Fly/Render where service variables are
///    the operator's native secret mechanism).
/// 3. `.env` files under each entry of `dotenv_search_dirs`, with
///    later directories overriding earlier ones (typical caller
///    passes `[user_home, project_root]` so project beats user).
///
/// `.env` is intentionally lower precedence than both vault and host
/// env — the vault is RyeOS's explicit operator store, host env is the
/// deployment platform's operator store, and project `.env` is a
/// convenience fallback for local/dev. Vault entries always win.
///
/// ## Refusal semantics
///
/// Refuses on any declared secret missing from ALL sources — that's
/// a misconfiguration the caller wants surfaced, not silently
/// absorbed (the alternative is a tool calling a provider with
/// `None` and emitting an opaque upstream auth error).
///
/// Empty `required_secrets` ⇒ empty map. Neither vault, host env, nor
/// `.env` files are read in that case (the dispatch fast-path).
pub fn read_required_secrets(
    vault: &dyn NodeVault,
    principal: &str,
    required_secrets: &[String],
    dotenv_search_dirs: &[PathBuf],
) -> std::result::Result<HashMap<String, String>, VaultReadError> {
    if required_secrets.is_empty() {
        return Ok(HashMap::new());
    }
    for key in required_secrets {
        validate_operator_secret_name(key)?;
        crate::process::validate_spawn_secret_name(key)
            .map_err(|e| anyhow!("vault: invalid declared secret `{key}`: {e:#}"))?;
    }
    let vault_map = vault.read_all(principal)?;
    let host_env_map = read_declared_host_env(required_secrets)?;
    // Only consult the `.env` overlay for secrets the higher-precedence sources
    // (vault, daemon host env) did not already satisfy. This keeps vault > host
    // > dotenv precedence at the I/O level: a poisoned or unreadable `.env`
    // cannot fail a launch whose secrets are already fully resolved upstream,
    // and the overlay only ever sees the declared keys it still needs.
    let dotenv_wanted: HashSet<String> = required_secrets
        .iter()
        .filter(|k| !vault_map.contains_key(k.as_str()) && !host_env_map.contains_key(k.as_str()))
        .cloned()
        .collect();
    let dotenv_map = ryeos_vault::dotenv::read_dotenv_overlay(dotenv_search_dirs, &dotenv_wanted)
        .map_err(|e| anyhow!("vault: dotenv overlay: {e:#}"))?;
    let mut out = HashMap::with_capacity(required_secrets.len());
    let mut missing: Vec<String> = Vec::new();
    for key in required_secrets {
        // Vault wins on conflict; host env is the deployment fallback;
        // .env is the local/dev convenience fallback.
        if let Some(v) = vault_map.get(key.as_str()) {
            out.insert(key.clone(), v.clone());
        } else if let Some(v) = host_env_map.get(key.as_str()) {
            out.insert(key.clone(), v.clone());
        } else if let Some(v) = dotenv_map.get(key.as_str()) {
            out.insert(key.clone(), v.clone());
        } else {
            missing.push(key.clone());
        }
    }
    if !missing.is_empty() {
        return Err(VaultReadError::MissingSecrets {
            principal: principal.to_string(),
            names: missing,
        });
    }
    Ok(out)
}

fn read_declared_host_env(required_secrets: &[String]) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for key in required_secrets {
        match std::env::var(key) {
            Ok(value) => {
                out.insert(key.clone(), value);
            }
            Err(VarError::NotPresent) => {}
            Err(VarError::NotUnicode(_)) => {
                bail!(
                    "vault: declared secret `{key}` is present in daemon host env \
                     but is not valid UTF-8"
                );
            }
        }
    }
    Ok(out)
}

/// Compute the conventional `.env` search directories for the dispatch path:
/// operator config first, then project root.
///
/// Order matters — later entries win on key collision (see
/// [`read_required_secrets`]). The operator `.env` is loaded first; the
/// project's `.env` overrides on top.
///
/// Returns an empty vector when neither is resolvable. The dispatch
/// path then degenerates to vault-only (the pre-step-7c behavior).
pub fn dotenv_search_dirs(project_path: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(root) = roots::runtime_root() {
        dirs.push(root.config());
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

    fn set_secret(&self, _principal: &str, _name: &str, _value: &str) -> Result<()> {
        bail!("vault: EmptyVault does not support writes")
    }

    fn list_keys(&self, _principal: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn delete_secret(&self, _principal: &str, _name: &str) -> Result<bool> {
        Ok(false)
    }
}

// ── V1 sealed-envelope vault ────────────────────────────────────────

/// V1: encrypted secret store backed by an X25519 sealed envelope.
///
/// Storage layout:
///   - Vault X25519 secret key: `<state>/.ai/node/vault/private_key.pem`
///     (generated at `ryeos init` time, file mode 0600).
///   - Encrypted store: `<state>/.ai/state/secrets/store.enc`. Single
///     TOML file containing the [`lillux::vault::SealedEnvelope`].
///     The decrypted plaintext is a TOML map of `KEY = "VALUE"`.
///
/// Trust boundary: the daemon process holds the vault secret key in memory,
/// but a filesystem-backed vault reloads it under the sealed-store lock before
/// every operation so `ryeos vault rewrap` can rotate a live daemon safely.
/// Subprocesses inherit secrets via env, NOT the key itself. The daemon refuses
/// to read envelopes whose `vault_pubkey_fingerprint` doesn't match.
#[derive(Debug, Clone)]
pub struct SealedEnvelopeVault {
    store_path: PathBuf,
    secret_key: lillux::vault::VaultSecretKey,
    key_paths: Option<(PathBuf, PathBuf)>,
    io_lock: Arc<Mutex<()>>,
}

impl SealedEnvelopeVault {
    /// Build a vault from an in-memory key + on-disk store path.
    pub fn new(store_path: PathBuf, secret_key: lillux::vault::VaultSecretKey) -> Self {
        Self {
            store_path,
            secret_key,
            key_paths: None,
            io_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Load the vault secret key from `<app_root>/.ai/node/vault/private_key.pem`
    /// and bind it to `<app_root>/.ai/state/secrets/store.enc`.
    pub fn load(app_root: &Path) -> Result<Self> {
        let secret_path = ryeos_vault::paths::default_vault_secret_key_path(app_root);
        let public_path = ryeos_vault::paths::default_vault_public_key_path(app_root);
        let store_path = default_sealed_store_path(app_root);
        with_store_lock(&store_path, || {
            recover_rewrap(&secret_path, &public_path, &store_path)
        })?;
        let secret_key = lillux::vault::read_secret_key(&secret_path)
            .map_err(|e| anyhow!("vault: load secret key {}: {e:#}", secret_path.display()))?;
        Ok(Self {
            store_path,
            secret_key,
            key_paths: Some((secret_path, public_path)),
            io_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub fn public_key(&self) -> lillux::vault::VaultPublicKey {
        self.key_paths
            .as_ref()
            .and_then(|(secret_path, _)| lillux::vault::read_secret_key(secret_path).ok())
            .unwrap_or_else(|| self.secret_key.clone())
            .public_key()
    }

    fn with_current_generation<T>(
        &self,
        operation: impl FnOnce(&lillux::vault::VaultSecretKey) -> Result<T>,
    ) -> Result<T> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| anyhow!("vault: sealed store lock poisoned"))?;
        with_store_lock(&self.store_path, || {
            let secret_key = if let Some((secret_path, public_path)) = &self.key_paths {
                recover_rewrap(secret_path, public_path, &self.store_path)?;
                lillux::vault::read_secret_key(secret_path).map_err(|e| {
                    anyhow!("vault: reload secret key {}: {e:#}", secret_path.display())
                })?
            } else {
                self.secret_key.clone()
            };
            operation(&secret_key)
        })
    }

    /// Internal read-all without the trait dispatch.
    fn read_all_with_key(
        &self,
        secret_key: &lillux::vault::VaultSecretKey,
    ) -> Result<HashMap<String, String>> {
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
        let plaintext = lillux::vault::open(secret_key, &envelope)
            .map_err(|e| anyhow!("vault: open envelope: {e:#}"))?;
        let plaintext_str = std::str::from_utf8(&plaintext)
            .map_err(|e| anyhow!("vault: decrypted plaintext is not UTF-8: {e}"))?;
        let map: HashMap<String, String> = toml::from_str(plaintext_str)
            .map_err(|e| anyhow!("vault: decrypted plaintext is not a valid TOML map: {e}"))?;
        validate_decrypted_keys(&map, &self.store_path)?;
        Ok(map)
    }

    fn read_all_internal(&self) -> Result<HashMap<String, String>> {
        self.with_current_generation(|secret_key| self.read_all_with_key(secret_key))
    }

    /// Atomic read-modify-write on the sealed store.
    ///
    /// If the store file exists but cannot be read (corrupt, wrong key,
    /// etc.), this returns an error rather than silently starting from
    /// an empty store. A missing store file is fine (first write).
    fn read_modify_write<T>(
        &self,
        modify: impl FnOnce(&mut HashMap<String, String>) -> Result<T>,
    ) -> Result<T> {
        self.with_current_generation(|secret_key| {
            let mut map = if self.store_path.exists() {
                self.read_all_with_key(secret_key)?
            } else {
                HashMap::new()
            };
            let result = modify(&mut map)?;
            validate_decrypted_keys(&map, &self.store_path)?;
            let pk = secret_key.public_key();
            write_sealed_secrets(&self.store_path, &pk, &map)?;
            Ok(result)
        })
    }
}

impl NodeVault for SealedEnvelopeVault {
    fn read_all(&self, _principal: &str) -> Result<HashMap<String, String>> {
        Ok(self
            .read_all_internal()?
            .into_iter()
            .filter(|(key, _)| !is_internal_runtime_vault_key(key))
            .collect())
    }

    fn set_secret(&self, _principal: &str, name: &str, value: &str) -> Result<()> {
        validate_operator_secret_name(name)?;
        // Validate key name
        validate_key_name(name)?;
        self.read_modify_write(|map| {
            map.insert(name.to_string(), value.to_string());
            Ok(())
        })
    }

    fn list_keys(&self, _principal: &str) -> Result<Vec<String>> {
        let map = self.read_all_internal()?;
        let mut keys: Vec<String> = map
            .into_keys()
            .filter(|key| !is_internal_runtime_vault_key(key))
            .collect();
        keys.sort();
        Ok(keys)
    }

    fn delete_secret(&self, _principal: &str, name: &str) -> Result<bool> {
        validate_operator_secret_name(name)?;
        // Validate key name before attempting delete
        validate_key_name(name)?;
        self.read_modify_write(|map| Ok(map.remove(name).is_some()))
    }

    fn put_scoped_secret(&self, scope: &VaultScope, key: &str, value: &str) -> Result<()> {
        match scope {
            VaultScope::OperatorEnv { principal } => self.set_secret(principal, key, value),
            VaultScope::RuntimeBundle {
                bundle_id,
                namespace,
            } => {
                let physical_key = runtime_physical_key(bundle_id, namespace, key)?;
                self.read_modify_write(|map| {
                    map.insert(physical_key, value.to_string());
                    Ok(())
                })
            }
        }
    }

    fn get_scoped_secret(&self, scope: &VaultScope, key: &str) -> Result<Option<String>> {
        match scope {
            VaultScope::OperatorEnv { principal } => read_named_secret(self, principal, key),
            VaultScope::RuntimeBundle {
                bundle_id,
                namespace,
            } => Ok(self
                .read_all_internal()?
                .get(&runtime_physical_key(bundle_id, namespace, key)?)
                .cloned()),
        }
    }

    fn delete_scoped_secret(&self, scope: &VaultScope, key: &str) -> Result<bool> {
        match scope {
            VaultScope::OperatorEnv { principal } => self.delete_secret(principal, key),
            VaultScope::RuntimeBundle {
                bundle_id,
                namespace,
            } => {
                let physical_key = runtime_physical_key(bundle_id, namespace, key)?;
                self.read_modify_write(|map| Ok(map.remove(&physical_key).is_some()))
            }
        }
    }

    fn list_scoped_secret_keys(&self, scope: &VaultScope) -> Result<Vec<String>> {
        match scope {
            VaultScope::OperatorEnv { principal } => self.list_keys(principal),
            VaultScope::RuntimeBundle {
                bundle_id,
                namespace,
            } => {
                let prefix = runtime_physical_prefix(bundle_id, namespace)?;
                let mut keys: Vec<String> = self
                    .read_all_internal()?
                    .into_keys()
                    .filter_map(|name| name.strip_prefix(&prefix).map(str::to_string))
                    .collect();
                keys.sort();
                Ok(keys)
            }
        }
    }
}

/// Read a single named secret from the vault. Returns `Ok(Some(value))`
/// when present, `Ok(None)` when absent. Use this for the narrow
/// provider-secret injection path after model-target preflight.
pub fn read_named_secret(
    vault: &dyn NodeVault,
    principal: &str,
    name: &str,
) -> Result<Option<String>> {
    validate_operator_secret_name(name)?;
    let map = vault.read_all(principal)?;
    Ok(map.get(name).cloned())
}

/// Resolve one explicitly requested secret name through the same source
/// stack used by `read_required_secrets`: sealed vault, daemon host env,
/// then `.env` overlay. Returns `Ok(None)` when absent from every source.
pub fn read_explicit_secret(
    vault: &dyn NodeVault,
    principal: &str,
    name: &str,
    dotenv_search_dirs: &[PathBuf],
) -> Result<Option<String>> {
    validate_operator_secret_name(name)?;
    crate::process::validate_spawn_secret_name(name)
        .map_err(|e| anyhow!("vault: invalid explicit secret `{name}`: {e:#}"))?;
    let required = vec![name.to_string()];
    let vault_map = vault.read_all(principal)?;
    if let Some(value) = vault_map.get(name) {
        return Ok(Some(value.clone()));
    }
    let host_env_map = read_declared_host_env(&required)?;
    if let Some(value) = host_env_map.get(name) {
        return Ok(Some(value.clone()));
    }
    // Vault and host env missed — consult `.env`, scoped to just this key so an
    // unrelated blocked/malformed line in the file cannot fail the resolution.
    let wanted: HashSet<String> = required.iter().cloned().collect();
    let dotenv_map = ryeos_vault::dotenv::read_dotenv_overlay(dotenv_search_dirs, &wanted)
        .map_err(|e| anyhow!("vault: dotenv overlay: {e:#}"))?;
    Ok(dotenv_map.get(name).cloned())
}

/// Which source would satisfy a declared secret. Reported by `env-check`
/// without ever exposing the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSource {
    /// Sealed operator vault.
    Vault,
    /// Daemon host environment.
    HostEnv,
    /// A `.env` overlay file in the given directory.
    Dotenv(PathBuf),
    /// Not found in any source.
    Missing,
}

impl SecretSource {
    /// Short stable label for wire/CLI output.
    pub fn label(&self) -> &'static str {
        match self {
            SecretSource::Vault => "vault",
            SecretSource::HostEnv => "host_env",
            SecretSource::Dotenv(_) => "dotenv",
            SecretSource::Missing => "missing",
        }
    }
}

/// Report, per requested name, WHICH source would satisfy it — without
/// returning any secret value. Mirrors the precedence of
/// [`read_required_secrets`] exactly (vault > host env > `.env` overlay, with
/// the overlay scoped to names unresolved by the higher sources), so a report
/// reflects what a real launch would resolve. Results preserve `names` order.
///
/// This is the engine behind `ryeos tool env-check`: it never reads or returns
/// the secret material, only presence and source.
pub fn resolve_secret_sources(
    vault: &dyn NodeVault,
    principal: &str,
    names: &[String],
    dotenv_search_dirs: &[PathBuf],
) -> Result<Vec<(String, SecretSource)>> {
    // Same boundary checks as `read_required_secrets`, so a report reflects
    // exactly what a real launch would do: empty fast-path (no source reads),
    // and reject invalid/blocked declared names up front rather than
    // misreporting them as `host_env`/`missing`.
    if names.is_empty() {
        return Ok(Vec::new());
    }
    for name in names {
        validate_operator_secret_name(name)?;
        crate::process::validate_spawn_secret_name(name)
            .map_err(|e| anyhow!("vault: invalid declared secret `{name}`: {e:#}"))?;
    }
    let vault_map = vault.read_all(principal)?;
    let host_env_map = read_declared_host_env(names)?;
    // Only the names the higher-precedence sources did not satisfy reach the
    // `.env` probe — and we probe each dir separately so we can attribute which
    // file would supply the key (later dir wins, matching resolution order).
    let unresolved: HashSet<String> = names
        .iter()
        .filter(|n| !vault_map.contains_key(n.as_str()) && !host_env_map.contains_key(n.as_str()))
        .cloned()
        .collect();
    let mut dotenv_dir_for: HashMap<String, PathBuf> = HashMap::new();
    if !unresolved.is_empty() {
        for dir in dotenv_search_dirs {
            let map =
                ryeos_vault::dotenv::read_dotenv_overlay(std::slice::from_ref(dir), &unresolved)
                    .map_err(|e| anyhow!("vault: dotenv overlay: {e:#}"))?;
            for key in map.keys() {
                dotenv_dir_for.insert(key.clone(), dir.clone());
            }
        }
    }
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let source = if vault_map.contains_key(name.as_str()) {
            SecretSource::Vault
        } else if host_env_map.contains_key(name.as_str()) {
            SecretSource::HostEnv
        } else if let Some(dir) = dotenv_dir_for.get(name) {
            SecretSource::Dotenv(dir.clone())
        } else {
            SecretSource::Missing
        };
        out.push((name.clone(), source));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        _guard: MutexGuard<'static, ()>,
        previous: Vec<(String, Option<OsString>)>,
    }

    impl EnvVarGuard {
        fn set(vars: &[(&str, Option<&str>)]) -> Self {
            let guard = ENV_LOCK.lock().unwrap();
            let previous = vars
                .iter()
                .map(|(key, _)| ((*key).to_string(), std::env::var_os(key)))
                .collect::<Vec<_>>();
            for (key, value) in vars {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (key, value) in &self.previous {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

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
        fn set_secret(&self, _: &str, _: &str, _: &str) -> Result<()> {
            bail!("FixedVault does not support writes")
        }
        fn list_keys(&self, principal: &str) -> Result<Vec<String>> {
            let map = self.read_all(principal)?;
            let mut keys: Vec<String> = map.into_keys().collect();
            keys.sort();
            Ok(keys)
        }
        fn delete_secret(&self, _: &str, _: &str) -> Result<bool> {
            bail!("FixedVault does not support writes")
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
            fn set_secret(&self, _: &str, _: &str, _: &str) -> Result<()> {
                panic!("set_secret should not be called")
            }
            fn list_keys(&self, _: &str) -> Result<Vec<String>> {
                panic!("list_keys should not be called")
            }
            fn delete_secret(&self, _: &str, _: &str) -> Result<bool> {
                panic!("delete_secret should not be called")
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
        assert!(
            msg.contains("MISSING_KEY"),
            "expected MISSING_KEY in error: {msg}"
        );
        assert!(
            msg.contains("missing declared secret"),
            "expected scoping note in error: {msg}"
        );
    }

    #[test]
    fn host_env_supplies_declared_secret() {
        let _env = EnvVarGuard::set(&[("SNAPTRACK_TEST_HOST_SECRET", Some("from-host"))]);
        let v = FixedVault(HashMap::new());
        let required = vec!["SNAPTRACK_TEST_HOST_SECRET".to_string()];

        let bindings = read_required_secrets(&v, "op", &required, &[]).unwrap();

        assert_eq!(
            bindings.get("SNAPTRACK_TEST_HOST_SECRET"),
            Some(&"from-host".to_string())
        );
    }

    #[test]
    fn vault_beats_host_env_for_declared_secret() {
        let _env = EnvVarGuard::set(&[("SNAPTRACK_TEST_PRECEDENCE", Some("from-host"))]);
        let mut all = HashMap::new();
        all.insert(
            "SNAPTRACK_TEST_PRECEDENCE".to_string(),
            "from-vault".to_string(),
        );
        let v = FixedVault(all);
        let required = vec!["SNAPTRACK_TEST_PRECEDENCE".to_string()];

        let bindings = read_required_secrets(&v, "op", &required, &[]).unwrap();

        assert_eq!(
            bindings.get("SNAPTRACK_TEST_PRECEDENCE"),
            Some(&"from-vault".to_string())
        );
    }

    #[test]
    fn host_env_beats_dotenv_for_declared_secret() {
        let _env = EnvVarGuard::set(&[("SNAPTRACK_TEST_HOST_DOTENV", Some("from-host"))]);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".env"),
            "SNAPTRACK_TEST_HOST_DOTENV=from-dotenv\n",
        )
        .unwrap();
        let v = FixedVault(HashMap::new());
        let required = vec!["SNAPTRACK_TEST_HOST_DOTENV".to_string()];
        let dirs = vec![tmp.path().to_path_buf()];

        let bindings = read_required_secrets(&v, "op", &required, &dirs).unwrap();

        assert_eq!(
            bindings.get("SNAPTRACK_TEST_HOST_DOTENV"),
            Some(&"from-host".to_string())
        );
    }

    #[test]
    fn host_env_only_returns_declared_keys() {
        let _env = EnvVarGuard::set(&[
            ("SNAPTRACK_TEST_DECLARED", Some("declared")),
            ("SNAPTRACK_TEST_UNDECLARED", Some("must-not-leak")),
        ]);
        let v = FixedVault(HashMap::new());
        let required = vec!["SNAPTRACK_TEST_DECLARED".to_string()];

        let bindings = read_required_secrets(&v, "op", &required, &[]).unwrap();

        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings.get("SNAPTRACK_TEST_DECLARED"),
            Some(&"declared".to_string())
        );
        assert!(!bindings.contains_key("SNAPTRACK_TEST_UNDECLARED"));
    }

    #[test]
    fn declared_secret_rejects_blocked_host_env_name() {
        let v = FixedVault(HashMap::new());
        let required = vec!["PATH".to_string()];

        let err = read_required_secrets(&v, "op", &required, &[]).unwrap_err();
        let msg = format!("{err:#}");

        assert!(msg.contains("invalid declared secret"), "got: {msg}");
        assert!(msg.contains("blocked list"), "got: {msg}");
    }

    #[test]
    fn missing_declared_secret_mentions_host_env_source() {
        let _env = EnvVarGuard::set(&[("SNAPTRACK_TEST_MISSING_SECRET", None)]);
        let v = FixedVault(HashMap::new());
        let required = vec!["SNAPTRACK_TEST_MISSING_SECRET".to_string()];

        let err = read_required_secrets(&v, "op", &required, &[]).unwrap_err();
        let msg = format!("{err:#}");

        assert!(msg.contains("SNAPTRACK_TEST_MISSING_SECRET"), "got: {msg}");
        assert!(msg.contains("daemon host environment"), "got: {msg}");
        assert!(msg.contains("sealed vault"), "got: {msg}");
        assert!(msg.contains(".env"), "got: {msg}");
    }

    // ── SealedEnvelopeVault tests ────────────────────────────────

    #[test]
    fn sealed_vault_missing_store_returns_empty() {
        let tmp =
            std::env::temp_dir().join(format!("ryeosd-sealed-missing-{}", std::process::id()));
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
        assert_eq!(v.public_key().fingerprint(), sk.public_key().fingerprint());
        assert_eq!(v.store_path(), default_sealed_store_path(state));
    }

    #[test]
    fn loaded_sealed_vault_reloads_key_after_external_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        let key_path = ryeos_vault::paths::default_vault_secret_key_path(state);
        let public_path = ryeos_vault::paths::default_vault_public_key_path(state);
        let store_path = default_sealed_store_path(state);
        let old_key = lillux::vault::VaultSecretKey::generate();
        lillux::vault::write_secret_key(&key_path, &old_key).unwrap();
        lillux::vault::write_public_key(&public_path, &old_key.public_key()).unwrap();
        let mut initial = HashMap::new();
        initial.insert("TOKEN".to_string(), "old".to_string());
        write_sealed_secrets(&store_path, &old_key.public_key(), &initial).unwrap();

        let vault = SealedEnvelopeVault::load(state).unwrap();
        assert_eq!(vault.read_all("op").unwrap(), initial);

        let new_key = lillux::vault::VaultSecretKey::generate();
        let mut rotated = HashMap::new();
        rotated.insert("TOKEN".to_string(), "rotated".to_string());
        with_store_lock(&store_path, || {
            write_sealed_secrets(&store_path, &new_key.public_key(), &rotated)?;
            lillux::vault::write_public_key(&public_path, &new_key.public_key())?;
            lillux::vault::write_secret_key(&key_path, &new_key)
        })
        .unwrap();

        assert_eq!(vault.read_all("op").unwrap(), rotated);
        vault.set_secret("op", "ADDED", "after-rotation").unwrap();
        let persisted = ryeos_vault::sealed::read_sealed_secrets(&store_path, &new_key).unwrap();
        assert_eq!(persisted.get("TOKEN").map(String::as_str), Some("rotated"));
        assert_eq!(
            persisted.get("ADDED").map(String::as_str),
            Some("after-rotation")
        );
        assert_eq!(
            vault.public_key().fingerprint(),
            new_key.public_key().fingerprint()
        );
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
        std::fs::write(project.path().join(".env"), "API_KEY=project-override\n").unwrap();

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
    fn dotenv_overlay_ignores_unrelated_blocked_name() {
        let tmp = tempfile::tempdir().unwrap();
        // A project `.env` legitimately mixes a declared tool secret with
        // unrelated control config. The blocked control key must be ignored,
        // not fail the resolution of the secret the tool actually declared.
        // (A blocked name can never itself be a declared secret — see
        // `declared_secret_rejects_blocked_host_env_name`.)
        std::fs::write(
            tmp.path().join(".env"),
            "PATH=/evil\nRYEOSD_URL=https://x\nMY_SECRET=ok\n",
        )
        .unwrap();

        let v = FixedVault(HashMap::new());
        let required = vec!["MY_SECRET".to_string()];
        let dirs = vec![tmp.path().to_path_buf()];
        let bindings = read_required_secrets(&v, "op", &required, &dirs).unwrap();
        assert_eq!(bindings.get("MY_SECRET"), Some(&"ok".to_string()));
        assert!(!bindings.contains_key("PATH"));
        assert!(!bindings.contains_key("RYEOSD_URL"));
    }

    #[test]
    fn dotenv_not_consulted_when_vault_satisfies_all() {
        // vault provides every required secret, so a poisoned `.env` (here a
        // malformed line that WOULD fail if parsed for a wanted key) is never
        // read — vault > host > dotenv precedence holds at the I/O level.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "MALFORMED LINE NO EQUALS\n").unwrap();

        let mut all = HashMap::new();
        all.insert("FOO".to_string(), "from-vault".to_string());
        let v = FixedVault(all);

        let required = vec!["FOO".to_string()];
        let dirs = vec![tmp.path().to_path_buf()];
        let bindings = read_required_secrets(&v, "op", &required, &dirs).unwrap();
        assert_eq!(bindings.get("FOO"), Some(&"from-vault".to_string()));
    }

    #[test]
    fn read_explicit_secret_ignores_unrelated_blocked_dotenv_key() {
        // Provider-key path: an unrelated blocked control key in `.env` must
        // not fail resolution of the provider's auth secret.
        let tmp = tempfile::tempdir().unwrap();
        // Unique key name (not ZEN_API_KEY) so a concurrent host-env test can't
        // shadow it — this test reads host env but takes no ENV_LOCK.
        std::fs::write(
            tmp.path().join(".env"),
            "RYEOSD_URL=https://x\nPROVIDER_IGNORE_KEY=zk\n",
        )
        .unwrap();

        let v = FixedVault(HashMap::new());
        let dirs = vec![tmp.path().to_path_buf()];
        let got = read_explicit_secret(&v, "op", "PROVIDER_IGNORE_KEY", &dirs).unwrap();
        assert_eq!(got, Some("zk".to_string()));
    }

    #[test]
    fn host_env_satisfied_skips_poisoned_dotenv() {
        // Host env provides the only required secret; the `.env` (a bare wanted
        // key that WOULD fail if parsed) is never read because dotenv_wanted is
        // empty once vault + host satisfy everything.
        let _env = EnvVarGuard::set(&[("SNAPTRACK_TEST_HOST_POISON", Some("from-host"))]);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "SNAPTRACK_TEST_HOST_POISON\n").unwrap();
        let v = FixedVault(HashMap::new());
        let required = vec!["SNAPTRACK_TEST_HOST_POISON".to_string()];
        let dirs = vec![tmp.path().to_path_buf()];
        let bindings = read_required_secrets(&v, "op", &required, &dirs).unwrap();
        assert_eq!(
            bindings.get("SNAPTRACK_TEST_HOST_POISON"),
            Some(&"from-host".to_string())
        );
    }

    #[test]
    fn read_explicit_secret_vault_beats_host_and_dotenv() {
        let _env = EnvVarGuard::set(&[("ZEN_API_KEY", Some("from-host"))]);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "ZEN_API_KEY=from-dotenv\n").unwrap();
        let mut all = HashMap::new();
        all.insert("ZEN_API_KEY".to_string(), "from-vault".to_string());
        let v = FixedVault(all);
        let dirs = vec![tmp.path().to_path_buf()];
        let got = read_explicit_secret(&v, "op", "ZEN_API_KEY", &dirs).unwrap();
        assert_eq!(got, Some("from-vault".to_string()));
    }

    #[test]
    fn read_explicit_secret_host_beats_dotenv() {
        let _env = EnvVarGuard::set(&[("ZEN_API_KEY", Some("from-host"))]);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "ZEN_API_KEY=from-dotenv\n").unwrap();
        let v = FixedVault(HashMap::new());
        let dirs = vec![tmp.path().to_path_buf()];
        let got = read_explicit_secret(&v, "op", "ZEN_API_KEY", &dirs).unwrap();
        assert_eq!(got, Some("from-host".to_string()));
    }

    #[test]
    fn read_explicit_secret_vault_skips_poisoned_dotenv() {
        // vault satisfies the key, so the bare-wanted-key `.env` (which would
        // fail if parsed) is never read.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "ZEN_API_KEY\n").unwrap();
        let mut all = HashMap::new();
        all.insert("ZEN_API_KEY".to_string(), "from-vault".to_string());
        let v = FixedVault(all);
        let dirs = vec![tmp.path().to_path_buf()];
        let got = read_explicit_secret(&v, "op", "ZEN_API_KEY", &dirs).unwrap();
        assert_eq!(got, Some("from-vault".to_string()));
    }

    #[test]
    fn resolve_secret_sources_reports_per_secret_source() {
        let _env = EnvVarGuard::set(&[("ENVCHECK_HOST", Some("h"))]);
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // project `.env` has a wanted key plus an unrelated blocked control key
        // that must be ignored (not fail the report).
        std::fs::write(
            project.path().join(".env"),
            "ENVCHECK_DOTENV=d\nRYEOSD_URL=x\n",
        )
        .unwrap();
        let mut all = HashMap::new();
        all.insert("ENVCHECK_VAULT".to_string(), "v".to_string());
        let v = FixedVault(all);

        let names = vec![
            "ENVCHECK_VAULT".to_string(),
            "ENVCHECK_HOST".to_string(),
            "ENVCHECK_DOTENV".to_string(),
            "ENVCHECK_MISSING".to_string(),
        ];
        let dirs = vec![user.path().to_path_buf(), project.path().to_path_buf()];
        let report = resolve_secret_sources(&v, "op", &names, &dirs).unwrap();

        // Order preserved.
        let labels: Vec<&str> = report.iter().map(|(_, s)| s.label()).collect();
        assert_eq!(labels, vec!["vault", "host_env", "dotenv", "missing"]);
        assert_eq!(report[0].0, "ENVCHECK_VAULT");

        // The dotenv hit is attributed to the project dir (later overrides user).
        let dotenv = &report[2].1;
        assert!(
            matches!(dotenv, SecretSource::Dotenv(d) if d == project.path()),
            "expected project dotenv dir, got: {dotenv:?}"
        );
    }

    #[test]
    fn resolve_secret_sources_empty_names_reads_nothing() {
        // No declared secrets: report is empty and no source is consulted —
        // matches `read_required_secrets`' empty fast-path.
        let v = FixedVault(HashMap::new());
        let report = resolve_secret_sources(&v, "op", &[], &[]).unwrap();
        assert!(report.is_empty());
    }

    #[test]
    fn resolve_secret_sources_rejects_blocked_name_like_launch() {
        // A blocked name can never be a declared secret; env-check must reject
        // it up front (as a real launch would), not report it as host_env.
        let _env = EnvVarGuard::set(&[("PATH", Some("/evil"))]);
        let v = FixedVault(HashMap::new());
        let err = resolve_secret_sources(&v, "op", &["PATH".to_string()], &[]).unwrap_err();
        assert!(
            format!("{err:#}").contains("PATH") || format!("{err:#}").contains("blocked"),
            "got: {err:#}"
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

    // ── read_named_secret tests ──────────────────────────────────

    #[test]
    fn read_named_secret_present() {
        let mut map = HashMap::new();
        map.insert("ZEN_API_KEY".to_string(), "sk-zen".to_string());
        map.insert("OTHER".to_string(), "other-val".to_string());
        let v = FixedVault(map);

        let result = read_named_secret(&v, "op", "ZEN_API_KEY").unwrap();
        assert_eq!(result, Some("sk-zen".to_string()));
    }

    #[test]
    fn read_named_secret_absent() {
        let mut map = HashMap::new();
        map.insert("OTHER".to_string(), "other-val".to_string());
        let v = FixedVault(map);

        let result = read_named_secret(&v, "op", "ZEN_API_KEY").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn read_named_secret_only_asked_one_back() {
        let mut map = HashMap::new();
        map.insert("ZEN_API_KEY".to_string(), "sk-zen".to_string());
        map.insert("OPENROUTER_API_KEY".to_string(), "sk-or".to_string());
        let v = FixedVault(map);

        let result = read_named_secret(&v, "op", "ZEN_API_KEY").unwrap();
        assert_eq!(result, Some("sk-zen".to_string()));
        // Confirm we only got the one we asked for — no multi-key
        // injection in the return value.
    }

    // ── Vault CRUD tests via SealedEnvelopeVault ──

    #[test]
    fn vault_set_list_delete_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let v = SealedEnvelopeVault::new(store_path, sk);

        // Set secrets
        v.set_secret("op", "API_KEY", "sk-123").unwrap();
        v.set_secret("op", "DB_URL", "postgres://host/db").unwrap();

        // List keys
        let mut keys = v.list_keys("op").unwrap();
        keys.sort();
        assert_eq!(keys, vec!["API_KEY", "DB_URL"]);

        // Read back
        let all = v.read_all("op").unwrap();
        assert_eq!(all.get("API_KEY"), Some(&"sk-123".to_string()));
        assert_eq!(all.get("DB_URL"), Some(&"postgres://host/db".to_string()));

        // Delete one
        let deleted = v.delete_secret("op", "API_KEY").unwrap();
        assert!(deleted, "should return true for existing key");

        // Verify deleted
        let keys = v.list_keys("op").unwrap();
        assert_eq!(keys, vec!["DB_URL"]);

        // Delete non-existent returns false
        let deleted = v.delete_secret("op", "NOPE").unwrap();
        assert!(!deleted, "should return false for missing key");
    }

    #[test]
    fn vault_set_rejects_invalid_key_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let v = SealedEnvelopeVault::new(store_path, sk);

        // Hyphens are not allowed (only [A-Za-z0-9_])
        let err = v.set_secret("op", "my-key", "val").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("key name"),
            "expected key name error, got: {msg}"
        );

        // Blocked name
        let err = v.set_secret("op", "PATH", "/evil").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("blocked"),
            "expected blocked error, got: {msg}"
        );

        // Empty name
        let err = v.set_secret("op", "", "val").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "expected empty error, got: {msg}");
    }

    #[test]
    fn vault_delete_rejects_invalid_key_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let v = SealedEnvelopeVault::new(store_path, sk);

        let err = v.delete_secret("op", "bad-key").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("key name"),
            "expected key name error, got: {msg}"
        );
    }

    #[test]
    fn vault_set_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let v = SealedEnvelopeVault::new(store_path, sk);

        v.set_secret("op", "KEY", "old").unwrap();
        v.set_secret("op", "KEY", "new").unwrap();

        let all = v.read_all("op").unwrap();
        assert_eq!(all.get("KEY"), Some(&"new".to_string()));
        assert_eq!(all.len(), 1, "should have exactly one key");
    }

    #[test]
    fn runtime_bundle_scope_is_hidden_from_operator_env_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let v = SealedEnvelopeVault::new(store_path, sk);
        let scope = VaultScope::runtime_bundle("agent-kiwi", "oauth").unwrap();

        v.put_scoped_secret(&scope, "google_account_123", "refresh-token")
            .unwrap();
        v.set_secret("op", "OPENAI_API_KEY", "sk-op").unwrap();

        assert_eq!(
            v.get_scoped_secret(&scope, "google_account_123").unwrap(),
            Some("refresh-token".to_string())
        );
        assert_eq!(
            read_named_secret(&v, "op", "OPENAI_API_KEY").unwrap(),
            Some("sk-op".into())
        );
        assert_eq!(
            read_named_secret(&v, "op", "google_account_123").unwrap(),
            None
        );
        assert_eq!(
            v.read_all("op").unwrap(),
            HashMap::from([("OPENAI_API_KEY".to_string(), "sk-op".to_string())])
        );
        assert_eq!(v.list_keys("op").unwrap(), vec!["OPENAI_API_KEY"]);
    }

    #[test]
    fn runtime_bundle_scope_disambiguates_namespace_key_underscores() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let v = SealedEnvelopeVault::new(store_path, sk);
        let scope_a = VaultScope::runtime_bundle("agent-kiwi", "a").unwrap();
        let scope_b = VaultScope::runtime_bundle("agent-kiwi", "a_b").unwrap();

        v.put_scoped_secret(&scope_a, "b_c", "one").unwrap();
        v.put_scoped_secret(&scope_b, "c", "two").unwrap();

        assert_eq!(
            v.get_scoped_secret(&scope_a, "b_c").unwrap(),
            Some("one".to_string())
        );
        assert_eq!(
            v.get_scoped_secret(&scope_b, "c").unwrap(),
            Some("two".to_string())
        );
        assert_eq!(v.list_scoped_secret_keys(&scope_a).unwrap(), vec!["b_c"]);
        assert_eq!(v.list_scoped_secret_keys(&scope_b).unwrap(), vec!["c"]);
    }

    #[test]
    fn operator_secret_reads_reject_internal_runtime_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let v = SealedEnvelopeVault::new(store_path, sk);
        let name = format!("{INTERNAL_RUNTIME_VAULT_PREFIX}abc_oauth_key");

        let err = read_required_secrets(&v, "op", std::slice::from_ref(&name), &[]).unwrap_err();
        assert!(format!("{err:#}").contains("reserved internal runtime vault prefix"));
        let err = read_named_secret(&v, "op", &name).unwrap_err();
        assert!(format!("{err:#}").contains("reserved internal runtime vault prefix"));
        let err = read_explicit_secret(&v, "op", &name, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("reserved internal runtime vault prefix"));
    }

    // ── Remote vault E2E simulation ──
    // Tests the vault handler contract (set → list → delete → list)
    // against a real SealedEnvelopeVault. The vault_set / vault_list /
    // vault_delete handlers delegate to these same trait methods.

    #[test]
    fn vault_handler_e2e_set_list_delete_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let store_path = tmp.path().join("store.enc");
        let sk = lillux::vault::VaultSecretKey::generate();
        let vault = SealedEnvelopeVault::new(store_path, sk);

        // Simulate vault_set("API_KEY", "sk-secret-123")
        vault
            .set_secret("caller", "API_KEY", "sk-secret-123")
            .unwrap();
        // Simulate vault_set("DB_URL", "postgres://localhost/db")
        vault
            .set_secret("caller", "DB_URL", "postgres://localhost/db")
            .unwrap();

        // Simulate vault_list
        let mut keys = vault.list_keys("caller").unwrap();
        keys.sort();
        assert_eq!(keys, vec!["API_KEY", "DB_URL"]);

        // Simulate vault_set overwriting (handler returns vault_fingerprint)
        vault.set_secret("caller", "API_KEY", "sk-new-key").unwrap();

        // Verify overwrite
        let all = vault.read_all("caller").unwrap();
        assert_eq!(all.get("API_KEY"), Some(&"sk-new-key".to_string()));
        assert_eq!(all.len(), 2);

        // Simulate vault_delete
        let deleted = vault.delete_secret("caller", "API_KEY").unwrap();
        assert!(deleted);

        // Verify deleted
        let keys = vault.list_keys("caller").unwrap();
        assert_eq!(keys, vec!["DB_URL"]);

        // Simulate vault_delete with invalid name (handler returns 400)
        let err = vault.delete_secret("caller", "bad-key").unwrap_err();
        assert!(format!("{err:#}").contains("key name"));

        // Simulate vault_set with blocked name (handler returns 400)
        let err = vault.set_secret("caller", "PATH", "/evil").unwrap_err();
        assert!(format!("{err:#}").contains("blocked"));
    }
}
